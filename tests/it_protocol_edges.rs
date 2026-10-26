// Copyright 2026 Salesforce, Inc. All rights reserved.

//! JSON-RPC message shapes, `resources/templates/list`, `requireCallerAuth`,
//! `maxUpstreamResponseBytes`, upstreams without a list method, and the
//! split between shared and per-caller caches.

mod common;

use common::{json, rpc_result, tools_list_response, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
use std::cell::RefCell;
use std::rc::Rc;

/// `jira` (static bearer) + `erp` (passthrough), with `extra` merged in at
/// the top level.
fn config(extra: serde_json::Value) -> serde_json::Value {
    let mut cfg = serde_json::json!({
        "mcpEndpoint": "/mcp",
        "mcpServers": [
            {"name": "jira", "endpoint": "http://jira.test/mcp",
             "authType": "bearerToken", "token": "jira-secret"},
            {"name": "erp", "endpoint": "http://erp.test/mcp", "authType": "passthrough"}
        ]
    });
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    cfg
}

fn tester(cfg: serde_json::Value, jira: RouterBackend, erp: RouterBackend) -> pdk_unit::UnitTest {
    UnitTestBuilder::default()
        .with_config(cfg.to_string())
        .with_http_upstream_from_authority("jira.test", jira)
        .with_http_upstream_from_authority("erp.test", erp)
        .with_entrypoint(configure)
}

fn post_raw(body: &str) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(body.as_bytes().to_vec())
}

fn rpc(method: &str, params: serde_json::Value) -> UnitHttpRequest {
    post_raw(
        &serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})
            .to_string(),
    )
}

fn body(resp: &UnitHttpResponse) -> serde_json::Value {
    serde_json::from_slice(resp.body()).unwrap()
}

/// Methods an upstream received, in order.
type Log = Rc<RefCell<Vec<String>>>;

/// MCP server exposing `tool`; `prompts/list` answers -32601, and
/// `on_call` builds the `tools/call` reply from the request id.
fn backend(
    tool: &'static str,
    log: Log,
    on_call: impl Fn(&str) -> UnitHttpResponse + 'static,
) -> RouterBackend {
    RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let method = b["method"].as_str().unwrap_or("").to_string();
        log.borrow_mut().push(method.clone());
        let id = b["id"].as_str().unwrap_or("").to_string();
        match method.as_str() {
            "initialize" => json(
                200,
                &rpc_result(
                    &id,
                    serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
                ),
            ),
            "notifications/initialized" => UnitHttpResponse::new(202),
            "tools/list" => json(200, &tools_list_response(&id, &[(tool, "a tool")])),
            "prompts/list" => json(
                200,
                &serde_json::json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "Method not found"}})
                .to_string(),
            ),
            "tools/call" => on_call(&id),
            other => json(500, &format!("unexpected {other}")),
        }
    })
}

fn ok_call(id: &str) -> UnitHttpResponse {
    json(200, &rpc_result(id, serde_json::json!({"content": []})))
}

fn healthy(tool: &'static str) -> RouterBackend {
    backend(tool, Default::default(), ok_call)
}

fn plain() -> pdk_unit::UnitTest {
    tester(
        config(serde_json::json!({})),
        healthy("search"),
        healthy("orders"),
    )
}

#[test]
fn batches_are_refused() {
    let resp = plain().request(post_raw(
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#,
    ));
    assert_eq!(resp.status_code(), 200);
    let b = body(&resp);
    assert_eq!(b["error"]["code"], -32600);
    assert!(b["id"].is_null());
}

