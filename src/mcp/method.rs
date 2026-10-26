//! Classification of inbound MCP JSON-RPC methods.
//!
//! The hub serves the full Streamable HTTP control plane on `/mcp`:
//!
//!   * `initialize`            — handshake; the hub answers locally.
//!   * `tools/list`            — aggregate from all enabled servers.
//!   * `tools/call`            — route to the owning server.
//!   * `resources/list`        — aggregate.
//!   * `resources/read`        — route by `uri`.
//!   * `resources/templates/list` — always empty: `resources/read` routes
//!     only URIs some `resources/list` returned.
//!   * `prompts/list`          — aggregate.
//!   * `prompts/get`           — route.
//!   * `ping`                  — local 200.
//!   * `notifications/...`     — 202; forwarded best-effort to passthrough
//!     upstreams where the caller has a session.

/// An inbound JSON-RPC request method, classified for dispatch.
/// Notifications (no `id`) are handled before classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMethod {
    Initialize,
    Ping,
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourcesRead,
    ResourceTemplatesList,
    PromptsList,
    PromptsGet,
    /// A method we don't recognize. Always answered with
    /// `METHOD_NOT_FOUND`.
    Other,
}

impl McpMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            "initialize" => Self::Initialize,
            "ping" => Self::Ping,
            "tools/list" => Self::ToolsList,
            "tools/call" => Self::ToolsCall,
            "resources/list" => Self::ResourcesList,
            "resources/read" => Self::ResourcesRead,
            "resources/templates/list" => Self::ResourceTemplatesList,
            "prompts/list" => Self::PromptsList,
            "prompts/get" => Self::PromptsGet,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_methods() {
        assert_eq!(McpMethod::parse("initialize"), McpMethod::Initialize);
        assert_eq!(McpMethod::parse("tools/list"), McpMethod::ToolsList);
        assert_eq!(McpMethod::parse("tools/call"), McpMethod::ToolsCall);
        assert_eq!(McpMethod::parse("prompts/get"), McpMethod::PromptsGet);
        assert_eq!(
            McpMethod::parse("resources/templates/list"),
            McpMethod::ResourceTemplatesList
        );
    }

    #[test]
    fn unknown_method_is_other() {
        assert_eq!(McpMethod::parse("foo/bar"), McpMethod::Other);
    }
}
