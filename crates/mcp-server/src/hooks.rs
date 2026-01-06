// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Server hooks for intercepting MCP request/response lifecycle.
//!
//! This module provides the [`ServerHooks`] trait for customizing server behavior
//! and [`MiddlewareStack`] for chaining multiple hooks together.

use rmcp::model::{CallToolRequestParam, CallToolResult, ErrorData, Tool};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Context passed to hooks before a tool call.
#[derive(Debug)]
pub struct ToolCallContext {
    /// The tool name being called
    pub tool_name: String,
    /// The arguments passed to the tool (mutable for transformation)
    pub arguments: Option<serde_json::Map<String, Value>>,
    /// Request metadata for sharing data between hooks
    pub metadata: HashMap<String, Value>,
    /// Set to true to block execution
    pub blocked: bool,
    /// Reason for blocking (returned to client)
    pub block_reason: Option<String>,
}

impl ToolCallContext {
    /// Create context from request params
    pub fn from_params(params: &CallToolRequestParam) -> Self {
        Self {
            tool_name: params.name.to_string(),
            arguments: params.arguments.clone(),
            metadata: HashMap::new(),
            blocked: false,
            block_reason: None,
        }
    }

    /// Block this tool call with a reason
    pub fn block(&mut self, reason: impl Into<String>) {
        self.blocked = true;
        self.block_reason = Some(reason.into());
    }

    /// Rebuild params with potentially modified arguments
    pub fn to_params(&self) -> CallToolRequestParam {
        CallToolRequestParam {
            name: self.tool_name.clone().into(),
            arguments: self.arguments.clone(),
        }
    }
}

/// Context passed to hooks after a tool call completes.
#[derive(Debug)]
pub struct ToolResultContext {
    /// The tool name that was called
    pub tool_name: String,
    /// The result (mutable for transformation)
    pub result: CallToolResult,
    /// Request metadata (same instance as before_tool_call)
    pub metadata: HashMap<String, Value>,
    /// Execution duration
    pub duration: std::time::Duration,
}

/// Hooks for customizing MCP server behavior.
///
/// Implement this trait to intercept and modify requests/responses.
/// All methods have default no-op implementations.
///
/// # Example
///
/// ```ignore
/// use mcp_server::{ServerHooks, ToolCallContext};
///
/// struct LoggingHooks;
///
/// impl ServerHooks for LoggingHooks {
///     fn before_tool_call(&self, ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
///         tracing::info!("Calling tool: {}", ctx.tool_name);
///         Ok(())
///     }
/// }
/// ```
pub trait ServerHooks: Send + Sync {
    /// Called before a tool is executed.
    ///
    /// Use this to:
    /// - Validate or transform arguments
    /// - Block calls by calling `ctx.block("reason")`
    /// - Add metadata for later hooks
    fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
        Ok(())
    }

    /// Called after a tool is executed successfully.
    ///
    /// Use this to:
    /// - Transform or filter results
    /// - Log execution metrics
    /// - Audit trail
    fn after_tool_call(&self, _ctx: &mut ToolResultContext) -> Result<(), ErrorData> {
        Ok(())
    }

    /// Called when the tool list is requested.
    ///
    /// Use this to filter or modify the visible tools.
    fn on_list_tools(&self, _tools: &mut Vec<Tool>) {}

    /// Hook name for logging/debugging.
    fn name(&self) -> &'static str {
        "unnamed"
    }
}

/// Default no-op hooks implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpHooks;

impl ServerHooks for NoOpHooks {}

/// A stack of middleware that executes hooks in order.
///
/// # Example
///
/// ```ignore
/// use mcp_server::{MiddlewareStack, ServerHooks};
///
/// let stack = MiddlewareStack::new()
///     .push(LoggingMiddleware)
///     .push(AuthMiddleware::new(api_key))
///     .push(RateLimitMiddleware::new(100));
///
/// let server = McpServer::builder(lifecycle_manager)
///     .with_hooks(stack)
///     .build();
/// ```
pub struct MiddlewareStack {
    middlewares: Vec<Arc<dyn ServerHooks>>,
}

impl Default for MiddlewareStack {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareStack {
    /// Create an empty middleware stack.
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    /// Add a middleware to the stack.
    pub fn push<H: ServerHooks + 'static>(mut self, hooks: H) -> Self {
        self.middlewares.push(Arc::new(hooks));
        self
    }

