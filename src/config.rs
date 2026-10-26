//! Typed view over the policy configuration.
//!
//! `cargo anypoint config-gen` regenerates `src/generated/config.rs`
//! from `definition/gcl.yaml`. This module wraps that into a clean
//! typed `PolicyConfig` after defaulting + validating.
//!
//! Errors in one `mcpServers` entry leave only that server out (see
//! [`InvalidServer`]); errors in hub-wide settings reject the whole
//! configuration ([`ConfigError`]).

use serde::{Deserialize, Serialize};

use crate::generated::config::{Config, Headers0Config, McpServers0Config};

/// Why a policy configuration was rejected as a whole.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("mcpServers must contain at least one entry")]
    NoServers,

    #[error("too many servers ({0}); maximum is {MAX_SERVERS}")]
    TooManyServers(usize),

    #[error("unknown collisionStrategy: {0}")]
    UnknownCollisionStrategy(String),

    #[error("authorizationServers requires a non-empty 'resourceUrl'")]
    MissingResourceUrl,

    #[error("forwardHeaders: '{0}' cannot be forwarded")]
    ForwardHeader(String),
}

/// How to handle name collisions between MCP servers when aggregating
/// `tools/list`, `resources/list`, `prompts/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollisionStrategy {
    /// Rename to `<server>_<original>` (default).
    Prefix,
    /// Reject duplicate names with an MCP-compliant error.
    Reject,
    /// Keep the first server's entry, drop subsequent dupes.
    FirstWins,
    /// Overwrite with the most recently seen entry.
    LastWins,
}

impl CollisionStrategy {
    /// Parse the `collisionStrategy` config value.
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        match s {
            "prefix" => Ok(Self::Prefix),
            "reject" => Ok(Self::Reject),
            "firstWins" => Ok(Self::FirstWins),
            "lastWins" => Ok(Self::LastWins),
            other => Err(ConfigError::UnknownCollisionStrategy(other.to_string())),
        }
    }
}

/// Per-server authentication strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthConfig {
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    ApiKey {
        header_name: String,
        api_key: String,
    },
    /// Auth materials live entirely in the per-server `headers` array.
    CustomHeaders,
    /// Forward the caller's own inbound headers (lowercase names, e.g.
    /// `client_id` + `client_secret`) to this server. Without all of them,
    /// list calls skip this server and routed calls fail with HTTP 401.
    Passthrough {
        header_names: Vec<String>,
    },
}

impl AuthConfig {
    fn parse(raw: &McpServers0Config) -> Result<Self, String> {
        let kind = raw.auth_type.as_deref().unwrap_or("none");
        let required = |value: &Option<String>, field: &str| {
            nonempty(value.clone())
                .ok_or_else(|| format!("authType={kind} requires non-empty '{field}'"))
        };
        match kind {
            "none" => Ok(Self::None),
            "bearerToken" => Ok(Self::Bearer {
                token: required(&raw.token, "token")?,
            }),
            "basicAuth" => Ok(Self::Basic {
                username: required(&raw.username, "username")?,
                password: required(&raw.password, "password")?,
            }),
            "apiKeyHeader" => Ok(Self::ApiKey {
                header_name: required(&raw.header_name, "headerName")?,
                api_key: required(&raw.api_key, "apiKey")?,
            }),
            "customHeaders" => Ok(Self::CustomHeaders),
            "passthrough" => {
                let header_names = passthrough_names(raw);
                if let Some(name) = header_names.iter().find(|n| is_reserved(n)) {
                    return Err(format!("headerName '{name}' cannot be passed through"));
                }
                Ok(Self::Passthrough { header_names })
            }
            other => Err(format!("unknown authType '{other}'")),
        }
    }
}

/// `headerName` of a passthrough entry as lowercase names, deduplicated;
/// `authorization` when empty. Comma-separated: header names can't
/// contain commas.
fn passthrough_names(raw: &McpServers0Config) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in raw.header_name.as_deref().unwrap_or("").split(',') {
        let name = name.trim().to_ascii_lowercase();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        names.push("authorization".into());
    }
    names
}

