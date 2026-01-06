// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Middleware trait and chain implementation

use crate::context::{ToolCallContext, ToolCallResultContext, ToolListContext};
use async_trait::async_trait;
use rmcp::model::CallToolResult;
use std::sync::Arc;

/// Result type for middleware operations
pub type MiddlewareResult<T> = Result<T, MiddlewareError>;

/// Error type for middleware operations
#[derive(Debug, Clone)]
pub struct MiddlewareError {
    /// Error message
    pub message: String,
    /// Whether this error should be returned to the client
    pub is_client_error: bool,
}

impl MiddlewareError {
    /// Create a new middleware error
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_client_error: true,
        }
    }

    /// Create an internal error (logged but not exposed to client)
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_client_error: false,
        }
    }
}

impl std::fmt::Display for MiddlewareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for MiddlewareError {}

/// Middleware trait for hooking into MCP request/response lifecycle
///
/// Implement this trait to create custom middleware that can:
/// - Inspect and modify tool call arguments
/// - Block tool calls based on custom logic
/// - Transform tool results
/// - Filter the tool list
/// - Add logging, metrics, or audit trails
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Called before a tool is executed
    ///
    /// Use this to:
    /// - Validate or transform arguments
    /// - Block calls by setting `ctx.skip_execution = true`
    /// - Add request metadata
    async fn before_tool_call(&self, _ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        Ok(())
    }

    /// Called after a tool is executed (only if not skipped)
    ///
    /// Use this to:
    /// - Transform or filter results
    /// - Log execution metrics
    /// - Audit trail
    async fn after_tool_call(&self, _ctx: &mut ToolCallResultContext) -> MiddlewareResult<()> {
        Ok(())
    }

    /// Called when tool list is requested
    ///
    /// Use this to:
    /// - Filter available tools based on permissions
    /// - Add or modify tool metadata
    async fn on_list_tools(&self, _ctx: &mut ToolListContext) -> MiddlewareResult<()> {
        Ok(())
    }

    /// Middleware name for logging/debugging
    fn name(&self) -> &'static str {
        "unnamed"
    }
}

/// A chain of middleware that executes in order
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl Default for MiddlewareChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareChain {
    /// Create an empty middleware chain
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    /// Add a middleware to the chain
    pub fn with<M: Middleware + 'static>(mut self, middleware: M) -> Self {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    /// Add a middleware to the chain (Arc version)
    pub fn with_arc(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middlewares.push(middleware);
        self
    }

    /// Execute before_tool_call on all middlewares
    pub async fn run_before_tool_call(
        &self,
        ctx: &mut ToolCallContext,
    ) -> MiddlewareResult<()> {
        for middleware in &self.middlewares {
            tracing::trace!(
                middleware = middleware.name(),
                tool = %ctx.tool_name,
                "Running before_tool_call"
            );
            middleware.before_tool_call(ctx).await?;
            if ctx.skip_execution {
                tracing::debug!(
                    middleware = middleware.name(),
                    tool = %ctx.tool_name,
                    reason = ?ctx.skip_reason,
                    "Tool call blocked by middleware"
                );
                break;
            }
        }
        Ok(())
    }

    /// Execute after_tool_call on all middlewares (reverse order)
    pub async fn run_after_tool_call(
        &self,
        ctx: &mut ToolCallResultContext,
    ) -> MiddlewareResult<()> {
        for middleware in self.middlewares.iter().rev() {
            tracing::trace!(
                middleware = middleware.name(),
                tool = %ctx.tool_name,
                "Running after_tool_call"
            );
            middleware.after_tool_call(ctx).await?;
        }
        Ok(())
    }

    /// Execute on_list_tools on all middlewares
    pub async fn run_on_list_tools(&self, ctx: &mut ToolListContext) -> MiddlewareResult<()> {
        for middleware in &self.middlewares {
            tracing::trace!(middleware = middleware.name(), "Running on_list_tools");
            middleware.on_list_tools(ctx).await?;
        }
        Ok(())
    }

    /// Check if chain is empty
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Get number of middlewares
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }
}

impl Clone for MiddlewareChain {
    fn clone(&self) -> Self {
        Self {
            middlewares: self.middlewares.clone(),
        }
    }
}

/// Create a blocked tool result
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
