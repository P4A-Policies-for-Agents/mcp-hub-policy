// Copyright 2026 Salesforce, Inc. All rights reserved.

//! Upstream failure mapping (status relay, backoff, `exposeUpstreamErrors`),
//! configuration isolation / fail-closed, and outbound headers
//! (`forwardHeaders`, `MCP-Protocol-Version`).

mod common;

use common::{header, json, rpc_result, tools_list_response, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `jira` (static bearer) + `erp` (passthrough), with `extra` merged in at
/// the top level.
fn config(extra: serde_json::Value) -> serde_json::Value {
    let mut cfg = serde_json::json!({
        "mcpEndpoint": "/mcp",
        "strictMode": true,
        "collisionStrategy": "prefix",
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

fn post(body: serde_json::Value) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(body.to_string().into_bytes())
}

fn rpc(method: &str, params: serde_json::Value) -> UnitHttpRequest {
    post(serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}))
}

fn call(tool: &str) -> UnitHttpRequest {
    rpc("tools/call", serde_json::json!({"name": tool}))
}

fn body(resp: &UnitHttpResponse) -> serde_json::Value {
    serde_json::from_slice(resp.body()).unwrap()
}

/// Requests an upstream received: (method, headers).
type Log = Rc<RefCell<Vec<(String, HashMap<String, String>)>>>;

/// MCP server exposing `tool`. `init_version` is the protocol version it
/// answers `initialize` with; `on_call` overrides the `tools/call` reply.
fn backend(
    tool: &'static str,
    init_version: &'static str,
    log: Log,
    on_call: impl Fn(&str) -> UnitHttpResponse + 'static,
) -> RouterBackend {
    RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let method = b["method"].as_str().unwrap_or("").to_string();
        let headers = [
            "authorization",
            "mcp-protocol-version",
            "traceparent",
            "tracestate",
            "x-correlation-id",
            "x-tenant",
        ]
        .iter()
        .filter_map(|h| header(&req, h).map(|v| (h.to_string(), v)))
        .collect();
        log.borrow_mut().push((method.clone(), headers));
        let id = b["id"].as_str().unwrap_or("").to_string();
        match method.as_str() {
            "initialize" => json(
                200,
                &rpc_result(
                    &id,
                    serde_json::json!({"protocolVersion": init_version, "capabilities": {}}),
                ),
            ),
            "notifications/initialized" => UnitHttpResponse::new(202),
            "tools/list" => json(200, &tools_list_response(&id, &[(tool, "a tool")])),
            "tools/call" => on_call(&id),
            other => json(500, &format!("unexpected {other}")),
        }
    })
}

fn ok_call(id: &str) -> UnitHttpResponse {
    json(200, &rpc_result(id, serde_json::json!({"content": []})))
}

fn healthy(tool: &'static str) -> RouterBackend {
    backend(tool, "2025-06-18", Default::default(), ok_call)
}

/// `jira` whose `tools/call` answers HTTP 429 with `Retry-After: 7`.
fn rate_limited_jira() -> RouterBackend {
    backend("search", "2025-06-18", Default::default(), |_| {
        UnitHttpResponse::new(429)
            .with_header("retry-after", "7")
            .with_body(b"slow down".to_vec())
    })
}

#[test]
fn upstream_429_keeps_status_and_retry_after() {
    let mut t = tester(
        config(serde_json::json!({})),
        rate_limited_jira(),
        healthy("orders"),
    );
    let resp = t.request(call("jira_search"));
    assert_eq!(resp.status_code(), 429);
    assert_eq!(resp.header("retry-after"), Some("7"));
    let b = body(&resp);
    assert_eq!(b["id"], 7);
    assert_eq!(b["error"]["code"], -32010);
    assert_eq!(
        b["error"]["data"],
        serde_json::json!({"server": "jira", "reason": "error", "httpStatus": 429}),
        "no body excerpt unless exposeUpstreamErrors"
    );
}

#[test]
fn expose_upstream_errors_adds_body_excerpt() {
    let mut t = tester(
        config(serde_json::json!({"exposeUpstreamErrors": true})),
        rate_limited_jira(),
        healthy("orders"),
    );
    let resp = t.request(call("jira_search"));
    assert_eq!(resp.status_code(), 429);
    assert_eq!(body(&resp)["error"]["data"]["body"], "slow down");
}

