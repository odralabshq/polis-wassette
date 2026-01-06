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
