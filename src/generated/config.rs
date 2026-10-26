use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct Headers0Config {
    #[serde(alias = "name")]
    pub name: String,
    #[serde(alias = "value")]
    pub value: String,
}
#[derive(Deserialize, Clone, Debug)]
pub struct McpServers0Config {
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    #[serde(alias = "authType")]
    pub auth_type: Option<String>,
    #[serde(alias = "enabled")]
    pub enabled: Option<bool>,
    #[serde(alias = "endpoint", deserialize_with = "pdk::serde::deserialize_service")]
    pub endpoint: pdk::hl::Service,
    #[serde(alias = "headerName")]
    pub header_name: Option<String>,
    #[serde(alias = "headers")]
    pub headers: Option<Vec<Headers0Config>>,
    #[serde(alias = "name")]
    pub name: String,
    #[serde(alias = "password")]
    pub password: Option<String>,
    #[serde(alias = "priority")]
    pub priority: Option<i64>,
    #[serde(alias = "timeoutMs")]
    pub timeout_ms: Option<i64>,
    #[serde(alias = "token")]
    pub token: Option<String>,
    #[serde(alias = "username")]
    pub username: Option<String>,
}
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "aggregationTimeoutMs")]
    pub aggregation_timeout_ms: Option<i64>,
    #[serde(alias = "allowedOrigins")]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(alias = "authorizationServers")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(alias = "collisionStrategy")]
    pub collision_strategy: Option<String>,
    #[serde(alias = "diagnosticContinueFlow")]
    pub diagnostic_continue_flow: Option<bool>,
    #[serde(alias = "exposeUpstreamErrors")]
    pub expose_upstream_errors: Option<bool>,
    #[serde(alias = "forwardHeaders")]
    pub forward_headers: Option<Vec<String>>,
    #[serde(alias = "manifestCacheTtlSeconds")]
    pub manifest_cache_ttl_seconds: Option<i64>,
    #[serde(alias = "maxUpstreamResponseBytes")]
    pub max_upstream_response_bytes: Option<i64>,
    #[serde(alias = "mcpEndpoint")]
    pub mcp_endpoint: Option<String>,
    #[serde(alias = "mcpServers")]
    pub mcp_servers: Vec<McpServers0Config>,
    #[serde(alias = "perRequestTimeoutMs")]
    pub per_request_timeout_ms: Option<i64>,
    #[serde(alias = "requireCallerAuth")]
    pub require_caller_auth: Option<bool>,
    #[serde(alias = "resourceUrl")]
    pub resource_url: Option<String>,
    #[serde(alias = "scopesSupported")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(alias = "strictMode")]
    pub strict_mode: Option<bool>,
    #[serde(alias = "upstreamFailureBackoffSeconds")]
    pub upstream_failure_backoff_seconds: Option<i64>,
    #[serde(alias = "upstreamSessionTtlSeconds")]
    pub upstream_session_ttl_seconds: Option<i64>,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    let config: Config = serde_json::from_slice(abi.get_configuration())
        .map_err(|err| {
            anyhow::anyhow!(
                "Failed to parse configuration '{}'. Cause: {}",
                String::from_utf8_lossy(abi.get_configuration()), err
            )
        })?;
    for current in config.mcp_servers {
        abi.service_create(current.endpoint)?;
    }
    abi.setup()?;
    Ok(())
}