#[test]
fn upstream_404_and_400_stay_http_200() {
    for status in [400, 404] {
        let jira = backend("search", "2025-06-18", Default::default(), move |_| {
            UnitHttpResponse::new(status)
        });
        let mut t = tester(config(serde_json::json!({})), jira, healthy("orders"));
        let resp = t.request(call("jira_search"));
        assert_eq!(resp.status_code(), 200, "upstream {status}");
        assert!(resp.header("retry-after").is_none());
        let b = body(&resp);
        assert_eq!(b["error"]["code"], -32010);
        assert_eq!(b["error"]["data"]["httpStatus"], status);
    }
}

/// Upstream whose `initialize` always answers `status`; counts handshakes.
fn failing_init(status: u32, inits: Rc<RefCell<u32>>) -> RouterBackend {
    RouterBackend::new(move |_| {
        *inits.borrow_mut() += 1;
        UnitHttpResponse::new(status).with_header("retry-after", "3")
    })
}

#[test]
fn failed_handshake_relays_5xx_then_backs_off() {
    let inits = Rc::new(RefCell::new(0u32));
    let mut t = tester(
        config(serde_json::json!({})),
        failing_init(503, inits.clone()),
        healthy("orders"),
    );

    let first = t.request(call("jira_search"));
    assert_eq!(first.status_code(), 503);
    assert_eq!(first.header("retry-after"), Some("3"));

    let second = t.request(call("jira_search"));
    assert_eq!(second.status_code(), 503);
    let retry: u64 = second.header("retry-after").unwrap().parse().unwrap();
    assert!(
        retry <= 3,
        "the upstream's shorter Retry-After wins: {retry}"
    );
    assert_eq!(body(&second)["error"]["data"]["reason"], "error");
    assert_eq!(*inits.borrow(), 1, "backed-off upstream is not contacted");

    // The backoff also keeps the dead upstream out of `tools/list`
    // without waiting for it.
    let list = t
        .request(rpc("tools/list", serde_json::json!({})).with_header("authorization", "Bearer a"));
    let names: Vec<_> = body(&list)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["erp_orders"]);
    assert_eq!(*inits.borrow(), 1);
}

#[test]
fn unauthorized_handshake_is_not_backed_off() {
    let inits = Rc::new(RefCell::new(0u32));
    let mut t = tester(
        config(serde_json::json!({})),
        healthy("search"),
        failing_init(401, inits.clone()),
    );
    for _ in 0..2 {
        let resp = t.request(call("erp_orders").with_header("authorization", "Bearer expired"));
        assert_eq!(resp.status_code(), 401);
    }
    assert_eq!(
        *inits.borrow(),
        2,
        "a refreshed token must be tried at once"
    );
}

#[test]
fn skipped_servers_are_reported_only_when_exposed() {
    for expose in [false, true] {
        let mut t = tester(
            config(serde_json::json!({"exposeUpstreamErrors": expose})),
            healthy("search"),
            healthy("orders"),
        );
        // No Authorization: the passthrough server is skipped.
        let b = body(&t.request(rpc("tools/list", serde_json::json!({}))));
        assert_eq!(b["result"]["tools"].as_array().unwrap().len(), 1);
        let meta = &b["result"]["_meta"];
        if expose {
            assert_eq!(
                meta["mcp-hub/skipped"],
                serde_json::json!([{"server": "erp", "reason": "missing_credentials"}])
            );
        } else {
            assert!(meta.is_null(), "unexpected _meta: {meta}");
        }
    }
}

#[test]
fn adopted_protocol_version_is_sent_after_initialize() {
    let log: Log = Default::default();
    let jira = backend("search", "2025-03-26", log.clone(), ok_call);
    let mut t = tester(config(serde_json::json!({})), jira, healthy("orders"));
    assert_eq!(t.request(call("jira_search")).status_code(), 200);

    let log = log.borrow();
    let version = |m: &str| {
        log.iter()
            .find(|(method, _)| method == m)
            .and_then(|(_, h)| h.get("mcp-protocol-version").cloned())
    };
    assert_eq!(version("initialize"), None);
    assert_eq!(
        version("notifications/initialized").as_deref(),
        Some("2025-03-26")
    );
    assert_eq!(version("tools/call").as_deref(), Some("2025-03-26"));
}

#[test]
fn default_forward_headers_reach_upstreams() {
    let log: Log = Default::default();
    let jira = backend("search", "2025-06-18", log.clone(), ok_call);
    let mut t = tester(config(serde_json::json!({})), jira, healthy("orders"));
    t.request(
        call("jira_search")
            .with_header("traceparent", "00-abc-def-01")
            .with_header("x-correlation-id", "corr-1")
            .with_header("x-tenant", "acme"),
    );
    let log = log.borrow();
    let (_, h) = log.iter().find(|(m, _)| m == "tools/call").unwrap();
    assert_eq!(
        h.get("traceparent").map(String::as_str),
        Some("00-abc-def-01")
    );
    assert_eq!(
        h.get("x-correlation-id").map(String::as_str),
        Some("corr-1")
    );
    assert_eq!(h.get("x-tenant"), None, "not in the default list");
    assert_eq!(
        h.get("authorization").map(String::as_str),
        Some("Bearer jira-secret")
    );
}