    /// Add a middleware to the stack (Arc version).
    pub fn push_arc(mut self, hooks: Arc<dyn ServerHooks>) -> Self {
        self.middlewares.push(hooks);
        self
    }

    /// Check if stack is empty.
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Get number of middlewares.
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }
}

impl ServerHooks for MiddlewareStack {
    fn before_tool_call(&self, ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
        for middleware in &self.middlewares {
            tracing::trace!(hook = middleware.name(), tool = %ctx.tool_name, "before_tool_call");
            middleware.before_tool_call(ctx)?;
            if ctx.blocked {
                tracing::debug!(
                    hook = middleware.name(),
                    tool = %ctx.tool_name,
                    reason = ?ctx.block_reason,
                    "Tool call blocked"
                );
                break;
            }
        }
        Ok(())
    }

    fn after_tool_call(&self, ctx: &mut ToolResultContext) -> Result<(), ErrorData> {
        // Run in reverse order (like middleware unwinding)
        for middleware in self.middlewares.iter().rev() {
            tracing::trace!(hook = middleware.name(), tool = %ctx.tool_name, "after_tool_call");
            middleware.after_tool_call(ctx)?;
        }
        Ok(())
    }

    fn on_list_tools(&self, tools: &mut Vec<Tool>) {
        for middleware in &self.middlewares {
            tracing::trace!(hook = middleware.name(), "on_list_tools");
            middleware.on_list_tools(tools);
        }
    }

    fn name(&self) -> &'static str {
        "middleware_stack"
    }
}

