//! MCP Hub policy entrypoint.
//!
//! Single-phase architecture: everything happens in the request filter.
//! The hub is stateless toward clients: it issues no `mcp-session-id`
//! and ignores any it receives. Upstream sessions are managed internally
//! (see `session.rs`).
//!
//! 0. `<...>/.well-known/oauth-protected-resource` (and the RFC 9728
//!    `/.well-known/oauth-protected-resource/<path>` form) serves the
//!    metadata when `authorizationServers` is configured.
//! 1. Match the inbound path against `mcpEndpoint` (defaults to `/mcp`).
//!    Non-matches are short-circuited with 404 (strict) or returned as
//!    `Flow::Continue(())` so the gateway's own upstream handles them.
//! 2. Every method but POST gets 405 with `Allow: POST` — the hub pushes
//!    no server-initiated messages, so it offers no GET stream. `Origin`
//!    (when `allowedOrigins` is set) is validated first; then
//!    `Authorization` (with `requireCallerAuth`) and
//!    `MCP-Protocol-Version`.
//! 3. For `POST /mcp` drain the body, parse one JSON-RPC message (batches
//!    are refused), and dispatch inline — local short-circuit
//!    (`initialize`, `ping`, `resources/templates/list`), aggregate
//!    (`tools/list`, etc.), or route (`tools/call`, etc.) — then
//!    `Flow::Break` with the synthesized response. Aggregation calls
//!    every enabled upstream concurrently and caches each upstream's
//!    answer (see `manifest.rs`). Notifications (no `id`) are fanned out
//!    best-effort and, like client JSON-RPC responses, answered with 202.
//!
//! A configuration with invalid hub-wide settings is logged and every MCP
//! request answers 503 with the reason (fail closed). An invalid
//! `mcpServers` entry only takes that server out of service.
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
    endpoint_settings, CollisionStrategy, PolicyConfig, ProtectedResource,
    WELL_KNOWN_PROTECTED_RESOURCE,
};
use crate::generated::config::Config;
use crate::jsonrpc::{
    error_response, error_response_with_data, parse_inbound, success_response, Inbound,
    JsonRpcOutbound, JsonRpcRequest, JsonRpcResponse, Refusal, AUTH_REQUIRED, INTERNAL_ERROR,
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PROMPT_NOT_FOUND, RESOURCE_NOT_FOUND,
    SERVER_UNAVAILABLE, TOOL_NOT_FOUND,
};
use crate::manifest::ManifestCache;
use crate::mcp::aggregate::{
    aggregate, resolve_owner, strip_internal, AggregateError, UpstreamItem,
};
use crate::mcp::method::McpMethod;
use crate::mcp::types::{server_capabilities, server_info};
use crate::session::{Session, SessionStore};
use crate::upstream::{Credential, Outbound, UpstreamClient, UpstreamError};

const HUB_NAME: &str = "mcp-hub-policy";
const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol revisions the hub speaks toward clients, newest first.
/// `initialize` echoes the client's requested version when listed here
/// and otherwise answers with the newest one, per MCP version negotiation.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Revision the hub requests when initializing upstreams. The upstream's
/// answer is adopted and sent as `MCP-Protocol-Version` afterwards.
const UPSTREAM_PROTOCOL_VERSION: &str = "2025-06-18";
// Static-auth and per-caller (passthrough) entries live in separate caches:
// the PDK cache evicts its oldest entries when full, so callers sending
// fresh tokens must not be able to push out the sessions and manifests
// every caller shares. Static caches hold at most two entries per server
// (session + backoff, or the three lists).
const SESSION_CACHE_ID: &str = "mcp-hub-upstream-sessions";
const SESSION_CACHE_MAX_ENTRIES: usize = 256;
const CALLER_SESSION_CACHE_ID: &str = "mcp-hub-caller-sessions";
const CALLER_SESSION_CACHE_MAX_ENTRIES: usize = 1024;
const MANIFEST_CACHE_ID: &str = "mcp-hub-upstream-manifests";
/// Kept small: the PDK cache rewrites an index of every key on each save.
const MANIFEST_CACHE_MAX_ENTRIES: usize = 256;
const MANIFEST_MAX_ENTRY_BYTES: usize = 512 * 1024;
const CALLER_MANIFEST_CACHE_ID: &str = "mcp-hub-caller-manifests";
const CALLER_MANIFEST_CACHE_MAX_ENTRIES: usize = 256;
const CALLER_MANIFEST_MAX_ENTRY_BYTES: usize = 64 * 1024;
/// Tool-name length MCP clients commonly accept (`^[a-zA-Z0-9_-]{1,64}$`).
const MAX_CLIENT_NAME_LEN: usize = 64;
/// `_meta` key listing the servers a `*/list` left out.
const SKIPPED_META_KEY: &str = "mcp-hub/skipped";

/// Resolved upstreams (one [`UpstreamClient`] per enabled MCP server)
/// plus the upstream session and manifest caches, each split into
/// static-auth and per-caller stores.
#[derive(Clone)]
struct PolicyState {
    cfg: Rc<PolicyConfig>,
    upstreams: Rc<Vec<UpstreamClient>>,
    sessions: Rc<SessionStore>,
    caller_sessions: Rc<SessionStore>,
    manifests: Rc<ManifestCache>,
    caller_manifests: Rc<ManifestCache>,
    /// Hash of the raw configuration; scopes the caches' keys.
    config_tag: Rc<str>,
}

