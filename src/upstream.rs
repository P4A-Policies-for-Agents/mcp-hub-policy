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
        /// Upstream `Retry-After` header, relayed on 429 / 5xx.
        retry_after: Option<String>,
        /// Start of the response body (at most [`BODY_EXCERPT_BYTES`]).
        body: String,
    },

    #[error("MCP upstream '{server}' requires caller header(s): {}", headers.join(", "))]
    MissingCredential {
        server: String,
        /// Passthrough headers the caller didn't send.
        headers: Vec<String>,
    },

    #[error("MCP upstream '{server}' rejected {method}: {message} ({code})")]
    Rejected {
        server: String,
        method: String,
        code: i32,
        message: String,
    },

    #[error(
        "MCP upstream '{server}' is unavailable after a failed handshake; retry in {retry_after}s"
    )]
    BackingOff {
        server: String,
        /// Reason code of the failure being backed off from.
        reason: String,
        /// Seconds until the hub tries the upstream again.
        retry_after: u64,
    },

    #[error("MCP server '{server}' is misconfigured: {reason}")]
    Misconfigured { server: String, reason: String },

    #[error("MCP upstream '{server}' returned an invalid JSON-RPC body: {message}")]
    BadJson { server: String, message: String },

    #[error(
        "MCP upstream '{server}' returned {bytes} bytes, over maxUpstreamResponseBytes ({limit})"
    )]
    TooLarge {
        server: String,
        bytes: usize,
        limit: usize,
    },
}

/// Upstream error bodies are kept up to this many bytes.
pub const BODY_EXCERPT_BYTES: usize = 1024;

impl UpstreamError {
    /// Name of the MCP server the error is about.
    pub fn server(&self) -> &str {
        match self {
            Self::Transport { server, .. }
            | Self::HttpStatus { server, .. }
            | Self::MissingCredential { server, .. }
            | Self::Rejected { server, .. }
            | Self::BackingOff { server, .. }
            | Self::Misconfigured { server, .. }
            | Self::BadJson { server, .. }
            | Self::TooLarge { server, .. } => server,
        }
    }

    /// Short reason code reported for a server left out of a `*/list`.
    pub fn reason_code(&self) -> &str {
        match self {
            Self::MissingCredential { .. } => "missing_credentials",
            Self::HttpStatus {
                status: 401 | 403, ..
            } => "unauthorized",
            Self::Transport { .. } => "unavailable",
            Self::BackingOff { reason, .. } => reason,
            Self::Misconfigured { .. } => "misconfigured",
            Self::HttpStatus { .. }
            | Self::Rejected { .. }
            | Self::BadJson { .. }
            | Self::TooLarge { .. } => "error",
        }
    }
}

/// The caller's values for a passthrough server's headers (lowercase
/// name, value), in `headerName` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential(pub Vec<(String, String)>);