/// Create a blocked tool result.
pub fn blocked_result(reason: &str) -> CallToolResult {
    CallToolResult {
        content: Some(vec![rmcp::model::Content::text(format!(
            "Tool call blocked: {}",
            reason
        ))]),
        structured_content: None,
        is_error: Some(true),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Helper to create a basic ToolCallContext
    fn make_tool_context(name: &str) -> ToolCallContext {
        ToolCallContext {
            tool_name: name.to_string(),
            arguments: None,
            metadata: HashMap::new(),
            blocked: false,
            block_reason: None,
        }
    }

    // Helper to create a basic ToolResultContext
    fn make_result_context(name: &str) -> ToolResultContext {
        ToolResultContext {
            tool_name: name.to_string(),
            result: CallToolResult {
                content: Some(vec![Content::text("test result")]),
                structured_content: None,
                is_error: None,
            },
            metadata: HashMap::new(),
            duration: std::time::Duration::from_millis(100),
        }
    }

    #[test]
    fn test_noop_hooks_default_behavior() {
        let hooks = NoOpHooks;

        // before_tool_call should succeed without modification
        let mut ctx = make_tool_context("test_tool");
        assert!(hooks.before_tool_call(&mut ctx).is_ok());
        assert!(!ctx.blocked);
        assert!(ctx.block_reason.is_none());

        // after_tool_call should succeed without modification
        let mut result_ctx = make_result_context("test_tool");
        assert!(hooks.after_tool_call(&mut result_ctx).is_ok());

        // on_list_tools should not modify the list
        let mut tools = vec![Tool {
            name: "tool1".into(),
            description: Some("desc".into()),
            input_schema: serde_json::json!({}),
            annotations: None,
        }];
        let original_len = tools.len();
        hooks.on_list_tools(&mut tools);
        assert_eq!(tools.len(), original_len);
    }

    #[test]
    fn test_tool_call_context_block() {
        let mut ctx = make_tool_context("test_tool");
        assert!(!ctx.blocked);
        assert!(ctx.block_reason.is_none());

        ctx.block("Access denied");

        assert!(ctx.blocked);
        assert_eq!(ctx.block_reason, Some("Access denied".to_string()));
    }

    #[test]
    fn test_tool_call_context_from_params() {
        let params = CallToolRequestParam {
            name: "my_tool".into(),
            arguments: Some(serde_json::Map::from_iter([(
                "key".to_string(),
                Value::String("value".to_string()),
            )])),
        };

        let ctx = ToolCallContext::from_params(&params);
        assert_eq!(ctx.tool_name, "my_tool");
        assert!(ctx.arguments.is_some());
        assert!(!ctx.blocked);
    }

    #[test]
    fn test_tool_call_context_to_params() {
        let mut ctx = make_tool_context("test_tool");
        ctx.arguments = Some(serde_json::Map::from_iter([(
            "arg1".to_string(),
            Value::Number(42.into()),
        )]));

        let params = ctx.to_params();
        assert_eq!(params.name.as_ref(), "test_tool");
        assert!(params.arguments.is_some());
    }

    #[test]
    fn test_middleware_stack_execution_order() {
        // Track execution order using atomic counter
        static BEFORE_ORDER: AtomicUsize = AtomicUsize::new(0);
        static AFTER_ORDER: AtomicUsize = AtomicUsize::new(0);

        struct OrderTracker {
            id: usize,
            before_order: std::sync::Mutex<Option<usize>>,
            after_order: std::sync::Mutex<Option<usize>>,
        }

        impl ServerHooks for OrderTracker {
            fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                let order = BEFORE_ORDER.fetch_add(1, Ordering::SeqCst);
                *self.before_order.lock().unwrap() = Some(order);
                Ok(())
            }

            fn after_tool_call(&self, _ctx: &mut ToolResultContext) -> Result<(), ErrorData> {
                let order = AFTER_ORDER.fetch_add(1, Ordering::SeqCst);
                *self.after_order.lock().unwrap() = Some(order);
                Ok(())
            }

            fn name(&self) -> &'static str {
                "order_tracker"
            }
        }

        // Reset counters
        BEFORE_ORDER.store(0, Ordering::SeqCst);
        AFTER_ORDER.store(0, Ordering::SeqCst);

        let tracker1 = Arc::new(OrderTracker {
            id: 1,
            before_order: std::sync::Mutex::new(None),
            after_order: std::sync::Mutex::new(None),
        });
        let tracker2 = Arc::new(OrderTracker {
            id: 2,
            before_order: std::sync::Mutex::new(None),
            after_order: std::sync::Mutex::new(None),
        });
        let tracker3 = Arc::new(OrderTracker {
            id: 3,
            before_order: std::sync::Mutex::new(None),
            after_order: std::sync::Mutex::new(None),
        });

        let stack = MiddlewareStack::new()
            .push_arc(tracker1.clone())
            .push_arc(tracker2.clone())
            .push_arc(tracker3.clone());

        let mut ctx = make_tool_context("test");
        stack.before_tool_call(&mut ctx).unwrap();

        let mut result_ctx = make_result_context("test");
        stack.after_tool_call(&mut result_ctx).unwrap();

        // Before hooks run in order: 1, 2, 3
        assert_eq!(*tracker1.before_order.lock().unwrap(), Some(0));
        assert_eq!(*tracker2.before_order.lock().unwrap(), Some(1));
        assert_eq!(*tracker3.before_order.lock().unwrap(), Some(2));

        // After hooks run in reverse: 3, 2, 1
        assert_eq!(*tracker3.after_order.lock().unwrap(), Some(0));
        assert_eq!(*tracker2.after_order.lock().unwrap(), Some(1));
        assert_eq!(*tracker1.after_order.lock().unwrap(), Some(2));
    }

    #[test]
    fn test_middleware_stack_blocking_behavior() {
        struct BlockingHook;

        impl ServerHooks for BlockingHook {
            fn before_tool_call(&self, ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                ctx.block("Blocked by policy");
                Ok(())
            }

            fn name(&self) -> &'static str {
                "blocking_hook"
            }
        }

        struct AfterBlockHook {
            called: std::sync::Mutex<bool>,
        }

        impl ServerHooks for AfterBlockHook {
            fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                *self.called.lock().unwrap() = true;
                Ok(())
            }

            fn name(&self) -> &'static str {
                "after_block_hook"
            }
        }

        let after_hook = Arc::new(AfterBlockHook {
            called: std::sync::Mutex::new(false),
        });

        let stack = MiddlewareStack::new()
            .push(BlockingHook)
            .push_arc(after_hook.clone());

        let mut ctx = make_tool_context("test");
        stack.before_tool_call(&mut ctx).unwrap();

        // Should be blocked
        assert!(ctx.blocked);
        assert_eq!(ctx.block_reason, Some("Blocked by policy".to_string()));

        // Hook after blocking hook should NOT be called
        assert!(!*after_hook.called.lock().unwrap());
    }

    #[test]
    fn test_metadata_passing_between_hooks() {
        struct MetadataWriter;

        impl ServerHooks for MetadataWriter {
            fn before_tool_call(&self, ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                ctx.metadata
                    .insert("request_id".to_string(), Value::String("abc123".to_string()));
                ctx.metadata
                    .insert("timestamp".to_string(), Value::Number(12345.into()));
                Ok(())
            }

            fn name(&self) -> &'static str {
                "metadata_writer"
            }
        }

        struct MetadataReader {
            found_request_id: std::sync::Mutex<Option<String>>,
        }

        impl ServerHooks for MetadataReader {
            fn before_tool_call(&self, ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                if let Some(Value::String(id)) = ctx.metadata.get("request_id") {
                    *self.found_request_id.lock().unwrap() = Some(id.clone());
                }
                Ok(())
            }

            fn name(&self) -> &'static str {
                "metadata_reader"
            }
        }

        let reader = Arc::new(MetadataReader {
            found_request_id: std::sync::Mutex::new(None),
        });

        let stack = MiddlewareStack::new()
            .push(MetadataWriter)
            .push_arc(reader.clone());

        let mut ctx = make_tool_context("test");
        stack.before_tool_call(&mut ctx).unwrap();

        // Reader should have found the metadata written by writer
        assert_eq!(
            *reader.found_request_id.lock().unwrap(),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_error_handling_in_hooks() {
        struct ErrorHook;

        impl ServerHooks for ErrorHook {
            fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                Err(ErrorData::internal_error(
                    "Hook failed".to_string(),
                    None::<()>,
                ))
            }

            fn name(&self) -> &'static str {
                "error_hook"
            }
        }

        struct NeverCalledHook {
            called: std::sync::Mutex<bool>,
        }

        impl ServerHooks for NeverCalledHook {
            fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> Result<(), ErrorData> {
                *self.called.lock().unwrap() = true;
                Ok(())
            }

            fn name(&self) -> &'static str {
                "never_called"
            }
        }

        let never_called = Arc::new(NeverCalledHook {
            called: std::sync::Mutex::new(false),
        });

        let stack = MiddlewareStack::new()
            .push(ErrorHook)
            .push_arc(never_called.clone());

        let mut ctx = make_tool_context("test");
        let result = stack.before_tool_call(&mut ctx);

        // Should return error
        assert!(result.is_err());

        // Hook after error should NOT be called
        assert!(!*never_called.called.lock().unwrap());
    }

    #[test]
    fn test_middleware_stack_len_and_is_empty() {
        let empty_stack = MiddlewareStack::new();
        assert!(empty_stack.is_empty());
        assert_eq!(empty_stack.len(), 0);

        let stack = MiddlewareStack::new().push(NoOpHooks).push(NoOpHooks);
        assert!(!stack.is_empty());
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn test_blocked_result_helper() {
        let result = blocked_result("Access denied");

        assert_eq!(result.is_error, Some(true));
        assert!(result.content.is_some());

        let content = result.content.unwrap();
        assert_eq!(content.len(), 1);

        // Check the text content contains the reason
        if let Content::Text(text_content) = &content[0] {
            assert!(text_content.text.contains("Access denied"));
            assert!(text_content.text.contains("blocked"));
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_on_list_tools_filtering() {
        struct ToolFilter;

        impl ServerHooks for ToolFilter {
            fn on_list_tools(&self, tools: &mut Vec<Tool>) {
                tools.retain(|t| !t.name.as_ref().starts_with("internal_"));
            }

            fn name(&self) -> &'static str {
                "tool_filter"
            }
        }

        let stack = MiddlewareStack::new().push(ToolFilter);

        let mut tools = vec![
            Tool {
                name: "public_tool".into(),
                description: Some("Public".into()),
                input_schema: serde_json::json!({}),
                annotations: None,
            },
            Tool {
                name: "internal_debug".into(),
                description: Some("Internal".into()),
                input_schema: serde_json::json!({}),
                annotations: None,
            },
            Tool {
                name: "another_public".into(),
                description: Some("Another".into()),
                input_schema: serde_json::json!({}),
                annotations: None,
            },
        ];

        stack.on_list_tools(&mut tools);

        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| !t.name.as_ref().starts_with("internal_")));
    }

    #[test]
    fn test_middleware_stack_default() {
        let stack = MiddlewareStack::default();
        assert!(stack.is_empty());
    }
}
