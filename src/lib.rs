//! MCP Hub policy entrypoint.
//!
//! Single-phase architecture: everything happens in the request filter.
//! The hub is stateless toward clients: it issues no `mcp-session-id`
//! and ignores any it receives. Upstream sessions are managed internally
//! (see `session.rs`).
//!
//! 0. `<...>/.well-known/oauth-protected-resource` serves RFC 9728
//!    metadata when `authorizationServers` is configured.
//! 1. Match the inbound path against `mcpEndpoint` (defaults to `/mcp`).
//!    Non-matches are short-circuited with 404 (strict) or returned as
//!    `Flow::Continue(())` so the gateway's own upstream handles them.
//! 2. `GET /mcp`: if the client advertises `Accept: text/event-stream`,
//!    reply 200 with an immediate empty SSE stream so the EventSource
//!    is happy (the hub never pushes server-initiated events). For
//!    plain GETs (no SSE Accept) reply 405 with `Allow: POST`. Other
//!    non-POST methods always get 405. `Origin` (when `allowedOrigins` is
//!    set) is validated first; `MCP-Protocol-Version` on POST.
//! 3. For `POST /mcp` drain the body, parse the JSON-RPC envelope, and
//!    dispatch inline — local short-circuit (`initialize`, `ping`),
//!    aggregate (`tools/list`, etc.), or route (`tools/call`, etc.) —
//!    then `Flow::Break` with the synthesized response. Aggregation calls
//!    every enabled upstream concurrently and caches each upstream's
//!    answer (see `manifest.rs`). Notifications (no `id`) are fanned out
//!    best-effort and answered with 202.
//!
//! Why no response phase: when an API instance has no healthy upstream
//! cluster (which is the normal case for a hub that synthesizes every
//! response itself), Flex Gateway returns `503 no healthy upstream`
//! before our response filter would have a chance to override the body
//! via `send_response`. Breaking in the request phase bypasses the
//! upstream-cluster check entirely.

mod config;
mod generated;
mod jsonrpc;
mod manifest;
mod mcp;
mod session;
mod upstream;

use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use futures::future::join_all;
use pdk::cache::CacheBuilder;
use pdk::hl::*;
use pdk::logger;
use serde_json::Value;

use crate::config::{
    CollisionStrategy, PolicyConfig, ProtectedResource, WELL_KNOWN_PROTECTED_RESOURCE,
};
use crate::generated::config::Config;
use crate::jsonrpc::{
    error_response, error_response_with_data, success_response, JsonRpcOutbound, JsonRpcRequest,
    JsonRpcResponse, AUTH_REQUIRED, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    METHOD_NOT_FOUND, PARSE_ERROR, PROMPT_NOT_FOUND, RESOURCE_NOT_FOUND, SERVER_UNAVAILABLE,
    TOOL_NOT_FOUND,
};
use crate::manifest::ManifestCache;
use crate::mcp::aggregate::{
    aggregate, resolve_owner, strip_internal, AggregateError, UpstreamItem,
};
use crate::mcp::method::McpMethod;
use crate::mcp::types::{server_capabilities, server_info};
use crate::session::SessionStore;
use crate::upstream::{UpstreamClient, UpstreamError};

const HUB_NAME: &str = "mcp-hub-policy";
const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol revisions the hub speaks toward clients, newest first.
/// `initialize` echoes the client's requested version when listed here
/// and otherwise answers with the newest one, per MCP version negotiation.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Revision the hub requests when initializing upstreams. This revision
/// predates the `MCP-Protocol-Version` request header, so none is sent.
const UPSTREAM_PROTOCOL_VERSION: &str = "2024-11-05";
const SESSION_CACHE_ID: &str = "mcp-hub-upstream-sessions";
const SESSION_CACHE_MAX_ENTRIES: usize = 1024;
const MANIFEST_CACHE_ID: &str = "mcp-hub-upstream-manifests";
/// Kept small: the PDK cache rewrites an index of every key on each save.
const MANIFEST_CACHE_MAX_ENTRIES: usize = 256;

/// Resolved upstreams (one [`UpstreamClient`] per enabled MCP server)
/// plus the upstream session and manifest caches.
#[derive(Clone)]
struct PolicyState {
    cfg: Rc<PolicyConfig>,
    upstreams: Rc<Vec<UpstreamClient>>,
    sessions: Rc<SessionStore>,
    manifests: Rc<ManifestCache>,
    /// Hash of the raw configuration; scopes both caches' keys.
    config_tag: Rc<str>,
}

