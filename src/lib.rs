//! MCP Hub policy entrypoint.
//!
//! Single-phase architecture: everything happens in the request filter.
//!
//! 1. Match the inbound path against `mcpEndpoint` (defaults to `/mcp`).
//!    Non-matches are short-circuited with 404 (strict) or returned as
//!    `Flow::Continue(())` so the gateway's own upstream handles them.
//! 2. `GET /mcp`: if the client advertises `Accept: text/event-stream`,
//!    reply 200 with an immediate empty SSE stream so the EventSource
//!    is happy (the hub never pushes server-initiated events). For
//!    plain GETs (no SSE Accept) reply 405 with `Allow: POST`. Other
//!    non-POST methods always get 405.
//! 3. For `POST /mcp` drain the body, parse the JSON-RPC envelope, and
//!    dispatch inline — local short-circuit (`initialize`, `ping`),
//!    aggregate (`tools/list`, etc.), or route (`tools/call`, etc.) —
//!    then `Flow::Break` with the synthesized response. Notifications
//!    (no `id`) are fanned out best-effort and answered with 202.
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
mod mcp;
mod upstream;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use pdk::cache::CacheBuilder;
use pdk::hl::*;
use pdk::logger;
use serde_json::Value;

use crate::config::{CollisionStrategy, McpServerConfig, PolicyConfig};
use crate::generated::config::Config;
use crate::jsonrpc::{
    error_response, error_response_with_data, success_response, JsonRpcOutbound, JsonRpcRequest,
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
    PROMPT_NOT_FOUND, RESOURCE_NOT_FOUND, SERVER_UNAVAILABLE, TOOL_NOT_FOUND,
};
use crate::mcp::aggregate::{aggregate, resolve_owner, strip_internal, AggregateError, UpstreamItem};
use crate::mcp::method::McpMethod;
use crate::mcp::types::{server_capabilities, server_info};
use crate::upstream::{UpstreamClient, UpstreamError};

const HUB_NAME: &str = "mcp-hub-policy";
const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol version the hub claims on `initialize`. Clients are
/// expected to negotiate to the lowest mutually supported revision.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Resolved upstreams (one [`UpstreamClient`] per enabled MCP server).
///
/// `upstream_sessions` is a per-replica cache of `<server_name> ->
/// mcp-session-id` learned from each upstream's `initialize` response.
/// Streamable-HTTP MCP servers gate post-initialize traffic on this id;
/// the hub initializes each upstream lazily on the first request that
/// needs it, then reuses the cached id for the lifetime of the worker.
#[derive(Clone)]
struct PolicyState {
    cfg: Rc<PolicyConfig>,
    upstreams: Rc<Vec<UpstreamClient>>,
    upstream_sessions: Rc<RefCell<HashMap<String, String>>>,
}

async fn request_filter(
    request: RequestHeadersState,
    state: PolicyState,
    client: HttpClient,
) -> Flow<()> {
    let method = request.method();
    let path = request.path();
    let bare = path.split_once('?').map(|(p, _)| p).unwrap_or(&path).to_string();

    if !path_matches(&bare, &state.cfg.mcp_endpoint) {
        if state.cfg.strict_mode {
            logger::debug!("mcp-hub: rejecting non-MCP path '{}' (strictMode)", bare);
            return Flow::Break(make_text_response(404, "not found"));
        }
        // Permissive mode: let the gateway's upstream handle this path.
        return Flow::Continue(());
    }

    let m = method.to_ascii_uppercase();
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
        let accepts_sse = request
            .handler()
            .headers()
            .into_iter()
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

    let client_session_id = request
        .handler()
        .headers()
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
        .map(|(_, v)| v);

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

    let resp = dispatch_rpc(&state, &client, rpc, client_session_id).await;
    Flow::Break(resp)
}

