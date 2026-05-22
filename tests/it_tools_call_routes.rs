// Copyright 2026 Salesforce, Inc. All rights reserved.

//! `tools/call` routes to the owning upstream by inspecting the
//! `<server>_<tool>` prefix. The original (un-prefixed) name is forwarded
//! to the upstream.

mod common;

use common::{header, json, rpc_result, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitTestBuilder};
use std::cell::RefCell;
use std::rc::Rc;

fn policy_config() -> String {
    serde_json::json!({
        "mcpEndpoint": "/mcp",
        "strictMode": true,
        "collisionStrategy": "prefix",
        "mcpServers": [
            {
                "name": "jira",
                "endpoint": "http://jira.test/mcp",
                "enabled": true,
                "authType": "bearerToken",
                "token": "jira-secret"
            },
            {
                "name": "slack",
                "endpoint": "http://slack.test/mcp",
                "enabled": true,
                "authType": "none"
            }
        ]
    })
    .to_string()
}

#[test]
fn tools_call_dispatches_to_owning_upstream_with_unprefixed_name() {
    let last_jira_body: Rc<RefCell<Option<serde_json::Value>>> = Rc::new(RefCell::new(None));
    let last_slack_body: Rc<RefCell<Option<serde_json::Value>>> = Rc::new(RefCell::new(None));

    let jira_capture = last_jira_body.clone();
    let jira_backend = RouterBackend::new(move |req| {
        let body: serde_json::Value =
            serde_json::from_slice(req.body()).unwrap_or(serde_json::Value::Null);
        *jira_capture.borrow_mut() = Some(body.clone());
        let auth = header(&req, "authorization").unwrap_or_default();
        assert_eq!(auth, "Bearer jira-secret");
        let id = body["id"].as_str().unwrap_or("rpc-1").to_string();
        json(
            200,
            &rpc_result(
                &id,
                serde_json::json!({
                    "content": [{"type": "text", "text": "issue ABC-123"}],
                    "isError": false
                }),
            ),
        )
    });

    let slack_capture = last_slack_body.clone();
    let slack_backend = RouterBackend::new(move |req| {
        let body: serde_json::Value =
            serde_json::from_slice(req.body()).unwrap_or(serde_json::Value::Null);
        *slack_capture.borrow_mut() = Some(body);
        json(500, "slack should not be called")
    });

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("jira.test", jira_backend)
        .with_http_upstream_from_authority("slack.test", slack_backend)
        .with_entrypoint(configure);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "jira_search",
            "arguments": {"q": "fix bug"}
        }
    })
    .to_string();

    let req = UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(body.into_bytes());
    let resp = tester.request(req);

    assert_eq!(
        resp.status_code(),
        200,
        "body: {}",
        String::from_utf8_lossy(resp.body())
    );
    let v: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["result"]["content"][0]["text"], "issue ABC-123");

    // Jira saw `name = "search"` (un-prefixed), preserving the upstream's
    // own namespace.
    let jira_seen = last_jira_body.borrow();
    let jira_seen = jira_seen.as_ref().expect("jira got the call");
    assert_eq!(jira_seen["method"], "tools/call");
    assert_eq!(jira_seen["params"]["name"], "search");
    assert_eq!(jira_seen["params"]["arguments"]["q"], "fix bug");

    // Slack was not contacted at all.
    assert!(last_slack_body.borrow().is_none(), "slack should not be hit");
}

#[test]
fn tools_call_unknown_server_returns_tool_not_found() {
    let jira_backend = RouterBackend::new(|_req| json(500, "should not be called"));
    let slack_backend = RouterBackend::new(|_req| json(500, "should not be called"));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("jira.test", jira_backend)
        .with_http_upstream_from_authority("slack.test", slack_backend)
        .with_entrypoint(configure);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "call-1",
        "method": "tools/call",
        "params": {"name": "ghost_search"}
    })
    .to_string();

    let req = UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(body.into_bytes());
    let resp = tester.request(req);

    let v: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(v["id"], "call-1");
    let code = v["error"]["code"].as_i64().expect("error code present");
    assert_eq!(code, -32011, "expected TOOL_NOT_FOUND, got {v:#}");
}