impl PolicyState {
    /// Session (and manifest) cache key for `upstream` and this caller.
    fn upstream_key(&self, upstream: &UpstreamClient, credential: Option<&str>) -> String {
        SessionStore::key(&self.config_tag, &upstream.server.name, credential)
    }
}

/// Per-request inputs captured from the inbound headers before the body
/// is drained: the caller's values for every header that some
/// `authType=passthrough` server forwards (lowercase name, value).
struct RequestCtx {
    credentials: Vec<(String, String)>,
}

impl RequestCtx {
    fn from_headers(state: &PolicyState, headers: &[(String, String)]) -> Self {
        let mut credentials: Vec<(String, String)> = Vec::new();
        for name in state
            .upstreams
            .iter()
            .filter_map(|u| u.server.passthrough_header())
        {
            if credentials.iter().any(|(k, _)| k == name) {
                continue;
            }
            if let Some(value) = find_header(headers, name) {
                credentials.push((name.to_string(), value.to_string()));
            }
        }
        Self { credentials }
    }

    /// The caller's credential for `upstream`: `Ok(None)` for servers that
    /// don't forward one, `Err(MissingCredential)` when a passthrough
    /// server needs it but the caller sent none.
    fn credential_for(&self, upstream: &UpstreamClient) -> Result<Option<&str>, UpstreamError> {
        let Some(name) = upstream.server.passthrough_header() else {
            return Ok(None);
        };
        self.credentials
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| Some(v.as_str()))
            .ok_or_else(|| UpstreamError::MissingCredential {
                server: upstream.server.name.clone(),
            })
    }
}

async fn request_filter(
    request: RequestHeadersState,
    state: PolicyState,
    client: HttpClient,
) -> Flow<()> {
    let method = request.method();
    let path = request.path();
    let bare = path
        .split_once('?')
        .map(|(p, _)| p)
        .unwrap_or(&path)
        .to_string();
    let m = method.to_ascii_uppercase();

    if let Some(pr) = state.cfg.protected_resource.as_ref() {
        if bare.ends_with(WELL_KNOWN_PROTECTED_RESOURCE) {
            return Flow::Break(protected_resource_response(pr, &m));
        }
    }

    if !path_matches(&bare, &state.cfg.mcp_endpoint) {
        if state.cfg.strict_mode {
            logger::debug!("mcp-hub: rejecting non-MCP path '{bare}' (strictMode)");
            return Flow::Break(make_text_response(404, "not found"));
        }
        // Permissive mode: let the gateway's upstream handle this path.
        return Flow::Continue(());
    }

    let headers = request.handler().headers();
    if origin_rejected(&state.cfg, &headers) {
        return Flow::Break(make_json_response(
            403,
            &error_response(None, INVALID_REQUEST, "origin not allowed"),
        ));
    }

    if m == "GET" {
        // Streamable-HTTP clients open GET expecting a server-initiated
        // SSE stream. The hub never pushes server-initiated events, but
        // we still need to humour SSE-capable clients (MCP Inspector's
        // legacy SSE fallback, EventSource probes) so they don't error
        // out of the connection. Strategy:
        //   * If the request advertises `Accept: text/event-stream`,
        //     reply 200 with an immediate empty SSE stream (just a
        //     keep-alive comment) so EventSource is satisfied.
        //   * Otherwise reply 405 with `Allow: POST` so plain GETs
        //     (browsers, health probes) get a clean spec-compliant
        //     answer instead of an empty SSE.
        let accepts_sse = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("accept"))
            .any(|(_, v)| v.to_ascii_lowercase().contains("text/event-stream"));
        if accepts_sse {
            return Flow::Break(
                Response::new(200)
                    .with_headers(vec![
                        ("content-type".into(), "text/event-stream".into()),
                        ("cache-control".into(), "no-cache".into()),
                        ("connection".into(), "keep-alive".into()),
                    ])
                    .with_body(b": mcp-hub-policy idle stream\n\n".to_vec()),
            );
        }
        return Flow::Break(
            Response::new(405)
                .with_headers(vec![
                    ("allow".into(), "POST".into()),
                    ("content-type".into(), "application/json".into()),
                ])
                .with_body(error_response(
                    None,
                    INVALID_REQUEST,
                    "method not allowed; use POST",
                )),
        );
    }
    if m != "POST" {
        return Flow::Break(
            Response::new(405)
                .with_headers(vec![
                    ("allow".into(), "POST".into()),
                    ("content-type".into(), "application/json".into()),
                ])
                .with_body(error_response(
                    None,
                    INVALID_REQUEST,
                    "method not allowed; use POST",
                )),
        );
    }

    if !request.contains_body() {
        return Flow::Break(make_json_response(
            400,
            &error_response(None, INVALID_REQUEST, "request body required"),
        ));
    }

    if let Some(version) = find_header(&headers, "mcp-protocol-version") {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Flow::Break(make_json_response(
                400,
                &error_response(
                    None,
                    INVALID_REQUEST,
                    &format!(
                        "unsupported MCP-Protocol-Version '{version}'; supported: {}",
                        SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                    ),
                ),
            ));
        }
    }

    let ctx = RequestCtx::from_headers(&state, &headers);

    let body_state = request.into_body_state().await;
    let body = body_state.handler().body().to_vec();

    let parsed: Result<JsonRpcRequest, _> = serde_json::from_slice(&body);
    let rpc = match parsed {
        Ok(rpc) if rpc.jsonrpc != crate::jsonrpc::JSONRPC_VERSION => {
            return Flow::Break(make_json_response(
                200,
                &error_response(
                    rpc.id,
                    PARSE_ERROR,
                    &format!("expected jsonrpc=2.0, got '{}'", rpc.jsonrpc),
                ),
            ));
        }
        Ok(rpc) => rpc,
        Err(e) => {
            return Flow::Break(make_json_response(
                200,
                &error_response(None, PARSE_ERROR, &format!("invalid JSON-RPC body: {e}")),
            ));
        }
    };

    let resp = dispatch_rpc(&state, &client, &ctx, rpc).await;
    Flow::Break(resp)
}