/// Lowercase names of the headers that carry an `mcpServers` entry's
/// credential, read from the raw entry so invalid entries count too.
fn credential_header_names(raw: &McpServers0Config) -> Vec<String> {
    match raw.auth_type.as_deref().unwrap_or("none") {
        "bearerToken" | "basicAuth" => vec!["authorization".into()],
        "apiKeyHeader" => raw
            .header_name
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect(),
        "passthrough" => passthrough_names(raw),
        "customHeaders" => raw
            .headers
            .iter()
            .flatten()
            .map(|h| h.name.trim().to_ascii_lowercase())
            .collect(),
        _ => Vec::new(),
    }
}

/// Single configured (enabled, valid) MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    /// Position in `mcpServers`, which pairs the server with the Service
    /// the generated code registered for its endpoint.
    pub index: usize,
    pub auth: AuthConfig,
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: Option<u32>,
    pub priority: u32,
    /// Path on the registered upstream Service. Extracted from the
    /// configured endpoint URL so request builders can target the right
    /// path on the registered authority.
    pub path: String,
}

impl McpServerConfig {
    /// Inbound headers forwarded to this server when it uses
    /// `authType=passthrough`, minus those a configured `headers` entry of
    /// the same name overrides (configuration wins over the caller).
    /// Empty for every other auth type.
    pub fn passthrough_headers(&self) -> Vec<&str> {
        let AuthConfig::Passthrough { header_names } = &self.auth else {
            return Vec::new();
        };
        header_names
            .iter()
            .map(String::as_str)
            .filter(|name| !self.has_header(name))
            .collect()
    }

    /// True when the server's own `headers` set `name` (any case).
    pub fn has_header(&self, name: &str) -> bool {
        self.extra_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }
}

/// An enabled `mcpServers` entry left out because its settings are
/// invalid. The reason names fields, never their values.
#[derive(Debug, Clone)]
pub struct InvalidServer {
    /// Server name, or `mcpServers[<i>]` when the name is empty.
    pub name: String,
    pub reason: String,
}

/// Validated policy configuration, built once in `configure` from the
/// raw GCL config.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub mcp_endpoint: String,
    pub strict_mode: bool,
    pub collision_strategy: CollisionStrategy,
    pub aggregation_timeout_ms: u32,
    pub per_request_timeout_ms: u32,
    /// How long each upstream's `*/list` answer is reused. `0` disables
    /// the manifest cache.
    pub manifest_cache_ttl_seconds: u32,
    pub upstream_session_ttl_seconds: u32,
    /// How long an upstream whose handshake failed is skipped. `0` = never.
    pub upstream_failure_backoff_seconds: u32,
    /// Allowed inbound `Origin` values. Empty = no Origin check.
    pub allowed_origins: Vec<String>,
    /// Lowercase inbound headers copied to every upstream call.
    pub forward_headers: Vec<String>,
    /// Report skipped servers and upstream error bodies to callers.
    pub expose_upstream_errors: bool,
    /// Largest upstream response body the hub parses.
    pub max_upstream_response_bytes: usize,
    /// Answer MCP POSTs without `Authorization` with 401.
    pub require_caller_auth: bool,
    /// RFC 9728 protected-resource metadata. `None` when no
    /// `authorizationServers` are configured.
    pub protected_resource: Option<ProtectedResource>,
    /// Enabled, valid servers in priority order.
    pub servers: Vec<McpServerConfig>,
    /// Enabled servers left out because their entry is invalid.
    pub invalid_servers: Vec<InvalidServer>,
}

/// RFC 9728 protected-resource settings, used for the metadata
/// document and `WWW-Authenticate` challenges.
#[derive(Debug, Clone)]
pub struct ProtectedResource {
    pub resource_url: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<String>,
}

impl ProtectedResource {
    /// URL of the metadata document, advertised in `WWW-Authenticate`.
    pub fn metadata_url(&self) -> String {
        format!("{}{WELL_KNOWN_PROTECTED_RESOURCE}", self.resource_url)
    }
}

/// Transport / session headers the hub sets itself (plus hop-by-hop
/// headers); letting callers supply them would allow session or request
/// smuggling. HTTP/2 pseudo-headers (`:authority`, ...) are reserved too.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
];

/// Credential headers that `forwardHeaders` may never copy to every
/// upstream (on top of [`RESERVED_HEADERS`] and each server's auth headers).
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie"];