impl PolicyState {
    /// Session store for calls with (passthrough) or without a credential.
    fn sessions_for(&self, credential: Option<&Credential>) -> &SessionStore {
        match credential {
            Some(_) => &self.caller_sessions,
            None => &self.sessions,
        }
    }

    fn manifests_for(&self, credential: Option<&Credential>) -> &ManifestCache {
        match credential {
            Some(_) => &self.caller_manifests,
            None => &self.manifests,
        }
    }

    /// Session (and manifest) cache key for `upstream` and this caller.
    fn upstream_key(&self, upstream: &UpstreamClient, credential: Option<&Credential>) -> String {
        let fingerprint = credential.map(Credential::fingerprint);
        SessionStore::key(
            &self.config_tag,
            &upstream.server.name,
            fingerprint.as_deref(),
        )
    }
}

/// Per-request inputs captured from the inbound headers before the body
/// is drained (lowercase name, value).
struct RequestCtx {
    /// The caller's values for every header some `authType=passthrough`
    /// server forwards.
    credentials: Vec<(String, String)>,
    /// The caller's values for `forwardHeaders`.
    forward: Vec<(String, String)>,
}

impl RequestCtx {
    fn from_headers(state: &PolicyState, headers: &[(String, String)]) -> Self {
        // An empty value counts as not sent.
        let present = |name: &str| {
            find_header(headers, name)
                .filter(|v| !v.trim().is_empty())
                .map(|v| (name.to_string(), v.to_string()))
        };
        let mut credentials: Vec<(String, String)> = Vec::new();
        for name in state
            .upstreams
            .iter()
            .flat_map(|u| u.server.passthrough_headers())
        {
            if !credentials.iter().any(|(k, _)| k == name) {
                credentials.extend(present(name));
            }
        }
        let forward = state
            .cfg
            .forward_headers
            .iter()
            .filter_map(|name| present(name))
            .collect();
        Self {
            credentials,
            forward,
        }
    }

    /// The caller's credential for `upstream`: `Ok(None)` for servers that
    /// don't forward one, `Err(MissingCredential)` when a passthrough
    /// server needs headers the caller didn't send (all are required).
    fn credential_for(
        &self,
        upstream: &UpstreamClient,
    ) -> Result<Option<Credential>, UpstreamError> {
        let names = upstream.server.passthrough_headers();
        if names.is_empty() {
            return Ok(None);
        }
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for name in names {
            match self.credentials.iter().find(|(k, _)| k == name) {
                Some(header) => found.push(header.clone()),
                None => missing.push(name.to_string()),
            }
        }
        if !missing.is_empty() {
            return Err(UpstreamError::MissingCredential {
                server: upstream.server.name.clone(),
                headers: missing,
            });
        }
        Ok(Some(Credential(found)))
    }

    /// Outbound headers for one call inside `session` (`None` before the
    /// handshake). `forward` adds the caller's `forwardHeaders`: only
    /// routed calls carry them, since handshakes and `*/list` answers are
    /// shared by every caller of a static-auth server (and cached without
    /// regard to those headers).
    fn outbound<'a>(
        &'a self,
        credential: Option<&'a Credential>,
        session: Option<&'a Session>,
        forward: bool,
    ) -> Outbound<'a> {
        let nonempty = |s: &'a String| Some(s.as_str()).filter(|s| !s.is_empty());
        Outbound {
            session_id: session.and_then(|s| nonempty(&s.id)),
            protocol_version: session.and_then(|s| nonempty(&s.protocol_version)),
            credential,
            forward: if forward { &self.forward } else { &[] },
        }
    }
}

/// A routed method: which field names its target and which list owns it.
struct Route {
    method: &'static str,
    key_field: &'static str,
    list: List,
    not_found: i32,
}

/// An aggregated `*/list` method and the result key holding its entries.
#[derive(Clone, Copy)]
struct List {
    method: &'static str,
    key: &'static str,
}

const TOOLS: List = List {
    method: "tools/list",
    key: "tools",
};
const RESOURCES: List = List {
    method: "resources/list",
    key: "resources",
};
const PROMPTS: List = List {
    method: "prompts/list",
    key: "prompts",
};

/// One JSON-RPC call to an upstream, as [`call_upstream`] runs it.
struct Call<'a> {
    method: &'a str,
    params: Option<Value>,
    deadline: Deadline,
    /// Send the caller's `forwardHeaders` (routed calls).
    forward: bool,
}

/// One time budget shared by every HTTP request of an upstream call (the
/// handshake, the call and a retry after an expired session).
#[derive(Clone, Copy)]
struct Deadline {
    budget: Duration,
    end_ms: u128,
}

impl Deadline {
    fn after(budget: Duration) -> Self {
        Self {
            budget,
            end_ms: now_nanos() / 1_000_000 + budget.as_millis(),
        }
    }