#[test]
fn null_id_and_missing_method_are_invalid_requests() {
    let mut t = plain();
    let b = body(&t.request(post_raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)));
    assert_eq!(b["error"]["code"], -32600);
    assert!(b["id"].is_null());

    let b = body(&t.request(post_raw(r#"{"jsonrpc":"2.0","id":9}"#)));
    assert_eq!(b["error"]["code"], -32600);
    assert_eq!(b["id"], 9);
}

#[test]
fn client_responses_are_accepted_without_a_body() {
    let resp = plain().request(post_raw(r#"{"jsonrpc":"2.0","id":"s-1","result":{}}"#));
    assert_eq!(resp.status_code(), 202);
    assert!(resp.body().is_empty());
}

#[test]
fn resource_templates_list_is_empty() {
    let b = body(&plain().request(rpc("resources/templates/list", serde_json::json!({}))));
    assert_eq!(b["id"], 7);
    assert_eq!(b["result"], serde_json::json!({"resourceTemplates": []}));
}

#[test]
fn upstream_without_a_list_method_is_an_empty_cached_list() {
    let jira_log: Log = Default::default();
    let mut t = tester(
        config(serde_json::json!({"exposeUpstreamErrors": true})),
        backend("search", jira_log.clone(), ok_call),
        healthy("orders"),
    );
    for _ in 0..2 {
        let b = body(&t.request(
            rpc("prompts/list", serde_json::json!({})).with_header("authorization", "Bearer a"),
        ));
        assert_eq!(b["result"]["prompts"], serde_json::json!([]));
        assert!(b["result"]["_meta"].is_null(), "not skipped: {b}");
    }
    let lists = jira_log
        .borrow()
        .iter()
        .filter(|m| *m == "prompts/list")
        .count();
    assert_eq!(lists, 1, "the -32601 answer is cached");
}

#[test]
fn oversized_upstream_answer_is_an_error_naming_the_server() {
    let big = |id: &str| {
        let text = "x".repeat(2 << 20);
        json(
            200,
            &rpc_result(
                id,
                serde_json::json!({"content": [{"type": "text", "text": text}]}),
            ),
        )
    };
    let mut t = tester(
        config(serde_json::json!({"maxUpstreamResponseBytes": 1 << 20})),
        backend("search", Default::default(), big),
        healthy("orders"),
    );
    let resp = t.request(rpc(
        "tools/call",
        serde_json::json!({"name": "jira_search"}),
    ));
    assert_eq!(resp.status_code(), 200);
    let b = body(&resp);
    assert_eq!(b["error"]["code"], -32603);
    assert_eq!(b["error"]["data"]["server"], "jira");
    assert!(b["error"]["message"]
        .as_str()
        .unwrap()
        .contains("maxUpstreamResponseBytes"));
}

#[test]
fn require_caller_auth_challenges_posts_without_authorization() {
    let jira_log: Log = Default::default();
    let mut t = tester(
        config(serde_json::json!({
            "requireCallerAuth": true,
            "resourceUrl": "https://hub.test/mcp",
            "authorizationServers": ["https://idp.test"]
        })),
        backend("search", jira_log.clone(), ok_call),
        healthy("orders"),
    );
    for req in [
        rpc("initialize", serde_json::json!({})),
        rpc("tools/list", serde_json::json!({})).with_header("authorization", " "),
    ] {
        let resp = t.request(req);
        assert_eq!(resp.status_code(), 401);
        assert_eq!(
            resp.header("www-authenticate"),
            Some(
                "Bearer resource_metadata=\"https://hub.test/mcp/.well-known/oauth-protected-resource\""
            )
        );
        assert_eq!(body(&resp)["error"]["code"], -32014);
    }
    assert!(jira_log.borrow().is_empty());

    let ok = t.request(rpc("ping", serde_json::json!({})).with_header("authorization", "Bearer a"));
    assert_eq!(ok.status_code(), 200);

    // The metadata itself stays public, in both path forms.
    for path in [
        "/mcp/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resp = t.request(UnitHttpRequest::get().with_path(path));
        assert_eq!(resp.status_code(), 200, "{path}");
        assert_eq!(body(&resp)["resource"], "https://hub.test/mcp");
    }
}

#[test]
fn require_caller_auth_without_metadata_sends_a_bare_challenge() {
    let mut t = tester(
        config(serde_json::json!({"requireCallerAuth": true})),
        healthy("search"),
        healthy("orders"),
    );
    let resp = t.request(rpc("ping", serde_json::json!({})));
    assert_eq!(resp.status_code(), 401);
    assert_eq!(resp.header("www-authenticate"), Some("Bearer"));
}

#[test]
fn caller_sessions_cannot_evict_the_shared_session() {
    let jira_log: Log = Default::default();
    let mut t = tester(
        config(serde_json::json!({})),
        backend("search", jira_log.clone(), ok_call),
        healthy("orders"),
    );
    let call = |tool: &str| rpc("tools/call", serde_json::json!({"name": tool}));
    assert_eq!(t.request(call("jira_search")).status_code(), 200);
    // More distinct callers than the session caches hold.
    for i in 0..1100 {
        let resp =
            t.request(call("erp_orders").with_header("authorization", format!("Bearer {i}")));
        assert_eq!(resp.status_code(), 200);
    }
    assert_eq!(t.request(call("jira_search")).status_code(), 200);
    let inits = jira_log
        .borrow()
        .iter()
        .filter(|m| *m == "initialize")
        .count();
    assert_eq!(inits, 1, "the shared jira session survived the flood");
}
