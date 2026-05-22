// Copyright 2026 Salesforce, Inc. All rights reserved.

//! Helpers shared across mcp-hub integration tests.

use pdk_unit::{Backend, UnitHttpMessage, UnitHttpRequest, UnitHttpResponse};

/// HTTP backend that dispatches to a closure based on the request.
pub struct RouterBackend {
    inner: Box<dyn Fn(UnitHttpRequest) -> UnitHttpResponse>,
}

impl RouterBackend {
    pub fn new<F: Fn(UnitHttpRequest) -> UnitHttpResponse + 'static>(f: F) -> Self {
        Self { inner: Box::new(f) }
    }
}

impl Backend for RouterBackend {
    fn call(&self, req: UnitHttpRequest) -> UnitHttpResponse {
        (self.inner)(req)
    }
}

pub fn header(req: &UnitHttpRequest, name: &str) -> Option<String> {
    req.header(name).map(|s| s.to_string())
}

pub fn json(status: u32, body: &str) -> UnitHttpResponse {
    UnitHttpResponse::new(status)
        .with_header("content-type", "application/json")
        .with_body(body.as_bytes().to_vec())
}

/// Build a JSON-RPC success response with the given result body.
pub fn rpc_result(id: &str, result: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

/// Build a `tools/list` response containing N tools.
pub fn tools_list_response(id: &str, tools: &[(&str, &str)]) -> String {
    let arr: Vec<serde_json::Value> = tools
        .iter()
        .map(|(name, description)| {
            serde_json::json!({
                "name": name,
                "description": description,
                "inputSchema": {"type": "object"},
            })
        })
        .collect();
    rpc_result(id, serde_json::json!({"tools": arr}))
}