    /// Timeout for the next request to `server`; a transport error once
    /// the budget is spent.
    fn left(&self, server: &str) -> Result<Duration, UpstreamError> {
        let left = self.end_ms.saturating_sub(now_nanos() / 1_000_000);
        if left == 0 {
            return Err(UpstreamError::Transport {
                server: server.to_string(),
                message: format!("timed out after {} ms", self.budget.as_millis()),
            });
        }
        Ok(Duration::from_millis(left as u64))
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
        if is_metadata_path(&bare) {
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

    // MCP Streamable HTTP lets a server without server-initiated messages
    // answer the client's GET stream request with 405; clients then use
    // POST only. An SSE stream that ends at once makes clients reconnect
    // in a loop.
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

    if state.cfg.require_caller_auth
        && find_header(&headers, "authorization").is_none_or(|v| v.trim().is_empty())
    {
        return Flow::Break(auth_challenge(
            &state.cfg,
            true,
            None,
            401,
            None,
            "mcp-hub requires an Authorization header",
            None,
        ));
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

    let rpc = match parse_inbound(&body) {
        Ok(Inbound::Request(rpc)) => rpc,
        Ok(Inbound::Response) => return Flow::Break(make_response(202, b"")),
        Err(Refusal { id, code, message }) => {
            return Flow::Break(make_json_response(200, &error_response(id, code, &message)));
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
    if rpc.is_notification() {
        // Notifications are best-effort: fan out to all enabled upstreams,
        // ignore errors, return 202 Accepted (no body) per MCP Streamable
        // HTTP spec. 204 trips undici's null-body-status check in the
        // MCP Inspector proxy. A request method without an `id` (e.g.
        // `tools/call`) gets no answer and is not forwarded.
        if rpc.method.starts_with("notifications/") {
            fan_out_notification(state, client, ctx, &rpc).await;
        }
        return make_response(202, b"");
    }

    let JsonRpcRequest {
        id, method, params, ..
    } = rpc;
    let route = match McpMethod::parse(&method) {
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
            return make_json_response(200, &success_response(id.unwrap_or(Value::Null), payload));
        }
        McpMethod::Ping => {
            return make_json_response(
                200,
                &success_response(id.unwrap_or(Value::Null), serde_json::json!({})),
            )
        }
        McpMethod::ToolsList => return handle_aggregate_list(state, client, ctx, id, TOOLS).await,
        McpMethod::ResourcesList => {
            return handle_aggregate_list(state, client, ctx, id, RESOURCES).await
        }
        McpMethod::PromptsList => {
            return handle_aggregate_list(state, client, ctx, id, PROMPTS).await
        }
        McpMethod::ResourceTemplatesList => {
            return make_json_response(
                200,
                &success_response(
                    id.unwrap_or(Value::Null),
                    serde_json::json!({"resourceTemplates": []}),
                ),
            )
        }
        McpMethod::ToolsCall => Route {
            method: "tools/call",
            key_field: "name",
            list: TOOLS,
            not_found: TOOL_NOT_FOUND,
        },
        McpMethod::ResourcesRead => Route {
            method: "resources/read",
            key_field: "uri",
            list: RESOURCES,
            not_found: RESOURCE_NOT_FOUND,
        },
        McpMethod::PromptsGet => Route {
            method: "prompts/get",
            key_field: "name",
            list: PROMPTS,
            not_found: PROMPT_NOT_FOUND,
        },
        McpMethod::Other => {
            return make_json_response(
                200,
                &error_response(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("method '{method}' is not supported by mcp-hub"),
                ),
            )
        }
    };
    handle_route(state, client, ctx, id, route, params).await
}

/// Fan an aggregated `*list` call across all enabled upstreams. Slow,
/// failing, or unauthorized upstreams are logged and skipped — partial
/// degradation is preferable to a total aggregate failure, so a caller
/// without a token still sees every server that doesn't need one. With
/// `exposeUpstreamErrors` the skipped servers are named in `_meta`.
async fn handle_aggregate_list(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    id: Option<Value>,
    list: List,
) -> Response {
    let (all, skipped) = collect_list_items(state, client, ctx, list).await;

    let aggregated = match aggregate(all, state.cfg.collision_strategy) {
        Ok(a) => strip_internal(a),
        Err(AggregateError::Duplicate(name)) => {
            return make_json_response(
                200,
                &error_response_with_data(
                    id,
                    INTERNAL_ERROR,
                    &format!("duplicate {} entry '{name}' across MCP servers", list.key),
                    Some(serde_json::json!({"collisionStrategy": "reject"})),
                ),
            );
        }
    };

    let mut payload = serde_json::json!({ list.key: aggregated });
    if state.cfg.expose_upstream_errors && !skipped.is_empty() {
        let skipped: Vec<Value> = skipped
            .iter()
            .map(|(server, reason)| serde_json::json!({"server": server, "reason": reason}))
            .collect();
        payload["_meta"] = serde_json::json!({ SKIPPED_META_KEY: skipped });
    }
    make_json_response(200, &success_response(id.unwrap_or(Value::Null), payload))
}

/// Call `<kind>/list` on every enabled upstream concurrently. Returns the
/// entries of those that answered, in upstream order, and the
/// (server, reason code) of those left out — misconfigured servers last.
async fn collect_list_items<'a>(
    state: &'a PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    list: List,
) -> (Vec<UpstreamItem<'a>>, Vec<(&'a str, String)>) {
    let answers = join_all(
        state
            .upstreams
            .iter()
            .map(|upstream| list_upstream(state, client, ctx, upstream, list)),
    )
    .await;

    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for (upstream, answer) in state.upstreams.iter().zip(answers) {
        let server = upstream.server.name.as_str();
        match answer {
            Ok(values) => items.extend(
                values
                    .into_iter()
                    .map(|value| UpstreamItem { server, value }),
            ),
            Err(e) => {
                // A caller without a passthrough credential is routine.
                if matches!(e, UpstreamError::MissingCredential { .. }) {
                    logger::debug!(
                        "mcp-hub: skipping upstream '{server}' for {}: {e}",
                        list.method
                    );
                } else {
                    logger::warn!(
                        "mcp-hub: skipping upstream '{server}' for {}: {e}",
                        list.method
                    );
                }
                skipped.push((server, e.reason_code().to_string()));
            }
        }
    }
    skipped.extend(
        state
            .cfg
            .invalid_servers
            .iter()
            .map(|s| (s.name.as_str(), "misconfigured".to_string())),
    );
    (items, skipped)
}

/// One upstream's `<kind>/list` entries, from the manifest cache or the
/// upstream itself.
async fn list_upstream(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    list: List,
) -> Result<Vec<Value>, UpstreamError> {
    let credential = ctx.credential_for(upstream)?;
    let manifests = state.manifests_for(credential.as_ref());
    let cache_key = ManifestCache::key(
        list.method,
        &state.upstream_key(upstream, credential.as_ref()),
    );
    if let Some(items) = manifests.get(&cache_key) {
        return Ok(items);
    }

    let call = Call {
        method: list.method,
        params: Some(serde_json::json!({})),
        deadline: Deadline::after(Duration::from_millis(
            state.cfg.aggregation_timeout_ms as u64,
        )),
        forward: false,
    };
    let resp = call_upstream(state, client, ctx, upstream, call).await?;
    let name = &upstream.server.name;
    if let Some(err) = resp.error {
        // A server without this kind of entry (e.g. a tools-only server
        // asked for prompts) has nothing to add; cache that like a list.
        if err.code == METHOD_NOT_FOUND {
            manifests.put(&cache_key, &[]);
            return Ok(Vec::new());
        }
        return Err(UpstreamError::Rejected {
            server: name.clone(),
            method: list.method.to_string(),
            code: err.code,
            message: err.message,
        });
    }
    let Some(Value::Array(items)) = resp
        .result
        .and_then(|mut r| r.get_mut(list.key).map(Value::take))
    else {
        return Err(UpstreamError::BadJson {
            server: name.clone(),
            message: format!("{} result has no '{}' array", list.method, list.key),
        });
    };
    warn_long_names(state, upstream, list, &items);
    manifests.put(&cache_key, &items);
    Ok(items)
}

/// Warn (on each fresh answer — every request when
/// `manifestCacheTtlSeconds` is 0 — not per cached one) about entries whose
/// prefixed name exceeds what MCP clients commonly accept.
fn warn_long_names(state: &PolicyState, upstream: &UpstreamClient, list: List, items: &[Value]) {
    if state.cfg.collision_strategy != CollisionStrategy::Prefix || list.key == RESOURCES.key {
        return;
    }
    let server = &upstream.server.name;
    let long: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("name")?.as_str())
        .filter(|name| server.len() + 1 + name.len() > MAX_CLIENT_NAME_LEN)
        .collect();
    if !long.is_empty() {
        logger::warn!(
            "mcp-hub: {} '{server}_<name>' name(s) exceed {MAX_CLIENT_NAME_LEN} characters, \
             which some MCP clients reject: {}",
            list.key,
            long.join(", ")
        );
    }
}

/// Route a `tools/call` / `resources/read` / `prompts/get` to the owning
/// upstream. With the `prefix` strategy the owner of a tool or prompt is
/// parsed from its name; otherwise (and for resource URIs) the owner is
/// looked up in the aggregated manifest.
async fn handle_route(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    id: Option<Value>,
    route: Route,
    params: Option<Value>,
) -> Response {
    let Route {
        method, key_field, ..
    } = route;
    let Some(mut params) = params else {
        return make_json_response(
            200,
            &error_response(id, INVALID_PARAMS, &format!("'{method}' requires params")),
        );
    };
    let Some(requested) = params.get(key_field).and_then(|v| v.as_str()) else {
        return make_json_response(
            200,
            &error_response(
                id,
                INVALID_PARAMS,
                &format!("'{method}' params missing required '{key_field}'"),
            ),
        );
    };
    let requested = requested.to_string();

    let fast = match key_field {
        "name" => resolve_owner_fast(state, &requested),
        _ => Ok(None),
    };
    let owner = match fast {
        Ok(Some(owner)) => Some(owner),
        Ok(None) => resolve_owner_via_aggregate(state, client, ctx, &route, &requested).await,
        Err(err) => return upstream_failure_response(&state.cfg, &[], id, &err),
    };
    let Some((upstream, original_key)) = owner else {
        if let Some(err) = misconfigured_owner(state, key_field, &requested) {
            return upstream_failure_response(&state.cfg, &[], id, &err);
        }
        return make_json_response(
            200,
            &error_response(
                id,
                route.not_found,
                &format!("no MCP server owns '{key_field}' = '{requested}'"),
            ),
        );
    };

    // Rewrite the key field with the original (un-prefixed) name so the
    // upstream sees its native namespace.
    if let Some(obj) = params.as_object_mut() {
        obj.insert(key_field.into(), Value::String(original_key));
    }

    let call = Call {
        method,
        params: Some(params),
        deadline: Deadline::after(upstream_timeout(state, upstream)),
        forward: true,
    };
    match call_upstream(state, client, ctx, upstream, call).await {
        Ok(JsonRpcResponse {
            error: Some(err), ..
        }) => make_json_response(
            200,
            &error_response_with_data(id, err.code, &err.message, err.data),
        ),
        Ok(resp) => make_json_response(
            200,
            &success_response(
                id.unwrap_or(Value::Null),
                resp.result.unwrap_or(Value::Null),
            ),
        ),
        Err(e) => {
            logger::warn!(
                "mcp-hub: routed {method} to '{}' failed: {e}",
                upstream.server.name
            );
            upstream_failure_response(&state.cfg, &upstream.server.passthrough_headers(), id, &e)
        }
    }
}

/// With the `prefix` strategy, the misconfigured server a name like
/// `<server>_<tool>` was meant for, as the error to answer with.
fn misconfigured_owner(
    state: &PolicyState,
    key_field: &str,
    requested: &str,
) -> Option<UpstreamError> {
    if key_field != "name" || state.cfg.collision_strategy != CollisionStrategy::Prefix {
        return None;
    }
    let bad = state.cfg.invalid_servers.iter().find(|s| {
        requested
            .strip_prefix(s.name.as_str())
            .is_some_and(|rest| rest.starts_with('_'))
    })?;
    Some(UpstreamError::Misconfigured {
        server: bad.name.clone(),
        reason: bad.reason.clone(),
    })
}

/// One JSON-RPC call to an upstream inside its MCP session.
///
/// Resolves the caller's credential (passthrough servers), initializes the
/// upstream session on a cache miss — unless a recent handshake failed and
/// the upstream is still backed off — and, when the upstream answers 404
/// to a request that carried a session id (session expired / server
/// restarted), drops the session, re-initializes, and retries once. All
/// of it runs within `call.deadline`.
async fn call_upstream(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    call: Call<'_>,
) -> Result<JsonRpcResponse, UpstreamError> {
    let credential = ctx.credential_for(upstream)?;
    let credential = credential.as_ref();
    let sessions = state.sessions_for(credential);
    let key = state.upstream_key(upstream, credential);
    let mut retried = false;

    loop {
        // A session opened by this call is fresh: it can't be stale (no
        // re-initialize on a 404), and a timeout of the call that follows
        // says nothing about it.
        let (session, fresh) = match sessions.get(&key) {
            Some(session) => (session, false),
            None => {
                let session = open_session(
                    state,
                    client,
                    ctx,
                    upstream,
                    &key,
                    credential,
                    call.deadline,
                    retried,
                )
                .await?;
                retried = true;
                (session, true)
            }
        };
        let request = JsonRpcOutbound::new(next_rpc_id(), call.method, call.params.clone());
        let out = ctx.outbound(credential, Some(&session), call.forward);
        let timeout = call.deadline.left(&upstream.server.name)?;

        match upstream.call(client, request, timeout, &out).await {
            Ok(result) => {
                if let Some(new_sid) = result.mcp_session_id.filter(|n| *n != session.id) {
                    let session = Session {
                        id: new_sid,
                        ..session
                    };
                    sessions.put(&key, &session);
                }
                return Ok(result.response);
            }
            Err(UpstreamError::HttpStatus { status: 404, .. })
                if out.session_id.is_some() && !retried =>
            {
                logger::info!(
                    "mcp-hub: upstream '{}' rejected its session (404); re-initializing",
                    upstream.server.name
                );
                sessions.remove(&key);
                retried = true;
            }
            Err(e) => {
                // Unreachable or timed out: the next call re-runs the
                // handshake, which starts a backoff if the upstream is down.
                if !fresh && matches!(e, UpstreamError::Transport { .. }) {
                    sessions.remove(&key);
                }
                return Err(e);
            }
        }
    }
}

/// [`initialize_upstream`] behind the failure backoff: while a recent
/// handshake failure is remembered the upstream isn't contacted, and a new
/// failure of the upstream's own starts one (see [`backoff_for`]).
/// `retry` marks the handshake after an upstream rejected its session: the
/// upstream just answered, so running out of time then isn't recorded.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    state: &PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    key: &str,
    credential: Option<&Credential>,
    deadline: Deadline,
    retry: bool,
) -> Result<Session, UpstreamError> {
    let sessions = state.sessions_for(credential);
    // Backoffs are scoped by the call's whole budget, not the time left.
    let timeout_ms = deadline.budget.as_millis() as u64;
    if let Some((reason, retry_after)) = sessions.failure(key, timeout_ms) {
        return Err(UpstreamError::BackingOff {
            server: upstream.server.name.clone(),
            reason,
            retry_after,
        });
    }
    // Out of time before contacting the upstream: nothing to record.
    deadline.left(&upstream.server.name)?;
    let result = initialize_upstream(client, ctx, upstream, credential, deadline).await;
    match &result {
        Ok(session) => sessions.put(key, session),
        Err(UpstreamError::Transport { .. }) if retry => {}
        Err(e) => {
            let max = state.cfg.upstream_failure_backoff_seconds as u64;
            if let Some((seconds, scope)) = backoff_for(e, max, timeout_ms) {
                sessions.record_failure(key, e.reason_code(), seconds, scope);
            }
        }
    }
    result
}

