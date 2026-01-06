// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Context types passed to middleware hooks

use rmcp::model::{CallToolRequestParam, CallToolResult, ListToolsResult, Tool};
use serde_json::Value;
use std::collections::HashMap;

/// Metadata about the current request
#[derive(Debug, Clone, Default)]
pub struct RequestMetadata {
    /// Unique request ID (generated per request)
    pub request_id: String,
    /// Timestamp when request was received
    pub timestamp: std::time::Instant,
    /// Custom key-value store for middleware to share data
    pub extensions: HashMap<String, Value>,
}

impl RequestMetadata {
    /// Create new request metadata with a generated ID
    pub fn new() -> Self {
        Self {
            request_id: uuid_simple(),
            timestamp: std::time::Instant::now(),
            extensions: HashMap::new(),
        }
    }

    /// Insert a value into extensions
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.extensions.insert(key.into(), value);
    }

    /// Get a value from extensions
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.extensions.get(key)
    }
}

/// Context for tool call middleware hooks
#[derive(Debug)]
pub struct ToolCallContext {
    /// The tool name being called
    pub tool_name: String,
    /// The arguments passed to the tool (mutable for transformation)
    pub arguments: Option<serde_json::Map<String, Value>>,
    /// Request metadata
    pub metadata: RequestMetadata,
    /// Whether to skip execution (set by middleware to block the call)
    pub skip_execution: bool,
    /// Custom error to return if skipping (optional)
    pub skip_reason: Option<String>,
}

impl ToolCallContext {
    /// Create a new tool call context from request params
    pub fn from_params(params: &CallToolRequestParam) -> Self {
        Self {
            tool_name: params.name.to_string(),
            arguments: params.arguments.clone(),
            metadata: RequestMetadata::new(),
            skip_execution: false,
            skip_reason: None,
        }
    }

    /// Block this tool call with a reason
    pub fn block(&mut self, reason: impl Into<String>) {
        self.skip_execution = true;
        self.skip_reason = Some(reason.into());
    }

    /// Rebuild CallToolRequestParam with potentially modified arguments
    pub fn to_params(&self) -> CallToolRequestParam {
        CallToolRequestParam {
            name: self.tool_name.clone().into(),
            arguments: self.arguments.clone(),
        }
    }
}

/// Context for tool call result (after execution)
#[derive(Debug)]
pub struct ToolCallResultContext {
    /// The tool name that was called
    pub tool_name: String,
    /// The result (mutable for transformation)
    pub result: CallToolResult,
    /// Request metadata (same as before_tool_call)
    pub metadata: RequestMetadata,
    /// Execution duration
    pub duration: std::time::Duration,
}

/// Context for tool list middleware hooks
#[derive(Debug)]
pub struct ToolListContext {
    /// The list of tools (mutable for filtering/transformation)
    pub tools: Vec<Tool>,
    /// Request metadata
    pub metadata: RequestMetadata,
}

impl ToolListContext {
    /// Create a new tool list context
    pub fn new(tools: Vec<Tool>) -> Self {
        Self {
            tools,
            metadata: RequestMetadata::new(),
        }
    }

    /// Filter tools by predicate
    pub fn filter<F>(&mut self, predicate: F)
    where
        F: Fn(&Tool) -> bool,
    {
        self.tools.retain(predicate);
    }
}

/// Simple UUID-like string generator (no external dependency)
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:x}-{:x}",
        duration.as_secs(),
        duration.subsec_nanos() ^ std::process::id()
    )
}
