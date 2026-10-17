//! Manifest aggregation across upstream MCP servers.
//!
//! Each `*list` method (tools/list, resources/list, prompts/list) returns
//! `{ "<kind>": [ ... ] }`. The hub:
//!   1. Calls each enabled upstream's `<kind>/list`.
//!   2. Extracts each entry's `name`.
//!   3. Applies [`crate::config::CollisionStrategy`] across servers.
//!   4. Stamps an `_origin` hint on each entry so the routing layer knows
//!      which server owns each name (and its original, un-prefixed
//!      name). The markers are stripped before the list goes to clients.
//!
//! Output of [`aggregate`] is the JSON array ready to be wrapped in the
//! corresponding response key (`{"tools": [...]}` etc.).

use serde_json::Value;

use crate::config::CollisionStrategy;

/// Per-(server, item) tuple yielded by the upstream `*list` calls.
pub struct UpstreamItem<'a> {
    pub server: &'a str,
    pub value: Value,
}

/// Aggregate per-server lists into a single ordered array, applying the
/// collision strategy. Each entry in the output has an `_origin` field
/// set to the configured server name so the routing layer can resolve
/// `name -> server`.
///
/// Returns `Err` only when `strategy = Reject` and a duplicate is seen.
pub fn aggregate(
    items: Vec<UpstreamItem<'_>>,
    strategy: CollisionStrategy,
) -> Result<Vec<Value>, AggregateError> {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    let mut seen: Vec<(String, usize)> = Vec::new(); // (name, index in `out`)

    for item in items {
        let UpstreamItem { server, value } = item;
        let mut entry = match value {
            Value::Object(_) => value,
            _ => continue, // skip malformed
        };

        let original_name = match entry.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Stamp the origin server so the router knows where to dispatch.
        entry["_origin"] = Value::String(server.to_string());
        // Preserve the un-prefixed name for clients.
        entry["_originalName"] = Value::String(original_name.clone());

        match strategy {
            CollisionStrategy::Prefix => {
                // Use `_` as the separator. MCP clients (Claude Desktop, OpenAI
                // function-calling, etc.) constrain tool names to
                // `^[a-zA-Z0-9_-]{1,64}$`, which forbids `.`.
                let prefixed = format!("{server}_{original_name}");
                entry["name"] = Value::String(prefixed.clone());
                if seen.iter().any(|(n, _)| n == &prefixed) {
                    // Two servers prefixed to the same name (same server
                    // name + same tool name) — fall back to firstWins.
                    continue;
                }
                seen.push((prefixed, out.len()));
                out.push(entry);
            }
            CollisionStrategy::Reject => {
                if let Some((dup, _)) = seen.iter().find(|(n, _)| n == &original_name) {
                    return Err(AggregateError::Duplicate(dup.clone()));
                }
                seen.push((original_name, out.len()));
                out.push(entry);
            }
            CollisionStrategy::FirstWins => {
                if seen.iter().any(|(n, _)| n == &original_name) {
                    continue;
                }
                seen.push((original_name, out.len()));
                out.push(entry);
            }
            CollisionStrategy::LastWins => {
                if let Some((_, idx)) = seen.iter().find(|(n, _)| n == &original_name) {
                    let i = *idx;
                    out[i] = entry;
                } else {
                    seen.push((original_name, out.len()));
                    out.push(entry);
                }
            }
        }
    }

    Ok(out)
}

/// Look up which server owns the entry whose `key_field` (`name` for
/// tools and prompts, `uri` for resources) equals `requested`. The
/// aggregated array is the authoritative source; the lookup is linear in
/// the number of entries. Resource URIs are never prefixed, so the
/// original key of a `uri` lookup is the URI itself.
pub fn resolve_owner(items: &[Value], key_field: &str, requested: &str) -> Option<OwnerHint> {
    let item = items
        .iter()
        .find(|item| item.get(key_field).and_then(|v| v.as_str()) == Some(requested))?;
    let origin = item.get("_origin").and_then(|v| v.as_str())?.to_string();
    let original = match key_field {
        "name" => item.get("_originalName").and_then(|v| v.as_str()),
        _ => None,
    }
    .unwrap_or(requested)
    .to_string();
    Some(OwnerHint {
        server: origin,
        original_name: original,
    })
}