async fn dispatch_rpc(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    rpc: JsonRpcRequest,
) -> Response {
    let id = rpc.id.clone();
    let params = rpc.params.clone();
    let method = McpMethod::parse(&rpc.method, rpc.is_notification());

    if rpc.is_notification() {
        // Notifications are best-effort: fan out to all enabled upstreams,
        // ignore errors, return 202 Accepted (no body) per MCP Streamable
        // HTTP spec. 204 trips undici's null-body-status check in the
        // MCP Inspector proxy.
        fan_out_notification(state, client, ctx, &rpc).await;
        return make_response(202, b"");
    }

    let id_value = id.clone().unwrap_or(Value::Null);
    match method {
        McpMethod::Initialize => {
            let requested = params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str());
            let payload = serde_json::json!({
                "protocolVersion": negotiate_protocol_version(requested),
                "capabilities": server_capabilities(),
                "serverInfo": server_info(HUB_NAME, HUB_VERSION),
            });
            // Stateless: no `mcp-session-id` is issued.
            make_json_response(200, &success_response(id_value, payload))
        }
        McpMethod::Ping => make_json_response(
            200,
            &success_response(id_value, Value::Object(Default::default())),
        ),
        McpMethod::ToolsList => {
            handle_aggregate_list(state, client, ctx, id.clone(), "tools/list", "tools").await
        }
        McpMethod::ResourcesList => {
            handle_aggregate_list(
                state,
                client,
                ctx,
                id.clone(),
                "resources/list",
                "resources",
            )
            .await
        }
        McpMethod::PromptsList => {
            handle_aggregate_list(state, client, ctx, id.clone(), "prompts/list", "prompts").await
        }
        McpMethod::ToolsCall => {
            handle_route(
                state,
                client,
                ctx,
                id,
                "tools/call",
                params,
                "name",
                TOOL_NOT_FOUND,
            )
            .await
        }
        McpMethod::ResourcesRead => {
            handle_route(
                state,
                client,
                ctx,
                id,
                "resources/read",
                params,
                "uri",
                RESOURCE_NOT_FOUND,
            )
            .await
        }
        McpMethod::PromptsGet => {
            handle_route(
                state,
                client,
                ctx,
                id,
                "prompts/get",
                params,
                "name",
                PROMPT_NOT_FOUND,
            )
            .await
        }
        McpMethod::Other(other) => make_json_response(
            200,
            &error_response(
                id,
                METHOD_NOT_FOUND,
                &format!("method '{other}' is not supported by mcp-hub"),
            ),
        ),
        McpMethod::Notification(_) => unreachable!("notifications handled above"),
    }
}

