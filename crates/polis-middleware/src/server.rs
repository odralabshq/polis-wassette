// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! PolisServer - Wrapper around McpServer with middleware support

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use mcp_server::{
    handle_prompts_list, handle_resources_list, handle_tools_call, handle_tools_list,
    LifecycleManager,
};
use rmcp::model::{
    CallToolRequestParam, CallToolResult, ErrorData, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParam, ServerCapabilities, ServerInfo, ToolsCapability,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;

use crate::context::{ToolCallContext, ToolCallResultContext, ToolListContext};
use crate::middleware::{blocked_result, MiddlewareChain};

/// MCP Server with middleware support
///
/// This wraps the core Wassette functionality and adds middleware hooks
/// for the request/response lifecycle.
#[derive(Clone)]
pub struct PolisServer {
    lifecycle_manager: LifecycleManager,
    peer: Arc<Mutex<Option<rmcp::Peer<rmcp::RoleServer>>>>,
    disable_builtin_tools: bool,
    middleware: MiddlewareChain,
    server_instructions: Option<String>,
}

impl PolisServer {
    /// Creates a new Polis server with middleware support
    ///
    /// # Arguments
    /// * `lifecycle_manager` - The lifecycle manager for handling component operations
    /// * `disable_builtin_tools` - Whether to disable built-in tools
    /// * `middleware` - The middleware chain to execute on requests
    pub fn new(
        lifecycle_manager: LifecycleManager,
        disable_builtin_tools: bool,
        middleware: MiddlewareChain,
    ) -> Self {
        Self {
            lifecycle_manager,
            peer: Arc::new(Mutex::new(None)),
            disable_builtin_tools,
            middleware,
            server_instructions: None,
        }
    }

    /// Set custom server instructions (shown to MCP clients)
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.server_instructions = Some(instructions.into());
        self
    }

    /// Store the peer for background notifications
    fn store_peer_if_empty(&self, peer: rmcp::Peer<rmcp::RoleServer>) {
        let mut peer_guard = self.peer.lock().unwrap();
        if peer_guard.is_none() {
            *peer_guard = Some(peer);
        }
    }

    /// Get a clone of the stored peer if available
    pub fn get_peer(&self) -> Option<rmcp::Peer<rmcp::RoleServer>> {
        self.peer.lock().unwrap().clone()
    }

    /// Get the lifecycle manager
    pub fn lifecycle_manager(&self) -> &LifecycleManager {
        &self.lifecycle_manager
    }

    /// Get the middleware chain
    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    fn default_instructions() -> String {
        r#"This server runs tools in sandboxed WebAssembly environments with no default access to host resources.

Key points:
- Tools must be loaded before use: "Load component from oci://registry/tool:version" or "file:///path/to/tool.wasm"
- When the server starts, it will load all tools present in the component directory.
- You can list loaded tools with 'list-components' tool.
- Each tool only accesses resources explicitly granted by a policy file (filesystem paths, network domains, etc.)
- You MUST never modify the policy file directly, use tools to grant permissions instead.
- Tools needs permission for that resource
- If access is denied, suggest alternatives within allowed permissions or propose to grant permission"#.to_string()
    }
}

#[allow(refining_impl_trait_reachable)]
impl ServerHandler for PolisServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(true),
                }),
                ..Default::default()
            },
            instructions: Some(
                self.server_instructions
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
        let middleware = self.middleware.clone();

        Box::pin(async move {
            // Create middleware context
            let mut tool_ctx = ToolCallContext::from_params(&params);
            let start_time = std::time::Instant::now();

            // Run before hooks
            if let Err(e) = middleware.run_before_tool_call(&mut tool_ctx).await {
                tracing::error!(error = %e, "Middleware before_tool_call failed");
                return Err(ErrorData::internal_error(e.message, None));
            }

            // Check if middleware blocked the call
            if tool_ctx.skip_execution {
                let reason = tool_ctx
                    .skip_reason
                    .unwrap_or_else(|| "Blocked by middleware".to_string());
                tracing::info!(
                    tool = %tool_ctx.tool_name,
                    reason = %reason,
                    "Tool call blocked"
                );
                return Ok(blocked_result(&reason));
            }

            // Rebuild params with potentially modified arguments
            let modified_params = tool_ctx.to_params();

            // Execute the actual tool call
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
                    let mut result_ctx = ToolCallResultContext {
                        tool_name: tool_ctx.tool_name,
                        result: call_result,
                        metadata: tool_ctx.metadata,
                        duration,
                    };

                    if let Err(e) = middleware.run_after_tool_call(&mut result_ctx).await {
                        tracing::error!(error = %e, "Middleware after_tool_call failed");
                        // Continue with original result on middleware error
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
        let middleware = self.middleware.clone();

        Box::pin(async move {
            let result = handle_tools_list(&self.lifecycle_manager, disable_builtin_tools).await;

            match result {
                Ok(value) => {
                    let mut list_result: ListToolsResult =
                        serde_json::from_value(value).map_err(|e| {
                            ErrorData::parse_error(format!("Failed to parse result: {e}"), None)
                        })?;

                    // Run middleware hooks
                    let mut list_ctx = ToolListContext::new(list_result.tools);

                    if let Err(e) = middleware.run_on_list_tools(&mut list_ctx).await {
                        tracing::error!(error = %e, "Middleware on_list_tools failed");
                        // Continue with original list on middleware error
                    } else {
                        list_result.tools = list_ctx.tools;
                    }

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
