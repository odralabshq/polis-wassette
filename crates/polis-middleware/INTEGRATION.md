# Integration Guide

## Changes to src/main.rs

### Step 1: Add conditional imports

At the top of `src/main.rs`, add:

```rust
#[cfg(feature = "polis")]
use polis_middleware::{MiddlewareChain, PolisServer};
#[cfg(feature = "polis")]
use polis_middleware::examples::LoggingMiddleware;
```

### Step 2: Replace McpServer with PolisServer

Find the `Commands::Run` and `Commands::Serve` blocks. Replace:

```rust
let server = McpServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools);
```

With:

```rust
#[cfg(feature = "polis")]
let server = {
    let middleware = MiddlewareChain::new()
        .with(LoggingMiddleware::default());
    PolisServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools, middleware)
};

#[cfg(not(feature = "polis"))]
let server = McpServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools);
```

### Step 3: Update get_peer calls

The `PolisServer` has the same `get_peer()` method, so no changes needed there.

## Full Diff Example

```diff
 use server::McpServer;
+#[cfg(feature = "polis")]
+use polis_middleware::{MiddlewareChain, PolisServer};
+#[cfg(feature = "polis")]
+use polis_middleware::examples::LoggingMiddleware;

 // ... in Commands::Run ...

-                let server = McpServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools);
+                #[cfg(feature = "polis")]
+                let server = {
+                    let middleware = MiddlewareChain::new()
+                        .with(LoggingMiddleware::default());
+                    PolisServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools, middleware)
+                };
+                #[cfg(not(feature = "polis"))]
+                let server = McpServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools);
```

## Custom Middleware Example

Create your own middleware in a separate file:

```rust
// src/my_middleware.rs
use polis_middleware::{Middleware, MiddlewareResult, ToolCallContext, ToolListContext};
use async_trait::async_trait;
use std::collections::HashSet;

pub struct PolicyMiddleware {
    blocked_tools: HashSet<String>,
}

impl PolicyMiddleware {
    pub fn new() -> Self {
        let mut blocked = HashSet::new();
        // Block dangerous tools
        blocked.insert("load-component".to_string());
        blocked.insert("unload-component".to_string());
        Self { blocked_tools: blocked }
    }
}

#[async_trait]
impl Middleware for PolicyMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        if self.blocked_tools.contains(&ctx.tool_name) {
            ctx.block(format!(
                "Tool '{}' is blocked by policy",
                ctx.tool_name
            ));
        }
        Ok(())
    }

    async fn on_list_tools(&self, ctx: &mut ToolListContext) -> MiddlewareResult<()> {
        // Hide blocked tools from the list
        ctx.filter(|tool| !self.blocked_tools.contains(tool.name.as_ref()));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "policy"
    }
}
```

Then use it:

```rust
let middleware = MiddlewareChain::new()
    .with(LoggingMiddleware::default())
    .with(PolicyMiddleware::new());
```
