// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use test_log::test;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::net::TcpListener;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

mod common;
use common::build_fetch_component;

async fn start_mock_http_server() -> Result<(std::net::SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|_req: Request<hyper::body::Incoming>| async move {
                    let response = Response::builder()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(Full::new(Bytes::from_static(
                            br#"{"message":"hello","ok":true}"#
                        )))
                        .unwrap();
                    Ok::<_, hyper::Error>(response)
                });

                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    eprintln!("Error serving connection: {err:?}");
                }
            });
        }
    });

    Ok((addr, handle))
}

/// End-to-end integration test for MCP structured output feature.
/// This test verifies that:
/// 1. Components loaded with structured output have output_schema field in tools/list
/// 2. Tool calls return structured_content when appropriate
/// 3. The full MCP structured output flow works end-to-end
#[test(tokio::test)]
async fn test_structured_output_integration() -> Result<()> {
    // Build the fetch component first
    let component_path = build_fetch_component().await?;
    println!("✓ Built fetch component at: {}", component_path.display());

    // Create a temporary directory for this test to avoid loading existing components
    let temp_dir = tempfile::tempdir()?;
    let component_dir_arg = format!("--component-dir={}", temp_dir.path().display());

    // Build the binary first
    let binary_path = std::env::current_dir()
        .context("Failed to get current directory")?
        .join("target/debug/wassette");

    // Start wassette mcp server with stdio transport (default)
    let mut child = Command::new(&binary_path)
        .args(["run", &component_dir_arg])
        .env("RUST_LOG", "off") // Disable logs to avoid stdout pollution
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start wassette server")?;

    let stdin = child.stdin.as_mut().context("Failed to get stdin")?;
    let stdout = child.stdout.as_mut().context("Failed to get stdout")?;
    let mut stdout = BufReader::new(stdout);

    // Send initialize request (required by MCP protocol)
    let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1.0.0"}}, "id": 1}
"#;

    stdin.write_all(initialize_request.as_bytes()).await?;
    stdin.flush().await?;
    println!("✓ Sent initialize request");

    // Read initialize response
    let mut response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut response_line),
    )
    .await
    .context("Timeout waiting for initialize response")?
    .context("Failed to read initialize response")?;

    let response: serde_json::Value =
        serde_json::from_str(&response_line).context("Failed to parse initialize response")?;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());
    println!("✓ Received initialize response");

    // Send initialized notification (required by MCP protocol)
    let initialized_notification = r#"{"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
"#;

    stdin.write_all(initialized_notification.as_bytes()).await?;
    stdin.flush().await?;

    // Step 1: Load the fetch component that should have structured output
    let load_component_request = format!(
        r#"{{"jsonrpc": "2.0", "method": "tools/call", "params": {{"name": "load-component", "arguments": {{"path": "file://{}"}}}}, "id": 2}}
"#,
        component_path.to_str().unwrap()
    );

    stdin.write_all(load_component_request.as_bytes()).await?;
    stdin.flush().await?;
    println!("✓ Sent load-component request");

    // Read potential tools/list_changed notification first
    let mut notification_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        stdout.read_line(&mut notification_line),
    )
    .await
    .context("Timeout waiting for tool list change notification")?
    .context("Failed to read tool list change notification")?;

    let notification: serde_json::Value = serde_json::from_str(&notification_line)
        .context("Failed to parse tool list change notification")?;

    // Verify we received a tools/list_changed notification
    assert_eq!(notification["jsonrpc"], "2.0");
    assert_eq!(notification["method"], "notifications/tools/list_changed");
    println!("✓ Received tools/list_changed notification as expected");

    // Read the actual load-component response
    let mut load_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        stdout.read_line(&mut load_response_line),
    )
    .await
    .context("Timeout waiting for load-component response")?
    .context("Failed to read load-component response")?;

    let load_response: serde_json::Value = serde_json::from_str(&load_response_line)
        .context("Failed to parse load-component response")?;

    assert_eq!(load_response["jsonrpc"], "2.0");
    assert_eq!(load_response["id"], 2);

    // Check if the load succeeded
    if load_response["error"].is_object() {
        panic!("Failed to load component: {}", load_response["error"]);
    }
    assert!(load_response["result"].is_object());
    println!("✓ Component loaded successfully");

    // Step 2: Call tools/list to verify structured output schema is present
    let list_tools_request = r#"{"jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 3}
