//! Smart per-host profile cache (`SmartHostProfileCache` / `HostCapabilityCache` — `P1.6-T04` & `P1.7-T06`).
//!
//! Tracks per-host capabilities in a bounded LRU-like map (`max 512` hosts, `30 min` TTL):
//! - **`hard_cap`**: discovered strict server concurrency ceiling (e.g. `pixeldrain.com -> 1` stream)
//!   with a `5 min` Half-Open probe window so transient rate limits heal automatically.
//! - **`proven_optimal_workers`**: proven high-throughput concurrency on uncapped hosts (`0ms` parallel
//!   burst on subsequent downloads without slow-start ramp delay!).
//! - **`prefers_h2`**: remembers HTTP/2 ALPN capability on the host.
//!
//! Crucially, transient socket drops (`VTX-NET-003`) MUST NEVER write to `hard_cap` (`P1.7-T06`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vortex_domain::Clock;

/// Default TTL (`30 minutes`) for cached per-host profiles (`P1.6-T04` / `P1.7-T06`).
pub const DEFAULT_HOST_CAPABILITY_TTL_MS: i64 = 30 * 60 * 1_000;

/// Half-Open recovery probe interval (`5 minutes`) before gently trying `hard_cap + 1` once (`P1.7-T06`).
pub const HALF_OPEN_PROBE_INTERVAL_MS: i64 = 5 * 60 * 1_000;

/// Maximum distinct hosts retained in memory (`512` — prevents unbounded memory growth).
pub const MAX_HOST_CAPABILITY_ENTRIES: usize = 512;

#[derive(Debug, Clone, Copy)]
struct HostProfileEntry {
    hard_cap: Option<u16>,
    cap_expires_at_ms: i64,
    half_open_probe_after_ms: i64,
    proven_optimal_workers: Option<u16>,
    prefers_h2: bool,
    updated_at_ms: i64,
    expires_at_ms: i64,
}

/// Thread-safe, deterministic-clock per-host capability & throughput profile cache (`P1.7-T06`).
#[derive(Clone)]
pub struct SmartHostProfileCache {
    clock: Arc<dyn Clock>,
    ttl_ms: i64,
    half_open_interval_ms: i64,
    entries: Arc<Mutex<HashMap<String, HostProfileEntry>>>,
}

/// Backward-compatible alias retained from Phase 1.6 (`P1.6-T04`).
pub type HostCapabilityCache = SmartHostProfileCache;

impl std::fmt::Debug for SmartHostProfileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartHostProfileCache")
            .field("ttl_ms", &self.ttl_ms)
            .field("half_open_interval_ms", &self.half_open_interval_ms)
            .finish_non_exhaustive()
    }
}

