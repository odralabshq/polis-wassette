# Polis Middleware

A middleware layer for Wassette MCP server that hooks into the request/response lifecycle without modifying upstream code.

## Usage

### 1. Enable the feature in your fork

In your `Cargo.toml`:

```toml
[dependencies]
wassette-mcp-server = { features = ["polis"] }
```

Or build with:

```bash
cargo build --features polis
```

### 2. Create custom middleware

```rust
use polis_middleware::{Middleware, MiddlewareResult, ToolCallContext};
use async_trait::async_trait;

struct MyAuthMiddleware {
    api_key: String,
}

#[async_trait]
impl Middleware for MyAuthMiddleware {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext) -> MiddlewareResult<()> {
        // Check auth, validate request, etc.
        if !self.is_authorized(&ctx.tool_name) {
            ctx.block("Unauthorized");
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "auth"
    }
}
```

### 3. Use PolisServer instead of McpServer

In `src/main.rs`, replace:

```rust
let server = McpServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools);
```

With:

```rust
use polis_middleware::{MiddlewareChain, PolisServer};
use polis_middleware::examples::LoggingMiddleware;

let middleware = MiddlewareChain::new()
    .with(LoggingMiddleware::default())
    .with(MyAuthMiddleware { api_key: "...".into() });

let server = PolisServer::new(lifecycle_manager.clone(), cfg.disable_builtin_tools, middleware);
```

## Built-in Example Middleware

- `LoggingMiddleware` - Logs all tool calls with timing
- `AllowlistMiddleware` - Only allows specific tools
- `DenylistMiddleware` - Blocks specific tools
- `RateLimitMiddleware` - Rate limits tool calls
- `AuditMiddleware` - Records all calls for compliance

## Middleware Hooks

| Hook | When | Use Case |
|------|------|----------|
| `before_tool_call` | Before execution | Auth, validation, rate limiting |
| `after_tool_call` | After execution | Logging, metrics, result transformation |
| `on_list_tools` | When listing tools | Filter visible tools |

## Staying Mergeable with Upstream

This crate is designed to minimize conflicts with upstream Wassette:

1. All middleware code lives in `crates/polis-middleware/`
2. Only `src/main.rs` needs minimal changes (swap `McpServer` → `PolisServer`)
3. Use feature flags to conditionally enable middleware

When rebasing on upstream:
- Conflicts will only be in `src/main.rs` and `Cargo.toml`
- The middleware crate itself won't conflict
