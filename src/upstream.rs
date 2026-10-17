//! Outbound JSON-RPC client to one upstream MCP server.
//!
//! Each upstream is registered with Flex Gateway as a Service. The hub
//! issues `POST <path>` carrying the JSON-RPC envelope and the
//! per-server auth materials (Bearer / Basic / API key / custom
//! headers).
//!
//! Streaming (SSE / chunked) is not passed through: the hub collects the
//! full response body and returns the JSON-RPC response it contains.
//! Notifications an upstream sends on the stream before the response
//! are dropped.

use std::rc::Rc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use pdk::hl::{HttpClient, HttpClientResponse, Service};
use pdk::logger;
use thiserror::Error;

use crate::config::{AuthConfig, McpServerConfig};
use crate::jsonrpc::{JsonRpcOutbound, JsonRpcResponse};

#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("transport error talking to MCP upstream '{server}': {message}")]
    Transport { server: String, message: String },

    #[error("MCP upstream '{server}' returned HTTP {status}")]
    HttpStatus {
        server: String,
        status: u32,
        /// Upstream `WWW-Authenticate` header, kept for 401/403 propagation.
        www_authenticate: Option<String>,
    },

    #[error("MCP upstream '{server}' requires the caller's credential, none was sent")]
    MissingCredential { server: String },

    #[error("MCP upstream '{server}' rejected initialize: {message} ({code})")]
    InitializeRejected {
        server: String,
        code: i32,
        message: String,
    },

    #[error("MCP upstream returned non-JSON-RPC body: {0}")]
    BadJson(String),
}

/// Resolved per-call auth materials (one or more headers).
pub struct OutboundHeaders(Vec<(String, String)>);

impl OutboundHeaders {
    /// Build the header set for an outbound call: auth materials +
    /// per-server `headers` overrides + a constant content-type/accept.
    /// `credential` is the caller's inbound header value, used only for
    /// `AuthConfig::Passthrough` servers.
    pub fn build(server: &McpServerConfig, credential: Option<&str>) -> Self {
        let mut headers: Vec<(String, String)> = vec![
            ("content-type".into(), "application/json".into()),
            // MCP Streamable HTTP advertises both via `Accept` so the
            // upstream may stream when it chooses.
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ];
        match &server.auth {
            AuthConfig::None => {}
            AuthConfig::Bearer { token } => {
                headers.push(("authorization".into(), format!("Bearer {token}")));
            }
            AuthConfig::Basic { username, password } => {
                let raw = format!("{username}:{password}");
                let encoded = B64.encode(raw.as_bytes());
                headers.push(("authorization".into(), format!("Basic {encoded}")));
            }
            AuthConfig::ApiKey {
                header_name,
                api_key,
            } => {
                headers.push((header_name.clone(), api_key.clone()));
            }
            AuthConfig::CustomHeaders => {
                // Auth materials are sourced from `extra_headers` below.
            }
            AuthConfig::Passthrough { .. } => {
                if let (Some(name), Some(value)) = (server.passthrough_header(), credential) {
                    headers.push((name.to_string(), value.to_string()));
                }
            }
        }
        for (k, v) in &server.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        Self(headers)
    }

