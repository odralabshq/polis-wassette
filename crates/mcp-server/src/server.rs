// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! MCP Server implementation for handling WebAssembly components.
//!
//! This module provides [`McpServer`] which implements the MCP protocol
//! and can be customized via [`ServerHooks`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParam, CallToolResult, ErrorData, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParam, ServerCapabilities, ServerInfo, ToolsCapability,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;

use crate::hooks::{blocked_result, NoOpHooks, ServerHooks, ToolCallContext, ToolResultContext};
use crate::{handle_prompts_list, handle_resources_list, handle_tools_call, handle_tools_list};
use wassette::LifecycleManager;

/// MCP server for running WebAssembly components.
///
/// # Example
///
/// ```ignore
/// use mcp_server::{McpServer, LifecycleManager};
///
/// // Simple usage
/// let server = McpServer::new(lifecycle_manager, false);
///
/// // With hooks
/// let server = McpServer::builder(lifecycle_manager)
///     .with_hooks(MyHooks)
///     .build();
/// ```
#[derive(Clone)]
pub struct McpServer {
    lifecycle_manager: LifecycleManager,
    peer: Arc<Mutex<Option<rmcp::Peer<rmcp::RoleServer>>>>,
    disable_builtin_tools: bool,
    hooks: Arc<dyn ServerHooks>,
    instructions: Option<String>,
}

impl McpServer {
    /// Creates a new MCP server instance.
    ///
    /// # Arguments
    /// * `lifecycle_manager` - The lifecycle manager for handling component operations
    /// * `disable_builtin_tools` - Whether to disable built-in tools
    pub fn new(lifecycle_manager: LifecycleManager, disable_builtin_tools: bool) -> Self {
        Self {
            lifecycle_manager,
            peer: Arc::new(Mutex::new(None)),
            disable_builtin_tools,
            hooks: Arc::new(NoOpHooks),
            instructions: None,
        }
    }

    /// Create a builder for more advanced configuration.
    pub fn builder(lifecycle_manager: LifecycleManager) -> McpServerBuilder {
        McpServerBuilder::new(lifecycle_manager)
    }

    /// Store the peer for background notifications (called on first request).
    fn store_peer_if_empty(&self, peer: rmcp::Peer<rmcp::RoleServer>) {
        let mut peer_guard = self.peer.lock().unwrap();
        if peer_guard.is_none() {
            *peer_guard = Some(peer);
        }
    }

    /// Get a clone of the stored peer if available.
    pub fn get_peer(&self) -> Option<rmcp::Peer<rmcp::RoleServer>> {
        self.peer.lock().unwrap().clone()
    }

    /// Get the lifecycle manager.
    pub fn lifecycle_manager(&self) -> &LifecycleManager {
        &self.lifecycle_manager
    }

    fn default_instructions() -> String {
        r#"This server runs tools in sandboxed WebAssembly environments with no default access to host resources.

Key points:
- Tools must be loaded before use: "Load component from oci://registry/tool:version" or "file:///path/to/tool.wasm"
- When the server starts, it will load all tools present in the component directory.
- You can list loaded tools with 'list-components' tool.
- Each tool only accesses resources explicitly granted by a policy file (filesystem paths, network domains, etc.)
- You MUST never modify the policy file directly, use tools to grant permissions instead.
- Tools need explicit permission for each resource they access
- If access is denied, suggest alternatives within allowed permissions or propose to grant permission"#.to_string()
    }
}

