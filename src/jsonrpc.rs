//! JSON-RPC 2.0 envelope used by the MCP Streamable HTTP transport.
//!
//! All MCP control-plane messages (`initialize`, `tools/list`,
//! `tools/call`, `resources/list`, `resources/read`, `prompts/list`,
//! `prompts/get`, notifications) are JSON-RPC 2.0 over a single endpoint.
//! The hub speaks JSON-RPC both as the *server* (toward MCP clients) and
//! as the *client* (toward each upstream MCP server).

use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// MCP application-level error codes (extension space; -32000 to -32099).
pub const SERVER_UNAVAILABLE: i32 = -32010;
pub const TOOL_NOT_FOUND: i32 = -32011;
pub const RESOURCE_NOT_FOUND: i32 = -32012;
pub const PROMPT_NOT_FOUND: i32 = -32013;

/// Inbound JSON-RPC request from the MCP client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// Notifications carry no `id`. Requests carry a string or number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Outbound JSON-RPC request envelope written when the hub is the client
/// (egress to an upstream MCP server).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcOutbound {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcOutbound {
    pub fn new(
        id: impl Into<serde_json::Value>,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        serde_json::to_vec(&self).expect("serializing JsonRpcOutbound never fails")
    }
}

/// Incoming JSON-RPC response envelope from an upstream MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Build a JSON-RPC success response body for the inbound client.
pub fn success_response(id: serde_json::Value, result: serde_json::Value) -> Vec<u8> {
    let body = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });
    serde_json::to_vec(&body).expect("success response serializes")
}

/// Build a JSON-RPC error response body for the inbound client.
pub fn error_response(id: Option<serde_json::Value>, code: i32, message: &str) -> Vec<u8> {
    error_response_with_data(id, code, message, None)
}

pub fn error_response_with_data(
    id: Option<serde_json::Value>,
    code: i32,
    message: &str,
    data: Option<serde_json::Value>,
) -> Vec<u8> {
    let mut err = serde_json::json!({
        "code": code,
        "message": message,
    });
    if let Some(d) = data {
        err["data"] = d;
    }
    let body = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(serde_json::Value::Null),
        "error": err,
    });
    serde_json::to_vec(&body).expect("error response serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_serializes_with_jsonrpc_2() {
        let req = JsonRpcOutbound::new(
            serde_json::json!("rid-1"),
            "tools/list",
            Some(serde_json::json!({})),
        );
        let bytes = req.into_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "rid-1");
        assert_eq!(v["method"], "tools/list");
        assert!(v["params"].is_object());
    }

    #[test]
    fn outbound_omits_params_when_none() {
        let req = JsonRpcOutbound::new(serde_json::json!(1), "ping", None);
        let bytes = req.into_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("params").is_none());
    }

    #[test]
    fn request_is_notification_when_id_missing() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn error_response_includes_code_and_message() {
        let bytes = error_response(Some(serde_json::json!(7)), METHOD_NOT_FOUND, "no such method");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "no such method");
        assert_eq!(v["id"], 7);
    }
}