/// `forwardHeaders` when the field is not set.
const DEFAULT_FORWARD_HEADERS: &[&str] = &["traceparent", "tracestate", "x-correlation-id"];

fn is_reserved(name: &str) -> bool {
    name.starts_with(':') || RESERVED_HEADERS.contains(&name)
}

/// Path suffix the hub serves its protected-resource metadata on.
pub const WELL_KNOWN_PROTECTED_RESOURCE: &str = "/.well-known/oauth-protected-resource";

/// Upper bound on configured upstream MCP servers.
pub const MAX_SERVERS: usize = 50;

impl PolicyConfig {
    /// Validate the raw config: defaults, clamps, auth fields, unique
    /// names and the [`MAX_SERVERS`] limit. Disabled servers are ignored.
    pub fn from_config(raw: &Config) -> Result<Self, ConfigError> {
        if raw.mcp_servers.is_empty() {
            return Err(ConfigError::NoServers);
        }
        if raw.mcp_servers.len() > MAX_SERVERS {
            return Err(ConfigError::TooManyServers(raw.mcp_servers.len()));
        }

        let collision_strategy =
            CollisionStrategy::parse(raw.collision_strategy.as_deref().unwrap_or("prefix"))?;
        let protected_resource = parse_protected_resource(raw)?;

        let enabled: Vec<(usize, &McpServers0Config)> = raw
            .mcp_servers
            .iter()
            .enumerate()
            .filter(|(_, row)| row.enabled.unwrap_or(true))
            .collect();
        let mut servers = Vec::with_capacity(enabled.len());
        let mut invalid_servers = Vec::new();
        for &(index, row) in &enabled {
            let name = row.name.trim();
            let duplicate = enabled
                .iter()
                .filter(|(_, other)| other.name.trim() == name)
                .count()
                > 1;
            let parsed = if name.is_empty() {
                Err("missing/empty 'name'".to_string())
            } else if !valid_server_name(name) {
                // Names prefix tool names, which MCP clients limit to this
                // charset; `|` would also break cache keys.
                Err("'name' may only contain letters, digits, '_' and '-'".to_string())
            } else if duplicate {
                Err("duplicate server name".to_string())
            } else {
                parse_server(index, row)
            };
            match parsed {
                Ok(server) => servers.push(server),
                Err(reason) => invalid_servers.push(InvalidServer {
                    name: if name.is_empty() {
                        format!("mcpServers[{index}]")
                    } else {
                        name.to_string()
                    },
                    reason,
                }),
            }
        }
        servers.sort_by_key(|s| s.priority);

        let (mcp_endpoint, strict_mode) = endpoint_settings(raw);
        Ok(Self {
            mcp_endpoint,
            strict_mode,
            collision_strategy,
            aggregation_timeout_ms: clamp_u32(raw.aggregation_timeout_ms, 100, 60_000, 5_000),
            per_request_timeout_ms: clamp_u32(raw.per_request_timeout_ms, 100, 600_000, 30_000),
            manifest_cache_ttl_seconds: clamp_u32(raw.manifest_cache_ttl_seconds, 0, 86_400, 60),
            upstream_session_ttl_seconds: clamp_u32(
                raw.upstream_session_ttl_seconds,
                0,
                86_400,
                1_800,
            ),
            upstream_failure_backoff_seconds: clamp_u32(
                raw.upstream_failure_backoff_seconds,
                0,
                3_600,
                10,
            ),
            allowed_origins: nonempty_list(raw.allowed_origins.as_deref()),
            forward_headers: parse_forward_headers(
                raw.forward_headers.as_deref(),
                enabled.iter().map(|(_, row)| *row),
            )?,
            expose_upstream_errors: raw.expose_upstream_errors.unwrap_or(false),
            max_upstream_response_bytes: clamp_u32(
                raw.max_upstream_response_bytes,
                1 << 20,
                64 << 20,
                8 << 20,
            ) as usize,
            require_caller_auth: raw.require_caller_auth.unwrap_or(false),
            protected_resource,
            servers,
            invalid_servers,
        })
    }
}

