// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Polis Middleware - A middleware layer for Wassette MCP server
//!
//! This crate provides a pluggable middleware system that hooks into the MCP
//! request/response lifecycle without modifying upstream Wassette code.
//!
//! # Example
//!
//! ```ignore
//! use polis_middleware::{Middleware, MiddlewareChain, PolisServer};
//!
//! // Create your custom middleware
//! struct LoggingMiddleware;
//!
//! #[async_trait::async_trait]
//! impl Middleware for LoggingMiddleware {
//!     async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
//!         tracing::info!("Tool call: {}", ctx.tool_name);
//!         Ok(())
//!     }
//! }
//!
//! // Build the server with middleware
//! let chain = MiddlewareChain::new().with(LoggingMiddleware);
//! let server = PolisServer::new(lifecycle_manager, false, chain);
//! ```

#![warn(missing_docs)]

mod context;
mod middleware;
mod server;

pub mod examples;

pub use context::{RequestMetadata, ToolCallContext, ToolCallResultContext, ToolListContext};
pub use middleware::{Middleware, MiddlewareChain, MiddlewareError, MiddlewareResult};
pub use server::PolisServer;
