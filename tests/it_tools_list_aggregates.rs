// Copyright 2026 Salesforce, Inc. All rights reserved.

//! `tools/list` aggregates manifests from every enabled upstream and
//! prefixes tool names with the server name to disambiguate collisions.

mod common;

use common::{header, json, tools_list_response, RouterBackend};
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
                "authType": "apiKeyHeader",
                "headerName": "x-api-key",
                "apiKey": "slack-key"
            }
        ]
    })
    .to_string()
}

fn jira_backend(log: Rc<RefCell<Vec<String>>>) -> RouterBackend {
    RouterBackend::new(move |req| {
        let path = header(&req, ":path").unwrap_or_default();
        let auth = header(&req, "authorization").unwrap_or_default();
        log.borrow_mut().push(format!("jira {path} {auth}"));
        // Both servers happen to expose a "search" tool — the hub must
        // prefix-rename them.
        json(
            200,
            &tools_list_response(
                "tools-list-jira",
                &[("search", "jira search"), ("ticket", "jira ticket")],
            ),
        )
    })
}

fn slack_backend(log: Rc<RefCell<Vec<String>>>) -> RouterBackend {
    RouterBackend::new(move |req| {
        let path = header(&req, ":path").unwrap_or_default();
        let api_key = header(&req, "x-api-key").unwrap_or_default();
        log.borrow_mut().push(format!("slack {path} {api_key}"));
        json(
            200,
            &tools_list_response(
                "tools-list-slack",
                &[("search", "slack search"), ("post", "slack post")],
            ),
        )
    })
}

#[test]
fn tools_list_aggregates_with_prefix_strategy() {
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let mut tester = UnitTestBuilder::default()
        .with_config(policy_config())
        .with_http_upstream_from_authority("jira.test", jira_backend(log.clone()))
        .with_http_upstream_from_authority("slack.test", slack_backend(log.clone()))
        .with_entrypoint(configure);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "list-1",
        "method": "tools/list"
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
    let tools = v["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"jira.search"), "names={names:?}");
    assert!(names.contains(&"slack.search"), "names={names:?}");
    assert!(names.contains(&"jira.ticket"), "names={names:?}");
    assert!(names.contains(&"slack.post"), "names={names:?}");
    assert_eq!(tools.len(), 4);

    // Internal markers must be stripped.
    for t in tools {
        assert!(t.get("_origin").is_none());
        assert!(t.get("_originalName").is_none());
    }

    // Per-server auth was applied.
    let calls = log.borrow();
    assert!(
        calls.iter().any(|c| c.contains("jira") && c.contains("Bearer jira-secret")),
        "no jira bearer in {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("slack") && c.contains("slack-key")),
        "no slack api-key in {calls:?}"
    );
}