#[test]
fn forward_headers_are_sent_on_routed_calls_only() {
    // Handshakes and `*/list` answers of a static-auth server are shared
    // by every caller, so one caller's headers must not shape them.
    let log: Log = Default::default();
    let jira = backend("search", "2025-06-18", log.clone(), ok_call);
    let mut t = tester(config(serde_json::json!({})), jira, healthy("orders"));
    let traced = |req: UnitHttpRequest| {
        req.with_header("traceparent", "00-abc-def-01")
            .with_header("x-correlation-id", "corr-1")
    };
    t.request(traced(rpc("tools/list", serde_json::json!({}))));
    t.request(traced(call("jira_search")));

    let log = log.borrow();
    let methods: Vec<_> = log.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    for (method, h) in log.iter() {
        let forwarded = h.contains_key("traceparent") || h.contains_key("x-correlation-id");
        assert_eq!(forwarded, method == "tools/call", "{method}: {h:?}");
    }
}

#[test]
fn configured_forward_headers_replace_defaults_and_server_headers_win() {
    let jira_log: Log = Default::default();
    let erp_log: Log = Default::default();
    let mut cfg = config(serde_json::json!({"forwardHeaders": ["X-Tenant"]}));
    cfg["mcpServers"][0]["headers"] = serde_json::json!([{"name": "x-tenant", "value": "fixed"}]);
    let mut t = tester(
        cfg,
        backend("search", "2025-06-18", jira_log.clone(), ok_call),
        backend("orders", "2025-06-18", erp_log.clone(), ok_call),
    );
    let req = |tool: &str| {
        call(tool)
            .with_header("authorization", "Bearer alice")
            .with_header("traceparent", "00-abc-def-01")
            .with_header("x-tenant", "acme")
    };
    t.request(req("jira_search"));
    t.request(req("erp_orders"));

    let sent = |log: &Log, name: &str| {
        log.borrow()
            .iter()
            .find(|(m, _)| m == "tools/call")
            .and_then(|(_, h)| h.get(name).cloned())
    };
    assert_eq!(sent(&jira_log, "x-tenant").as_deref(), Some("fixed"));
    assert_eq!(sent(&erp_log, "x-tenant").as_deref(), Some("acme"));
    assert_eq!(sent(&erp_log, "traceparent"), None);
}

#[test]
fn misconfigured_server_is_isolated() {
    let mut cfg = config(serde_json::json!({"exposeUpstreamErrors": true}));
    cfg["mcpServers"][0]
        .as_object_mut()
        .unwrap()
        .remove("token");
    let jira_calls = Rc::new(RefCell::new(0u32));
    let mut t = tester(
        cfg,
        failing_init(500, jira_calls.clone()),
        healthy("orders"),
    );

    let list = body(&t.request(
        rpc("tools/list", serde_json::json!({})).with_header("authorization", "Bearer a"),
    ));
    assert_eq!(list["result"]["tools"][0]["name"], "erp_orders");
    assert_eq!(
        list["result"]["_meta"]["mcp-hub/skipped"],
        serde_json::json!([{"server": "jira", "reason": "misconfigured"}])
    );

    let resp = t.request(call("jira_search"));
    assert_eq!(resp.status_code(), 200);
    let b = body(&resp);
    assert_eq!(b["error"]["code"], -32010);
    assert_eq!(b["error"]["data"]["reason"], "misconfigured");
    assert!(b["error"]["message"]
        .as_str()
        .unwrap()
        .contains("requires non-empty 'token'"));
    assert_eq!(*jira_calls.borrow(), 0);
}