/// Fan an aggregated `*list` call across all enabled upstreams. Slow,
/// failing, or unauthorized upstreams are logged and skipped — partial
/// degradation is preferable to a total aggregate failure, so a caller
/// without a token still sees every server that doesn't need one.
async fn handle_aggregate_list(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    id: Option<Value>,
    method: &str,
    list_key: &str,
) -> Response {
    let all = collect_list_items(state, client, ctx, method, list_key).await;

    let aggregated = match aggregate(all, state.cfg.collision_strategy) {
        Ok(a) => strip_internal(a),
        Err(AggregateError::Duplicate(name)) => {
            return make_json_response(
                200,
                &error_response_with_data(
                    id,
                    INTERNAL_ERROR,
                    &format!("duplicate {list_key} entry '{name}' across MCP servers"),
                    Some(serde_json::json!({"collisionStrategy": "reject"})),
                ),
            );
        }
    };

    let payload = serde_json::json!({ list_key: aggregated });
    make_json_response(200, &success_response(id.unwrap_or(Value::Null), payload))
}

/// Call `<kind>/list` on every enabled upstream concurrently and collect
/// the entries of those that answered successfully, in upstream order.
async fn collect_list_items<'a>(
    state: &'a PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    method: &str,
    list_key: &str,
) -> Vec<UpstreamItem<'a>> {
    let lists = join_all(
        state
            .upstreams
            .iter()
            .map(|upstream| list_upstream(state, client, ctx, upstream, method, list_key)),
    )
    .await;

    state
        .upstreams
        .iter()
        .zip(lists)
        .flat_map(|(upstream, items)| {
            items.into_iter().map(move |value| UpstreamItem {
                server: upstream.server.name.as_str(),
                value,
            })
        })
        .collect()
}

/// One upstream's `<kind>/list` entries, from the manifest cache or the
/// upstream itself. Empty (and logged) when the upstream failed, answered
/// with an error, or needs a credential the caller didn't send.
async fn list_upstream(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    method: &str,
    list_key: &str,
) -> Vec<Value> {
    let name = upstream.server.name.as_str();
    let credential = match ctx.credential_for(upstream) {
        Ok(credential) => credential,
        Err(e) => {
            logger::warn!("mcp-hub: skipping upstream '{name}' for {method}: {e}");
            return Vec::new();
        }
    };
    let cache_key = ManifestCache::key(method, &state.upstream_key(upstream, credential));
    if let Some(items) = state.manifests.get(&cache_key) {
        return items;
    }

    let timeout = Duration::from_millis(state.cfg.aggregation_timeout_ms as u64);
    let resp = match call_upstream(
        state,
        client,
        ctx,
        upstream,
        method,
        Some(serde_json::json!({})),
        timeout,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            logger::warn!("mcp-hub: skipping upstream '{name}' for {method}: {e}");
            return Vec::new();
        }
    };
    if let Some(err) = resp.error.as_ref() {
        logger::warn!(
            "mcp-hub: upstream '{name}' {method} error: {} ({})",
            err.message,
            err.code
        );
        return Vec::new();
    }
    let Some(Value::Array(items)) = resp
        .result
        .and_then(|mut r| r.get_mut(list_key).map(Value::take))
    else {
        logger::warn!("mcp-hub: upstream '{name}' {method} did not return '{list_key}' array");
        return Vec::new();
    };
    state.manifests.put(&cache_key, &items);
    items
}