async fn dispatch_rpc(
    state: &PolicyState,
    client: &HttpClient,
    rpc: JsonRpcRequest,
    client_session_id: Option<String>,
) -> Response {
    let id = rpc.id.clone();
    let params = rpc.params.clone();
    let method = McpMethod::parse(&rpc.method, rpc.is_notification());

    if rpc.is_notification() {
        // Notifications are best-effort: fan out to all enabled upstreams,
        // ignore errors, return 202 Accepted (no body) per MCP Streamable
        // HTTP spec. 204 trips undici's null-body-status check in the
        // MCP Inspector proxy.
        fan_out_notification(state, client, &rpc).await;
        return make_response(202, b"");
    }

    let id_value = id.clone().unwrap_or(Value::Null);
    match method {
        McpMethod::Initialize => {
            let payload = serde_json::json!({
                "protocolVersion": params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .cloned()
                    .unwrap_or_else(|| Value::String(MCP_PROTOCOL_VERSION.into())),
                "capabilities": server_capabilities(),
                "serverInfo": server_info(HUB_NAME, HUB_VERSION),
            });
            // Mint a hub session id so clients that follow the
            // Streamable HTTP convention can echo it on subsequent
            // requests.
            let hub_session_id = client_session_id
                .clone()
                .unwrap_or_else(|| mint_session_id());
            make_json_response_with_headers(
                200,
                &success_response(id_value, payload),
                vec![("mcp-session-id".into(), hub_session_id)],
            )
        }
        McpMethod::Ping => make_json_response(200, &success_response(id_value, Value::Object(Default::default()))),
        McpMethod::ToolsList => {
            handle_aggregate_list(state, client, id.clone(), "tools/list", "tools").await
        }
        McpMethod::ResourcesList => {
            handle_aggregate_list(state, client, id.clone(), "resources/list", "resources").await
        }
        McpMethod::PromptsList => {
            handle_aggregate_list(state, client, id.clone(), "prompts/list", "prompts").await
        }
        McpMethod::ToolsCall => {
            handle_route(
                state,
                client,
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

/// Fan an aggregated `*list` call across all enabled upstreams. Slow or
/// failing upstreams are logged and skipped — partial degradation is
/// preferable to a total aggregate failure.
async fn handle_aggregate_list(
    state: &PolicyState,
    client: &HttpClient,
    id: Option<Value>,
    method: &str,
    list_key: &str,
) -> Response {
    let timeout = Duration::from_millis(state.cfg.aggregation_timeout_ms as u64);
    let mut all: Vec<UpstreamItem<'_>> = Vec::new();

    for upstream in state.upstreams.iter() {
        if let Err(e) = ensure_upstream_initialized(state, client, upstream).await {
            logger::warn!(
                "mcp-hub: upstream '{}' init failed: {}",
                upstream.server.name,
                e
            );
            continue;
        }
        let rpc_id = format!("{}-{}", method.replace('/', "-"), upstream.server.name);
        let request = JsonRpcOutbound::new(rpc_id, method, Some(serde_json::json!({})));
        let session = state
            .upstream_sessions
            .borrow()
            .get(&upstream.server.name)
            .cloned();
        match upstream
            .call(client, request, timeout, session.as_deref())
            .await
        {
            Ok(call_result) => {
                if let Some(sid) = call_result.mcp_session_id {
                    state
                        .upstream_sessions
                        .borrow_mut()
                        .insert(upstream.server.name.clone(), sid);
                }
                let resp = call_result.response;
                if let Some(err) = resp.error.as_ref() {
                    logger::warn!(
                        "mcp-hub: upstream '{}' {method} error: {} ({})",
                        upstream.server.name,
                        err.message,
                        err.code
                    );
                    continue;
                }
                let Some(result) = resp.result else { continue };
                let Some(items) = result.get(list_key).and_then(|v| v.as_array()) else {
                    logger::warn!(
                        "mcp-hub: upstream '{}' {method} did not return '{list_key}' array",
                        upstream.server.name
                    );
                    continue;
                };
                for item in items.iter().cloned() {
                    all.push(UpstreamItem {
                        server: upstream.server.name.as_str(),
                        value: item,
                    });
                }
            }
            Err(e) => {
                logger::warn!(
                    "mcp-hub: upstream '{}' {method} call failed: {}",
                    upstream.server.name,
                    e
                );
            }
        }
    }

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

/// Route a `tools/call` / `resources/read` / `prompts/get` to the owning
/// upstream. The mapping is rebuilt on every call by re-aggregating —
/// the manifest cardinality is small and the operation is rare enough
/// that the simplicity is worth the per-request fan-out (a TTL cache
/// is wired up via `manifestCacheTtlSeconds` and is documented as a
/// future optimization).
async fn handle_route(
    state: &PolicyState,
    client: &HttpClient,
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

    // Choose owner. With prefix strategy, names look like `<server>.<orig>`
    // so we can resolve cheaply by parsing the prefix; with other
    // strategies we re-aggregate manifests to find the owner.
    let (server_name, original_key) = match resolve_owner_fast(state, &requested) {
        Some(hint) => hint,
        None => match resolve_owner_via_aggregate(state, client, method, key_field).await {
            Some(map) => match map.into_iter().find(|(name, _)| name == &requested) {
                Some((_, hint)) => hint,
                None => {
                    return make_json_response(
                        200,
                        &error_response(
                            id,
                            not_found_code,
                            &format!("no MCP server owns '{}' = '{requested}'", key_field),
                        ),
                    );
                }
            },
            None => {
                return make_json_response(
                    200,
                    &error_response(
                        id,
                        not_found_code,
                        &format!("no MCP server owns '{}' = '{requested}'", key_field),
                    ),
                );
            }
        },
    };

    let upstream = match state.upstreams.iter().find(|u| u.server.name == server_name) {
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

    let timeout = Duration::from_millis(
        upstream
            .server
            .timeout_ms
            .unwrap_or(state.cfg.per_request_timeout_ms) as u64,
    );

    if let Err(e) = ensure_upstream_initialized(state, client, upstream).await {
        return make_json_response(
            200,
            &error_response(
                id,
                SERVER_UNAVAILABLE,
                &format!("upstream '{server_name}' init failed: {e}"),
            ),
        );
    }
    let session = state
        .upstream_sessions
        .borrow()
        .get(&server_name)
        .cloned();

    let rpc_id = format!("{}-{}-{}", method.replace('/', "-"), server_name, requested);
    let request = JsonRpcOutbound::new(rpc_id, method, Some(params));

    match upstream
        .call(client, request, timeout, session.as_deref())
        .await
    {
        Ok(call_result) => {
            if let Some(sid) = call_result.mcp_session_id {
                state
                    .upstream_sessions
                    .borrow_mut()
                    .insert(server_name.clone(), sid);
            }
            let resp = call_result.response;
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
            logger::error!("mcp-hub: routed call to '{}' failed: {}", server_name, e);
            make_json_response(
                200,
                &error_response(id, upstream_error_code(&e), &format!("{e}")),
            )
        }
    }
}

/// Ensure an upstream MCP server has a known session id. Streamable
/// HTTP servers gate post-initialize calls on this id; others ignore
/// it. The hub initializes each upstream lazily on the first call that
/// needs it and caches the id for the lifetime of the worker.
async fn ensure_upstream_initialized(
    state: &PolicyState,
    client: &HttpClient,
    upstream: &UpstreamClient,
) -> Result<(), UpstreamError> {
    if state
        .upstream_sessions
        .borrow()
        .contains_key(&upstream.server.name)
    {
        return Ok(());
    }
    let timeout = Duration::from_millis(
        upstream
            .server
            .timeout_ms
            .unwrap_or(state.cfg.per_request_timeout_ms) as u64,
    );
    let init_request = JsonRpcOutbound::new(
        format!("init-{}", upstream.server.name),
        "initialize",
        Some(serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": HUB_NAME, "version": HUB_VERSION }
        })),
    );
    let result = upstream.call(client, init_request, timeout, None).await?;
    if let Some(sid) = result.mcp_session_id {
        state
            .upstream_sessions
            .borrow_mut()
            .insert(upstream.server.name.clone(), sid);
    } else {
        // Server doesn't use session ids — record a sentinel so we
        // don't re-initialize on every call.
        state
            .upstream_sessions
            .borrow_mut()
            .insert(upstream.server.name.clone(), String::new());
    }
    // After initialize, MCP requires a `notifications/initialized`. Best
    // effort — many servers tolerate its absence but the spec requires it.
    let session = state
        .upstream_sessions
        .borrow()
        .get(&upstream.server.name)
        .cloned()
        .filter(|s| !s.is_empty());
    let initialized_notif = JsonRpcOutbound::new(
        Value::Null,
        "notifications/initialized",
        Some(serde_json::json!({})),
    );
    let _ = upstream
        .call(client, initialized_notif, timeout, session.as_deref())
        .await;
    Ok(())
}

fn mint_session_id() -> String {
    use sha2::{Digest, Sha256};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(now.to_be_bytes());
    h.update(HUB_NAME.as_bytes());
    let d = h.finalize();
    let b = &d[..16];
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0xf, b[7], (b[8] & 0x3f) | 0x80, b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
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
        if requested.starts_with(server) && requested.as_bytes()[server.len()] == b'_' {
            if best.map(|(_, l)| plen > l).unwrap_or(true) {
                best = Some((server, plen));
            }
        }
    }
    let (server, plen) = best?;
    Some((server.to_string(), requested[plen..].to_string()))
}

/// Slow-path resolver: re-aggregate the relevant `*list` manifest and
/// build a name-to-(server, original_name) map. Used when the prefix
/// fast-path doesn't apply (collisionStrategy != prefix).
async fn resolve_owner_via_aggregate(
    state: &PolicyState,
    client: &HttpClient,
    method: &str,
    key_field: &str,
) -> Option<Vec<(String, (String, String))>> {
    let list_method = match method {
        "tools/call" => "tools/list",
        "resources/read" => "resources/list",
        "prompts/get" => "prompts/list",
        _ => return None,
    };
    let list_key = match list_method {
        "tools/list" => "tools",
        "resources/list" => "resources",
        "prompts/list" => "prompts",
        _ => return None,
    };

    let timeout = Duration::from_millis(state.cfg.aggregation_timeout_ms as u64);
    let mut all: Vec<UpstreamItem<'_>> = Vec::new();
    for upstream in state.upstreams.iter() {
        if ensure_upstream_initialized(state, client, upstream).await.is_err() {
            continue;
        }
        let session = state
            .upstream_sessions
            .borrow()
            .get(&upstream.server.name)
            .cloned()
            .filter(|s| !s.is_empty());
        let rpc_id = format!("{}-{}", list_method.replace('/', "-"), upstream.server.name);
        let request = JsonRpcOutbound::new(rpc_id, list_method, Some(serde_json::json!({})));
        if let Ok(call_result) = upstream
            .call(client, request, timeout, session.as_deref())
            .await
        {
            if let Some(sid) = call_result.mcp_session_id {
                state
                    .upstream_sessions
                    .borrow_mut()
                    .insert(upstream.server.name.clone(), sid);
            }
            let resp = call_result.response;
            if let Some(items) = resp
                .result
                .as_ref()
                .and_then(|r| r.get(list_key))
                .and_then(|v| v.as_array())
            {
                for item in items.iter().cloned() {
                    all.push(UpstreamItem {
                        server: upstream.server.name.as_str(),
                        value: item,
                    });
                }
            }
        }
    }

    let aggregated = aggregate(all, state.cfg.collision_strategy).ok()?;
    let mut out = Vec::with_capacity(aggregated.len());
    for entry in aggregated.iter() {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let _ = key_field; // signature parity; key in the manifest list is always `name`
        let hint = resolve_owner(&aggregated, name)?;
        out.push((name.to_string(), (hint.server.clone(), hint.original_name.clone())));
    }
    Some(out)
}

/// Best-effort fan-out of a JSON-RPC notification to every enabled
/// upstream. Errors are swallowed; notifications have no response.
async fn fan_out_notification(state: &PolicyState, client: &HttpClient, rpc: &JsonRpcRequest) {
    let timeout = Duration::from_millis(state.cfg.per_request_timeout_ms as u64);
    for upstream in state.upstreams.iter() {
        // Notifications must omit the `id` field. Our outbound builder
        // requires an id; fabricate a placeholder and rely on the
        // upstream to ignore the response.
        let request = JsonRpcOutbound::new(
            Value::Null,
            rpc.method.clone(),
            rpc.params.clone(),
        );
        let session = state
            .upstream_sessions
            .borrow()
            .get(&upstream.server.name)
            .cloned()
            .filter(|s| !s.is_empty());
        if let Err(e) = upstream
            .call(client, request, timeout, session.as_deref())
            .await
        {
            logger::debug!(
                "mcp-hub: notification fan-out to '{}' failed (ignored): {}",
                upstream.server.name,
                e
            );
        }
    }
}

fn upstream_error_code(err: &UpstreamError) -> i32 {
    match err {
        UpstreamError::HttpStatus { .. } => SERVER_UNAVAILABLE,
        UpstreamError::Transport { .. } => SERVER_UNAVAILABLE,
        UpstreamError::BadJson(_) => INTERNAL_ERROR,
    }
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
        .with_headers(vec![(
            "content-type".into(),
            "application/json".into(),
        )])
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
    _cache_builder: CacheBuilder,
) -> anyhow::Result<()> {
    let raw: Config = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("invalid policy configuration: {e}"))?;
    let cfg = PolicyConfig::from_config(&raw)
        .map_err(|e| anyhow!("policy configuration rejected: {e}"))?;

    logger::info!(
        "mcp-hub: loaded {} server(s); endpoint='{}' collision={:?} strict={} manifestTtl={}s diagContinue={}",
        cfg.servers.len(),
        cfg.mcp_endpoint,
        cfg.collision_strategy,
        cfg.strict_mode,
        cfg.manifest_cache_ttl_seconds,
        cfg.diagnostic_continue_flow,
    );

    // Pair each typed McpServerConfig with the codegen-registered
    // Service. Order is preserved by the codegen, so we can zip — but
    // only after sorting both lists by name to be safe against any
    // future codegen reorderings.
    let mut typed: Vec<McpServerConfig> = cfg.servers.clone();
    typed.sort_by(|a, b| a.name.cmp(&b.name));
    let mut raw_servers = raw.mcp_servers.clone();
    raw_servers.sort_by(|a, b| a.name.cmp(&b.name));

    let mut upstreams = Vec::with_capacity(typed.len());
    for (typed_server, raw_server) in typed.into_iter().zip(raw_servers.into_iter()) {
        if !typed_server.enabled {
            continue;
        }
        let service = Rc::new(raw_server.endpoint);
        upstreams.push(UpstreamClient::new(typed_server, service));
    }

    // Restore the priority-sorted order on the runtime list so iteration
    // is deterministic at request time.
    upstreams.sort_by_key(|u| u.server.priority);

    let state = PolicyState {
        cfg: Rc::new(cfg),
        upstreams: Rc::new(upstreams),
        upstream_sessions: Rc::new(RefCell::new(HashMap::new())),
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