#[allow(refining_impl_trait_reachable)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(true),
                }),
                ..Default::default()
            },
            instructions: Some(
                self.instructions
                    .clone()
                    .unwrap_or_else(Self::default_instructions),
            ),
            ..Default::default()
        }
    }

    fn call_tool<'a>(
        &'a self,
        params: CallToolRequestParam,
        ctx: RequestContext<RoleServer>,
    ) -> Pin<Box<dyn Future<Output = Result<CallToolResult, ErrorData>> + Send + 'a>> {
        let peer_clone = ctx.peer.clone();
        self.store_peer_if_empty(peer_clone.clone());

        let disable_builtin_tools = self.disable_builtin_tools;
        let hooks = self.hooks.clone();

        Box::pin(async move {
            let start_time = std::time::Instant::now();

            // Create hook context (no cloning yet - arguments borrowed)
            let mut tool_ctx = ToolCallContext::from_params(&params);
            let tool_name = tool_ctx.tool_name.clone();

            // Run before hooks
            if let Err(e) = hooks.before_tool_call(&mut tool_ctx) {
                tracing::error!(error = ?e, "Hook before_tool_call failed");
                return Err(e);
            }

            // Check if blocked
            if tool_ctx.blocked {
                let reason = tool_ctx
                    .block_reason
                    .unwrap_or_else(|| "Blocked by hook".to_string());
                tracing::info!(tool = %tool_name, reason = %reason, "Tool call blocked");
                return Ok(blocked_result(&reason));
            }

            // Get params - only clones arguments if they were modified by hooks
            let metadata = tool_ctx.metadata;
            let final_params = tool_ctx.into_params(params);

            // Execute the tool
            let result = handle_tools_call(
                final_params,
                &self.lifecycle_manager,
                peer_clone,
                disable_builtin_tools,
            )
            .await;

            let duration = start_time.elapsed();

            match result {
                Ok(value) => {
                    let call_result: CallToolResult = serde_json::from_value(value).map_err(|e| {
                        ErrorData::parse_error(format!("Failed to parse result: {e}"), None)
                    })?;

                    // Run after hooks
                    let mut result_ctx = ToolResultContext {
                        tool_name,
                        result: call_result,
                        metadata,
                        duration,
                    };

                    if let Err(e) = hooks.after_tool_call(&mut result_ctx) {
                        tracing::error!(error = ?e, "Hook after_tool_call failed");
                        // Continue with result on hook error
                    }

                    Ok(result_ctx.result)
                }
                Err(err) => Err(ErrorData::parse_error(err.to_string(), None)),
            }
        })
    }

    fn list_tools<'a>(
        &'a self,
        _params: Option<PaginatedRequestParam>,
        ctx: RequestContext<RoleServer>,
    ) -> Pin<Box<dyn Future<Output = Result<ListToolsResult, ErrorData>> + Send + 'a>> {
        self.store_peer_if_empty(ctx.peer.clone());

        let disable_builtin_tools = self.disable_builtin_tools;
        let hooks = self.hooks.clone();

        Box::pin(async move {
            let result = handle_tools_list(&self.lifecycle_manager, disable_builtin_tools).await;

            match result {
                Ok(value) => {
                    let mut list_result: ListToolsResult =
                        serde_json::from_value(value).map_err(|e| {
                            ErrorData::parse_error(format!("Failed to parse result: {e}"), None)
                        })?;

                    // Run hook
                    hooks.on_list_tools(&mut list_result.tools);

                    Ok(list_result)
                }
                Err(err) => Err(ErrorData::parse_error(err.to_string(), None)),
            }
        })
    }

    fn list_prompts<'a>(
        &'a self,
        _params: Option<PaginatedRequestParam>,
        ctx: RequestContext<RoleServer>,
    ) -> Pin<Box<dyn Future<Output = Result<ListPromptsResult, ErrorData>> + Send + 'a>> {
        self.store_peer_if_empty(ctx.peer.clone());

        Box::pin(async move {
            let result = handle_prompts_list(serde_json::Value::Null).await;
            match result {
                Ok(value) => serde_json::from_value(value).map_err(|e| {
                    ErrorData::parse_error(format!("Failed to parse result: {e}"), None)
                }),
                Err(err) => Err(ErrorData::parse_error(err.to_string(), None)),
            }
        })
    }

    fn list_resources<'a>(
        &'a self,
        _params: Option<PaginatedRequestParam>,
        ctx: RequestContext<RoleServer>,
    ) -> Pin<Box<dyn Future<Output = Result<ListResourcesResult, ErrorData>> + Send + 'a>> {
        self.store_peer_if_empty(ctx.peer.clone());

        Box::pin(async move {
            let result = handle_resources_list(serde_json::Value::Null).await;
            match result {
                Ok(value) => serde_json::from_value(value).map_err(|e| {
                    ErrorData::parse_error(format!("Failed to parse result: {e}"), None)
                }),
                Err(err) => Err(ErrorData::parse_error(err.to_string(), None)),
            }
        })
    }
}

/// Builder for [`McpServer`] with advanced configuration options.
///
/// # Example
///
/// ```ignore
/// use mcp_server::{McpServer, MiddlewareStack};
///
/// let hooks = MiddlewareStack::new()
///     .push(LoggingMiddleware)
///     .push(AuthMiddleware::new(key));
///
/// let server = McpServer::builder(lifecycle_manager)
///     .with_builtin_tools_disabled(true)
///     .with_hooks(hooks)
///     .with_instructions("Custom instructions")
///     .build();
/// ```
pub struct McpServerBuilder {
    lifecycle_manager: LifecycleManager,
    disable_builtin_tools: bool,
    hooks: Option<Arc<dyn ServerHooks>>,
    instructions: Option<String>,
}