impl Credential {
    /// Stable input for session / manifest cache keys. Header values can't
    /// contain newlines, so joining on one is unambiguous; a single header
    /// gives its bare value.
    pub fn fingerprint(&self) -> String {
        self.0
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Per-call inputs that become outbound headers besides the server's own
/// auth and `headers`.
#[derive(Default, Clone, Copy)]
pub struct Outbound<'a> {
    /// Upstream `mcp-session-id`, once the upstream issued one.
    pub session_id: Option<&'a str>,
    /// Negotiated `MCP-Protocol-Version`, once known.
    pub protocol_version: Option<&'a str>,
    /// The caller's values for `authType=passthrough` servers.
    pub credential: Option<&'a Credential>,
    /// The caller's `forwardHeaders` values (lowercase name, value).
    pub forward: &'a [(String, String)],
}

/// Resolved per-call headers.
pub struct OutboundHeaders(Vec<(String, String)>);

impl OutboundHeaders {
    /// Build the header set for an outbound call: constant
    /// content-type/accept, session and protocol headers, forwarded
    /// tracing headers, auth materials, then the per-server `headers`
    /// (which win over forwarded and passthrough values of the same name).
    pub fn build(server: &McpServerConfig, out: &Outbound) -> Self {
        let mut headers: Vec<(String, String)> = vec![
            ("content-type".into(), "application/json".into()),
            // MCP Streamable HTTP advertises both via `Accept` so the
            // upstream may stream when it chooses.
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ];
        if let Some(sid) = out.session_id {
            headers.push(("mcp-session-id".into(), sid.into()));
        }
        if let Some(version) = out.protocol_version {
            headers.push(("mcp-protocol-version".into(), version.into()));
        }
        headers.extend(out.forward.iter().cloned());
        match &server.auth {
            AuthConfig::None | AuthConfig::CustomHeaders => {
                // customHeaders: auth materials come from `extra_headers`.
            }
            AuthConfig::Bearer { token } => {
                headers.push(("authorization".into(), format!("Bearer {token}")));
            }
            AuthConfig::Basic { username, password } => {
                let encoded = B64.encode(format!("{username}:{password}").as_bytes());
                headers.push(("authorization".into(), format!("Basic {encoded}")));
            }
            AuthConfig::ApiKey {
                header_name,
                api_key,
            } => {
                headers.push((header_name.clone(), api_key.clone()));
            }
            AuthConfig::Passthrough { .. } => {
                if let Some(credential) = out.credential {
                    let forwarded = server.passthrough_headers();
                    headers.extend(
                        credential
                            .0
                            .iter()
                            .filter(|(k, _)| forwarded.contains(&k.as_str()))
                            .cloned(),
                    );
                }
            }
        }
        // The server's own `headers` replace any header of the same name
        // (forwarded, passthrough or auth scheme).
        headers.retain(|(k, _)| !server.has_header(k));
        headers.extend(server.extra_headers.iter().cloned());
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
    /// `maxUpstreamResponseBytes`.
    max_response_bytes: usize,
}

impl UpstreamClient {
    pub fn new(server: McpServerConfig, service: Rc<Service>, max_response_bytes: usize) -> Self {
        Self {
            server,
            service,
            max_response_bytes,
        }
    }

    /// Issue one JSON-RPC call to the upstream server. `timeout` is the
    /// caller's (the aggregation pipeline uses a shorter one so a single
    /// slow upstream doesn't stall the manifest fan-out).
    pub async fn call(
        &self,
        http: &HttpClient,
        request: JsonRpcOutbound,
        timeout: Duration,
        out: &Outbound<'_>,
    ) -> Result<UpstreamCallResult, UpstreamError> {
        let response = self.send(http, request, timeout, out).await?;

        let returned_session_id = header_value(&response, "mcp-session-id");
        let content_type = header_value(&response, "content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();

        // The gateway has already buffered the body; the cap keeps the hub
        // from parsing (several times its size in memory) and relaying it.
        let body_bytes = response.body();
        if body_bytes.len() > self.max_response_bytes {
            return Err(UpstreamError::TooLarge {
                server: self.server.name.clone(),
                bytes: body_bytes.len(),
                limit: self.max_response_bytes,
            });
        }
        let parsed = if content_type.contains("text/event-stream") {
            let event = extract_jsonrpc_from_sse(body_bytes).ok_or_else(|| {
                self.bad_json(format!(
                    "no JSON-RPC `data:` line in SSE body ({} bytes)",
                    body_bytes.len()
                ))
            })?;
            serde_json::from_value(event)
        } else {
            serde_json::from_slice(body_bytes)
        };
        let parsed: JsonRpcResponse = parsed.map_err(|e| self.bad_json(e.to_string()))?;
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
        out: &Outbound<'_>,
    ) -> Result<(), UpstreamError> {
        self.send(http, notification, timeout, out)
            .await
            .map(|_| ())
    }

    fn bad_json(&self, message: String) -> UpstreamError {
        UpstreamError::BadJson {
            server: self.server.name.clone(),
            message,
        }
    }

    async fn send(
        &self,
        http: &HttpClient,
        request: JsonRpcOutbound,
        timeout: Duration,
        out: &Outbound<'_>,
    ) -> Result<HttpClientResponse, UpstreamError> {
        let body = request.into_bytes();
        let headers = OutboundHeaders::build(&self.server, out);

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
                retry_after: header_value(&response, "retry-after"),
                body: excerpt(response.body()),
            });
        }
        Ok(response)
    }
}

