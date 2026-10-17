// Copyright 2026 Salesforce, Inc. All rights reserved.

//! `authType: passthrough` — the caller's credential reaches only the
//! servers configured for it, upstream sessions are kept per caller, and
//! auth failures surface as real HTTP 401s with `WWW-Authenticate`.

mod common;

use common::{header, json, rpc_result, tools_list_response, RouterBackend};
use mcp_hub::*;
use pdk_unit::{UnitHttpMessage, UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
use std::cell::RefCell;
use std::rc::Rc;

const METADATA_URL: &str = "https://hub.test/mcp/.well-known/oauth-protected-resource";

fn policy_config(with_metadata: bool) -> String {
    let mut cfg = serde_json::json!({
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
                "name": "erp",
                "endpoint": "http://erp.test/mcp",
                "enabled": true,
                "authType": "passthrough"
            }
        ]
    });
    if with_metadata {
        cfg["resourceUrl"] = "https://hub.test/mcp/".into();
        cfg["authorizationServers"] = serde_json::json!(["https://idp.test/oauth2/default"]);
        cfg["scopesSupported"] = serde_json::json!(["mcp.read"]);
    }
    cfg.to_string()
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

/// Minimal MCP server: answers the handshake (issuing a session id derived
/// from the caller's Authorization when `issue_sessions`), `tools/list`
/// and `tools/call`. Records `(method, authorization, session id)`.
type Log = Rc<RefCell<Vec<(String, Option<String>, Option<String>)>>>;

fn mcp_backend(tool: &'static str, issue_sessions: bool, log: Log) -> RouterBackend {
    RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let method = b["method"].as_str().unwrap_or("").to_string();
        let auth = header(&req, "authorization");
        let sid = header(&req, "mcp-session-id");
        log.borrow_mut().push((method.clone(), auth.clone(), sid));
        let id = b["id"].as_str().unwrap_or("").to_string();
        match method.as_str() {
            "initialize" => {
                let resp = json(
                    200,
                    &rpc_result(&id, serde_json::json!({"capabilities": {}})),
                );
                match (issue_sessions, auth) {
                    (true, Some(a)) => resp.with_header("mcp-session-id", format!("sid-{a}")),
                    _ => resp,
                }
            }
            "notifications/initialized" => UnitHttpResponse::new(202),
            "tools/list" => json(200, &tools_list_response(&id, &[(tool, "a tool")])),
            "tools/call" => json(
                200,
                &rpc_result(&id, serde_json::json!({"content": [], "isError": false})),
            ),
            other => json(500, &format!("unexpected {other}")),
        }
    })
}

fn tester(cfg: String, jira: RouterBackend, erp: RouterBackend) -> pdk_unit::UnitTest {
    UnitTestBuilder::default()
        .with_config(cfg)
        .with_http_upstream_from_authority("jira.test", jira)
        .with_http_upstream_from_authority("erp.test", erp)
        .with_entrypoint(configure)
}

#[test]
fn caller_token_reaches_only_passthrough_servers() {
    let jira_log: Log = Default::default();
    let erp_log: Log = Default::default();
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, jira_log.clone()),
        mcp_backend("orders", false, erp_log.clone()),
    );

    let resp = t.request(
        rpc(1, "tools/list", serde_json::json!({})).with_header("authorization", "Bearer alice"),
    );
    assert_eq!(resp.status_code(), 200);
    let mut names: Vec<String> = body(&resp)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["erp_orders", "jira_search"]);

    assert!(jira_log
        .borrow()
        .iter()
        .all(|(_, auth, _)| auth.as_deref() == Some("Bearer jira-secret")));
    assert!(erp_log
        .borrow()
        .iter()
        .all(|(_, auth, _)| auth.as_deref() == Some("Bearer alice")));
}

#[test]
fn list_without_token_skips_passthrough_servers() {
    let erp_log: Log = Default::default();
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", false, erp_log.clone()),
    );

    let resp = t.request(rpc(1, "tools/list", serde_json::json!({})));
    assert_eq!(resp.status_code(), 200);
    let tools = body(&resp)["result"]["tools"].as_array().unwrap().clone();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "jira_search");
    assert!(
        erp_log.borrow().is_empty(),
        "erp must not be called without a token"
    );
}

