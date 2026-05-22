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
fn plain_get_on_mcp_returns_405() {
    // Plain GET (browser, health probe, no SSE Accept) → 405 + Allow: POST.
    let upstream = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("upstream.test", upstream)
        .with_entrypoint(configure);

    let req = UnitHttpRequest::get().with_path("/mcp");
    let resp = tester.request(req);

    assert_eq!(resp.status_code(), 405);
    let allow = resp
        .headers()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("allow"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(allow, "POST");
}

#[test]
fn sse_get_on_mcp_returns_event_stream() {
    // SSE clients (MCP Inspector legacy fallback, EventSource probes)
    // send `Accept: text/event-stream`. Respond 200 with an empty SSE
    // stream so the client connects without error.
    let upstream = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("upstream.test", upstream)
        .with_entrypoint(configure);

    let req = UnitHttpRequest::get()
        .with_path("/mcp")
        .with_header("accept", "text/event-stream");
    let resp = tester.request(req);

    assert_eq!(resp.status_code(), 200);
    let ct = resp
        .headers()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(ct, "text/event-stream");
}
