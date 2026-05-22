//! Typed view over the policy configuration.
//!
//! `cargo anypoint config-gen` regenerates `src/generated/config.rs`
//! from `definition/gcl.yaml`. This module wraps that into a clean
//! typed `PolicyConfig` after defaulting + validating.

use serde::{Deserialize, Serialize};

use crate::generated::config::{Config, Headers0Config, McpServers0Config};

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
}

/// How to handle name collisions between MCP servers when aggregating
/// `tools/list`, `resources/list`, `prompts/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollisionStrategy {
    /// Rename to `<server>.<original>` (default).
    Prefix,
    /// Reject duplicate names with an MCP-compliant error.
    Reject,
    /// Keep the first server's entry, drop subsequent dupes.
    FirstWins,
    /// Overwrite with the most recently seen entry.
    LastWins,
}

impl CollisionStrategy {
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
    Bearer { token: String },
    Basic { username: String, password: String },
    ApiKey { header_name: String, api_key: String },
    /// Auth materials live entirely in the per-server `headers` array.
    CustomHeaders,
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
                Ok(Self::ApiKey { header_name, api_key })
            }
            "customHeaders" => Ok(Self::CustomHeaders),
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

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub mcp_endpoint: String,
    pub strict_mode: bool,
    pub collision_strategy: CollisionStrategy,
    pub aggregation_timeout_ms: u32,
    pub per_request_timeout_ms: u32,
    pub manifest_cache_ttl_seconds: u32,
    pub diagnostic_continue_flow: bool,
    pub servers: Vec<McpServerConfig>,
}

impl PolicyConfig {
    pub fn from_config(raw: &Config) -> Result<Self, ConfigError> {
        if raw.mcp_servers.is_empty() {
            return Err(ConfigError::NoServers);
        }
        if raw.mcp_servers.len() > 20 {
            return Err(ConfigError::ServersInvalid(format!(
                "too many servers ({}) — maximum is 20",
                raw.mcp_servers.len()
            )));
        }

        let collision_strategy =
            CollisionStrategy::parse(raw.collision_strategy.as_deref().unwrap_or("prefix"))?;
        let mcp_endpoint = normalize_path(raw.mcp_endpoint.as_deref().unwrap_or("/mcp"));
        let strict_mode = raw.strict_mode.unwrap_or(true);
        let aggregation_timeout_ms = clamp_u32(raw.aggregation_timeout_ms, 100, 60_000, 5_000);
        let per_request_timeout_ms = clamp_u32(raw.per_request_timeout_ms, 100, 600_000, 30_000);
        let manifest_cache_ttl_seconds =
            clamp_u32(raw.manifest_cache_ttl_seconds, 0, 86_400, 60);
        let diagnostic_continue_flow = raw.diagnostic_continue_flow.unwrap_or(false);

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
            diagnostic_continue_flow,
            servers,
        })
    }

    /// Lookup a server by configured `name`.
    pub fn server(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.iter().find(|s| s.name == name && s.enabled)
    }

    pub fn enabled_servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        self.servers.iter().filter(|s| s.enabled)
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
}
