// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! MCP Server library for Wassette.
//!
//! This crate provides the MCP protocol implementation for running
//! WebAssembly components as tools.
//!
//! # Quick Start
//!
//! ```ignore
//! use mcp_server::{McpServer, LifecycleManager};
//!
//! let lifecycle_manager = LifecycleManager::builder(component_dir)
//!     .build()
//!     .await?;
//!
//! let server = McpServer::new(lifecycle_manager, false);
//! ```
//!
//! # Custom Hooks
//!
//! Use hooks to intercept and customize request handling:
//!
//! ```ignore
//! use mcp_server::{McpServer, ServerHooks, ToolCallContext, MiddlewareStack};
//! use rmcp::model::ErrorData;
//!
//! struct LoggingHooks;
//!
//! impl ServerHooks for LoggingHooks {
//!     fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<(), ErrorData> {
//!         tracing::info!("Calling: {}", ctx.tool_name);
//!         Ok(())
//!     }
//! }
//!
//! // Single hook
//! let server = McpServer::builder(lifecycle_manager)
//!     .with_hooks(LoggingHooks)
//!     .build();
//!
//! // Multiple hooks (middleware stack)
//! let hooks = MiddlewareStack::new()
//!     .push(LoggingHooks)
//!     .push(AuthHooks::new(key));
//!
//! let server = McpServer::builder(lifecycle_manager)
//!     .with_hooks(hooks)
//!     .build();
//! ```
//!
//! Note: `ErrorData` is re-exported from `rmcp::model::ErrorData`.

pub use wassette::LifecycleManager;

mod hooks;
mod server;

pub mod components;
pub mod prompts;
pub mod resources;
pub mod tools;

// Re-export hooks
pub use hooks::{
    blocked_result, MiddlewareStack, NoOpHooks, ServerHooks, ToolCallContext, ToolResultContext,
};

// Re-export server
pub use server::{McpServer, McpServerBuilder};

// Re-export handlers (for advanced use cases)
pub use prompts::{handle_prompts_get, handle_prompts_list};
pub use resources::handle_resources_list;
pub use tools::{handle_tools_call, handle_tools_list};