/// The start of `body` as text, cut to [`BODY_EXCERPT_BYTES`] on a
/// character boundary.
fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(BODY_EXCERPT_BYTES + 3)]);
    let mut end = text.len().min(BODY_EXCERPT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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
/// continuations are joined per the SSE spec. Returns the parsed event or
/// `None` if no event holds a JSON-RPC response.
fn extract_jsonrpc_from_sse(body: &[u8]) -> Option<serde_json::Value> {
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

/// The event's data when it is a JSON-RPC response (an object with
/// `result` or `error`); `None` for notifications, requests and non-JSON
/// data.
fn response_event(data: &str) -> Option<serde_json::Value> {
    let trimmed = data.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    (value.get("result").is_some() || value.get("error").is_some()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_auth(auth: AuthConfig) -> McpServerConfig {
        McpServerConfig {
            name: "test".into(),
            index: 0,
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
        let h = OutboundHeaders::build(&s, &Outbound::default());
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Bearer abc");
    }

    #[test]
    fn basic_emits_base64_credentials() {
        let s = server_with_auth(AuthConfig::Basic {
            username: "u".into(),
            password: "p".into(),
        });
        let h = OutboundHeaders::build(&s, &Outbound::default());
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
        let h = OutboundHeaders::build(&s, &Outbound::default());
        let api = h.0.iter().find(|(k, _)| k == "x-api-key").unwrap();
        assert_eq!(api.1, "k");
    }

    #[test]
    fn extra_headers_are_appended() {
        let mut s = server_with_auth(AuthConfig::None);
        s.extra_headers.push(("x-env".into(), "prod".into()));
        let h = OutboundHeaders::build(&s, &Outbound::default());
        assert!(h.0.iter().any(|(k, v)| k == "x-env" && v == "prod"));
    }

    fn caller(token: &str) -> Credential {
        Credential(vec![("authorization".into(), token.into())])
    }

    fn with_credential(credential: &Credential) -> Outbound<'_> {
        Outbound {
            credential: Some(credential),
            ..Default::default()
        }
    }

    #[test]
    fn forward_and_protocol_headers_are_sent_and_configured_headers_win() {
        let mut s = server_with_auth(AuthConfig::None);
        s.extra_headers
            .push(("X-Correlation-Id".into(), "fixed".into()));
        let forward = vec![
            ("traceparent".to_string(), "00-abc-def-01".to_string()),
            ("x-correlation-id".to_string(), "caller".to_string()),
        ];
        let h = OutboundHeaders::build(
            &s,
            &Outbound {
                session_id: Some("sid-1"),
                protocol_version: Some("2025-06-18"),
                forward: &forward,
                ..Default::default()
            },
        );
        let get = |name: &str| -> Vec<&str> {
            h.0.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
                .collect()
        };
        assert_eq!(get("traceparent"), vec!["00-abc-def-01"]);
        assert_eq!(get("x-correlation-id"), vec!["fixed"]);
        assert_eq!(get("mcp-session-id"), vec!["sid-1"]);
        assert_eq!(get("mcp-protocol-version"), vec!["2025-06-18"]);
    }

    #[test]
    fn error_body_excerpt_is_bounded_and_utf8_safe() {
        assert_eq!(excerpt(b"rate limited"), "rate limited");
        let long = "é".repeat(BODY_EXCERPT_BYTES);
        let cut = excerpt(long.as_bytes());
        assert!(cut.len() <= BODY_EXCERPT_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn reason_codes() {
        let status = |status| UpstreamError::HttpStatus {
            server: "s".into(),
            status,
            www_authenticate: None,
            retry_after: None,
            body: String::new(),
        };
        assert_eq!(status(401).reason_code(), "unauthorized");
        assert_eq!(status(503).reason_code(), "error");
        let backoff = UpstreamError::BackingOff {
            server: "s".into(),
            reason: "unavailable".into(),
            retry_after: 3,
        };
        assert_eq!(backoff.reason_code(), "unavailable");
    }

    #[test]
    fn passthrough_forwards_caller_credential() {
        let s = server_with_auth(AuthConfig::Passthrough {
            header_names: vec!["authorization".into()],
        });
        let h = OutboundHeaders::build(&s, &with_credential(&caller("Bearer user-jwt")));
        let auth = h.0.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Bearer user-jwt");
    }

    #[test]
    fn passthrough_forwards_every_listed_header() {
        let s = server_with_auth(AuthConfig::Passthrough {
            header_names: vec!["client_id".into(), "client_secret".into()],
        });
        let c = Credential(vec![
            ("client_id".into(), "app".into()),
            ("client_secret".into(), "s3cret".into()),
        ]);
        let h = OutboundHeaders::build(&s, &with_credential(&c));
        assert!(h.0.iter().any(|(k, v)| k == "client_id" && v == "app"));
        assert!(h
            .0
            .iter()
            .any(|(k, v)| k == "client_secret" && v == "s3cret"));
    }

    #[test]
    fn credential_fingerprint_covers_every_value() {
        let c = |secret: &str| {
            Credential(vec![
                ("client_id".into(), "app".into()),
                ("client_secret".into(), secret.into()),
            ])
        };
        assert_ne!(c("a").fingerprint(), c("b").fingerprint());
        assert_eq!(caller("Bearer x").fingerprint(), "Bearer x");
    }

    #[test]
    fn passthrough_sends_nothing_without_credential() {
        let s = server_with_auth(AuthConfig::Passthrough {
            header_names: vec!["authorization".into()],
        });
        let h = OutboundHeaders::build(&s, &Outbound::default());
        assert!(!h.0.iter().any(|(k, _)| k == "authorization"));
    }

    #[test]
    fn configured_header_overrides_passthrough_credential() {
        let mut s = server_with_auth(AuthConfig::Passthrough {
            header_names: vec!["authorization".into()],
        });
        s.extra_headers
            .push(("Authorization".into(), "Bearer configured".into()));
        let h = OutboundHeaders::build(&s, &with_credential(&caller("Bearer user-jwt")));
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
        let h = OutboundHeaders::build(&s, &with_credential(&caller("Bearer user-jwt")));
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
        let v = extract_jsonrpc_from_sse(body).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn sse_handles_no_space_after_colon() {
        let body = b"data:{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":[]}\n\n";
        let v = extract_jsonrpc_from_sse(body).unwrap();
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn sse_skips_notifications_before_the_response() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n\
event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n";
        let v = extract_jsonrpc_from_sse(body).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn sse_returns_error_responses() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":-32601,\"message\":\"nope\"}}";
        let v = extract_jsonrpc_from_sse(body).unwrap();
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

    #[test]
    fn configured_header_replaces_auth_scheme_header() {
        let mut server = server_with_auth(AuthConfig::Bearer { token: "t".into() });
        server.extra_headers = vec![("Authorization".into(), "Custom x".into())];
        let headers = OutboundHeaders::build(&server, &Outbound::default());
        let auth: Vec<_> = headers
            .0
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .collect();
        assert_eq!(
            auth,
            vec![&("Authorization".to_string(), "Custom x".to_string())]
        );
    }
}
