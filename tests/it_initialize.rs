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

fn initialize(version: &str) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": version, "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}
            })
            .to_string()
            .into_bytes(),
        )
}

fn tester(config: String) -> pdk_unit::UnitTest {
    UnitTestBuilder::default()
        .with_config(config)
        .with_http_upstream_from_authority(
            "upstream.test",
            RouterBackend::new(|_req| json(500, "should not be called")),
        )
        .with_entrypoint(configure)
}

#[test]
fn initialize_is_stateless_and_ignores_client_session_id() {
    let mut t = tester(policy_config());
    let resp = t.request(initialize("2025-06-18").with_header("mcp-session-id", "attacker-chosen"));
    assert_eq!(resp.status_code(), 200);
    assert!(resp.header("mcp-session-id").is_none());
}

#[test]
fn initialize_negotiates_protocol_version() {
    let mut t = tester(policy_config());
    for (requested, expected) in [
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2024-11-05"),
        ("2099-01-01", "2025-06-18"),
    ] {
        let resp = t.request(initialize(requested));
        let v: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        assert_eq!(
            v["result"]["protocolVersion"], expected,
            "requested {requested}"
        );
    }
}

#[test]
fn unsupported_protocol_version_header_is_400() {
    let mut t = tester(policy_config());
    let resp =
        t.request(initialize("2025-06-18").with_header("mcp-protocol-version", "1999-01-01"));
    assert_eq!(resp.status_code(), 400);
    let resp =
        t.request(initialize("2025-06-18").with_header("mcp-protocol-version", "2025-06-18"));
    assert_eq!(resp.status_code(), 200);
}

#[test]
fn foreign_origin_is_403_when_allowlist_configured() {
    let mut cfg: serde_json::Value = serde_json::from_str(&policy_config()).unwrap();
    cfg["allowedOrigins"] = serde_json::json!(["https://app.example"]);
    let mut t = tester(cfg.to_string());

    let resp = t.request(initialize("2025-06-18").with_header("origin", "https://evil.example"));
    assert_eq!(resp.status_code(), 403);
    let resp = t.request(initialize("2025-06-18").with_header("origin", "https://app.example"));
    assert_eq!(resp.status_code(), 200);
    let resp = t.request(initialize("2025-06-18"));
    assert_eq!(
        resp.status_code(),
        200,
        "non-browser clients send no Origin"
    );
}

#[test]
fn any_origin_allowed_without_allowlist() {
    let mut t = tester(policy_config());
    let resp = t.request(initialize("2025-06-18").with_header("origin", "https://evil.example"));
    assert_eq!(resp.status_code(), 200);
}