#[test]
fn call_without_token_challenges_with_hub_metadata() {
    let mut t = tester(
        policy_config(true),
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", false, Default::default()),
    );

    let resp = t.request(rpc(
        2,
        "tools/call",
        serde_json::json!({"name": "erp_orders", "arguments": {}}),
    ));
    assert_eq!(resp.status_code(), 401);
    assert_eq!(
        resp.header("www-authenticate"),
        Some(format!("Bearer resource_metadata=\"{METADATA_URL}\"").as_str())
    );
    assert_eq!(body(&resp)["error"]["code"], -32014);
}

#[test]
fn upstream_401_is_relayed_when_hub_has_no_metadata() {
    let challenge = "Bearer realm=\"JWT\", resource_metadata=\"https://erp.test/.well-known/x\"";
    let erp =
        RouterBackend::new(move |_req| json(401, "{}").with_header("www-authenticate", challenge));
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        erp,
    );

    let resp = t.request(
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", "Bearer expired"),
    );
    assert_eq!(resp.status_code(), 401);
    assert_eq!(resp.header("www-authenticate"), Some(challenge));
}

#[test]
fn configured_header_overrides_caller_token() {
    let cfg = {
        let mut v: serde_json::Value = serde_json::from_str(&policy_config(false)).unwrap();
        v["mcpServers"][1]["headers"] =
            serde_json::json!([{"name": "Authorization", "value": "Bearer service"}]);
        v.to_string()
    };
    let erp_log: Log = Default::default();
    let mut t = tester(
        cfg,
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", false, erp_log.clone()),
    );

    let resp = t.request(
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", "Bearer alice"),
    );
    assert_eq!(resp.status_code(), 200);
    assert!(erp_log
        .borrow()
        .iter()
        .all(|(_, auth, _)| auth.as_deref() == Some("Bearer service")));
}

#[test]
fn upstream_sessions_are_kept_per_caller() {
    let erp_log: Log = Default::default();
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", true, erp_log.clone()),
    );
    let call = |who: &str| {
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", format!("Bearer {who}"))
    };

    for who in ["alice", "bob", "alice", "bob"] {
        assert_eq!(t.request(call(who)).status_code(), 200);
    }

    let log = erp_log.borrow();
    let inits = log.iter().filter(|(m, _, _)| m == "initialize").count();
    assert_eq!(inits, 2, "one handshake per caller, then reuse: {log:?}");
    for (method, auth, sid) in log.iter().filter(|(m, _, _)| m != "initialize") {
        let expected = format!("sid-{}", auth.as_deref().unwrap());
        assert_eq!(sid.as_deref(), Some(expected.as_str()), "{method}");
    }
}

#[test]
fn upstream_404_reinitializes_and_retries_once() {
    let erp_log: Log = Default::default();
    let inits = Rc::new(RefCell::new(0u32));
    let expired = Rc::new(RefCell::new(false));
    let (log, n, gone) = (erp_log.clone(), inits.clone(), expired.clone());
    let erp = RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let method = b["method"].as_str().unwrap_or("").to_string();
        let sid = header(&req, "mcp-session-id");
        log.borrow_mut().push((method.clone(), None, sid.clone()));
        let id = b["id"].as_str().unwrap_or("").to_string();
        match method.as_str() {
            "initialize" => {
                *n.borrow_mut() += 1;
                json(200, &rpc_result(&id, serde_json::json!({})))
                    .with_header("mcp-session-id", format!("s{}", n.borrow()))
            }
            "notifications/initialized" => UnitHttpResponse::new(202),
            _ if *gone.borrow() && sid.as_deref() == Some("s1") => json(404, "session not found"),
            _ => json(200, &rpc_result(&id, serde_json::json!({"content": []}))),
        }
    });
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        erp,
    );
    let call = || {
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", "Bearer alice")
    };

    assert_eq!(t.request(call()).status_code(), 200);
    // The upstream restarts and forgets s1.
    *expired.borrow_mut() = true;
    let resp = t.request(call());
    assert_eq!(resp.status_code(), 200);
    assert!(body(&resp)["result"].is_object());
    assert_eq!(*inits.borrow(), 2);
    let calls: Vec<Option<String>> = erp_log
        .borrow()
        .iter()
        .filter(|(m, _, _)| m == "tools/call")
        .map(|(_, _, s)| s.clone())
        .collect();
    assert_eq!(
        calls,
        vec![Some("s1".into()), Some("s1".into()), Some("s2".into())]
    );
}