    pub fn as_refs(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// One configured upstream MCP server and its Flex Gateway service.
pub struct UpstreamClient {
    pub server: McpServerConfig,
    pub service: Rc<Service>,
}

impl UpstreamClient {
    pub fn new(server: McpServerConfig, service: Rc<Service>) -> Self {
        Self { server, service }
    }

    /// Issue one JSON-RPC call to the upstream server. `timeout` overrides
    /// the per-server / policy-wide default at the caller's discretion
    /// (used by the aggregation pipeline so a single slow upstream
    /// doesn't stall the manifest fan-out).
    ///
    /// `mcp_session_id` (when `Some`) is sent as the `mcp-session-id`
    /// header — required by Streamable HTTP MCP servers that gate
    /// post-`initialize` traffic on a per-session id.
    ///
    /// `credential` is the caller's inbound header value for
    /// `authType=passthrough` servers (see [`OutboundHeaders::build`]).
    pub async fn call(
        &self,
        http: &HttpClient,
        request: JsonRpcOutbound,
        timeout: Duration,
        mcp_session_id: Option<&str>,
        credential: Option<&str>,
    ) -> Result<UpstreamCallResult, UpstreamError> {
        let response = self
            .send(http, request, timeout, mcp_session_id, credential)
            .await?;

        let returned_session_id = header_value(&response, "mcp-session-id");
        let content_type = header_value(&response, "content-type").unwrap_or_default();

        let body_bytes = response.body();
        let json_bytes = if content_type.contains("text/event-stream") {
            extract_jsonrpc_from_sse(body_bytes).ok_or_else(|| {
                UpstreamError::BadJson(format!(
                    "no JSON-RPC `data:` line in SSE body ({} bytes)",
                    body_bytes.len()
                ))
            })?
        } else {
            body_bytes.to_vec()
        };

        let parsed: JsonRpcResponse = serde_json::from_slice(&json_bytes)
            .map_err(|e| UpstreamError::BadJson(e.to_string()))?;
        Ok(UpstreamCallResult {
            response: parsed,
            mcp_session_id: returned_session_id,
        })
    }

    /// Send a JSON-RPC notification. Upstreams answer 202 with no body,
    /// so only the HTTP status is checked.
    pub async fn notify(
        &self,
        http: &HttpClient,
        notification: JsonRpcOutbound,
        timeout: Duration,
        mcp_session_id: Option<&str>,
        credential: Option<&str>,
    ) -> Result<(), UpstreamError> {
        self.send(http, notification, timeout, mcp_session_id, credential)
            .await
            .map(|_| ())
    }

    async fn send(
        &self,
        http: &HttpClient,
        request: JsonRpcOutbound,
        timeout: Duration,
        mcp_session_id: Option<&str>,
        credential: Option<&str>,
    ) -> Result<HttpClientResponse, UpstreamError> {
        let body = request.into_bytes();
        let mut headers = OutboundHeaders::build(&self.server, credential);
        if let Some(sid) = mcp_session_id {
            headers.0.push(("mcp-session-id".into(), sid.to_string()));
        }

        let response = http
            .request(self.service.as_ref())
            .path(&self.server.path)
            .headers(headers.as_refs())
            .timeout(timeout)
            .body(body.as_slice())
            .post()
            .await
            .map_err(|e| UpstreamError::Transport {
                server: self.server.name.clone(),
                message: e.to_string(),
            })?;

        let status = response.status_code();
        if !(200..300).contains(&status) {
            logger::warn!(
                "mcp-hub: upstream '{}' returned HTTP {}",
                self.server.name,
                status
            );
            return Err(UpstreamError::HttpStatus {
                server: self.server.name.clone(),
                status,
                www_authenticate: header_value(&response, "www-authenticate"),
            });
        }
        Ok(response)
    }
}

fn header_value(response: &HttpClientResponse, name: &str) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.to_string())
}

/// Outcome of one upstream call. The session id is `Some` when the
/// upstream returned an `mcp-session-id` response header — for
/// Streamable HTTP servers, that's the id we must echo on subsequent
/// requests in the same logical session.
pub struct UpstreamCallResult {
    pub response: JsonRpcResponse,
    pub mcp_session_id: Option<String>,
}

/// Extract the JSON-RPC response from an SSE-formatted response body.
/// Upstreams may send notifications (progress, logs) or requests on the
/// stream before the response, so events are scanned in order and the
/// first one carrying `result` or `error` is returned. Multi-line `data:`
/// continuations are joined per the SSE spec. Returns the raw JSON bytes
/// or `None` if no event holds a JSON-RPC response.
fn extract_jsonrpc_from_sse(body: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(body).ok()?;
    let mut current = String::new();
    for raw_line in s.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            // Event boundary: return the event if it is the response.
            if let Some(json) = response_event(&current) {
                return Some(json);
            }
            current.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(rest);
        }
        // ignore other SSE fields (event:, id:, retry:, comments)
    }
    response_event(&current)
}

