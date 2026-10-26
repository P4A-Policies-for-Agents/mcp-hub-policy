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
pub const AUTH_REQUIRED: i32 = -32014;

/// Inbound JSON-RPC request from the MCP client, as accepted by
/// [`parse_inbound`] (which checks `jsonrpc`).
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    /// Notifications carry no `id`. Requests carry a string or number.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A POST body the hub accepts.
#[derive(Debug)]
pub enum Inbound {
    /// A request, or a notification when it has no `id`.
    Request(JsonRpcRequest),
    /// The client's answer to a server-to-client request. The hub sends
    /// none, so these are acknowledged and dropped.
    Response,
}

/// Why a POST body was refused: the id to answer with (when it was
/// readable), the error code and message.
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub id: Option<serde_json::Value>,
    pub code: i32,
    pub message: String,
}

impl Refusal {
    fn invalid(id: Option<serde_json::Value>, message: impl Into<String>) -> Self {
        Self {
            id,
            code: INVALID_REQUEST,
            message: message.into(),
        }
    }
}

/// Classify a POST body. The raw shape is checked before deserializing
/// because serde reads `"id": null` as a missing id (a notification).
/// Batches are refused: MCP 2025-06-18 removed them, and the hub answers
/// one message per HTTP exchange.
pub fn parse_inbound(body: &[u8]) -> Result<Inbound, Refusal> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|e| Refusal {
        id: None,
        code: PARSE_ERROR,
        message: format!("invalid JSON-RPC body: {e}"),
    })?;
    let obj = match value {
        serde_json::Value::Object(obj) => obj,
        serde_json::Value::Array(_) => {
            return Err(Refusal::invalid(None, "batch requests are not supported"))
        }
        _ => return Err(Refusal::invalid(None, "JSON-RPC message must be an object")),
    };
    // A client's response to a request it couldn't read carries `"id": null`.
    let response =
        !obj.contains_key("method") && (obj.contains_key("result") || obj.contains_key("error"));
    let id = match obj.get("id") {
        None => None,
        Some(serde_json::Value::Null) if response => None,
        Some(id @ (serde_json::Value::String(_) | serde_json::Value::Number(_))) => {
            Some(id.clone())
        }
        Some(_) => return Err(Refusal::invalid(None, "'id' must be a string or a number")),
    };
    match obj.get("jsonrpc").and_then(|v| v.as_str()) {
        Some(JSONRPC_VERSION) => {}
        other => {
            return Err(Refusal::invalid(
                id,
                format!("expected jsonrpc=2.0, got '{}'", other.unwrap_or_default()),
            ))
        }
    }
    match obj.get("method") {
        Some(serde_json::Value::String(_)) => {}
        None if response => return Ok(Inbound::Response),
        None => return Err(Refusal::invalid(id, "missing 'method'")),
        Some(_) => return Err(Refusal::invalid(id, "'method' must be a string")),
    }
    serde_json::from_value(serde_json::Value::Object(obj))
        .map(Inbound::Request)
        .map_err(|e| Refusal::invalid(id, format!("invalid JSON-RPC request: {e}")))
}

/// Outbound JSON-RPC request envelope written when the hub is the client
/// (egress to an upstream MCP server). `id` is `None` for notifications,
/// which JSON-RPC 2.0 requires to omit the member entirely.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcOutbound {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
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
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }

    pub fn notification(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: None,
            method: method.into(),
            params,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        serde_json::to_vec(&self).expect("serializing JsonRpcOutbound never fails")
    }
}

/// Incoming JSON-RPC response envelope from an upstream MCP server. The
/// hub issues one request per HTTP exchange, so `id` isn't checked.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
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
    fn outbound_notification_omits_id() {
        let req = JsonRpcOutbound::notification("notifications/initialized", None);
        let v: serde_json::Value = serde_json::from_slice(&req.into_bytes()).unwrap();
        assert!(
            v.get("id").is_none(),
            "notifications must not carry an id: {v}"
        );
    }

    #[test]
    fn request_is_notification_when_id_missing() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(req.is_notification());
    }

    fn refusal(body: &str) -> Refusal {
        parse_inbound(body.as_bytes()).unwrap_err()
    }

    #[test]
    fn inbound_requests_and_notifications_parse() {
        let Ok(Inbound::Request(req)) =
            parse_inbound(br#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#)
        else {
            panic!("request expected");
        };
        assert_eq!(req.id, Some(serde_json::json!("a")));
        let Ok(Inbound::Request(note)) =
            parse_inbound(br#"{"jsonrpc":"2.0","method":"notifications/x"}"#)
        else {
            panic!("notification expected");
        };
        assert!(note.is_notification());
    }

    #[test]
    fn client_responses_are_acknowledged() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#,
        ] {
            assert!(matches!(
                parse_inbound(body.as_bytes()),
                Ok(Inbound::Response)
            ));
        }
    }

    #[test]
    fn malformed_messages_are_refused() {
        assert_eq!(refusal("{").code, PARSE_ERROR);
        let batch = refusal(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
        assert_eq!((batch.code, batch.id), (INVALID_REQUEST, None));
        assert!(batch.message.contains("batch"));
        for body in [
            r#""ping""#,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#,
        ] {
            let r = refusal(body);
            assert_eq!((r.code, r.id), (INVALID_REQUEST, None), "{body}");
        }
        for body in [
            r#"{"jsonrpc":"2.0","id":5}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":7}"#,
            r#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#,
            r#"{"id":5,"method":"ping"}"#,
        ] {
            let r = refusal(body);
            assert_eq!(
                (r.code, r.id),
                (INVALID_REQUEST, Some(serde_json::json!(5))),
                "{body}"
            );
        }
    }

    #[test]
    fn error_response_includes_code_and_message() {
        let bytes = error_response(
            Some(serde_json::json!(7)),
            METHOD_NOT_FOUND,
            "no such method",
        );
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "no such method");
        assert_eq!(v["id"], 7);
    }
}
