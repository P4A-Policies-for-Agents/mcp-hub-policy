//! Typed view over the policy configuration.
//!
//! `cargo anypoint config-gen` regenerates `src/generated/config.rs`
//! from `definition/gcl.yaml`. This module wraps that into a clean
//! typed `PolicyConfig` after defaulting + validating.

use serde::{Deserialize, Serialize};

use crate::generated::config::{Config, Headers0Config, McpServers0Config};

/// Why a policy configuration was rejected at startup.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("mcpServers must contain at least one entry")]
    NoServers,

    #[error("mcpServers rejected: {0}")]
    ServersInvalid(String),

    #[error("unknown collisionStrategy: {0}")]
    UnknownCollisionStrategy(String),

    #[error("unknown authType: {0}")]
    UnknownAuthType(String),

    #[error("authorizationServers requires a non-empty 'resourceUrl'")]
    MissingResourceUrl,
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
    /// Forward the caller's own inbound header (lowercase name) to this
    /// server. Without it, list calls skip this server and routed calls
    /// fail with HTTP 401.
    Passthrough {
        header_name: String,
    },
}

impl AuthConfig {
    fn parse(server_name: &str, raw: &McpServers0Config) -> Result<Self, ConfigError> {
        let kind = raw.auth_type.as_deref().unwrap_or("none");
        match kind {
            "none" => Ok(Self::None),
            "bearerToken" => {
                let token = nonempty(raw.token.clone()).ok_or_else(|| {
                    ConfigError::ServersInvalid(format!(
                        "server '{server_name}': authType=bearerToken requires non-empty 'token'"
                    ))
                })?;
                Ok(Self::Bearer { token })
            }
            "basicAuth" => {
                let username = nonempty(raw.username.clone()).ok_or_else(|| {
                    ConfigError::ServersInvalid(format!(
                        "server '{server_name}': authType=basicAuth requires non-empty 'username'"
                    ))
                })?;
                let password = nonempty(raw.password.clone()).ok_or_else(|| {
                    ConfigError::ServersInvalid(format!(
                        "server '{server_name}': authType=basicAuth requires non-empty 'password'"
                    ))
                })?;
                Ok(Self::Basic { username, password })
            }
            "apiKeyHeader" => {
                let header_name = nonempty(raw.header_name.clone()).ok_or_else(|| {
                    ConfigError::ServersInvalid(format!(
                        "server '{server_name}': authType=apiKeyHeader requires non-empty 'headerName'"
                    ))
                })?;
                let api_key = nonempty(raw.api_key.clone()).ok_or_else(|| {
                    ConfigError::ServersInvalid(format!(
                        "server '{server_name}': authType=apiKeyHeader requires non-empty 'apiKey'"
                    ))
                })?;
                Ok(Self::ApiKey {
                    header_name,
                    api_key,
                })
            }
            "customHeaders" => Ok(Self::CustomHeaders),
            "passthrough" => {
                let header_name = nonempty(raw.header_name.clone())
                    .map(|h| h.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "authorization".into());
                if RESERVED_PASSTHROUGH_HEADERS.contains(&header_name.as_str()) {
                    return Err(ConfigError::ServersInvalid(format!(
                        "server '{server_name}': headerName '{header_name}' cannot be passed through"
                    )));
                }
                Ok(Self::Passthrough { header_name })
            }
            other => Err(ConfigError::UnknownAuthType(other.to_string())),
        }
    }
}

/// Single configured MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
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
    /// Inbound header forwarded to this server, if it uses
    /// `authType=passthrough` and no configured `headers` entry of the same
    /// name overrides it (configuration wins over the caller).
    pub fn passthrough_header(&self) -> Option<&str> {
        let AuthConfig::Passthrough { header_name } = &self.auth else {
            return None;
        };
        let overridden = self
            .extra_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(header_name));
        (!overridden).then_some(header_name.as_str())
    }
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
    /// Allowed inbound `Origin` values. Empty = no Origin check.
    pub allowed_origins: Vec<String>,
    /// RFC 9728 protected-resource metadata. `None` when no
    /// `authorizationServers` are configured.
    pub protected_resource: Option<ProtectedResource>,
    pub servers: Vec<McpServerConfig>,
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

/// Transport / session headers the hub sets itself; letting callers
/// supply them would allow session or request smuggling.
const RESERVED_PASSTHROUGH_HEADERS: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
];

/// Path suffix the hub serves its protected-resource metadata on.
pub const WELL_KNOWN_PROTECTED_RESOURCE: &str = "/.well-known/oauth-protected-resource";

/// Upper bound on configured upstream MCP servers.
pub const MAX_SERVERS: usize = 50;

