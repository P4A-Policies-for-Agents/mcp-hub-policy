//! Upstream `*/list` results, cached per (list method, server, caller
//! credential) for `manifestCacheTtlSeconds`.
//!
//!   * Backed by the PDK cache, so all workers of a replica share it.
//!     Replicas don't share state; a miss simply asks the upstream again.
//!   * Keys reuse the upstream session key (configuration hash, server,
//!     credential hash), so `authType=passthrough` results are per caller,
//!     raw tokens never land in shared memory, and a configuration change
//!     never serves entries cached under the old one.
//!   * Only successful answers are cached: a failing or unauthorized
//!     upstream is asked again on the next call.
//!   * The PDK cache bounds entry count, not bytes, so answers larger than
//!     [`MAX_ENTRY_BYTES`] are not cached.
//!   * Entries carry an expiry stamp; the PDK 1.8 cache has no native TTL.

use pdk::cache::Cache;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::now_seconds;

/// Largest serialized answer that is cached (bytes).
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
struct Entry {
    items: Vec<Value>,
    /// Expiry, seconds since the Unix epoch.
    exp: u64,
}

/// Serialization-side twin of [`Entry`], borrowing the items.
#[derive(Serialize)]
struct EntryRef<'a> {
    items: &'a [Value],
    exp: u64,
}

pub struct ManifestCache {
    cache: Box<dyn Cache>,
    ttl_seconds: u64,
}

impl ManifestCache {
    pub fn new(cache: impl Cache + 'static, ttl_seconds: u32) -> Self {
        Self {
            cache: Box::new(cache),
            ttl_seconds: ttl_seconds as u64,
        }
    }

    /// Cache key for one upstream's answer to `method`; `upstream_key`
    /// is the upstream's session key.
    pub fn key(method: &str, upstream_key: &str) -> String {
        format!("{method}|{upstream_key}")
    }

    /// Cached list entries, if present and not expired. A TTL of 0
    /// disables the cache.
    pub fn get(&self, key: &str) -> Option<Vec<Value>> {
        if self.ttl_seconds == 0 {
            return None;
        }
        let bytes = self.cache.get(key)?;
        match serde_json::from_slice::<Entry>(&bytes) {
            Ok(entry) if entry.exp > now_seconds() => Some(entry.items),
            _ => {
                self.cache.delete(key);
                None
            }
        }
    }

    /// Remember an upstream's list entries. A TTL of 0 disables the cache.
    pub fn put(&self, key: &str, items: &[Value]) {
        if self.ttl_seconds == 0 {
            return;
        }
        let entry = EntryRef {
            items,
            exp: now_seconds() + self.ttl_seconds,
        };
        let bytes = serde_json::to_vec(&entry).expect("manifest entry serializes");
        if bytes.len() > MAX_ENTRY_BYTES {
            pdk::logger::debug!(
                "mcp-hub: manifest answer of {} bytes not cached (limit {MAX_ENTRY_BYTES})",
                bytes.len()
            );
            return;
        }
        if let Err(e) = self.cache.save(key, bytes) {
            // Contention on the shared cache is harmless: the next call
            // simply asks the upstream again.
            pdk::logger::debug!("mcp-hub: manifest cache save failed: {e:?}");
        }
    }
}
