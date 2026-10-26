//! Upstream MCP session ids, cached per (server, caller credential).
//!
//! The hub is stateless toward its clients, but Streamable-HTTP upstreams
//! may gate post-`initialize` traffic on an `mcp-session-id`. The hub
//! initializes each upstream lazily and remembers the id here:
//!
//!   * Backed by the PDK cache, so all workers of a replica share it.
//!     Replicas don't share state; a miss simply re-initializes.
//!   * `authType=passthrough` servers are keyed by a SHA-256 of the
//!     caller's credential, so users never share an upstream session.
//!     Static-auth servers have one session per server (the identity is the
//!     hub's configured credential either way).
//!   * Keys are scoped by a hash of the policy configuration, so a
//!     changed configuration (or another policy sharing the cache) starts
//!     with fresh sessions.
//!   * Entries carry an expiry stamp; the PDK 1.8 cache has no native TTL.
//!   * An empty session id records "initialized, upstream issued no id" so
//!     session-less servers aren't re-initialized on every call.
//!   * A failed handshake is remembered under the same key for
//!     `upstreamFailureBackoffSeconds`, so a dead upstream isn't waited on
//!     by every request.

use std::time::{SystemTime, UNIX_EPOCH};

use pdk::cache::Cache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// An upstream MCP session as the hub negotiated it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// `mcp-session-id`; empty when the upstream issued none.
    pub id: String,
    /// Protocol version the upstream answered `initialize` with; empty
    /// when it didn't say.
    pub protocol_version: String,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    sid: String,
    #[serde(default)]
    ver: String,
    /// Expiry, seconds since the Unix epoch.
    exp: u64,
}

#[derive(Serialize, Deserialize)]
struct Failure {
    /// Skip reason code reported while backing off.
    reason: String,
    /// End of the backoff, seconds since the Unix epoch.
    exp: u64,
    /// Timeout the failure happened under, when it was one: only calls
    /// allowed as long or less are backed off. `None` backs off all.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// TTL-bounded upstream sessions and handshake failures, keyed by
/// [`SessionStore::key`].
pub struct SessionStore {
    cache: Box<dyn Cache>,
    ttl_seconds: u64,
}

impl SessionStore {
    pub fn new(cache: impl Cache + 'static, ttl_seconds: u32) -> Self {
        Self {
            cache: Box::new(cache),
            ttl_seconds: ttl_seconds as u64,
        }
    }

    /// Cache key for an upstream. `config_tag` (see [`config_tag`])
    /// scopes keys to one policy configuration, so policies that share the
    /// cache, or a changed configuration, never reuse each other's
    /// sessions. The credential is hashed so raw tokens never land in
    /// shared memory.
    pub fn key(config_tag: &str, server: &str, credential: Option<&str>) -> String {
        match credential {
            None => format!("{config_tag}|{server}"),
            Some(c) => format!("{config_tag}|{server}|{}", hex_prefix(c.as_bytes(), 16)),
        }
    }

    /// Cached session, if present and not expired.
    pub fn get(&self, key: &str) -> Option<Session> {
        let entry: Entry = self.read(key, |e: &Entry| e.exp)?;
        Some(Session {
            id: entry.sid,
            protocol_version: entry.ver,
        })
    }

    /// Remember a session. A TTL of 0 disables reuse.
    pub fn put(&self, key: &str, session: &Session) {
        if self.ttl_seconds == 0 {
            return;
        }
        let entry = Entry {
            sid: session.id.clone(),
            ver: session.protocol_version.clone(),
            exp: now_seconds() + self.ttl_seconds,
        };
        self.write(key, &entry);
    }

    pub fn remove(&self, key: &str) {
        self.cache.delete(key);
    }

    /// Skip the upstream behind `key` for `seconds`, reporting `reason`.
    /// With `timeout_ms`, only calls whose timeout is at most that are
    /// skipped (a call with more time may still succeed).
    pub fn record_failure(&self, key: &str, reason: &str, seconds: u64, timeout_ms: Option<u64>) {
        if seconds == 0 {
            return;
        }
        let failure = Failure {
            reason: reason.to_string(),
            exp: now_seconds() + seconds,
            timeout_ms,
        };
        self.write(&failure_key(key), &failure);
    }

    /// Active backoff for `key` covering a call with `timeout_ms`:
    /// (reason, seconds left, at least 1).
    pub fn failure(&self, key: &str, timeout_ms: u64) -> Option<(String, u64)> {
        let failure: Failure = self.read(&failure_key(key), |f: &Failure| f.exp)?;
        if failure.timeout_ms.is_some_and(|t| timeout_ms > t) {
            return None;
        }
        let left = failure.exp.saturating_sub(now_seconds()).max(1);
        Some((failure.reason, left))
    }

    /// Unexpired value under `key`; expired or unreadable entries are
    /// deleted.
    fn read<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
        exp: impl Fn(&T) -> u64,
    ) -> Option<T> {
        let bytes = self.cache.get(key)?;
        match serde_json::from_slice::<T>(&bytes) {
            Ok(value) if exp(&value) > now_seconds() => Some(value),
            _ => {
                self.cache.delete(key);
                None
            }
        }
    }

    fn write(&self, key: &str, value: &impl Serialize) {
        let bytes = serde_json::to_vec(value).expect("session entry serializes");
        if let Err(e) = self.cache.save(key, bytes) {
            // Contention on the shared cache is harmless: the next call
            // simply re-initializes.
            pdk::logger::debug!("mcp-hub: session cache save failed: {e:?}");
        }
    }
}

fn failure_key(key: &str) -> String {
    format!("{key}|failed")
}

/// Short hash of the raw policy configuration, used to scope cache keys.
pub fn config_tag(config: &[u8]) -> String {
    hex_prefix(config, 8)
}

/// Hex of the first `n` bytes of SHA-256(`data`).
fn hex_prefix(data: &[u8], n: usize) -> String {
    Sha256::digest(data)[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_per_server_without_credential() {
        assert_eq!(SessionStore::key("t1", "erp", None), "t1|erp");
        assert_ne!(
            SessionStore::key("t1", "erp", None),
            SessionStore::key("t2", "erp", None)
        );
    }

    #[test]
    fn key_separates_callers_and_hides_token() {
        let a = SessionStore::key("t1", "erp", Some("Bearer alice"));
        let b = SessionStore::key("t1", "erp", Some("Bearer bob"));
        assert_ne!(a, b);
        assert!(a.starts_with("t1|erp|"));
        assert!(!a.contains("alice"));
        assert_eq!(a, SessionStore::key("t1", "erp", Some("Bearer alice")));
    }
}