#[test]
fn notifications_to_upstreams_carry_no_id() {
    let bodies: Rc<RefCell<Vec<serde_json::Value>>> = Default::default();
    let captured = bodies.clone();
    let erp = RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        captured.borrow_mut().push(b.clone());
        let id = b["id"].as_str().unwrap_or("").to_string();
        match b["method"].as_str().unwrap_or("") {
            "notifications/initialized" => UnitHttpResponse::new(202),
            _ => json(200, &rpc_result(&id, serde_json::json!({}))),
        }
    });
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        erp,
    );

    t.request(
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", "Bearer alice"),
    );
    let bodies = bodies.borrow();
    let note = bodies
        .iter()
        .find(|b| b["method"] == "notifications/initialized")
        .expect("hub completes the upstream handshake");
    assert!(note.get("id").is_none(), "{note}");
}

#[test]
fn protected_resource_metadata_is_served() {
    let mut t = tester(
        policy_config(true),
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", false, Default::default()),
    );

    let resp =
        t.request(UnitHttpRequest::get().with_path("/mcp/.well-known/oauth-protected-resource"));
    assert_eq!(resp.status_code(), 200);
    let v = body(&resp);
    assert_eq!(v["resource"], "https://hub.test/mcp");
    assert_eq!(
        v["authorization_servers"][0],
        "https://idp.test/oauth2/default"
    );
    assert_eq!(v["scopes_supported"][0], "mcp.read");
    assert_eq!(v["bearer_methods_supported"][0], "header");
}

#[test]
fn protected_resource_metadata_is_not_served_when_unconfigured() {
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        mcp_backend("orders", false, Default::default()),
    );
    let resp =
        t.request(UnitHttpRequest::get().with_path("/mcp/.well-known/oauth-protected-resource"));
    // Falls through to the normal `/mcp` GET handling.
    assert_eq!(resp.status_code(), 405);
}

#[test]
fn static_auth_401_is_not_turned_into_a_client_challenge() {
    let jira = RouterBackend::new(|_req| {
        json(401, "{}").with_header("www-authenticate", "Bearer realm=\"internal\"")
    });
    let mut t = tester(
        policy_config(true),
        jira,
        mcp_backend("orders", false, Default::default()),
    );

    let resp = t.request(
        rpc(2, "tools/call", serde_json::json!({"name": "jira_search"}))
            .with_header("authorization", "Bearer alice"),
    );
    assert_eq!(resp.status_code(), 200);
    assert!(resp.header("www-authenticate").is_none());
    assert_eq!(body(&resp)["error"]["code"], -32010);
}

#[test]
fn rejected_initialize_is_not_cached() {
    let inits = Rc::new(RefCell::new(0u32));
    let n = inits.clone();
    let erp = RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let id = b["id"].as_str().unwrap_or("").to_string();
        match b["method"].as_str().unwrap_or("") {
            "initialize" => {
                *n.borrow_mut() += 1;
                json(
                    200,
                    &serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unsupported version"}})
                    .to_string(),
                )
            }
            _ => json(500, "must not be called without a session"),
        }
    });
    let mut t = tester(
        policy_config(false),
        mcp_backend("search", false, Default::default()),
        erp,
    );
    let call = || {
        rpc(2, "tools/call", serde_json::json!({"name": "erp_orders"}))
            .with_header("authorization", "Bearer alice")
    };

    for _ in 0..2 {
        let resp = t.request(call());
        assert!(body(&resp)["error"].is_object());
    }
    assert_eq!(*inits.borrow(), 2, "each call retries the handshake");
}

#[test]
fn outbound_request_ids_are_unique() {
    let ids: Rc<RefCell<Vec<String>>> = Default::default();
    let seen = ids.clone();
    let jira = RouterBackend::new(move |req| {
        let b: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
        let id = b["id"].as_str().unwrap_or("").to_string();
        if !id.is_empty() {
            seen.borrow_mut().push(id.clone());
        }
        match b["method"].as_str().unwrap_or("") {
            "notifications/initialized" => UnitHttpResponse::new(202),
            _ => json(200, &rpc_result(&id, serde_json::json!({"content": []}))),
        }
    });
    let mut t = tester(
        policy_config(false),
        jira,
        mcp_backend("orders", false, Default::default()),
    );
    for _ in 0..3 {
        t.request(rpc(
            2,
            "tools/call",
            serde_json::json!({"name": "jira_search"}),
        ));
    }
    let ids = ids.borrow();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), 4, "initialize + 3 calls: {ids:?}");
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
}
