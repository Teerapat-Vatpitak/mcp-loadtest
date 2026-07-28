//! Adapter for the official MCP client conformance harness.
//!
//! The harness appends its server URL and selects a scenario through
//! `MCP_CONFORMANCE_SCENARIO`. This intentionally covers only the protocol
//! surface mcp-loadtest uses: discovery, tools/list, tools/call, and the
//! 2026 HTTP metadata headers. It is not an OAuth or general MCP SDK client.

use std::time::Duration;

use mcp_loadtest_core::ProtocolVersion;
use mcp_loadtest_core::config::ServerConfig;
use mcp_loadtest_protocol::Session;
use mcp_loadtest_protocol::transport::HostGuard;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conformance adapter: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("official conformance harness did not append a server URL")?;
    let scenario = std::env::var("MCP_CONFORMANCE_SCENARIO")
        .map_err(|_| "MCP_CONFORMANCE_SCENARIO is not set")?;
    let requested =
        std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION").unwrap_or_else(|_| "2026-07-28".into());
    if requested != "2026-07-28" {
        return Err(format!("adapter only supports 2026-07-28, got `{requested}`").into());
    }

    let parsed = url::Url::parse(&url)?;
    let host = parsed
        .host_str()
        .ok_or("conformance server URL has no host")?
        .to_owned();
    let mut config = ServerConfig::stdio("conformance-adapter".into(), Vec::new());
    config.allowed_hosts = vec![host];
    let guard = HostGuard::from_config(&config);
    let transport = HttpTransport::connect(&url, &guard).await?;
    let mut session = Session::from_transport_stateless(
        transport,
        Duration::from_secs(10),
        ProtocolVersion::V2026_07_28,
    )
    .await?;

    match scenario.as_str() {
        "request-metadata" => {
            let _ = session.list_tools().await?;
        }
        "tools_call" => {
            let tools = session.list_tools().await?;
            let tool = tools
                .iter()
                .find(|tool| tool.name == "add_numbers")
                .ok_or("tools_call fixture did not advertise add_numbers")?;
            let _ = session
                .call_tool(&tool.name, &json!({"a": 20, "b": 22}))
                .await?;
        }
        "http-standard-headers" => {
            let tools = session.list_tools().await?;
            let tool = tools
                .first()
                .ok_or("standard-header fixture advertised no tools")?;
            let _ = session.call_tool(&tool.name, &json!({})).await?;
        }
        "http-custom-headers" => {
            let tools = session.list_tools().await?;
            require_tool(&tools, "test_custom_headers")?;
            require_tool(&tools, "test_custom_headers_null")?;
            let args = json!({
                "region": "us-west1",
                "priority": 42,
                "verbose": false,
                "debug": true,
                "empty_val": "",
                "method_val": "custom method",
                "float_val": 3.5,
                "non_ascii_val": "Hello, 世界",
                "whitespace_val": " padded ",
                "leading_space_val": " leading",
                "trailing_space_val": "trailing ",
                "internal_space_val": "hello world",
                "control_char_val": "line1\nline2",
                "crlf_val": "line1\r\nline2",
                "tab_val": "\tvalue",
                "query": "select 1"
            });
            let _ = session.call_tool("test_custom_headers", &args).await?;
            let _ = session
                .call_tool(
                    "test_custom_headers_null",
                    &json!({
                        "region": "us-west1",
                        "priority": 1,
                        "verbose": Value::Null,
                        "query": "select 1"
                    }),
                )
                .await?;
        }
        "http-invalid-tool-headers" => {
            let tools = session.list_tools().await?;
            if tools.len() != 1 || tools[0].name != "valid_tool" {
                return Err(format!(
                    "invalid x-mcp-header tools were not filtered: {:?}",
                    tools.iter().map(|tool| &tool.name).collect::<Vec<_>>()
                )
                .into());
            }
            let _ = session
                .call_tool("valid_tool", &json!({"region": "us-west1"}))
                .await?;
        }
        other => return Err(format!("unsupported conformance scenario `{other}`").into()),
    }

    session.shutdown().await?;
    Ok(())
}

fn require_tool(
    tools: &[mcp_loadtest_protocol::mcp::Tool],
    expected: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if tools.iter().any(|tool| tool.name == expected) {
        Ok(())
    } else {
        Err(format!("fixture did not advertise `{expected}`").into())
    }
}
