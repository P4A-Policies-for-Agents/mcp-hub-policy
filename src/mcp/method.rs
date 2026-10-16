//! Classification of inbound MCP JSON-RPC methods.
//!
//! The hub serves the full Streamable HTTP control plane on `/mcp`:
//!
//!   * `initialize`            — handshake; the hub answers locally.
//!   * `tools/list`            — aggregate from all enabled servers.
//!   * `tools/call`            — route to the owning server.
//!   * `resources/list`        — aggregate.
//!   * `resources/read`        — route by `uri`.
//!   * `prompts/list`          — aggregate.
//!   * `prompts/get`           — route.
//!   * `ping`                  — local 200.
//!   * `notifications/...`     — 202; forwarded best-effort to passthrough
//!     upstreams where the caller has a session.

/// An inbound JSON-RPC method, classified for dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpMethod {
    Initialize,
    Ping,
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourcesRead,
    PromptsList,
    PromptsGet,
    /// Notification (no `id`). Client → hub. Answered 202; forwarded
    /// best-effort to passthrough upstreams where the caller has a session.
    Notification(String),
    /// A method we don't recognize. Always answered with
    /// `METHOD_NOT_FOUND`.
    Other(String),
}

impl McpMethod {
    /// Classify `method`; anything without an `id` is a notification.
    pub fn parse(method: &str, is_notification: bool) -> Self {
        if is_notification {
            return Self::Notification(method.to_string());
        }
        match method {
            "initialize" => Self::Initialize,
            "ping" => Self::Ping,
            "tools/list" => Self::ToolsList,
            "tools/call" => Self::ToolsCall,
            "resources/list" => Self::ResourcesList,
            "resources/read" => Self::ResourcesRead,
            "prompts/list" => Self::PromptsList,
            "prompts/get" => Self::PromptsGet,
            other => Self::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_methods() {
        assert_eq!(McpMethod::parse("initialize", false), McpMethod::Initialize);
        assert_eq!(McpMethod::parse("tools/list", false), McpMethod::ToolsList);
        assert_eq!(McpMethod::parse("tools/call", false), McpMethod::ToolsCall);
    }

    #[test]
    fn parses_notifications() {
        assert!(matches!(
            McpMethod::parse("notifications/initialized", true),
            McpMethod::Notification(_)
        ));
    }

    #[test]
    fn unknown_method_is_other() {
        assert!(matches!(
            McpMethod::parse("foo/bar", false),
            McpMethod::Other(_)
        ));
    }
}
