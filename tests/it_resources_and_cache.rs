// Copyright 2026 Salesforce, Inc. All rights reserved.

//! `resources/read` routes by URI, `*/list` answers are cached per
//! (server, caller) for `manifestCacheTtlSeconds`, and SSE upstreams may
//! send notifications before their response.

mod common;

use common::{header, json, rpc_result, tools_list_response, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
use std::cell::RefCell;
use std::rc::Rc;

/// Records `(method, authorization)` per upstream request.
type Log = Rc<RefCell<Vec<(String, Option<String>)>>>;

fn config(ttl: u32, docs_auth: &str) -> String {
    serde_json::json!({
        "mcpEndpoint": "/mcp",
        "collisionStrategy": "prefix",
        "manifestCacheTtlSeconds": ttl,
        "mcpServers": [
            {"name": "docs", "endpoint": "http://docs.test/mcp", "enabled": true, "authType": docs_auth},
            {"name": "wiki", "endpoint": "http://wiki.test/mcp", "enabled": true, "authType": "none"}
        ]
    })
    .to_string()
}

/// Minimal MCP server exposing one tool and one resource (`<uri>`). The
/// first `fail_lists` list calls answer HTTP 500.
fn backend(tool: &'static str, uri: &'static str, fail_lists: usize, log: Log) -> RouterBackend {
    let failures = Rc::new(RefCell::new(fail_lists));
    RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let method = b["method"].as_str().unwrap_or("").to_string();
        log.borrow_mut()
            .push((method.clone(), header(&req, "authorization")));
        let id = b["id"].as_str().unwrap_or("").to_string();
        if method.ends_with("/list") && *failures.borrow() > 0 {
            *failures.borrow_mut() -= 1;
            return json(500, "temporarily down");
        }
        match method.as_str() {
            "initialize" => json(
                200,
                &rpc_result(&id, serde_json::json!({"capabilities": {}})),
            ),
            "notifications/initialized" => UnitHttpResponse::new(202),
            "tools/list" => json(200, &tools_list_response(&id, &[(tool, "a tool")])),
            "resources/list" => json(
                200,
                &rpc_result(
                    &id,
                    serde_json::json!({"resources": [{"name": "page", "uri": uri}]}),
                ),
            ),
            "resources/read" => json(
                200,
                &rpc_result(
                    &id,
                    serde_json::json!({"contents": [{"uri": b["params"]["uri"], "text": tool}]}),
                ),
            ),
            other => json(500, &format!("unexpected {other}")),
        }
    })
}

fn tester(cfg: String, docs: RouterBackend, wiki: RouterBackend) -> pdk_unit::UnitTest {
    UnitTestBuilder::default()
        .with_config(cfg)
        .with_http_upstream_from_authority("docs.test", docs)
        .with_http_upstream_from_authority("wiki.test", wiki)
        .with_entrypoint(configure)
}

fn rpc(id: u32, method: &str, params: serde_json::Value) -> UnitHttpRequest {
    UnitHttpRequest::post()
        .with_path("/mcp")
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                .to_string()
                .into_bytes(),
        )
}

fn body(resp: &UnitHttpResponse) -> serde_json::Value {
    serde_json::from_slice(resp.body()).unwrap()
}

fn count(log: &Log, method: &str) -> usize {
    log.borrow().iter().filter(|(m, _)| m == method).count()
}

#[test]
fn resources_read_routes_by_uri_to_the_owner() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(60, "none"),
        backend("d", "file:///readme.md", 0, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    let v = body(&t.request(rpc(
        1,
        "resources/read",
        serde_json::json!({"uri": "wiki://home"}),
    )));
    assert_eq!(v["result"]["contents"][0]["uri"], "wiki://home", "{v:#}");
    assert_eq!(v["result"]["contents"][0]["text"], "w");
    assert_eq!(count(&wiki_log, "resources/read"), 1);
    assert_eq!(count(&docs_log, "resources/read"), 0);
}

#[test]
fn resources_read_unknown_uri_is_resource_not_found() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(60, "none"),
        backend("d", "file:///readme.md", 0, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    let v = body(&t.request(rpc(
        2,
        "resources/read",
        serde_json::json!({"uri": "wiki://nope"}),
    )));
    assert_eq!(v["error"]["code"], -32012, "{v:#}");
    assert_eq!(
        count(&docs_log, "resources/read") + count(&wiki_log, "resources/read"),
        0
    );
}