impl PolicyConfig {
    /// Validate the raw config: defaults, clamps, auth fields, unique
    /// names and the [`MAX_SERVERS`] limit.
    pub fn from_config(raw: &Config) -> Result<Self, ConfigError> {
        if raw.mcp_servers.is_empty() {
            return Err(ConfigError::NoServers);
        }
        if raw.mcp_servers.len() > MAX_SERVERS {
            return Err(ConfigError::ServersInvalid(format!(
                "too many servers ({}) — maximum is {MAX_SERVERS}",
                raw.mcp_servers.len()
            )));
        }

        let collision_strategy =
            CollisionStrategy::parse(raw.collision_strategy.as_deref().unwrap_or("prefix"))?;
        let mcp_endpoint = normalize_path(raw.mcp_endpoint.as_deref().unwrap_or("/mcp"));
        let strict_mode = raw.strict_mode.unwrap_or(true);
        let aggregation_timeout_ms = clamp_u32(raw.aggregation_timeout_ms, 100, 60_000, 5_000);
        let per_request_timeout_ms = clamp_u32(raw.per_request_timeout_ms, 100, 600_000, 30_000);
        let manifest_cache_ttl_seconds = clamp_u32(raw.manifest_cache_ttl_seconds, 0, 86_400, 60);
        let upstream_session_ttl_seconds =
            clamp_u32(raw.upstream_session_ttl_seconds, 0, 86_400, 1_800);
        let allowed_origins = nonempty_list(raw.allowed_origins.as_deref());
        let authorization_servers = nonempty_list(raw.authorization_servers.as_deref());
        let protected_resource = if authorization_servers.is_empty() {
            None
        } else {
            let resource_url = nonempty(raw.resource_url.clone())
                .ok_or(ConfigError::MissingResourceUrl)?
                .trim()
                .trim_end_matches('/')
                .to_string();
            Some(ProtectedResource {
                resource_url,
                authorization_servers,
                scopes_supported: nonempty_list(raw.scopes_supported.as_deref()),
            })
        };

        let mut servers = Vec::with_capacity(raw.mcp_servers.len());
        for (i, row) in raw.mcp_servers.iter().enumerate() {
            let name = row.name.trim();
            if name.is_empty() {
                return Err(ConfigError::ServersInvalid(format!(
                    "entry {i}: missing/empty 'name'"
                )));
            }
            if servers.iter().any(|s: &McpServerConfig| s.name == name) {
                return Err(ConfigError::ServersInvalid(format!(
                    "duplicate server name '{name}'"
                )));
            }
            let auth = AuthConfig::parse(name, row)?;
            let extra_headers = parse_headers(name, row.headers.as_deref())?;
            let timeout_ms = row
                .timeout_ms
                .filter(|v| (100..=600_000).contains(v))
                .map(|v| v as u32);
            let priority = row
                .priority
                .filter(|v| (0..=100).contains(v))
                .map(|v| v as u32)
                .unwrap_or(50);
            // Extract the path from the configured endpoint URL. The
            // codegen registers the URL as a Service (host-only); the
            // request builder needs the path component to reach the MCP
            // RPC entrypoint.
            let path = extract_path(row.endpoint.uri().to_string().as_str());

            servers.push(McpServerConfig {
                name: name.to_string(),
                enabled: row.enabled.unwrap_or(true),
                auth,
                extra_headers,
                timeout_ms,
                priority,
                path,
            });
        }

        servers.sort_by_key(|s| s.priority);

        Ok(Self {
            mcp_endpoint,
            strict_mode,
            collision_strategy,
            aggregation_timeout_ms,
            per_request_timeout_ms,
            manifest_cache_ttl_seconds,
            upstream_session_ttl_seconds,
            allowed_origins,
            protected_resource,
            servers,
        })
    }
}

fn parse_headers(
    server_name: &str,
    rows: Option<&[Headers0Config]>,
) -> Result<Vec<(String, String)>, ConfigError> {
    let Some(rows) = rows else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let name = row.name.trim();
        if name.is_empty() {
            return Err(ConfigError::ServersInvalid(format!(
                "server '{server_name}' header[{i}]: missing/empty 'name'"
            )));
        }
        out.push((name.to_string(), row.value.clone()));
    }
    Ok(out)
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
        assert_eq!(cfg.servers[0].passthrough_header(), Some("authorization"));
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
        assert_eq!(cfg.servers[0].passthrough_header(), Some("x-user-token"));
    }

    #[test]
    fn passthrough_rejects_transport_headers() {
        for name in ["mcp-session-id", "Host", "content-type"] {
            let raw: Config = serde_json::from_value(serde_json::json!({
                "mcpServers": [{
                    "name": "erp", "endpoint": "http://erp.test/mcp", "enabled": true,
                    "authType": "passthrough", "headerName": name
                }]
            }))
            .unwrap();
            assert!(PolicyConfig::from_config(&raw).is_err(), "{name}");
        }
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
            Err(ConfigError::ServersInvalid(_))
        ));
    }
}