#[test]
fn invalid_hub_settings_fail_closed() {
    for extra in [
        serde_json::json!({"collisionStrategy": "bogus"}),
        serde_json::json!({"forwardHeaders": ["authorization"]}),
        serde_json::json!({"mcpServers": []}),
    ] {
        let mut t = tester(config(extra.clone()), healthy("search"), healthy("orders"));
        let resp = t.request(rpc("tools/list", serde_json::json!({})));
        assert_eq!(resp.status_code(), 503, "{extra}");
        let b = body(&resp);
        assert_eq!(b["error"]["code"], -32010);
        assert!(b["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("mcp-hub configuration rejected: "));

        let other = t.request(UnitHttpRequest::get().with_path("/health"));
        assert_eq!(other.status_code(), 404, "strictMode still applies");
    }
}

#[test]
fn wrong_jsonrpc_version_is_invalid_request() {
    let mut t = tester(
        config(serde_json::json!({})),
        healthy("search"),
        healthy("orders"),
    );
    let resp = t.request(post(
        serde_json::json!({"jsonrpc": "1.0", "id": 3, "method": "ping"}),
    ));
    let b = body(&resp);
    assert_eq!(b["id"], 3);
    assert_eq!(b["error"]["code"], -32600);
}

#[test]
fn handshake_4xx_is_not_backed_off() {
    // A 4xx may be provoked by one caller's headers; backing off a shared
    // (static-auth) upstream would take it out for everyone.
    for status in [400, 431] {
        let inits = Rc::new(RefCell::new(0u32));
        let mut t = tester(
            config(serde_json::json!({})),
            failing_init(status, inits.clone()),
            healthy("orders"),
        );
        for _ in 0..2 {
            assert_eq!(t.request(call("jira_search")).status_code(), 200);
        }
        assert_eq!(*inits.borrow(), 2, "status {status}");
    }
}

#[test]
fn list_timeout_does_not_back_off_longer_routed_calls() {
    // `jira.test` has no backend: every call to it is a transport error.
    let mut t = UnitTestBuilder::default()
        .with_config(config(serde_json::json!({})).to_string())
        .with_http_upstream_from_authority("erp.test", healthy("orders"))
        .with_entrypoint(configure);

    let list = t.request(rpc("tools/list", serde_json::json!({})));
    assert_eq!(list.status_code(), 200);

    // Recorded under the 5 s list budget: a 30 s routed call still tries.
    let first = t.request(call("jira_search"));
    assert_eq!(first.status_code(), 200);
    assert_eq!(body(&first)["error"]["data"]["reason"], "unavailable");

    // Now recorded under the routed budget too.
    let second = t.request(call("jira_search"));
    assert_eq!(second.status_code(), 503);
    assert_eq!(body(&second)["error"]["data"]["reason"], "unavailable");
}

#[test]
fn call_for_misconfigured_server_answers_without_fan_out() {
    let mut cfg = config(serde_json::json!({}));
    cfg["mcpServers"][0]["authType"] = "basicAuth".into();
    let erp_log: Log = Default::default();
    let mut t = tester(
        cfg,
        healthy("search"),
        backend("orders", "2025-06-18", erp_log.clone(), ok_call),
    );
    let resp = t.request(call("jira_search").with_header("authorization", "Bearer a"));
    assert_eq!(body(&resp)["error"]["data"]["reason"], "misconfigured");
    assert!(erp_log.borrow().is_empty(), "no */list fan-out needed");
}

#[test]
fn prefix_shared_with_misconfigured_server_is_resolved_by_listing() {
    // Valid `erp` and invalid `erp_x`: `erp_x_tool` could be either.
    let mut cfg = config(serde_json::json!({}));
    cfg["mcpServers"].as_array_mut().unwrap().push(
        serde_json::json!({"name": "erp_x", "endpoint": "http://erpx.test/mcp",
            "authType": "apiKeyHeader"}),
    );
    let mut t = tester(cfg, healthy("search"), healthy("orders"));
    let resp = t.request(call("erp_x_tool").with_header("authorization", "Bearer a"));
    let b = body(&resp);
    assert_eq!(b["error"]["data"]["server"], "erp_x");
    assert_eq!(b["error"]["data"]["reason"], "misconfigured");
}

#[test]
fn only_notifications_are_fanned_out() {
    let erp_log: Log = Default::default();
    let mut t = tester(
        config(serde_json::json!({})),
        healthy("search"),
        backend("orders", "2025-06-18", erp_log.clone(), ok_call),
    );
    let auth = |req: UnitHttpRequest| req.with_header("authorization", "Bearer a");
    // Opens the caller's erp session; notifications only reach sessions.
    assert_eq!(t.request(auth(call("erp_orders"))).status_code(), 200);

    for method in ["tools/call", "notifications/roots/list_changed"] {
        let resp = t.request(auth(post(serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": {"name": "erp_orders"}
        }))));
        assert_eq!(resp.status_code(), 202, "{method}");
    }
    let methods: Vec<_> = erp_log.borrow().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/call",
            "notifications/roots/list_changed"
        ]
    );
}