fn parse_protected_resource(raw: &Config) -> Result<Option<ProtectedResource>, ConfigError> {
    let authorization_servers = nonempty_list(raw.authorization_servers.as_deref());
    if authorization_servers.is_empty() {
        return Ok(None);
    }
    let resource_url = nonempty(raw.resource_url.clone())
        .ok_or(ConfigError::MissingResourceUrl)?
        .trim()
        .trim_end_matches('/')
        .to_string();
    Ok(Some(ProtectedResource {
        resource_url,
        authorization_servers,
        scopes_supported: nonempty_list(raw.scopes_supported.as_deref()),
    }))
}

/// One enabled `mcpServers` entry; `Err` holds the reason it is invalid.
fn parse_server(index: usize, row: &McpServers0Config) -> Result<McpServerConfig, String> {
    let server = McpServerConfig {
        name: row.name.trim().to_string(),
        index,
        auth: AuthConfig::parse(row)?,
        extra_headers: parse_headers(row.headers.as_deref())?,
        timeout_ms: row
            .timeout_ms
            .filter(|v| (100..=600_000).contains(v))
            .map(|v| v as u32),
        priority: row
            .priority
            .filter(|v| (0..=100).contains(v))
            .map(|v| v as u32)
            .unwrap_or(50),
        // The codegen registers the URL as a Service (host only); requests
        // need the path component to reach the MCP endpoint.
        path: extract_path(row.endpoint.uri().to_string().as_str()),
    };
    // Overriding every forwarded header silently turns passthrough into
    // static auth (one session shared by all callers, no 401s).
    if matches!(server.auth, AuthConfig::Passthrough { .. })
        && server.passthrough_headers().is_empty()
    {
        return Err(
            "'headers' override every passthrough header; use authType customHeaders".into(),
        );
    }
    Ok(server)
}

/// `forwardHeaders`, checked against the credential headers of every
/// enabled entry (valid or not).
fn parse_forward_headers<'a>(
    raw: Option<&[String]>,
    rows: impl Iterator<Item = &'a McpServers0Config>,
) -> Result<Vec<String>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_FORWARD_HEADERS
            .iter()
            .map(|h| h.to_string())
            .collect());
    };
    let credentials: Vec<String> = rows.flat_map(credential_header_names).collect();
    let mut out: Vec<String> = Vec::new();
    for name in raw {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || out.contains(&name) {
            continue;
        }
        if is_reserved(&name)
            || CREDENTIAL_HEADERS.contains(&name.as_str())
            || credentials.contains(&name)
        {
            return Err(ConfigError::ForwardHeader(name));
        }
        out.push(name);
    }
    Ok(out)
}

fn parse_headers(rows: Option<&[Headers0Config]>) -> Result<Vec<(String, String)>, String> {
    let Some(rows) = rows else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let name = row.name.trim();
        if name.is_empty() {
            return Err(format!("headers[{i}]: missing/empty 'name'"));
        }
        if is_reserved(&name.to_ascii_lowercase()) {
            return Err(format!("headers[{i}]: '{name}' is set by the hub"));
        }
        out.push((name.to_string(), row.value.clone()));
    }
    Ok(out)
}