/// Route a `tools/call` / `resources/read` / `prompts/get` to the owning
/// upstream. With the `prefix` strategy the owner of a tool or prompt is
/// parsed from its name; otherwise (and for resource URIs) the owner is
/// looked up in the aggregated manifest.
#[allow(clippy::too_many_arguments)]
async fn handle_route(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    id: Option<Value>,
    method: &str,
    params: Option<Value>,
    key_field: &str,
    not_found_code: i32,
) -> Response {
    let Some(mut params) = params else {
        return make_json_response(
            200,
            &error_response(id, INVALID_PARAMS, &format!("'{method}' requires params")),
        );
    };
    let requested = match params.get(key_field).and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return make_json_response(
                200,
                &error_response(
                    id,
                    INVALID_PARAMS,
                    &format!("'{method}' params missing required '{key_field}'"),
                ),
            );
        }
    };

    let fast = match key_field {
        "name" => resolve_owner_fast(state, &requested),
        _ => None,
    };
    let owner = match fast {
        Some(hint) => Some(hint),
        None => {
            resolve_owner_via_aggregate(state, client, ctx, method, key_field, &requested).await
        }
    };
    let Some((server_name, original_key)) = owner else {
        return make_json_response(
            200,
            &error_response(
                id,
                not_found_code,
                &format!("no MCP server owns '{key_field}' = '{requested}'"),
            ),
        );
    };

    let upstream = match state
        .upstreams
        .iter()
        .find(|u| u.server.name == server_name)
    {
        Some(u) => u,
        None => {
            return make_json_response(
                200,
                &error_response(
                    id,
                    SERVER_UNAVAILABLE,
                    &format!("server '{server_name}' is not configured"),
                ),
            );
        }
    };

    // Rewrite the key field with the original (un-prefixed) name so the
    // upstream sees its native namespace.
    if let Some(obj) = params.as_object_mut() {
        obj.insert(key_field.into(), Value::String(original_key));
    }

    let timeout = upstream_timeout(state, upstream);
    match call_upstream(state, client, ctx, upstream, method, Some(params), timeout).await {
        Ok(resp) => {
            if let Some(err) = resp.error {
                make_json_response(
                    200,
                    &error_response_with_data(id, err.code, &err.message, err.data),
                )
            } else {
                let result = resp.result.unwrap_or(Value::Null);
                make_json_response(200, &success_response(id.unwrap_or(Value::Null), result))
            }
        }
        Err(e) => {
            logger::error!("mcp-hub: routed call to '{server_name}' failed: {e}");
            upstream_failure_response(&state.cfg, upstream, id, &e)
        }
    }
}

