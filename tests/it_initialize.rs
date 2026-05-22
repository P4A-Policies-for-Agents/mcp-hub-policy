// Copyright 2026 Salesforce, Inc. All rights reserved.

//! `initialize` is answered locally by the hub — the hub identifies
//! itself as the MCP server, no upstream call.

mod common;

use common::{json, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitTestBuilder};

fn policy_config() -> String {
    serde_json::json!({
        "mcpEndpoint": "/mcp",
        "strictMode": true,
        "collisionStrategy": "prefix",
        "mcpServers": [
            {
                "name": "jira",
                "endpoint": "http://upstream.test/mcp",
                "enabled": true,
                "authType": "none"
            }
        ]
    })
    .to_string()
}

#[test]
fn initialize_returns_hub_server_info() {
    let upstream = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("upstream.test", upstream)
        .with_entrypoint(configure);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "0.1"}
        }
    })
    .to_string();

    let req = UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(body.into_bytes());
    let resp = tester.request(req);

    assert_eq!(resp.status_code(), 200);
    let v: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["serverInfo"]["name"], "mcp-hub-policy");
    assert!(v["result"]["capabilities"].is_object());
}