/// The event's data as bytes when it is a JSON-RPC response (an object
/// with `result` or `error`); `None` for notifications, requests and
/// non-JSON data.
fn response_event(data: &str) -> Option<Vec<u8>> {
    let trimmed = data.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    (value.get("result").is_some() || value.get("error").is_some())
        .then(|| trimmed.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_auth(auth: AuthConfig) -> McpServerConfig {
        McpServerConfig {
            name: "test".into(),
            enabled: true,
            auth,
            extra_headers: vec![],
            timeout_ms: None,
            priority: 50,
            path: "/mcp".into(),
        }
    }

    #[test]
    fn bearer_emits_authorization_bearer() {
        let s = server_with_auth(AuthConfig::Bearer {
            token: "abc".into(),
        });
        let h = OutboundHeaders::build(&s, None);
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Bearer abc");
    }

    #[test]
    fn basic_emits_base64_credentials() {
        let s = server_with_auth(AuthConfig::Basic {
            username: "u".into(),
            password: "p".into(),
        });
        let h = OutboundHeaders::build(&s, None);
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        // base64("u:p") == "dTpw"
        assert_eq!(auth.1, "Basic dTpw");
    }

    #[test]
    fn api_key_emits_named_header() {
        let s = server_with_auth(AuthConfig::ApiKey {
            header_name: "x-api-key".into(),
            api_key: "k".into(),
        });
        let h = OutboundHeaders::build(&s, None);
        let api = h.0.iter().find(|(k, _)| k == "x-api-key").unwrap();
        assert_eq!(api.1, "k");
    }

    #[test]
    fn extra_headers_are_appended() {
        let mut s = server_with_auth(AuthConfig::None);
        s.extra_headers.push(("x-env".into(), "prod".into()));
        let h = OutboundHeaders::build(&s, None);
        assert!(h.0.iter().any(|(k, v)| k == "x-env" && v == "prod"));
    }

    #[test]
    fn passthrough_forwards_caller_credential() {
        let s = server_with_auth(AuthConfig::Passthrough {
            header_name: "authorization".into(),
        });
        let h = OutboundHeaders::build(&s, Some("Bearer user-jwt"));
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Bearer user-jwt");
    }

    #[test]
    fn passthrough_sends_nothing_without_credential() {
        let s = server_with_auth(AuthConfig::Passthrough {
            header_name: "authorization".into(),
        });
        let h = OutboundHeaders::build(&s, None);
        assert!(!h.0.iter().any(|(k, _)| k == "authorization"));
    }

    #[test]
    fn configured_header_overrides_passthrough_credential() {
        let mut s = server_with_auth(AuthConfig::Passthrough {
            header_name: "authorization".into(),
        });
        s.extra_headers
            .push(("Authorization".into(), "Bearer configured".into()));
        let h = OutboundHeaders::build(&s, Some("Bearer user-jwt"));
        let values: Vec<_> =
            h.0.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.as_str())
                .collect();
        assert_eq!(values, vec!["Bearer configured"]);
    }

    #[test]
    fn static_auth_ignores_caller_credential() {
        let s = server_with_auth(AuthConfig::Bearer {
            token: "abc".into(),
        });
        let h = OutboundHeaders::build(&s, Some("Bearer user-jwt"));
        let values: Vec<_> =
            h.0.iter()
                .filter(|(k, _)| k == "authorization")
                .map(|(_, v)| v.as_str())
                .collect();
        assert_eq!(values, vec!["Bearer abc"]);
    }

    #[test]
    fn sse_extracts_single_data_line() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let json = extract_jsonrpc_from_sse(body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn sse_handles_no_space_after_colon() {
        let body = b"data:{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":[]}\n\n";
        let json = extract_jsonrpc_from_sse(body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn sse_skips_notifications_before_the_response() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n\
event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n";
        let json = extract_jsonrpc_from_sse(body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn sse_returns_error_responses() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":-32601,\"message\":\"nope\"}}";
        let json = extract_jsonrpc_from_sse(body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn sse_returns_none_when_only_notifications() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n";
        assert!(extract_jsonrpc_from_sse(body).is_none());
    }

    #[test]
    fn sse_returns_none_for_non_json_body() {
        assert!(extract_jsonrpc_from_sse(b"event: ping\n\n").is_none());
        assert!(extract_jsonrpc_from_sse(b"").is_none());
    }
}
