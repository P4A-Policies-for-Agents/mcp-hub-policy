//! Minimal MCP domain types used during aggregation and routing.
//!
//! The hub treats tool/resource/prompt manifests as opaque JSON values
//! (forwarded byte-for-byte to clients) but needs the `name` and a few
//! optional fields out of each entry so it can:
//!
//!   * detect collisions across upstream servers,
//!   * apply the configured `collisionStrategy`,
//!   * remember which server owns each name for routing
//!     (`tools/call`, `resources/read`, `prompts/get`).

/// MCP `initialize` server-info block. The hub identifies itself rather
/// than echoing one of the upstream servers.
pub fn server_info(name: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "version": version,
    })
}

/// MCP `initialize` capabilities block. The hub advertises the union of
/// supported feature areas — clients infer support from the manifests
/// returned by the corresponding `*list` calls.
pub fn server_capabilities() -> serde_json::Value {
    serde_json::json!({
        "tools": { "listChanged": false },
        "resources": { "listChanged": false, "subscribe": false },
        "prompts": { "listChanged": false },
        "logging": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_includes_name_and_version() {
        let v = server_info("hub", "1.2.3");
        assert_eq!(v["name"], "hub");
        assert_eq!(v["version"], "1.2.3");
    }

    #[test]
    fn capabilities_advertise_tools_resources_prompts() {
        let v = server_capabilities();
        assert!(v.get("tools").is_some());
        assert!(v.get("resources").is_some());
        assert!(v.get("prompts").is_some());
    }
}
