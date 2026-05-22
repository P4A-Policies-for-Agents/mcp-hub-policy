//! Outbound JSON-RPC client to one upstream MCP server.
//!
//! Each upstream is registered with Flex Gateway as a Service. The hub
//! issues `POST <path>` carrying the JSON-RPC envelope and the
//! per-server auth materials (Bearer / Basic / API key / custom
//! headers).
//!
//! Streaming (SSE / chunked) is not currently materialized — the hub
//! collects the full response body before forwarding to the inbound
//! client. Streaming pass-through is tracked as future work in the
//! requirements doc (§6.12).

use std::rc::Rc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use pdk::hl::{HttpClient, Service};
use pdk::logger;
use thiserror::Error;

use crate::config::{AuthConfig, McpServerConfig};
use crate::jsonrpc::{JsonRpcOutbound, JsonRpcResponse};

#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("transport error talking to MCP upstream '{server}': {message}")]
    Transport { server: String, message: String },

    #[error("MCP upstream '{server}' returned HTTP {status}")]
    HttpStatus { server: String, status: u32 },

    #[error("MCP upstream returned non-JSON-RPC body: {0}")]
    BadJson(String),
}

/// Resolved per-call auth materials (one or more headers).
pub struct OutboundHeaders(Vec<(String, String)>);

impl OutboundHeaders {
    /// Build the header set for an outbound call: auth materials +
    /// per-server `headers` overrides + a constant content-type/accept.
    pub fn build(server: &McpServerConfig) -> Self {
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
            AuthConfig::ApiKey { header_name, api_key } => {
                headers.push((header_name.clone(), api_key.clone()));
            }
            AuthConfig::CustomHeaders => {
                // Auth materials are sourced from `extra_headers` below.
            }
        }
        for (k, v) in &server.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        Self(headers)
    }

    pub fn as_refs(&self) -> Vec<(&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }
}

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
    pub async fn call(
        &self,
        http: &HttpClient,
        request: JsonRpcOutbound,
        timeout: Duration,
        mcp_session_id: Option<&str>,
    ) -> Result<UpstreamCallResult, UpstreamError> {
        let body = request.into_bytes();
        let mut headers = OutboundHeaders::build(&self.server);
        if let Some(sid) = mcp_session_id {
            headers.0.push(("mcp-session-id".into(), sid.to_string()));
        }
        let header_refs = headers.as_refs();

        let response = http
            .request(self.service.as_ref())
            .path(&self.server.path)
            .headers(header_refs)
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
            });
        }

        let returned_session_id = response
            .headers()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, v)| v.to_string());
        let content_type = response
            .headers()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

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
}

/// Outcome of one upstream call. The session id is `Some` when the
/// upstream returned an `mcp-session-id` response header — for
/// Streamable HTTP servers, that's the id we must echo on subsequent
/// requests in the same logical session.
pub struct UpstreamCallResult {
    pub response: JsonRpcResponse,
    pub mcp_session_id: Option<String>,
}

/// Extract the first JSON-RPC payload from an SSE-formatted response
/// body. Looks for `data: { ... }` lines, concatenating multi-line
/// `data:` continuations per the SSE spec. Returns the raw JSON bytes
/// or `None` if no usable `data:` line is found.
fn extract_jsonrpc_from_sse(body: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(body).ok()?;
    let mut current = String::new();
    for raw_line in s.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            // Event boundary. If we have an accumulated `data:`, return it.
            if !current.is_empty() {
                let trimmed = current.trim();
                if trimmed.starts_with('{') {
                    return Some(trimmed.as_bytes().to_vec());
                }
                current.clear();
            }
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
    let trimmed = current.trim();
    if trimmed.starts_with('{') {
        Some(trimmed.as_bytes().to_vec())
    } else {
        None
    }
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
        let s = server_with_auth(AuthConfig::Bearer { token: "abc".into() });
        let h = OutboundHeaders::build(&s);
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Bearer abc");
    }

    #[test]
    fn basic_emits_base64_credentials() {
        let s = server_with_auth(AuthConfig::Basic {
            username: "u".into(),
            password: "p".into(),
        });
        let h = OutboundHeaders::build(&s);
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
        let h = OutboundHeaders::build(&s);
        let api = h.0.iter().find(|(k, _)| k == "x-api-key").unwrap();
        assert_eq!(api.1, "k");
    }

    #[test]
    fn extra_headers_are_appended() {
        let mut s = server_with_auth(AuthConfig::None);
        s.extra_headers
            .push(("x-env".into(), "prod".into()));
        let h = OutboundHeaders::build(&s);
        assert!(h.0.iter().any(|(k, v)| k == "x-env" && v == "prod"));
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
    fn sse_returns_none_for_non_json_body() {
        assert!(extract_jsonrpc_from_sse(b"event: ping\n\n").is_none());
        assert!(extract_jsonrpc_from_sse(b"").is_none());
    }
}