/// How long to back off after a failed handshake, and the call timeout it
/// applies to (`None`: every call). Only failures the caller can't cause
/// count — a static-auth upstream's backoff is shared by every caller, so
/// a 4xx provoked by one caller's headers must not take it out for all;
/// 401/403 are fixed by the caller at once. A timeout or unreachable
/// upstream only backs off calls allowed as long or less, so a `*/list`
/// timeout doesn't block routed calls with a longer budget. An upstream
/// `Retry-After` shorter than `max` wins.
fn backoff_for(err: &UpstreamError, max: u64, timeout_ms: u64) -> Option<(u64, Option<u64>)> {
    match err {
        UpstreamError::Transport { .. } => Some((max, Some(timeout_ms))),
        UpstreamError::HttpStatus {
            status: 408 | 429 | 500..=599,
            retry_after,
            ..
        } => {
            let upstream = retry_after.as_deref().and_then(|r| r.trim().parse().ok());
            Some((upstream.map_or(max, |r: u64| r.min(max)), None))
        }
        UpstreamError::Rejected { .. }
        | UpstreamError::BadJson { .. }
        | UpstreamError::TooLarge { .. } => Some((max, None)),
        _ => None,
    }
}

/// Run the MCP handshake (`initialize` + `notifications/initialized`)
/// against an upstream. Returns the session it opened: the id it issued
/// (empty when none) and the protocol version it chose. `deadline` is the
/// calling request's, so a dead upstream can't hold a `*/list` fan-out
/// past `aggregationTimeoutMs`.
async fn initialize_upstream(
    client: &HttpClient,
    ctx: &RequestCtx,
    upstream: &UpstreamClient,
    credential: Option<&Credential>,
    deadline: Deadline,
) -> Result<Session, UpstreamError> {
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
        .call(
            client,
            init_request,
            deadline.left(&upstream.server.name)?,
            &ctx.outbound(credential, None, false),
        )
        .await?;
    if let Some(err) = result.response.error {
        return Err(UpstreamError::Rejected {
            server: upstream.server.name.clone(),
            method: "initialize".into(),
            code: err.code,
            message: err.message,
        });
    }
    let session = Session {
        id: result.mcp_session_id.unwrap_or_default(),
        protocol_version: result
            .response
            .result
            .as_ref()
            .and_then(|r| r.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    };

    // MCP requires `notifications/initialized` after `initialize`. Best
    // effort — many servers tolerate its absence. Not cut shorter than the
    // deadline: an aborted send may never reach the server.
    let initialized = JsonRpcOutbound::notification("notifications/initialized", None);
    let sent = match deadline.left(&upstream.server.name) {
        Ok(timeout) => {
            let out = ctx.outbound(credential, Some(&session), false);
            upstream.notify(client, initialized, timeout, &out).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = sent {
        logger::debug!(
            "mcp-hub: notifications/initialized to '{}' failed (ignored): {}",
            upstream.server.name,
            e
        );
    }
    Ok(session)
}

/// Outbound JSON-RPC id, unique per call. Static-auth upstream sessions
/// are shared by every caller (and every worker), so ids must never
/// repeat within a session — MCP forbids it, and SDK transports route
/// responses by id.
fn next_rpc_id() -> String {
    thread_local! {
        static SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// Random per worker: shared upstream sessions are used by every
        /// worker, whose counters all start at 0.
        static WORKER: u64 = {
            let mut buf = [0u8; 8];
            match getrandom::getrandom(&mut buf) {
                Ok(()) => u64::from_le_bytes(buf),
                Err(_) => now_nanos() as u64,
            }
        };
    }
    let seq = SEQ.with(|c| {
        c.set(c.get().wrapping_add(1));
        c.get()
    });
    let worker = WORKER.with(|w| *w);
    format!("hub-{worker:x}-{:x}-{seq}", now_nanos())
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn upstream_timeout(state: &PolicyState, upstream: &UpstreamClient) -> Duration {
    Duration::from_millis(
        upstream
            .server
            .timeout_ms
            .unwrap_or(state.cfg.per_request_timeout_ms) as u64,
    )
}

/// Resolve owner for the prefix-strategy fast path: the one server whose
/// `<server>_` the requested name starts with — `Err(Misconfigured)` when
/// that server is out of service. `_` can appear inside both server and
/// tool names, so when several servers match (`a` + tool `b_c` and `a_b` +
/// tool `c` are both `a_b_c`) the name is ambiguous and `Ok(None)` sends
/// the caller to the aggregated manifest, which routes to the entry
/// `tools/list` actually showed.
fn resolve_owner_fast<'a>(
    state: &'a PolicyState,
    requested: &str,
) -> Result<Option<(&'a UpstreamClient, String)>, UpstreamError> {
    if state.cfg.collision_strategy != CollisionStrategy::Prefix {
        return Ok(None);
    }
    let unprefixed = |server: &str| {
        let rest = requested.strip_prefix(server)?;
        rest.strip_prefix('_')
            .filter(|r| !r.is_empty())
            .map(str::to_string)
    };
    let mut valid = state
        .upstreams
        .iter()
        .filter_map(|u| Some((u, unprefixed(&u.server.name)?)));
    let mut invalid = state
        .cfg
        .invalid_servers
        .iter()
        .filter(|s| unprefixed(&s.name).is_some());
    match (valid.next(), invalid.next()) {
        (Some(owner), None) if valid.next().is_none() => Ok(Some(owner)),
        (None, Some(bad)) if invalid.next().is_none() => Err(UpstreamError::Misconfigured {
            server: bad.name.clone(),
            reason: bad.reason.clone(),
        }),
        _ => Ok(None),
    }
}

/// Slow-path resolver: aggregate the route's `*list` manifest (served
/// from the manifest cache when fresh) and find the entry whose key field
/// equals `requested`. Returns (owner, key as the upstream knows it). Used
/// when the prefix fast path doesn't apply: other collision strategies,
/// resource URIs, and ambiguous or unprefixed names.
async fn resolve_owner_via_aggregate<'a>(
    state: &'a PolicyState,
    client: &HttpClient,
    ctx: &RequestCtx,
    route: &Route,
    requested: &str,
) -> Option<(&'a UpstreamClient, String)> {
    let (all, _) = collect_list_items(state, client, ctx, route.list).await;
    let (server, original) = if route.key_field == "uri" {
        // Resources are de-duplicated by name, which says nothing about
        // URIs: look the URI up before collision handling, so a resource
        // dropped from the merged list as a name duplicate is still
        // readable. Upstreams are in priority order, so the first wins.
        let owner = all
            .iter()
            .find(|item| item.value.get("uri").and_then(|v| v.as_str()) == Some(requested))?;
        (owner.server.to_string(), requested.to_string())
    } else {
        let aggregated = aggregate(all, state.cfg.collision_strategy).ok()?;
        let hint = resolve_owner(&aggregated, route.key_field, requested)?;
        (hint.server, hint.original_name)
    };
    let upstream = state.upstreams.iter().find(|u| u.server.name == server)?;
    Some((upstream, original))
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
        let Ok(Some(credential)) = ctx.credential_for(upstream) else {
            return None;
        };
        let session = state
            .caller_sessions
            .get(&state.upstream_key(upstream, Some(&credential)))?;
        let notification = JsonRpcOutbound::notification(rpc.method.clone(), rpc.params.clone());
        Some(async move {
            let out = ctx.outbound(Some(&credential), Some(&session), false);
            if let Err(e) = upstream.notify(client, notification, timeout, &out).await {
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

/// Map a failed routed call to the client response. `forwarded` is the
/// upstream's passthrough headers.
///
///   * Auth failures of passthrough upstreams become real HTTP 401/403
///     with `WWW-Authenticate`, so MCP clients can start (or step up)
///     their OAuth flow or fix their headers.
///   * Upstream 429 and 5xx keep their status and `Retry-After`; an
///     upstream in handshake backoff answers 503 with the time left.
///   * Everything else — including a static-auth upstream rejecting the
///     hub's own credential, which the client can't fix, and an upstream
///     404, which would make clients drop their session — is a JSON-RPC
///     error on HTTP 200.
fn upstream_failure_response(
    cfg: &PolicyConfig,
    forwarded: &[&str],
    id: Option<Value>,
    err: &UpstreamError,
) -> Response {
    let message = err.to_string();
    let data = Some(error_data(cfg, err));
    // Only a forwarded `Authorization` is an OAuth token the hub's
    // protected-resource metadata can help the client obtain.
    let oauth = forwarded.contains(&"authorization");
    let (status, code, extra_headers) = match err {
        UpstreamError::MissingCredential { .. } => {
            return auth_challenge(cfg, oauth, None, 401, id, &message, data)
        }
        UpstreamError::HttpStatus {
            status: status @ (401 | 403),
            www_authenticate,
            ..
        } if !forwarded.is_empty() => {
            let upstream = www_authenticate.as_deref();
            return auth_challenge(cfg, oauth, upstream, *status, id, &message, data);
        }
        UpstreamError::HttpStatus {
            status: status @ (429 | 500..=599),
            retry_after,
            ..
        } => (
            *status,
            SERVER_UNAVAILABLE,
            retry_after
                .iter()
                .map(|r| ("retry-after".to_string(), r.clone()))
                .collect(),
        ),
        UpstreamError::BackingOff { retry_after, .. } => (
            503,
            SERVER_UNAVAILABLE,
            vec![("retry-after".to_string(), retry_after.to_string())],
        ),
        UpstreamError::BadJson { .. } | UpstreamError::TooLarge { .. } => {
            (200, INTERNAL_ERROR, Vec::new())
        }
        UpstreamError::HttpStatus { .. }
        | UpstreamError::Transport { .. }
        | UpstreamError::Rejected { .. }
        | UpstreamError::Misconfigured { .. } => (200, SERVER_UNAVAILABLE, Vec::new()),
    };
    make_json_response_with_headers(
        status,
        &error_response_with_data(id, code, &message, data),
        extra_headers,
    )
}

/// `error.data` for a failed routed call. The upstream's body is included
/// only with `exposeUpstreamErrors`.
fn error_data(cfg: &PolicyConfig, err: &UpstreamError) -> Value {
    let mut data = serde_json::json!({ "server": err.server(), "reason": err.reason_code() });
    match err {
        UpstreamError::MissingCredential { headers, .. } => {
            data["missingHeaders"] = serde_json::json!(headers);
        }
        UpstreamError::HttpStatus { status, body, .. } => {
            data["httpStatus"] = serde_json::json!(status);
            if cfg.expose_upstream_errors && !body.is_empty() {
                data["body"] = Value::String(body.clone());
            }
        }
        _ => {}
    }
    data
}

/// Build a 401/403 response. For OAuth upstreams (`oauth`: the caller's
/// `Authorization` is forwarded) with hub protected-resource metadata
/// configured, the challenge points at the hub's metadata (clients must
/// get tokens for the hub's own resource URL). Otherwise the upstream's
/// challenge is relayed verbatim; a non-OAuth upstream (e.g. Client ID
/// Enforcement) whose challenge is unknown gets none, so clients aren't
/// sent into an OAuth flow that can't help.
fn auth_challenge(
    cfg: &PolicyConfig,
    oauth: bool,
    upstream: Option<&str>,
    status: u32,
    id: Option<Value>,
    message: &str,
    data: Option<Value>,
) -> Response {
    let header = match (&cfg.protected_resource, upstream) {
        (Some(pr), _) if oauth && status == 403 => Some(format!(
            "Bearer error=\"insufficient_scope\", resource_metadata=\"{}\"",
            pr.metadata_url()
        )),
        (Some(pr), _) if oauth => Some(format!(
            "Bearer resource_metadata=\"{}\"",
            pr.metadata_url()
        )),
        (_, Some(upstream)) => Some(upstream.to_string()),
        (_, None) => oauth.then(|| "Bearer".to_string()),
    };
    make_json_response_with_headers(
        status,
        &error_response_with_data(id, AUTH_REQUIRED, message, data),
        header
            .map(|h| ("www-authenticate".to_string(), h))
            .into_iter()
            .collect(),
    )
}

/// True for the protected-resource metadata path: appended to the MCP
/// endpoint (`<resource>/.well-known/oauth-protected-resource`, the URL
/// the hub advertises) or in the RFC 9728 form that inserts it before the
/// resource path (`/.well-known/oauth-protected-resource/<path>`).
fn is_metadata_path(path: &str) -> bool {
    path.ends_with(WELL_KNOWN_PROTECTED_RESOURCE)
        || path
            .find(WELL_KNOWN_PROTECTED_RESOURCE)
            .is_some_and(|i| path[i + WELL_KNOWN_PROTECTED_RESOURCE.len()..].starts_with('/'))
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
    if raw.diagnostic_continue_flow == Some(true) {
        logger::warn!("mcp-hub: diagnosticContinueFlow is deprecated and ignored");
    }

    let cfg = match PolicyConfig::from_config(&raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            // Fail closed: answer every MCP request with the reason rather
            // than leave the API unprotected or serve a partial hub.
            logger::error!("mcp-hub: policy configuration rejected: {e}");
            let (endpoint, strict) = endpoint_settings(&raw);
            let body = error_response(
                None,
                SERVER_UNAVAILABLE,
                &format!("mcp-hub configuration rejected: {e}"),
            );
            let filter = on_request(move |request: RequestHeadersState| {
                let path = request.path();
                let bare = path.split_once('?').map_or(path.as_str(), |(p, _)| p);
                let flow = if path_matches(bare, &endpoint) {
                    Flow::Break(make_json_response(503, &body))
                } else if strict {
                    Flow::Break(make_text_response(404, "not found"))
                } else {
                    Flow::Continue(())
                };
                async move { flow }
            });
            launcher.launch(filter).await?;
            return Ok(());
        }
    };

    logger::info!(
        "mcp-hub: loaded {} server(s); endpoint='{}' collision={:?} strict={} manifestTtl={}s",
        cfg.servers.len(),
        cfg.mcp_endpoint,
        cfg.collision_strategy,
        cfg.strict_mode,
        cfg.manifest_cache_ttl_seconds,
    );
    for bad in &cfg.invalid_servers {
        logger::error!(
            "mcp-hub: MCP server '{}' taken out of service: {}",
            bad.name,
            bad.reason
        );
    }

    // `servers` is already in priority order; each keeps the row index of
    // its codegen-registered Service.
    let upstreams = cfg
        .servers
        .iter()
        .map(|server| {
            let service = Rc::new(raw.mcp_servers[server.index].endpoint.clone());
            UpstreamClient::new(server.clone(), service, cfg.max_upstream_response_bytes)
        })
        .collect();

    let config_tag: Rc<str> = crate::session::config_tag(&bytes).into();
    let ttl_seconds = cfg.upstream_session_ttl_seconds;
    let manifest_ttl_seconds = cfg.manifest_cache_ttl_seconds;
    let cache = |id: &str, max_entries: usize| {
        cache_builder
            .new(id.to_string())
            .max_entries(max_entries)
            .shared()
            .build()
    };
    let state = PolicyState {
        cfg: Rc::new(cfg),
        upstreams: Rc::new(upstreams),
        sessions: Rc::new(SessionStore::new(
            cache(SESSION_CACHE_ID, SESSION_CACHE_MAX_ENTRIES),
            ttl_seconds,
        )),
        caller_sessions: Rc::new(SessionStore::new(
            cache(CALLER_SESSION_CACHE_ID, CALLER_SESSION_CACHE_MAX_ENTRIES),
            ttl_seconds,
        )),
        manifests: Rc::new(ManifestCache::new(
            cache(MANIFEST_CACHE_ID, MANIFEST_CACHE_MAX_ENTRIES),
            manifest_ttl_seconds,
            MANIFEST_MAX_ENTRY_BYTES,
        )),
        caller_manifests: Rc::new(ManifestCache::new(
            cache(CALLER_MANIFEST_CACHE_ID, CALLER_MANIFEST_CACHE_MAX_ENTRIES),
            manifest_ttl_seconds,
            CALLER_MANIFEST_MAX_ENTRY_BYTES,
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

    #[test]
    fn deadline_is_shared_and_runs_out() {
        let d = Deadline::after(Duration::from_secs(10));
        let left = d.left("s").unwrap();
        assert!(left <= Duration::from_secs(10) && left > Duration::from_secs(9));
        let spent = Deadline::after(Duration::ZERO).left("s").unwrap_err();
        assert!(matches!(spent, UpstreamError::Transport { .. }));
        assert_eq!(spent.reason_code(), "unavailable");
    }

    #[test]
    fn metadata_path_forms() {
        assert!(is_metadata_path(
            "/hub/mcp/.well-known/oauth-protected-resource"
        ));
        assert!(is_metadata_path(
            "/.well-known/oauth-protected-resource/hub/mcp"
        ));
        assert!(!is_metadata_path("/.well-known/oauth-protected-resourcex"));
        assert!(!is_metadata_path("/hub/mcp"));
    }
}
