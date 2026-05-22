//! MCP Hub policy entrypoint.
//!
//! Two-phase architecture (mirrors `acp-to-a2a`):
//!
//! On request (`on_request`):
//!   1. Match the inbound path against `mcpEndpoint` (defaults to `/mcp`).
//!      Non-matches are short-circuited with 404 (strict) or passed
//!      through (permissive).
//!   2. For `GET /mcp` return a 200 placeholder describing the hub.
//!      Some MCP clients probe with GET before POSTing.
//!   3. For `POST /mcp` drain the body, parse the JSON-RPC envelope,
//!      classify the method, and stash it as a [`McpPending`] for the
//!      response phase to dispatch.
//!
//! On response (`on_response`):
//!   * Dispatch the [`McpPending`] — local short-circuit (`initialize`,
//!     `ping`), aggregate (`tools/list`, etc.), or route (`tools/call`,
//!     etc.) — and replace the upstream response via `send_response`.

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
    RESOURCE_NOT_FOUND, SERVER_UNAVAILABLE, TOOL_NOT_FOUND, PROMPT_NOT_FOUND,
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

/// Per-request work carried from request_filter to response_filter.
#[derive(Clone, Debug)]
enum McpPending {
    /// Path didn't match the MCP endpoint and strictMode=false. Allow
    /// the gateway's own upstream to handle the request.
    PassThrough,
    /// `GET /mcp` informational placeholder.
    HubInfo,
    /// JSON-RPC request to dispatch in the response phase.
    Rpc {
        request: JsonRpcRequest,
        /// Inbound `mcp-session-id` header, if any. The hub mints one of
        /// these on `initialize` and clients are expected to echo it.
        client_session_id: Option<String>,
    },
    /// Body could not be parsed as JSON-RPC. Carry the error to
    /// surface in the response phase.
    ParseError {
        id: Option<Value>,
        message: String,
    },
}

async fn request_filter(
    request: RequestHeadersState,
    state: PolicyState,
) -> Flow<McpPending> {
    let method = request.method();
    let path = request.path();
    let bare = path.split_once('?').map(|(p, _)| p).unwrap_or(&path).to_string();

    if !path_matches(&bare, &state.cfg.mcp_endpoint) {
        if state.cfg.strict_mode {
            logger::debug!("mcp-hub: rejecting non-MCP path '{}' (strictMode)", bare);
            return Flow::Break(make_text_response(404, "not found"));
        }
        return Flow::Continue(McpPending::PassThrough);
    }

    let m = method.to_ascii_uppercase();
    if m == "GET" {
        return Flow::Continue(McpPending::HubInfo);
    }
    if m != "POST" {
        return Flow::Break(make_json_response(
            405,
            &error_response(None, INVALID_REQUEST, "method not allowed; use POST"),
        ));
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
    match parsed {
        Ok(rpc) if rpc.jsonrpc != crate::jsonrpc::JSONRPC_VERSION => Flow::Continue(
            McpPending::ParseError {
                id: rpc.id,
                message: format!("expected jsonrpc=2.0, got '{}'", rpc.jsonrpc),
            },
        ),
        Ok(rpc) => Flow::Continue(McpPending::Rpc {
            request: rpc,
            client_session_id,
        }),
        Err(e) => Flow::Continue(McpPending::ParseError {
            id: None,
            message: format!("invalid JSON-RPC body: {e}"),
        }),
    }
}

async fn response_filter(
    response: ResponseHeadersState,
    state: PolicyState,
    client: HttpClient,
    data: RequestData<McpPending>,
) {
    let pending = match data {
        RequestData::Continue(p) => p,
        RequestData::Break | RequestData::Cancel => return,
    };

    match pending {
        McpPending::PassThrough => {
            // Let the gateway's upstream response flow through unchanged.
        }
        McpPending::HubInfo => {
            response.send_response(make_json_response(
                200,
                &serde_json::to_vec(&hub_info_payload(&state)).unwrap_or_default(),
            ));
        }
        McpPending::ParseError { id, message } => {
            response.send_response(make_json_response(
                200,
                &error_response(id, PARSE_ERROR, &message),
            ));
        }
        McpPending::Rpc {
            request: rpc,
            client_session_id,
        } => {
            let resp = dispatch_rpc(&state, &client, rpc, client_session_id).await;
            response.send_response(resp);
        }
    }
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
        // ignore errors, return 204.
        fan_out_notification(state, client, &rpc).await;
        return make_response(204, b"");
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

/// Resolve owner for the prefix-strategy fast path: split `name` on the
/// first `.` and verify the first segment is a configured server.
fn resolve_owner_fast(state: &PolicyState, requested: &str) -> Option<(String, String)> {
    if state.cfg.collision_strategy != CollisionStrategy::Prefix {
        return None;
    }
    let (server, rest) = requested.split_once('.')?;
    if state.cfg.server(server).is_some() {
        Some((server.to_string(), rest.to_string()))
    } else {
        None
    }
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

fn hub_info_payload(state: &PolicyState) -> Value {
    let servers: Vec<Value> = state
        .cfg
        .enabled_servers()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "priority": s.priority,
            })
        })
        .collect();
    serde_json::json!({
        "name": HUB_NAME,
        "version": HUB_VERSION,
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "endpoint": state.cfg.mcp_endpoint,
        "servers": servers,
    })
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

    let request_state = state.clone();
    let response_state = state;
    let filter = on_request(move |request: RequestHeadersState, _client: HttpClient| {
        let s = request_state.clone();
        async move { request_filter(request, s).await }
    })
    .on_response(
        move |response, client: HttpClient, data: RequestData<McpPending>| {
            let s = response_state.clone();
            async move { response_filter(response, s, client, data).await }
        },
    );

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