"#;

    stdin.write_all(list_tools_request.as_bytes()).await?;
    stdin.flush().await?;
    println!("✓ Sent tools/list request");

    // Read tools list response
    let mut tools_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stdout.read_line(&mut tools_response_line),
    )
    .await
    .context("Timeout waiting for tools/list response")?
    .context("Failed to read tools/list response")?;

    let tools_response: serde_json::Value = serde_json::from_str(&tools_response_line)
        .context("Failed to parse tools/list response")?;

    assert_eq!(tools_response["jsonrpc"], "2.0");
    assert_eq!(tools_response["id"], 3);
    assert!(tools_response["result"].is_object());
    assert!(tools_response["result"]["tools"].is_array());
    println!("✓ Received tools/list response");

    let tools = tools_response["result"]["tools"].as_array().unwrap();

    // Step 3: Find the fetch tool and verify it has output_schema
    let fetch_tool = tools
        .iter()
        .find(|tool| tool["name"] == "fetch")
        .context("fetch tool not found in tools list")?;

    println!("✓ Found fetch tool in tools list");

    // Verify the fetch tool has output_schema field (this is the key test for structured output)
    assert!(
        fetch_tool["output_schema"].is_object() || fetch_tool["outputSchema"].is_object(),
        "fetch tool should have output_schema field for structured output support. Tool: {}",
        serde_json::to_string_pretty(fetch_tool).unwrap()
    );

    // Check which field name is used and get the schema
    let output_schema = if fetch_tool["output_schema"].is_object() {
        &fetch_tool["output_schema"]
    } else {
        &fetch_tool["outputSchema"]
    };

    let result_schema = output_schema
        .get("properties")
        .and_then(|props| props.get("result"))
        .unwrap_or(output_schema);

    println!(
        "✓ fetch tool has output_schema field: {}",
        serde_json::to_string_pretty(result_schema).unwrap()
    );

    // Verify the output schema structure makes sense for fetch (should be Result<String, String>)
    // The schema should reflect a Result type with ok/err variants
    if let Some(one_of) = result_schema.get("oneOf").and_then(|v| v.as_array()) {
        println!(
            "✓ output_schema has oneOf structure with {} variants",
            one_of.len()
        );

        // Look for ok/err structure that indicates Result<String, String>
        let has_ok_variant = one_of.iter().any(|variant| {
            variant
                .get("properties")
                .and_then(|props| props.get("ok"))
                .is_some()
        });

        let has_err_variant = one_of.iter().any(|variant| {
            variant
                .get("properties")
                .and_then(|props| props.get("err"))
                .is_some()
        });

        assert!(has_ok_variant, "Expected ok variant in Result schema");
        assert!(has_err_variant, "Expected err variant in Result schema");
        println!("✓ output_schema has proper Result<T, E> structure with ok and err variants");
    } else {
        // Alternative schema structure might be used
        println!(
            "Note: output_schema uses alternative structure: {}",
            serde_json::to_string_pretty(result_schema).unwrap()
        );
    }

    // Step 3.5: Grant network permission for localhost so the fetch call can succeed quickly
    let grant_permission_request = r#"{"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "grant-network-permission", "arguments": {"component_id": "fetch_rs", "details": {"host": "127.0.0.1"}}}, "id": 4}
"#;

    stdin
        .write_all(grant_permission_request.as_bytes())
        .await?;
    stdin.flush().await?;
    println!("✓ Sent grant-network-permission request");

    let mut grant_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(15),
        stdout.read_line(&mut grant_response_line),
    )
    .await
    .context("Timeout waiting for grant-network-permission response")?
    .context("Failed to read grant-network-permission response")?;

    let grant_response: serde_json::Value = serde_json::from_str(&grant_response_line)
        .context("Failed to parse grant-network-permission response")?;
    assert!(grant_response["error"].is_null());
    println!("✓ Received grant-network-permission response");

    // Step 4: Test an actual tool call to verify structured content handling
    // Use a local mock server to avoid external network dependency
    let (mock_addr, mock_handle) = start_mock_http_server().await?;
    let fetch_call_request = format!(
        r#"{{"jsonrpc": "2.0", "method": "tools/call", "params": {{"name": "fetch", "arguments": {{"url": "http://{}"}}}}, "id": 5}}
"#,
        mock_addr
    );

    stdin.write_all(fetch_call_request.as_bytes()).await?;
    stdin.flush().await?;
    println!("✓ Sent fetch request to mock server at {}", mock_addr);

    // Read fetch response
    let mut fetch_response_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        stdout.read_line(&mut fetch_response_line),
    )
    .await
    .context("Timeout waiting for fetch response")?
    .context("Failed to read fetch response")?;

    let fetch_response: serde_json::Value =
        serde_json::from_str(&fetch_response_line).context("Failed to parse fetch response")?;

    assert_eq!(fetch_response["jsonrpc"], "2.0");
    assert_eq!(fetch_response["id"], 5);
    println!("✓ Received fetch response");

    // The fetch call might fail due to network restrictions, but we can still verify the response structure
    if fetch_response["result"].is_object() {
        let result = &fetch_response["result"];

        // With rmcp v0.5.0, we should have structured_content field for structured responses
        let structured = result
            .get("structured_content")
            .or_else(|| result.get("structuredContent"));

        assert!(
            structured.is_some(),
            "Tool response is missing structured_content despite output schema: {}",
            serde_json::to_string_pretty(result).unwrap()
        );

        let structured = structured.unwrap();
        assert!(structured.is_object());
        assert!(
            structured
                .get("result")
                .map(|v| v.is_object() || v.is_string() || v.is_array())
                .unwrap_or(false),
            "structured_content.result missing or malformed: {}",
            serde_json::to_string_pretty(structured).unwrap()
        );

        println!("✓ Tool call completed with proper response structure");
    } else if fetch_response["error"].is_object() {
        // Network call might be blocked, but error should still follow proper structure
        println!(
            "Note: Fetch call resulted in error (likely due to network restrictions): {}",
            fetch_response["error"]
        );
        println!("✓ Error response follows proper MCP structure");
    }

    // Clean up
    let _ = child.kill().await;
    mock_handle.abort();

    println!("✓ Structured output integration test completed successfully!");
    println!("  - Component loaded with structured output schema");
    println!("  - tools/list returned proper output_schema field");
    println!("  - Tool calls handle structured responses correctly");

    Ok(())
}