#[test]
fn list_answers_are_cached_per_upstream() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(60, "none"),
        backend("d", "file:///readme.md", 0, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    for id in 1..=3 {
        let v = body(&t.request(rpc(id, "tools/list", serde_json::json!({}))));
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["docs_d", "wiki_w"], "{v:#}");
    }
    assert_eq!(count(&docs_log, "tools/list"), 1);
    assert_eq!(count(&wiki_log, "tools/list"), 1);

    // A routed call by name that isn't prefixed falls back to the cached
    // manifest too, without asking the upstreams again.
    let v = body(&t.request(rpc(4, "tools/call", serde_json::json!({"name": "ghost"}))));
    assert_eq!(v["error"]["code"], -32011, "{v:#}");
    assert_eq!(count(&docs_log, "tools/list"), 1);
}

#[test]
fn zero_ttl_disables_the_manifest_cache() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(0, "none"),
        backend("d", "file:///readme.md", 0, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    t.request(rpc(1, "tools/list", serde_json::json!({})));
    t.request(rpc(2, "tools/list", serde_json::json!({})));
    assert_eq!(count(&docs_log, "tools/list"), 2);
    assert_eq!(count(&wiki_log, "tools/list"), 2);
}

#[test]
fn failed_list_answers_are_not_cached() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(60, "none"),
        backend("d", "file:///readme.md", 1, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    let v = body(&t.request(rpc(1, "tools/list", serde_json::json!({}))));
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 1, "{v:#}");
    let v = body(&t.request(rpc(2, "tools/list", serde_json::json!({}))));
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 2, "{v:#}");
    assert_eq!(count(&docs_log, "tools/list"), 2);
    assert_eq!(count(&wiki_log, "tools/list"), 1);
}

#[test]
fn passthrough_list_answers_are_cached_per_caller() {
    let (docs_log, wiki_log): (Log, Log) = Default::default();
    let mut t = tester(
        config(60, "passthrough"),
        backend("d", "file:///readme.md", 0, docs_log.clone()),
        backend("w", "wiki://home", 0, wiki_log.clone()),
    );

    for token in ["Bearer alice", "Bearer bob", "Bearer alice"] {
        let v = body(&t.request(
            rpc(1, "tools/list", serde_json::json!({})).with_header("authorization", token),
        ));
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 2, "{v:#}");
    }
    let callers: Vec<_> = docs_log
        .borrow()
        .iter()
        .filter(|(m, _)| m == "tools/list")
        .map(|(_, a)| a.clone().unwrap_or_default())
        .collect();
    assert_eq!(callers, vec!["Bearer alice", "Bearer bob"]);
    // The static upstream is shared by every caller.
    assert_eq!(count(&wiki_log, "tools/list"), 1);
}

#[test]
fn sse_notifications_before_the_response_are_skipped() {
    let sse = RouterBackend::new(|req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let id = b["id"].as_str().unwrap_or("").to_string();
        match b["method"].as_str().unwrap_or("") {
            "notifications/initialized" => UnitHttpResponse::new(202),
            "initialize" => json(
                200,
                &rpc_result(&id, serde_json::json!({"capabilities": {}})),
            ),
            _ => {
                let progress = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": 1, "progress": 50}
                });
                let result = rpc_result(
                    &id,
                    serde_json::json!({"content": [{"type": "text", "text": "done"}], "isError": false}),
                );
                UnitHttpResponse::new(200)
                    .with_header("content-type", "text/event-stream")
                    .with_body(
                        format!("event: message\ndata: {progress}\n\nevent: message\ndata: {result}\n\n")
                            .into_bytes(),
                    )
            }
        }
    });
    let mut t = tester(
        config(60, "none"),
        sse,
        backend("w", "wiki://home", 0, Log::default()),
    );

    let v = body(&t.request(rpc(
        9,
        "tools/call",
        serde_json::json!({"name": "docs_run"}),
    )));
    assert_eq!(v["id"], 9);
    assert_eq!(v["result"]["content"][0]["text"], "done", "{v:#}");
}

#[test]
fn resources_read_finds_uris_hidden_by_name_collisions() {
    // Both servers name their resource "page"; firstWins drops wiki's from
    // the merged list and reject fails the merge, but its URI stays readable.
    for strategy in ["firstWins", "reject"] {
        let cfg = config(60, "none").replace("\"prefix\"", &format!("\"{strategy}\""));
        let (docs_log, wiki_log): (Log, Log) = Default::default();
        let mut t = tester(
            cfg,
            backend("d", "file:///readme.md", 0, docs_log.clone()),
            backend("w", "wiki://home", 0, wiki_log.clone()),
        );

        let v = body(&t.request(rpc(
            1,
            "resources/read",
            serde_json::json!({"uri": "wiki://home"}),
        )));
        assert_eq!(v["result"]["contents"][0]["text"], "w", "{strategy}: {v:#}");
        assert_eq!(count(&wiki_log, "resources/read"), 1, "{strategy}");
        assert_eq!(count(&docs_log, "resources/read"), 0, "{strategy}");
    }
}