/// Strip the internal `_origin` / `_originalName` markers before
/// returning the manifest to the client. The markers are an
/// implementation detail of the routing table.
pub fn strip_internal(items: Vec<Value>) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut v| {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("_origin");
                obj.remove("_originalName");
            }
            v
        })
        .collect()
}

#[derive(Debug)]
pub struct OwnerHint {
    pub server: String,
    /// Name as the upstream server knows it (un-prefixed). Used as the
    /// `name` argument when forwarding `tools/call` etc.
    pub original_name: String,
}

/// Aggregation failure (`collisionStrategy: reject` found a duplicate).
#[derive(Debug, thiserror::Error)]
pub enum AggregateError {
    #[error("duplicate name '{0}' across MCP servers (collisionStrategy=reject)")]
    Duplicate(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(server: &'static str, name: &str) -> UpstreamItem<'static> {
        UpstreamItem {
            server,
            value: json!({"name": name, "description": format!("from {server}")}),
        }
    }

    #[test]
    fn prefix_renames_with_server_underscore_name() {
        let items = vec![item("jira", "search"), item("slack", "search")];
        let out = aggregate(items, CollisionStrategy::Prefix).unwrap();
        let names: Vec<_> = out
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["jira_search", "slack_search"]);
        // Sanity-check the MCP-client regex.
        for n in &names {
            assert!(
                n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "name '{n}' violates ^[a-zA-Z0-9_-]+$"
            );
        }
    }

    #[test]
    fn reject_returns_error_on_duplicate() {
        let items = vec![item("jira", "search"), item("slack", "search")];
        let err = aggregate(items, CollisionStrategy::Reject).unwrap_err();
        assert!(matches!(err, AggregateError::Duplicate(_)));
    }

    #[test]
    fn first_wins_keeps_first() {
        let items = vec![item("jira", "search"), item("slack", "search")];
        let out = aggregate(items, CollisionStrategy::FirstWins).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["_origin"], "jira");
    }

    #[test]
    fn last_wins_keeps_last() {
        let items = vec![item("jira", "search"), item("slack", "search")];
        let out = aggregate(items, CollisionStrategy::LastWins).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["_origin"], "slack");
    }

    #[test]
    fn resolve_owner_strips_prefix() {
        let items = vec![item("jira", "search"), item("slack", "list")];
        let out = aggregate(items, CollisionStrategy::Prefix).unwrap();
        let hint = resolve_owner(&out, "name", "jira_search").unwrap();
        assert_eq!(hint.server, "jira");
        assert_eq!(hint.original_name, "search");
    }

    #[test]
    fn resolve_owner_by_uri_keeps_uri() {
        let items = vec![
            UpstreamItem {
                server: "docs",
                value: json!({"name": "readme", "uri": "file:///readme.md"}),
            },
            UpstreamItem {
                server: "wiki",
                value: json!({"name": "home", "uri": "wiki://home"}),
            },
        ];
        let out = aggregate(items, CollisionStrategy::Prefix).unwrap();
        let hint = resolve_owner(&out, "uri", "wiki://home").unwrap();
        assert_eq!(hint.server, "wiki");
        assert_eq!(hint.original_name, "wiki://home");
        assert!(resolve_owner(&out, "uri", "wiki://missing").is_none());
    }

    #[test]
    fn strip_internal_removes_origin_markers() {
        let items = vec![item("jira", "search")];
        let out = aggregate(items, CollisionStrategy::Prefix).unwrap();
        let cleaned = strip_internal(out);
        assert!(cleaned[0].get("_origin").is_none());
        assert!(cleaned[0].get("_originalName").is_none());
        assert_eq!(cleaned[0]["name"], "jira_search");
    }
}
