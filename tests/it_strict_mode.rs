// Copyright 2026 Salesforce, Inc. All rights reserved.

//! Non-`/mcp` paths are rejected in strict mode.

mod common;

use common::{json, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitTestBuilder};

fn policy_config() -> String {
    serde_json::json!({
        "mcpEndpoint": "/mcp",
        "strictMode": true,
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
fn strict_mode_rejects_non_mcp_paths() {
    let upstream = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("upstream.test", upstream)
        .with_entrypoint(configure);

    let req = UnitHttpRequest::get().with_path("/health");
    let resp = tester.request(req);

    assert_eq!(resp.status_code(), 404);
}

#[test]
fn get_on_mcp_returns_hub_info() {
    let upstream = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("upstream.test", upstream)
        .with_entrypoint(configure);

    let req = UnitHttpRequest::get().with_path("/mcp");
    let resp = tester.request(req);

    assert_eq!(resp.status_code(), 200);
    let v: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(v["name"], "mcp-hub-policy");
    let servers = v["servers"].as_array().expect("servers array");
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["name"], "jira");
}