/// One JSON-RPC call to an upstream inside its MCP session.
///
/// Resolves the caller's credential (passthrough servers), initializes the
/// upstream session on a cache miss, and — when the upstream answers 404
/// to a request that carried a session id (session expired / server
/// restarted) — drops the session, re-initializes, and retries once.
async fn call_upstream(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    method: &str,
    params: Option<Value>,
    timeout: Duration,
) -> Result<JsonRpcResponse, UpstreamError> {
    let credential = ctx.credential_for(upstream)?;
    let key = state.upstream_key(upstream, credential);
    let mut retried = false;

    loop {
        let session = match state.sessions.get(&key) {
            Some(sid) => sid,
            None => {
                // A session created just now can't be stale; don't
                // re-initialize again on a 404.
                retried = true;
                initialize_upstream(state, client, upstream, &key, credential).await?
            }
        };
        let sid = Some(session.as_str()).filter(|s| !s.is_empty());
        let request = JsonRpcOutbound::new(next_rpc_id(), method, params.clone());

        match upstream
            .call(client, request, timeout, sid, credential)
            .await
        {
            Ok(result) => {
                if let Some(new_sid) = result.mcp_session_id.filter(|n| Some(n.as_str()) != sid) {
                    state.sessions.put(&key, &new_sid);
                }
                return Ok(result.response);
            }
            Err(UpstreamError::HttpStatus { status: 404, .. }) if sid.is_some() && !retried => {
                logger::info!(
                    "mcp-hub: upstream '{}' rejected its session (404); re-initializing",
                    upstream.server.name
                );
                state.sessions.remove(&key);
                retried = true;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Run the MCP handshake (`initialize` + `notifications/initialized`)
/// against an upstream and cache the session id it issues (empty when it
/// issues none). Returns that id.
async fn initialize_upstream(
    state: &PolicyState,
    client: &HttpClient,
    upstream: &UpstreamClient,
    key: &str,
    credential: Option<&str>,
) -> Result<String, UpstreamError> {
    let timeout = upstream_timeout(state, upstream);
    let init_request = JsonRpcOutbound::new(
        next_rpc_id(),
        "initialize",
        Some(serde_json::json!({
            "protocolVersion": UPSTREAM_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": HUB_NAME, "version": HUB_VERSION }
        })),
    );
    let result = upstream
        .call(client, init_request, timeout, None, credential)
        .await?;
    if let Some(err) = result.response.error {
        return Err(UpstreamError::InitializeRejected {
            server: upstream.server.name.clone(),
            code: err.code,
            message: err.message,
        });
    }
    let sid = result.mcp_session_id.unwrap_or_default();

    // MCP requires `notifications/initialized` after `initialize`. Best
    // effort — many servers tolerate its absence.
    let initialized = JsonRpcOutbound::notification("notifications/initialized", None);
    if let Err(e) = upstream
        .notify(
            client,
            initialized,
            timeout,
            Some(sid.as_str()).filter(|s| !s.is_empty()),
            credential,
        )
        .await
    {
        logger::debug!(
            "mcp-hub: notifications/initialized to '{}' failed (ignored): {}",
            upstream.server.name,
            e
        );
    }

    state.sessions.put(key, &sid);
    Ok(sid)
}

/// Outbound JSON-RPC id, unique per call. Static-auth upstream sessions
/// are shared by every caller (and every worker), so ids must never
/// repeat within a session — MCP forbids it, and SDK transports route
/// responses by id.
fn next_rpc_id() -> String {
    thread_local! {
        static SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    let seq = SEQ.with(|c| {
        c.set(c.get().wrapping_add(1));
        c.get()
    });
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("hub-{nanos:x}-{seq}")
}

fn upstream_timeout(state: &PolicyState, upstream: &UpstreamClient) -> Duration {
    Duration::from_millis(
        upstream
            .server
            .timeout_ms
            .unwrap_or(state.cfg.per_request_timeout_ms) as u64,
    )
}

/// Resolve owner for the prefix-strategy fast path. The aggregator joins
/// `<server>_<tool>`, but `_` can appear inside both server names (e.g.
/// `jira_dev`) and tool names (e.g. `list_issues`). To disambiguate, we
/// pick the longest configured server name that the requested string
/// starts with, followed by `_`.
fn resolve_owner_fast(state: &PolicyState, requested: &str) -> Option<(String, String)> {
    if state.cfg.collision_strategy != CollisionStrategy::Prefix {
        return None;
    }
    let mut best: Option<(&str, usize)> = None;
    for server in state.cfg.servers.iter().map(|s| s.name.as_str()) {
        let plen = server.len() + 1;
        if requested.len() <= plen {
            continue;
        }
        if requested.starts_with(server)
            && requested.as_bytes()[server.len()] == b'_'
            && best.map(|(_, l)| plen > l).unwrap_or(true)
        {
            best = Some((server, plen));
        }
    }
    let (server, plen) = best?;
    Some((server.to_string(), requested[plen..].to_string()))
}

/// Slow-path resolver: aggregate the relevant `*list` manifest (served
/// from the manifest cache when fresh) and find the entry whose
/// `key_field` equals `requested`. Returns (server, key as the upstream
/// knows it). Used when the prefix fast path doesn't apply: other
/// collision strategies, resource URIs, and names without a server prefix.
async fn resolve_owner_via_aggregate(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    method: &str,
    key_field: &str,
    requested: &str,
) -> Option<(String, String)> {
    let (list_method, list_key) = match method {
        "tools/call" => ("tools/list", "tools"),
        "resources/read" => ("resources/list", "resources"),
        "prompts/get" => ("prompts/list", "prompts"),
        _ => return None,
    };

    let all = collect_list_items(state, client, ctx, list_method, list_key).await;
    if key_field == "uri" {
        // Resources are de-duplicated by name, which says nothing about
        // URIs: look the URI up before collision handling, so a resource
        // dropped from the merged list as a name duplicate is still
        // readable. Upstreams are in priority order, so the first wins.
        let owner = all
            .iter()
            .find(|item| item.value.get("uri").and_then(|v| v.as_str()) == Some(requested))?;
        return Some((owner.server.to_string(), requested.to_string()));
    }
    let aggregated = aggregate(all, state.cfg.collision_strategy).ok()?;
    let hint = resolve_owner(&aggregated, key_field, requested)?;
    Some((hint.server, hint.original_name))
}

/// Best-effort fan-out of a client notification to the upstreams where
/// this caller has their own (passthrough) session. Not forwarded:
///   * `notifications/initialized` — the hub completes each upstream
///     handshake itself in [`initialize_upstream`];
///   * `notifications/cancelled` — the hub rewrites request ids, so the
///     client's `requestId` means nothing upstream;
///   * anything to static-auth upstreams, whose session is shared by all
///     callers.
async fn fan_out_notification(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    rpc: &JsonRpcRequest,
) {
    if matches!(
        rpc.method.as_str(),
        "notifications/initialized" | "notifications/cancelled"
    ) {
        return;
    }
    // The client waits for its 202 until the fan-out finishes.
    let timeout = Duration::from_millis(state.cfg.aggregation_timeout_ms as u64);
    let sends = state.upstreams.iter().filter_map(|upstream| {
        let Ok(credential @ Some(_)) = ctx.credential_for(upstream) else {
            return None;
        };
        let key = state.upstream_key(upstream, credential);
        let session = state.sessions.get(&key)?;
        let notification = JsonRpcOutbound::notification(rpc.method.clone(), rpc.params.clone());
        Some(async move {
            let sid = Some(session.as_str()).filter(|s| !s.is_empty());
            if let Err(e) = upstream
                .notify(client, notification, timeout, sid, credential)
                .await
            {
                logger::debug!(
                    "mcp-hub: notification fan-out to '{}' failed (ignored): {}",
                    upstream.server.name,
                    e
                );
            }
        })
    });
    join_all(sends).await;
}

/// Map a failed routed call to the client response. Auth failures of
/// passthrough upstreams become real HTTP 401/403 with `WWW-Authenticate`
/// so MCP clients can start (or step up) their OAuth flow. A static-auth
/// upstream rejecting the hub's own credential is a hub misconfiguration
/// the client can't fix, so it stays a JSON-RPC error.
fn upstream_failure_response(
    cfg: &PolicyConfig,
    upstream: &UpstreamClient,
    id: Option<Value>,
    err: &UpstreamError,
) -> Response {
    let message = err.to_string();
    let passthrough = upstream.server.passthrough_header().is_some();
    match err {
        UpstreamError::MissingCredential { .. } => auth_challenge(cfg, 401, None, id, &message),
        UpstreamError::HttpStatus {
            status: status @ (401 | 403),
            www_authenticate,
            ..
        } if passthrough => auth_challenge(cfg, *status, www_authenticate.as_deref(), id, &message),
        UpstreamError::HttpStatus { .. }
        | UpstreamError::Transport { .. }
        | UpstreamError::InitializeRejected { .. } => {
            make_json_response(200, &error_response(id, SERVER_UNAVAILABLE, &message))
        }
        UpstreamError::BadJson(_) => {
            make_json_response(200, &error_response(id, INTERNAL_ERROR, &message))
        }
    }
}

/// Build a 401/403 challenge. When the hub has protected-resource metadata
/// the challenge points at it (clients must get tokens for the hub's own
/// resource URL); otherwise the upstream's challenge is relayed verbatim.
fn auth_challenge(
    cfg: &PolicyConfig,
    status: u32,
    upstream_challenge: Option<&str>,
    id: Option<Value>,
    message: &str,
) -> Response {
    let challenge = match (&cfg.protected_resource, upstream_challenge) {
        (Some(pr), _) if status == 403 => format!(
            "Bearer error=\"insufficient_scope\", resource_metadata=\"{}\"",
            pr.metadata_url()
        ),
        (Some(pr), _) => format!("Bearer resource_metadata=\"{}\"", pr.metadata_url()),
        (None, Some(upstream)) => upstream.to_string(),
        (None, None) => "Bearer".to_string(),
    };
    make_json_response_with_headers(
        status,
        &error_response(id, AUTH_REQUIRED, message),
        vec![("www-authenticate".into(), challenge)],
    )
}

/// RFC 9728 protected-resource metadata document.
fn protected_resource_response(pr: &ProtectedResource, method: &str) -> Response {
    if method != "GET" {
        return Response::new(405).with_headers(vec![("allow".into(), "GET".into())]);
    }
    let mut doc = serde_json::json!({
        "resource": pr.resource_url,
        "authorization_servers": pr.authorization_servers,
        "bearer_methods_supported": ["header"],
    });
    if !pr.scopes_supported.is_empty() {
        doc["scopes_supported"] = serde_json::json!(pr.scopes_supported);
    }
    make_json_response(200, &serde_json::to_vec(&doc).expect("metadata serializes"))
}

/// Pick the protocol revision to answer `initialize` with.
fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|v| Some(*v) == requested)
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
}

/// True when `allowedOrigins` is configured and the request carries an
/// `Origin` outside it. Requests without `Origin` (non-browser clients)
/// are allowed.
fn origin_rejected(cfg: &PolicyConfig, headers: &[(String, String)]) -> bool {
    if cfg.allowed_origins.is_empty() {
        return false;
    }
    let Some(origin) = find_header(headers, "origin") else {
        return false;
    };
    let origin = origin.trim_end_matches('/');
    !cfg.allowed_origins
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Match the inbound URL path to the configured MCP endpoint. We accept
/// any path that *contains* the endpoint as a suffix-or-segment, so
/// API Manager can mount the policy at an arbitrary prefix
/// (e.g. `/api/v1/mcp` matches `mcpEndpoint=/mcp`).
fn path_matches(path: &str, mcp_endpoint: &str) -> bool {
    if path == mcp_endpoint {
        return true;
    }
    if let Some(idx) = path.rfind(mcp_endpoint) {
        let after = &path[idx + mcp_endpoint.len()..];
        if after.is_empty() || after.starts_with('/') {
            return true;
        }
    }
    false
}

fn make_response(status: u32, body: &[u8]) -> Response {
    Response::new(status).with_body(body.to_vec())
}

fn make_text_response(status: u32, body: &str) -> Response {
    Response::new(status)
        .with_headers(vec![(
            "content-type".into(),
            "text/plain; charset=utf-8".into(),
        )])
        .with_body(body.as_bytes().to_vec())
}

fn make_json_response(status: u32, body: &[u8]) -> Response {
    Response::new(status)
        .with_headers(vec![("content-type".into(), "application/json".into())])
        .with_body(body.to_vec())
}

fn make_json_response_with_headers(
    status: u32,
    body: &[u8],
    extra_headers: Vec<(String, String)>,
) -> Response {
    let mut headers: Vec<(String, String)> =
        vec![("content-type".into(), "application/json".into())];
    headers.extend(extra_headers);
    Response::new(status)
        .with_headers(headers)
        .with_body(body.to_vec())
}

#[entrypoint]
pub async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    cache_builder: CacheBuilder,
) -> anyhow::Result<()> {
    let raw: Config =
        serde_json::from_slice(&bytes).map_err(|e| anyhow!("invalid policy configuration: {e}"))?;
    let cfg = PolicyConfig::from_config(&raw)
        .map_err(|e| anyhow!("policy configuration rejected: {e}"))?;

    logger::info!(
        "mcp-hub: loaded {} server(s); endpoint='{}' collision={:?} strict={} manifestTtl={}s",
        cfg.servers.len(),
        cfg.mcp_endpoint,
        cfg.collision_strategy,
        cfg.strict_mode,
        cfg.manifest_cache_ttl_seconds,
    );
    if raw.diagnostic_continue_flow == Some(true) {
        logger::warn!("mcp-hub: diagnosticContinueFlow is deprecated and ignored");
    }

    // Pair each typed McpServerConfig with its codegen-registered
    // Service by (trimmed, unique) name — a mismatch would send one
    // server's credentials to another server's endpoint.
    let mut upstreams = Vec::with_capacity(cfg.servers.len());
    for typed_server in cfg.servers.iter().filter(|s| s.enabled) {
        let raw_server = raw
            .mcp_servers
            .iter()
            .find(|r| r.name.trim() == typed_server.name)
            .ok_or_else(|| anyhow!("no endpoint registered for '{}'", typed_server.name))?;
        let service = Rc::new(raw_server.endpoint.clone());
        upstreams.push(UpstreamClient::new(typed_server.clone(), service));
    }

    // Restore the priority-sorted order on the runtime list so iteration
    // is deterministic at request time.
    upstreams.sort_by_key(|u| u.server.priority);

    let config_tag: Rc<str> = crate::session::config_tag(&bytes).into();
    let ttl_seconds = cfg.upstream_session_ttl_seconds;
    let manifest_ttl_seconds = cfg.manifest_cache_ttl_seconds;
    let state = PolicyState {
        cfg: Rc::new(cfg),
        upstreams: Rc::new(upstreams),
        sessions: Rc::new(SessionStore::new(
            cache_builder
                .new(SESSION_CACHE_ID.to_string())
                .max_entries(SESSION_CACHE_MAX_ENTRIES)
                .shared()
                .build(),
            ttl_seconds,
        )),
        manifests: Rc::new(ManifestCache::new(
            cache_builder
                .new(MANIFEST_CACHE_ID.to_string())
                .max_entries(MANIFEST_CACHE_MAX_ENTRIES)
                .shared()
                .build(),
            manifest_ttl_seconds,
        )),
        config_tag,
    };

    let filter = on_request(move |request: RequestHeadersState, client: HttpClient| {
        let s = state.clone();
        async move { request_filter(request, s, client).await }
    });

    launcher.launch(filter).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matches_root_endpoint() {
        assert!(path_matches("/mcp", "/mcp"));
        assert!(path_matches("/api/v1/mcp", "/mcp"));
        assert!(path_matches("/mcp/sub", "/mcp"));
        assert!(!path_matches("/mcpfoo", "/mcp"));
        assert!(!path_matches("/health", "/mcp"));
    }
}
