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
    assert!(
        last_slack_body.borrow().is_none(),
        "slack should not be hit"
    );
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

/// Server `a` has tool `b_c` and server `a_b` has tool `c`: both prefix to
/// `a_b_c`. The call must reach the tool `tools/list` showed (first wins,
/// i.e. `a`'s `b_c`), not the longest matching server prefix.
type Calls = Rc<RefCell<Vec<String>>>;

fn ambiguous_backend(tool: &'static str, called: Calls) -> RouterBackend {
    RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let id = b["id"].as_str().unwrap_or("").to_string();
        match b["method"].as_str().unwrap_or("") {
            "tools/list" => json(200, &common::tools_list_response(&id, &[(tool, "t")])),
            "tools/call" => {
                called
                    .borrow_mut()
                    .push(b["params"]["name"].as_str().unwrap().to_string());
                json(200, &rpc_result(&id, serde_json::json!({"content": []})))
            }
            _ => json(200, &rpc_result(&id, serde_json::json!({}))),
        }
    })
}

fn ambiguous_tester(a_b_enabled: bool, a: Calls, a_b: Calls) -> pdk_unit::UnitTest {
    let cfg = serde_json::json!({
        "mcpEndpoint": "/mcp",
        "collisionStrategy": "prefix",
        "mcpServers": [
            {"name": "a", "endpoint": "http://a.test/mcp", "enabled": true, "authType": "none"},
            {"name": "a_b", "endpoint": "http://ab.test/mcp", "enabled": a_b_enabled, "authType": "none"}
        ]
    });
    UnitTestBuilder::default()
        .with_config(cfg.to_string())
        .with_http_upstream_from_authority("a.test", ambiguous_backend("b_c", a))
        .with_http_upstream_from_authority("ab.test", ambiguous_backend("c", a_b))
        .with_entrypoint(configure)
}

fn call(name: &str) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": {}}})
            .to_string()
            .into_bytes(),
        )
}

#[test]
fn ambiguous_prefix_routes_to_the_listed_tool() {
    let (a, a_b): (Calls, Calls) = Default::default();
    let mut t = ambiguous_tester(true, a.clone(), a_b.clone());
    assert_eq!(t.request(call("a_b_c")).status_code(), 200);
    assert_eq!(*a.borrow(), vec!["b_c"]);
    assert!(
        a_b.borrow().is_empty(),
        "a_b must not run: {:?}",
        a_b.borrow()
    );
}

#[test]
fn disabled_server_does_not_capture_a_prefix() {
    let (a, a_b): (Calls, Calls) = Default::default();
    let mut t = ambiguous_tester(false, a.clone(), a_b.clone());
    let resp = t.request(call("a_b_c"));
    assert_eq!(resp.status_code(), 200);
    let b: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
    assert!(b.get("error").is_none(), "{b}");
    assert_eq!(*a.borrow(), vec!["b_c"]);
}