fn valid_server_name(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn nonempty_list(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `(mcpEndpoint, strictMode)` with defaults applied. Needed even when the
/// rest of the configuration is rejected, to scope the fail-closed answer.
pub fn endpoint_settings(raw: &Config) -> (String, bool) {
    (
        normalize_path(raw.mcp_endpoint.as_deref().unwrap_or("/mcp")),
        raw.strict_mode.unwrap_or(true),
    )
}

fn normalize_path(s: &str) -> String {
    let mut s = s.trim().to_string();
    if s.is_empty() {
        return "/mcp".into();
    }
    if !s.starts_with('/') {
        s = format!("/{s}");
    }
    s
}

fn clamp_u32(v: Option<i64>, min: i64, max: i64, default: i64) -> u32 {
    let v = v.unwrap_or(default).clamp(min, max);
    v as u32
}

/// Extract the path component from an absolute URL. Falls back to `/`
/// when the URL is host-only or unparseable. Always begins with `/`.
fn extract_path(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match after_scheme.find('/') {
        Some(idx) => {
            let p = &after_scheme[idx..];
            // Drop any query or fragment — the policy issues path-only requests.
            let p = p.split_once('?').map(|(p, _)| p).unwrap_or(p);
            let p = p.split_once('#').map(|(p, _)| p).unwrap_or(p);
            if p.is_empty() {
                "/".into()
            } else {
                p.to_string()
            }
        }
        None => "/".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_strategy_parses() {
        assert_eq!(
            CollisionStrategy::parse("prefix").unwrap(),
            CollisionStrategy::Prefix
        );
        assert_eq!(
            CollisionStrategy::parse("firstWins").unwrap(),
            CollisionStrategy::FirstWins
        );
        assert!(CollisionStrategy::parse("nope").is_err());
    }

    #[test]
    fn extract_path_handles_common_shapes() {
        assert_eq!(extract_path("https://h.example.com/mcp"), "/mcp");
        assert_eq!(
            extract_path("https://h.example.com/foo/bar?x=1"),
            "/foo/bar"
        );
        assert_eq!(extract_path("https://h.example.com"), "/");
        assert_eq!(extract_path("https://h.example.com/"), "/");
    }

    #[test]
    fn normalize_path_keeps_leading_slash() {
        assert_eq!(normalize_path("/mcp"), "/mcp");
        assert_eq!(normalize_path("mcp"), "/mcp");
        assert_eq!(normalize_path(""), "/mcp");
    }

    fn parse(extra: serde_json::Value) -> Result<PolicyConfig, ConfigError> {
        let mut v = serde_json::json!({
            "mcpServers": [{
                "name": "erp",
                "endpoint": "http://erp.test/mcp",
                "enabled": true,
                "authType": "passthrough"
            }]
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        let raw: Config = serde_json::from_value(v).unwrap();
        PolicyConfig::from_config(&raw)
    }

    #[test]
    fn passthrough_defaults_to_authorization_header() {
        let cfg = parse(serde_json::json!({})).unwrap();
        assert_eq!(cfg.servers[0].passthrough_headers(), vec!["authorization"]);
        assert_eq!(cfg.upstream_session_ttl_seconds, 1_800);
        assert!(cfg.allowed_origins.is_empty());
        assert!(cfg.protected_resource.is_none());
    }

    #[test]
    fn passthrough_header_name_is_lowercased() {
        let raw: Config = serde_json::from_value(serde_json::json!({
            "mcpServers": [{
                "name": "erp", "endpoint": "http://erp.test/mcp", "enabled": true,
                "authType": "passthrough", "headerName": "X-User-Token"
            }]
        }))
        .unwrap();
        let cfg = PolicyConfig::from_config(&raw).unwrap();
        assert_eq!(cfg.servers[0].passthrough_headers(), vec!["x-user-token"]);
    }

    #[test]
    fn passthrough_header_name_accepts_a_list() {
        let raw: Config = serde_json::from_value(serde_json::json!({
            "mcpServers": [{
                "name": "erp", "endpoint": "http://erp.test/mcp", "enabled": true,
                "authType": "passthrough", "headerName": " Client_ID, client_secret ,,client_id"
            }]
        }))
        .unwrap();
        let cfg = PolicyConfig::from_config(&raw).unwrap();
        assert_eq!(
            cfg.servers[0].passthrough_headers(),
            vec!["client_id", "client_secret"]
        );
    }

    #[test]
    fn passthrough_rejects_transport_headers() {
        for name in ["mcp-session-id", "Host", "content-type", "client_id, host"] {
            let raw: Config = serde_json::from_value(serde_json::json!({
                "mcpServers": [{
                    "name": "erp", "endpoint": "http://erp.test/mcp", "enabled": true,
                    "authType": "passthrough", "headerName": name
                }]
            }))
            .unwrap();
            let cfg = PolicyConfig::from_config(&raw).unwrap();
            assert!(cfg.servers.is_empty(), "{name}");
            assert!(cfg.invalid_servers[0]
                .reason
                .contains("cannot be passed through"));
        }
    }

    fn servers(list: serde_json::Value) -> PolicyConfig {
        let raw: Config =
            serde_json::from_value(serde_json::json!({ "mcpServers": list })).unwrap();
        PolicyConfig::from_config(&raw).unwrap()
    }

    #[test]
    fn invalid_server_is_isolated() {
        let cfg = servers(serde_json::json!([
            {"name": "ok", "endpoint": "http://ok.test/mcp", "authType": "none"},
            {"name": "bad", "endpoint": "http://bad.test/mcp", "authType": "bearerToken"},
            {"name": " ", "endpoint": "http://x.test/mcp"},
            {"name": "off", "endpoint": "http://off.test/mcp", "enabled": false, "authType": "bearerToken"}
        ]));
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "ok");
        assert_eq!(cfg.servers[0].index, 0);
        let invalid: Vec<_> = cfg
            .invalid_servers
            .iter()
            .map(|s| (s.name.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            invalid,
            vec![
                ("bad", "authType=bearerToken requires non-empty 'token'"),
                ("mcpServers[2]", "missing/empty 'name'"),
            ]
        );
    }

    #[test]
    fn duplicate_names_isolate_every_copy() {
        let cfg = servers(serde_json::json!([
            {"name": "a", "endpoint": "http://a1.test/mcp"},
            {"name": "a ", "endpoint": "http://a2.test/mcp"},
            {"name": "b", "endpoint": "http://b.test/mcp"},
            {"name": "b", "endpoint": "http://b2.test/mcp", "enabled": false}
        ]));
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "b");
        assert_eq!(cfg.invalid_servers.len(), 2);
        assert!(cfg.invalid_servers.iter().all(|s| s.name == "a"));
    }

    #[test]
    fn overriding_every_passthrough_header_is_invalid() {
        let cfg = servers(serde_json::json!([{
            "name": "esc", "endpoint": "http://esc.test/mcp", "authType": "passthrough",
            "headerName": "client_id, client_secret",
            "headers": [{"name": "Client_Id", "value": "x"}, {"name": "client_secret", "value": "y"}]
        }]));
        assert!(cfg.servers.is_empty());
        assert!(cfg.invalid_servers[0].reason.contains("customHeaders"));

        let cfg = servers(serde_json::json!([{
            "name": "esc", "endpoint": "http://esc.test/mcp", "authType": "passthrough",
            "headerName": "client_id, client_secret",
            "headers": [{"name": "client_id", "value": "x"}]
        }]));
        assert_eq!(cfg.servers[0].passthrough_headers(), vec!["client_secret"]);
    }

    #[test]
    fn forward_headers_default_and_validation() {
        let cfg = parse(serde_json::json!({})).unwrap();
        assert_eq!(
            cfg.forward_headers,
            vec!["traceparent", "tracestate", "x-correlation-id"]
        );
        let cfg = parse(serde_json::json!({"forwardHeaders": []})).unwrap();
        assert!(cfg.forward_headers.is_empty());
        let cfg = parse(serde_json::json!({"forwardHeaders": [" X-Request-Id ", "x-request-id"]}))
            .unwrap();
        assert_eq!(cfg.forward_headers, vec!["x-request-id"]);
        // `erp` passes `authorization` through; reserved and credential
        // headers are never forwarded to every upstream.
        for name in [
            "Authorization",
            "cookie",
            "host",
            ":authority",
            "mcp-session-id",
        ] {
            assert!(
                matches!(
                    parse(serde_json::json!({"forwardHeaders": [name]})),
                    Err(ConfigError::ForwardHeader(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn forward_headers_cannot_carry_a_server_credential() {
        let raw: Config = serde_json::from_value(serde_json::json!({
            "forwardHeaders": ["X-Api-Key"],
            "mcpServers": [{
                "name": "k", "endpoint": "http://k.test/mcp",
                "authType": "apiKeyHeader", "headerName": "x-api-key", "apiKey": "s"
            }]
        }))
        .unwrap();
        assert!(matches!(
            PolicyConfig::from_config(&raw),
            Err(ConfigError::ForwardHeader(_))
        ));
    }

    #[test]
    fn protected_resource_requires_resource_url() {
        let err = parse(serde_json::json!({
            "authorizationServers": ["https://idp.test"]
        }));
        assert!(matches!(err, Err(ConfigError::MissingResourceUrl)));

        let cfg = parse(serde_json::json!({
            "authorizationServers": ["https://idp.test"],
            "resourceUrl": " https://hub.test/mcp/ "
        }))
        .unwrap();
        let pr = cfg.protected_resource.unwrap();
        assert_eq!(pr.resource_url, "https://hub.test/mcp");
        assert_eq!(
            pr.metadata_url(),
            "https://hub.test/mcp/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn server_names_are_limited_to_the_tool_name_charset() {
        let cfg = servers(serde_json::json!([
            {"name": "ok_1-a", "endpoint": "http://ok.test/mcp"},
            {"name": "a|b", "endpoint": "http://ab.test/mcp"},
            {"name": "my server", "endpoint": "http://ms.test/mcp"},
            {"name": "dotted.name", "endpoint": "http://dn.test/mcp"}
        ]));
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "ok_1-a");
        let invalid: Vec<_> = cfg
            .invalid_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(invalid, vec!["a|b", "my server", "dotted.name"]);
        assert!(cfg.invalid_servers[0].reason.contains("letters, digits"));
    }

    #[test]
    fn response_cap_and_caller_auth_defaults() {
        let cfg = parse(serde_json::json!({})).unwrap();
        assert_eq!(cfg.max_upstream_response_bytes, 8 << 20);
        assert!(!cfg.require_caller_auth);
        let cfg = parse(serde_json::json!({"maxUpstreamResponseBytes": 1})).unwrap();
        assert_eq!(cfg.max_upstream_response_bytes, 1 << 20);
        let cfg = parse(serde_json::json!({"maxUpstreamResponseBytes": i64::MAX})).unwrap();
        assert_eq!(cfg.max_upstream_response_bytes, 64 << 20);
    }

    #[test]
    fn session_ttl_is_clamped() {
        let cfg = parse(serde_json::json!({"upstreamSessionTtlSeconds": 999_999})).unwrap();
        assert_eq!(cfg.upstream_session_ttl_seconds, 86_400);
        let cfg = parse(serde_json::json!({"upstreamSessionTtlSeconds": 0})).unwrap();
        assert_eq!(cfg.upstream_session_ttl_seconds, 0);
    }

    #[test]
    fn server_count_is_capped_at_max() {
        let servers = |n: usize| -> Config {
            let list: Vec<_> = (0..n)
                .map(|i| {
                    serde_json::json!({
                        "name": format!("s{i}"), "endpoint": format!("http://s{i}.test/mcp"),
                        "enabled": true, "authType": "none"
                    })
                })
                .collect();
            serde_json::from_value(serde_json::json!({ "mcpServers": list })).unwrap()
        };
        assert_eq!(
            PolicyConfig::from_config(&servers(MAX_SERVERS))
                .unwrap()
                .servers
                .len(),
            MAX_SERVERS
        );
        assert!(matches!(
            PolicyConfig::from_config(&servers(MAX_SERVERS + 1)),
            Err(ConfigError::TooManyServers(_))
        ));
    }

    #[test]
    fn forward_headers_check_invalid_and_custom_header_servers() {
        let with = |servers: serde_json::Value| {
            let cfg = serde_json::json!({
                "forwardHeaders": ["x-user-token"],
                "mcpServers": servers,
            });
            PolicyConfig::from_config(&serde_json::from_value(cfg).unwrap())
        };
        // Both copies are isolated as duplicates, but still forward the header.
        let dup = serde_json::json!({"name": "erp", "endpoint": "http://erp.test/mcp",
            "authType": "passthrough", "headerName": "x-user-token"});
        assert!(matches!(
            with(serde_json::json!([dup, dup])),
            Err(ConfigError::ForwardHeader(_))
        ));
        let custom = serde_json::json!([{"name": "erp", "endpoint": "http://erp.test/mcp",
            "authType": "customHeaders", "headers": [{"name": "X-User-Token", "value": "s"}]}]);
        assert!(matches!(with(custom), Err(ConfigError::ForwardHeader(_))));
    }

    #[test]
    fn reserved_extra_header_isolates_the_server() {
        let cfg = serde_json::json!({"mcpServers": [
            {"name": "ok", "endpoint": "http://ok.test/mcp"},
            {"name": "bad", "endpoint": "http://bad.test/mcp",
             "headers": [{"name": "MCP-Session-Id", "value": "x"}]}
        ]});
        let cfg = PolicyConfig::from_config(&serde_json::from_value(cfg).unwrap()).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.invalid_servers[0].name, "bad");
        assert!(cfg.invalid_servers[0].reason.contains("set by the hub"));
    }
}