impl McpServerBuilder {
    /// Create a new builder.
    pub fn new(lifecycle_manager: LifecycleManager) -> Self {
        Self {
            lifecycle_manager,
            disable_builtin_tools: false,
            hooks: None,
            instructions: None,
        }
    }

    /// Disable built-in tools (load-component, unload-component, etc.).
    pub fn with_builtin_tools_disabled(mut self, disabled: bool) -> Self {
        self.disable_builtin_tools = disabled;
        self
    }

    /// Set custom hooks for intercepting requests.
    pub fn with_hooks<H: ServerHooks + 'static>(mut self, hooks: H) -> Self {
        self.hooks = Some(Arc::new(hooks));
        self
    }

    /// Set custom hooks (Arc version).
    pub fn with_hooks_arc(mut self, hooks: Arc<dyn ServerHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Set custom server instructions shown to MCP clients.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Build the server.
    pub fn build(self) -> McpServer {
        McpServer {
            lifecycle_manager: self.lifecycle_manager,
            peer: Arc::new(Mutex::new(None)),
            disable_builtin_tools: self.disable_builtin_tools,
            hooks: self.hooks.unwrap_or_else(|| Arc::new(NoOpHooks)),
            instructions: self.instructions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Tool;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Helper to create a test LifecycleManager
    async fn create_test_lifecycle_manager() -> LifecycleManager {
        let tempdir = tempfile::tempdir().expect("Failed to create temp dir");
        LifecycleManager::new(&tempdir)
            .await
            .expect("Failed to create lifecycle manager")
    }

    // ==================== McpServer::new() Tests ====================

    #[tokio::test]
    async fn test_mcp_server_new_creates_server_with_defaults() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, false);

        // Verify default state
        assert!(!server.disable_builtin_tools);
        assert!(server.instructions.is_none());
        assert!(server.get_peer().is_none());
    }

    #[tokio::test]
    async fn test_mcp_server_new_with_builtin_tools_disabled() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, true);

        assert!(server.disable_builtin_tools);
    }

    // ==================== McpServerBuilder Tests ====================

    #[tokio::test]
    async fn test_builder_creates_server_with_defaults() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::builder(lifecycle_manager).build();

        // Default: builtin tools enabled, no custom instructions
        assert!(!server.disable_builtin_tools);
        assert!(server.instructions.is_none());
    }

    #[tokio::test]
    async fn test_builder_with_builtin_tools_disabled() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(true)
            .build();

        assert!(server.disable_builtin_tools);
    }

    #[tokio::test]
    async fn test_builder_with_builtin_tools_enabled_explicitly() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(false)
            .build();

        assert!(!server.disable_builtin_tools);
    }

    #[tokio::test]
    async fn test_builder_with_custom_instructions() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let custom_instructions = "Custom server instructions for testing";

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions(custom_instructions)
            .build();

        assert_eq!(server.instructions, Some(custom_instructions.to_string()));
    }

    #[tokio::test]
    async fn test_builder_with_instructions_from_string() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let custom_instructions = String::from("Instructions from String type");

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions(custom_instructions.clone())
            .build();

        assert_eq!(server.instructions, Some(custom_instructions));
    }

    #[tokio::test]
    async fn test_builder_chaining_multiple_options() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        let server = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(true)
            .with_instructions("Chained instructions")
            .build();

        assert!(server.disable_builtin_tools);
        assert_eq!(
            server.instructions,
            Some("Chained instructions".to_string())
        );
    }

    // ==================== Hook Integration Tests ====================

    /// Test hook that tracks calls
    struct TrackingHook {
        before_call_count: AtomicUsize,
        after_call_count: AtomicUsize,
        list_tools_count: AtomicUsize,
    }

    impl TrackingHook {
        fn new() -> Self {
            Self {
                before_call_count: AtomicUsize::new(0),
                after_call_count: AtomicUsize::new(0),
                list_tools_count: AtomicUsize::new(0),
            }
        }
    }

    impl ServerHooks for TrackingHook {
        fn before_tool_call(&self, _ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
            self.before_call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn after_tool_call(&self, _ctx: &mut ToolResultContext) -> Result<(), ErrorData> {
            self.after_call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn on_list_tools(&self, _tools: &mut Vec<Tool>) {
            self.list_tools_count.fetch_add(1, Ordering::SeqCst);
        }

        fn name(&self) -> &'static str {
            "tracking_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_hooks() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = TrackingHook::new();

        // Verify hook is accepted by builder
        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    #[tokio::test]
    async fn test_builder_with_hooks_arc() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = Arc::new(TrackingHook::new());

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks_arc(hook)
            .build();
    }

    /// Hook that blocks tool calls
    struct BlockingHook {
        block_reason: String,
    }

    impl BlockingHook {
        fn new(reason: &str) -> Self {
            Self {
                block_reason: reason.to_string(),
            }
        }
    }

    impl ServerHooks for BlockingHook {
        fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
            ctx.block(&self.block_reason);
            Ok(())
        }

        fn name(&self) -> &'static str {
            "blocking_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_blocking_hook() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = BlockingHook::new("Access denied by policy");

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    /// Hook that modifies tool arguments
    struct ArgumentModifyingHook {
        key_to_add: String,
        value_to_add: serde_json::Value,
    }

    impl ArgumentModifyingHook {
        fn new(key: &str, value: serde_json::Value) -> Self {
            Self {
                key_to_add: key.to_string(),
                value_to_add: value,
            }
        }
    }

    impl ServerHooks for ArgumentModifyingHook {
        fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
            let args = ctx.arguments_mut().get_or_insert_with(serde_json::Map::new);
            args.insert(self.key_to_add.clone(), self.value_to_add.clone());
            Ok(())
        }

        fn name(&self) -> &'static str {
            "argument_modifying_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_argument_modifying_hook() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = ArgumentModifyingHook::new("injected_key", json!("injected_value"));

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    /// Hook that filters tools from list
    struct ToolFilteringHook {
        prefix_to_hide: String,
    }

    impl ToolFilteringHook {
        fn new(prefix: &str) -> Self {
            Self {
                prefix_to_hide: prefix.to_string(),
            }
        }
    }

    impl ServerHooks for ToolFilteringHook {
        fn on_list_tools(&self, tools: &mut Vec<Tool>) {
            tools.retain(|t| !t.name.as_ref().starts_with(&self.prefix_to_hide));
        }

        fn name(&self) -> &'static str {
            "tool_filtering_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_tool_filtering_hook() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = ToolFilteringHook::new("internal-");

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    #[tokio::test]
    async fn test_builder_with_middleware_stack() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        let stack = MiddlewareStack::new()
            .push(TrackingHook::new())
            .push(ToolFilteringHook::new("debug-"));

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(stack)
            .build();
    }

    // ==================== lifecycle_manager() Getter Tests ====================

    #[tokio::test]
    async fn test_lifecycle_manager_getter_returns_reference() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, false);

        // Verify we can access the lifecycle manager
        let _lm_ref = server.lifecycle_manager();
    }

    #[tokio::test]
    async fn test_lifecycle_manager_getter_from_builder() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::builder(lifecycle_manager).build();

        let _lm_ref = server.lifecycle_manager();
    }

    // ==================== Instructions Tests ====================

    #[tokio::test]
    async fn test_default_instructions_content() {
        let default_instructions = McpServer::default_instructions();

        // Verify key content in default instructions
        assert!(default_instructions.contains("WebAssembly"));
        assert!(default_instructions.contains("sandboxed"));
        assert!(default_instructions.contains("permission"));
        assert!(default_instructions.contains("load"));
    }

    #[tokio::test]
    async fn test_get_info_returns_default_instructions_when_none_set() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, false);

        let info = server.get_info();

        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("WebAssembly"));
    }

    #[tokio::test]
    async fn test_get_info_returns_custom_instructions_when_set() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let custom = "My custom instructions";

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions(custom)
            .build();

        let info = server.get_info();

        assert!(info.instructions.is_some());
        assert_eq!(info.instructions.unwrap(), custom);
    }

    #[tokio::test]
    async fn test_get_info_capabilities() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, false);

        let info = server.get_info();

        // Verify tools capability is set
        assert!(info.capabilities.tools.is_some());
        let tools_cap = info.capabilities.tools.unwrap();
        assert_eq!(tools_cap.list_changed, Some(true));
    }

    // ==================== Peer Management Tests ====================

    #[tokio::test]
    async fn test_get_peer_returns_none_initially() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::new(lifecycle_manager, false);

        assert!(server.get_peer().is_none());
    }

    // ==================== Clone Tests ====================

    #[tokio::test]
    async fn test_server_is_cloneable() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let server = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(true)
            .with_instructions("Test instructions")
            .build();

        let cloned = server.clone();

        // Verify cloned server has same configuration
        assert!(cloned.disable_builtin_tools);
        assert_eq!(cloned.instructions, Some("Test instructions".to_string()));
    }

    // ==================== Complex Configuration Tests ====================

    #[tokio::test]
    async fn test_full_configuration_scenario() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        // Create a complex middleware stack
        let stack = MiddlewareStack::new()
            .push(TrackingHook::new())
            .push(ToolFilteringHook::new("hidden-"))
            .push(NoOpHooks);

        let server = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(true)
            .with_hooks(stack)
            .with_instructions("Production server with security hooks enabled")
            .build();

        // Verify all configurations applied
        assert!(server.disable_builtin_tools);
        assert_eq!(
            server.instructions,
            Some("Production server with security hooks enabled".to_string())
        );

        // Verify server info reflects configuration
        let info = server.get_info();
        assert_eq!(
            info.instructions.unwrap(),
            "Production server with security hooks enabled"
        );
    }

    #[tokio::test]
    async fn test_builder_order_independence() {
        let lifecycle_manager1 = create_test_lifecycle_manager().await;
        let lifecycle_manager2 = create_test_lifecycle_manager().await;

        // Build with options in different orders
        let server1 = McpServer::builder(lifecycle_manager1)
            .with_instructions("Instructions")
            .with_builtin_tools_disabled(true)
            .build();

        let server2 = McpServer::builder(lifecycle_manager2)
            .with_builtin_tools_disabled(true)
            .with_instructions("Instructions")
            .build();

        // Both should have same configuration
        assert_eq!(server1.disable_builtin_tools, server2.disable_builtin_tools);
        assert_eq!(server1.instructions, server2.instructions);
    }

    // ==================== Error Hook Tests ====================

    /// Hook that returns an error
    struct ErrorHook {
        error_message: String,
    }

    impl ErrorHook {
        fn new(message: &str) -> Self {
            Self {
                error_message: message.to_string(),
            }
        }
    }

    impl ServerHooks for ErrorHook {
        fn before_tool_call(&self, _ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
            Err(ErrorData::internal_error(
                self.error_message.clone(),
                None::<()>,
            ))
        }

        fn name(&self) -> &'static str {
            "error_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_error_hook() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = ErrorHook::new("Simulated hook failure");

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    // ==================== Metadata Passing Hook Tests ====================

    /// Hook that adds metadata
    struct MetadataHook {
        key: String,
        value: serde_json::Value,
    }

    impl MetadataHook {
        fn new(key: &str, value: serde_json::Value) -> Self {
            Self {
                key: key.to_string(),
                value,
            }
        }
    }

    impl ServerHooks for MetadataHook {
        fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
            ctx.metadata.insert(self.key.clone(), self.value.clone());
            Ok(())
        }

        fn name(&self) -> &'static str {
            "metadata_hook"
        }
    }

    #[tokio::test]
    async fn test_builder_with_metadata_hook() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let hook = MetadataHook::new("request_id", json!("test-123"));

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(hook)
            .build();
    }

    #[tokio::test]
    async fn test_middleware_stack_with_metadata_passing() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        let stack = MiddlewareStack::new()
            .push(MetadataHook::new("step1", json!("value1")))
            .push(MetadataHook::new("step2", json!("value2")));

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(stack)
            .build();
    }

    // ==================== Edge Case Tests ====================

    #[tokio::test]
    async fn test_empty_instructions_string() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions("")
            .build();

        // Empty string is still Some("")
        assert_eq!(server.instructions, Some(String::new()));

        let info = server.get_info();
        assert_eq!(info.instructions, Some(String::new()));
    }

    #[tokio::test]
    async fn test_very_long_instructions() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let long_instructions = "x".repeat(10000);

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions(long_instructions.clone())
            .build();

        assert_eq!(server.instructions, Some(long_instructions));
    }

    #[tokio::test]
    async fn test_instructions_with_special_characters() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let special_instructions = "Instructions with émojis 🚀 and unicode: 日本語";

        let server = McpServer::builder(lifecycle_manager)
            .with_instructions(special_instructions)
            .build();

        assert_eq!(
            server.instructions,
            Some(special_instructions.to_string())
        );
    }

    #[tokio::test]
    async fn test_empty_middleware_stack() {
        let lifecycle_manager = create_test_lifecycle_manager().await;
        let empty_stack = MiddlewareStack::new();

        let _server = McpServer::builder(lifecycle_manager)
            .with_hooks(empty_stack)
            .build();
    }

    // ==================== Builder Reuse Tests ====================

    #[tokio::test]
    async fn test_builder_consumed_on_build() {
        let lifecycle_manager = create_test_lifecycle_manager().await;

        let builder = McpServer::builder(lifecycle_manager)
            .with_builtin_tools_disabled(true);

        // Builder is consumed here
        let _server = builder.build();

        // Cannot reuse builder (this is enforced by Rust's ownership system)
        // The test verifies the builder pattern works correctly
    }
}