impl SmartHostProfileCache {
    /// Creates a new [`SmartHostProfileCache`] with default `30 min` TTL and `5 min` Half-Open probe window.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_ttl_ms(clock, DEFAULT_HOST_CAPABILITY_TTL_MS)
    }

    /// Creates a new [`SmartHostProfileCache`] with an explicit TTL in milliseconds.
    #[must_use]
    pub fn with_ttl_ms(clock: Arc<dyn Clock>, ttl_ms: i64) -> Self {
        let clamped_ttl = ttl_ms.max(1_000);
        let half_open = HALF_OPEN_PROBE_INTERVAL_MS.min((clamped_ttl / 2).max(500));
        Self {
            clock,
            ttl_ms: clamped_ttl,
            half_open_interval_ms: half_open,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns `requested` clamped to any active discovered hard cap for `host` (`>= 1` when `requested >= 1`).
    #[must_use]
    pub fn effective_limit(&self, host: &str, requested: u16) -> u16 {
        if requested <= 1 {
            return requested;
        }
        self.hard_cap(host)
            .map_or(requested, |cap| requested.min(cap).max(1))
    }

    /// Returns the currently valid cached hard concurrency ceiling for `host`, if any.
    #[must_use]
    pub fn cached_limit(&self, host: &str) -> Option<u16> {
        self.hard_cap(host)
    }

    /// Returns the currently valid discovered `hard_cap` for `host`, if any.
    #[must_use]
    pub fn hard_cap(&self, host: &str) -> Option<u16> {
        let key = normalize_host_key(host)?;
        let now = self.clock.now_unix_ms();
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.get_mut(&key)?;
        if now >= entry.expires_at_ms {
            map.remove(&key);
            return None;
        }
        if let Some(cap) = entry.hard_cap {
            if now < entry.cap_expires_at_ms {
                return Some(cap);
            }
            entry.hard_cap = None;
        }
        None
    }

    /// Returns the cached proven optimal worker count for an uncapped fast host (`0ms` parallel burst!).
    #[must_use]
    pub fn proven_concurrency(&self, host: &str) -> Option<u16> {
        let key = normalize_host_key(host)?;
        let now = self.clock.now_unix_ms();
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.get(&key).copied()?;
        if now >= entry.expires_at_ms {
            map.remove(&key);
            return None;
        }
        if entry.hard_cap.is_some() && now < entry.cap_expires_at_ms {
            return None;
        }
        entry.proven_optimal_workers
    }

    /// Returns whether `host` previously negotiated HTTP/2 successfully within the TTL window.
    #[must_use]
    pub fn prefers_h2(&self, host: &str) -> bool {
        let Some(key) = normalize_host_key(host) else {
            return false;
        };
        let now = self.clock.now_unix_ms();
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = map.get(&key).copied() else {
            return false;
        };
        if now >= entry.expires_at_ms {
            map.remove(&key);
            return false;
        }
        entry.prefers_h2
    }

    /// If `host` has an active `hard_cap` whose `5 min` Half-Open probe window has elapsed,
    /// atomically claims a single gentle `hard_cap + 1` recovery probe and advances the probe timer.
    #[must_use]
    pub fn try_claim_half_open_probe(&self, host: &str) -> Option<u16> {
        let key = normalize_host_key(host)?;
        let now = self.clock.now_unix_ms();
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.get_mut(&key)?;
        if now >= entry.expires_at_ms {
            map.remove(&key);
            return None;
        }
        let cap = entry.hard_cap?;
        if now >= entry.cap_expires_at_ms {
            entry.hard_cap = None;
            return None;
        }
        if now >= entry.half_open_probe_after_ms {
            entry.half_open_probe_after_ms = now.saturating_add(self.half_open_interval_ms);
            return Some(cap.saturating_add(1).min(32));
        }
        None
    }

    /// Records a discovered strict server concurrency cap (`403`/`429`/`503` rejection — NEVER transient TCP drops!).
    pub fn record_concurrency_cap(&self, host: &str, safe_limit: u16) {
        let Some(key) = normalize_host_key(host) else {
            return;
        };
        let clamped = safe_limit.clamp(1, 32);
        let now = self.clock.now_unix_ms();
        let expires = now.saturating_add(self.ttl_ms);
        let next_probe = now.saturating_add(self.half_open_interval_ms);

        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evict_expired_and_lru(&mut map, &key, now);

        let prev = map.get(&key).copied();
        let next_cap = prev
            .and_then(|p| p.hard_cap)
            .map_or(clamped, |prev_cap| prev_cap.min(clamped).max(1));
        let prefers_h2 = prev.is_some_and(|p| p.prefers_h2);

        map.insert(
            key,
            HostProfileEntry {
                hard_cap: Some(next_cap),
                cap_expires_at_ms: expires,
                half_open_probe_after_ms: next_probe,
                proven_optimal_workers: None,
                prefers_h2,
                updated_at_ms: now,
                expires_at_ms: expires,
            },
        );
    }

    /// Records proven fast multi-stream concurrency (and HTTP/2 support) after a clean completion.
    /// If a Half-Open probe succeeded above a prior `hard_cap`, clears the stale cap automatically!
    pub fn record_proven_concurrency(&self, host: &str, workers: u16, prefers_h2: bool) {
        let Some(key) = normalize_host_key(host) else {
            return;
        };
        let clean_workers = workers.clamp(1, 32);
        let now = self.clock.now_unix_ms();
        let expires = now.saturating_add(self.ttl_ms);

        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evict_expired_and_lru(&mut map, &key, now);

        let prev = map.get(&key).copied();
        let active_cap = prev
            .filter(|p| now < p.cap_expires_at_ms)
            .and_then(|p| p.hard_cap);

        // If we succeeded with strictly more workers than a prior `hard_cap` (Half-Open probe succeeded!),
        // promote the host back to uncapped; otherwise keep the existing cap intact.
        let (hard_cap, cap_expires_at_ms, half_open_probe_after_ms) = match active_cap {
            Some(cap) if clean_workers <= cap => (
                Some(cap),
                prev.map_or(expires, |p| p.cap_expires_at_ms),
                prev.map_or(now, |p| p.half_open_probe_after_ms),
            ),
            _ => (None, 0, 0),
        };

        let proven = if hard_cap.is_none() && clean_workers >= 2 {
            Some(
                prev.and_then(|p| p.proven_optimal_workers)
                    .map_or(clean_workers, |p| p.max(clean_workers)),
            )
        } else {
            None
        };

        map.insert(
            key,
            HostProfileEntry {
                hard_cap,
                cap_expires_at_ms,
                half_open_probe_after_ms,
                proven_optimal_workers: proven,
                prefers_h2: prefers_h2 || prev.is_some_and(|p| p.prefers_h2),
                updated_at_ms: now,
                expires_at_ms: expires,
            },
        );
    }
}

fn evict_expired_and_lru(map: &mut HashMap<String, HostProfileEntry>, key: &str, now: i64) {
    map.retain(|_, v| now < v.expires_at_ms);

    if map.len() >= MAX_HOST_CAPABILITY_ENTRIES
        && !map.contains_key(key)
        && let Some(oldest_key) = map
            .iter()
            .min_by_key(|(_, v)| v.updated_at_ms)
            .map(|(k, _)| k.clone())
    {
        map.remove(&oldest_key);
    }
}

fn normalize_host_key(host: &str) -> Option<String> {
    let clean = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if clean.is_empty() { None } else { Some(clean) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use vortex_domain::FakeClock;

    use super::*;

    #[test]
    fn caches_clamps_half_open_probes_and_expires_host_profiles_deterministically() {
        let clock = Arc::new(FakeClock::new(10_000));
        let cache = SmartHostProfileCache::with_ttl_ms(clock.clone(), 60_000);

        // Debug representation & blank host handling
        assert!(format!("{cache:?}").contains("SmartHostProfileCache"));
        cache.record_concurrency_cap("   .", 2);
        cache.record_proven_concurrency("", 8, true);
        assert_eq!(cache.cached_limit(""), None);
        assert_eq!(cache.proven_concurrency(""), None);
        assert!(!cache.prefers_h2(""));
        assert_eq!(cache.try_claim_half_open_probe(""), None);

        // Uncapped fast host remembers proven concurrency + HTTP/2 preference!
        cache.record_proven_concurrency("cdn.fast.test", 12, true);
        assert_eq!(cache.proven_concurrency("cdn.fast.test"), Some(12));
        assert!(cache.prefers_h2("cdn.fast.test"));
        assert_eq!(cache.effective_limit("cdn.fast.test", 16), 16);

        // Strict host hard cap clamps effective_limit and suppresses proven concurrency
        assert_eq!(cache.effective_limit("pixeldrain.com", 8), 8);
        cache.record_concurrency_cap("PixelDrain.com.", 2);
        assert_eq!(cache.effective_limit("pixeldrain.com", 8), 2);
        assert_eq!(cache.effective_limit("pixeldrain.com", 1), 1);

        // Tighter ceiling narrows cached limit
        cache.record_concurrency_cap("pixeldrain.com", 1);
        assert_eq!(cache.cached_limit("pixeldrain.com"), Some(1));
        assert_eq!(cache.proven_concurrency("pixeldrain.com"), None);

        // Before Half-Open interval (`30s` for `60s` TTL): no probe yet
        assert_eq!(cache.try_claim_half_open_probe("pixeldrain.com"), None);

        // Advance past Half-Open interval: allows ONE gentle `cap + 1 = 2` probe
        clock.advance_ms(30_001);
        assert_eq!(cache.try_claim_half_open_probe("pixeldrain.com"), Some(2));
        // Second immediate call is suppressed until next Half-Open window
        assert_eq!(cache.try_claim_half_open_probe("pixeldrain.com"), None);

        // Completing cleanly with 2 workers promotes host back to uncapped!
        cache.record_proven_concurrency("pixeldrain.com", 2, true);
        assert_eq!(cache.hard_cap("pixeldrain.com"), None);
        assert_eq!(cache.proven_concurrency("pixeldrain.com"), Some(2));
        assert!(cache.prefers_h2("pixeldrain.com"));

        // Advancing clock past TTL clears expiration cleanly across all accessors
        clock.advance_ms(60_001);
        assert_eq!(cache.cached_limit("pixeldrain.com"), None);
        assert_eq!(cache.proven_concurrency("pixeldrain.com"), None);
        assert!(!cache.prefers_h2("pixeldrain.com"));
        assert_eq!(cache.effective_limit("pixeldrain.com", 8), 8);

        // Also cover expiry cleanup paths inside `proven_concurrency`, `prefers_h2`, and `try_claim_half_open_probe`
        cache.record_proven_concurrency("exp-proven.test", 6, true);
        cache.record_concurrency_cap("exp-cap.test", 2);
        cache.record_proven_concurrency("exp-cap.test", 1, false);
        assert_eq!(cache.hard_cap("exp-cap.test"), Some(2));
        clock.advance_ms(60_001);
        assert_eq!(cache.proven_concurrency("exp-proven.test"), None);
        cache.record_proven_concurrency("exp-h2.test", 4, true);
        clock.advance_ms(60_001);
        assert!(!cache.prefers_h2("exp-h2.test"));
        assert_eq!(cache.try_claim_half_open_probe("exp-cap.test"), None);
    }

    #[test]
    fn evicts_oldest_host_when_exceeding_512_entries_lru_cap() {
        let clock = Arc::new(FakeClock::new(1_000));
        let cache = HostCapabilityCache::new(clock.clone());

        for idx in 0..=MAX_HOST_CAPABILITY_ENTRIES {
            clock.advance_ms(10);
            cache.record_concurrency_cap(&format!("host-{idx}.example.com"), 2);
        }

        // Oldest host (`host-0`) must have been evicted to stay within `512` max entries!
        assert_eq!(cache.cached_limit("host-0.example.com"), None);
        assert_eq!(
            cache.cached_limit(&format!("host-{MAX_HOST_CAPABILITY_ENTRIES}.example.com")),
            Some(2)
        );
    }
}
