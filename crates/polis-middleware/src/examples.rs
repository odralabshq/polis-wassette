// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Example middleware implementations
//!
//! These serve as templates for building custom middleware.

use crate::context::{ToolCallContext, ToolCallResultContext, ToolListContext};
use crate::middleware::{Middleware, MiddlewareResult};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Logging middleware - logs all tool calls
pub struct LoggingMiddleware {
    /// Log level for tool calls
    pub level: tracing::Level,
}

impl Default for LoggingMiddleware {
    fn default() -> Self {
        Self {
            level: tracing::Level::INFO,
        }
    }
}

#[async_trait]
impl Middleware for LoggingMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        tracing::info!(
            tool = %ctx.tool_name,
            request_id = %ctx.metadata.request_id,
            "Tool call started"
        );
        Ok(())
    }

    async fn after_tool_call(&self, ctx: &mut ToolCallResultContext) -> MiddlewareResult<()> {
        tracing::info!(
            tool = %ctx.tool_name,
            request_id = %ctx.metadata.request_id,
            duration_ms = %ctx.duration.as_millis(),
            is_error = ?ctx.result.is_error,
            "Tool call completed"
        );
        Ok(())
    }

    fn name(&self) -> &'static str {
        "logging"
    }
}

/// Tool allowlist middleware - only allows specific tools
pub struct AllowlistMiddleware {
    allowed_tools: HashSet<String>,
}

impl AllowlistMiddleware {
    /// Create a new allowlist middleware
    pub fn new(allowed: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_tools: allowed.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl Middleware for AllowlistMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        if !self.allowed_tools.contains(&ctx.tool_name) {
            ctx.block(format!("Tool '{}' is not in the allowlist", ctx.tool_name));
        }
        Ok(())
    }

    async fn on_list_tools(&self, ctx: &mut ToolListContext) -> MiddlewareResult<()> {
        ctx.filter(|tool| self.allowed_tools.contains(tool.name.as_ref()));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "allowlist"
    }
}

/// Tool denylist middleware - blocks specific tools
pub struct DenylistMiddleware {
    denied_tools: HashSet<String>,
}

impl DenylistMiddleware {
    /// Create a new denylist middleware
    pub fn new(denied: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            denied_tools: denied.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl Middleware for DenylistMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        if self.denied_tools.contains(&ctx.tool_name) {
            ctx.block(format!("Tool '{}' is blocked", ctx.tool_name));
        }
        Ok(())
    }

    async fn on_list_tools(&self, ctx: &mut ToolListContext) -> MiddlewareResult<()> {
        ctx.filter(|tool| !self.denied_tools.contains(tool.name.as_ref()));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "denylist"
    }
}

/// Rate limiting middleware
pub struct RateLimitMiddleware {
    /// Maximum calls per window
    max_calls: usize,
    /// Window duration
    window: std::time::Duration,
    /// Call timestamps
    calls: Arc<RwLock<Vec<std::time::Instant>>>,
}

impl RateLimitMiddleware {
    /// Create a new rate limit middleware
    pub fn new(max_calls: usize, window: std::time::Duration) -> Self {
        Self {
            max_calls,
            window,
            calls: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Middleware for RateLimitMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        let now = std::time::Instant::now();
        let mut calls = self.calls.write().await;

        // Remove old calls outside the window
        calls.retain(|t| now.duration_since(*t) < self.window);

        if calls.len() >= self.max_calls {
            ctx.block("Rate limit exceeded");
            return Ok(());
        }

        calls.push(now);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "rate_limit"
    }
}

/// Audit middleware - records all tool calls for compliance
pub struct AuditMiddleware {
    /// Audit log entries
    entries: Arc<RwLock<Vec<AuditEntry>>>,
}

/// An audit log entry
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// Request ID
    pub request_id: String,
    /// Tool name
    pub tool_name: String,
    /// Timestamp
    pub timestamp: std::time::SystemTime,
    /// Duration (if completed)
    pub duration_ms: Option<u64>,
    /// Whether the call was blocked
    pub blocked: bool,
    /// Whether the call resulted in an error
    pub is_error: Option<bool>,
}

impl Default for AuditMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditMiddleware {
    /// Create a new audit middleware
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Get all audit entries
    pub async fn entries(&self) -> Vec<AuditEntry> {
        self.entries.read().await.clone()
    }

    /// Clear audit entries
    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }
}

#[async_trait]
impl Middleware for AuditMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        // We'll record the entry in after_tool_call with full details
        // Store the start info in metadata for later
        ctx.metadata.insert(
            "_audit_start".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64),
        );
        Ok(())
    }

    async fn after_tool_call(&self, ctx: &mut ToolCallResultContext) -> MiddlewareResult<()> {
        let entry = AuditEntry {
            request_id: ctx.metadata.request_id.clone(),
            tool_name: ctx.tool_name.clone(),
            timestamp: std::time::SystemTime::now(),
            duration_ms: Some(ctx.duration.as_millis() as u64),
            blocked: false,
            is_error: ctx.result.is_error,
        };

        self.entries.write().await.push(entry);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "audit"
    }
}
