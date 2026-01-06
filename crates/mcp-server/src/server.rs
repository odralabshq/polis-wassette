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
- Tools need permission for that resource
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

            // Create hook context
            let mut tool_ctx = ToolCallContext::from_params(&params);

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
                tracing::info!(tool = %tool_ctx.tool_name, reason = %reason, "Tool call blocked");
                return Ok(blocked_result(&reason));
            }

            // Rebuild params with potentially modified arguments
            let modified_params = tool_ctx.to_params();

            // Execute the tool
            let result = handle_tools_call(
                modified_params,
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
                        tool_name: tool_ctx.tool_name,
                        result: call_result,
                        metadata: tool_ctx.metadata,
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
