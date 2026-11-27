//! Network resilience engine: AWS Full-Jitter Backoff, Per-Host Circuit Breaker, and
//! 30-second Stall Watchdog (`P1-T17` / `Vortex-Architecture-Design.md` §8.6.2).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rustc_hash::FxHashMap;
use vortex_application::{AppResult, ByteStream, TransportMetrics};
use vortex_domain::Clock;
use vortex_error::{ApiError, ErrorCode, Recoverability, TraceId, VortexError};

/// Base retry delay (`500 ms` — `§8.6.2`).
pub const BASE_BACKOFF_MS: u64 = 500;

/// Maximum retry delay ceiling (`300,000 ms` = `5 minutes` — `§8.6.2`).
pub const MAX_BACKOFF_MS: u64 = 300_000;

/// Default consecutive transient failure count before opening host circuit (`5`).
pub const DEFAULT_CIRCUIT_FAILURE_THRESHOLD: u32 = 5;

/// Default Circuit Breaker open-state cooldown (`15,000 ms`).
pub const DEFAULT_CIRCUIT_OPEN_MS: u64 = 15_000;

/// Computes Full-Jitter Exponential Backoff (`§8.6.2`):
/// `d_k = Uniform(0, min(d_max, d_base * 2^k))`
/// giving absolute priority to `server_retry_after_ms` when provided by `429` / `503`.
#[must_use]
pub fn compute_backoff_delay_ms(
    attempt: u32,
    server_retry_after_ms: Option<u64>,
    jitter_seed: u64,
) -> u64 {
    if let Some(explicit_ms) = server_retry_after_ms {
        return explicit_ms.clamp(100, MAX_BACKOFF_MS);
    }
    let shift = attempt.min(20);
    let exp_cap = BASE_BACKOFF_MS
        .saturating_mul(1_u64 << shift)
        .min(MAX_BACKOFF_MS);
    if exp_cap == 0 {
        return BASE_BACKOFF_MS;
    }
    // SplitMix64 deterministic/fast uniform draw in `[50ms, exp_cap]`
    let mut z = jitter_seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    let rnd = z ^ (z >> 31);
    (rnd % (exp_cap + 1)).max(50)
}

/// Circuit breaker state for an origin host (`Closed -> Open -> HalfOpen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Healthy traffic allowed.
    Closed {
        /// Consecutive transient failures observed so far.
        consecutive_failures: u32,
    },
    /// Host temporarily tripped; fast-fails until `reopen_at_ms`.
    Open {
        /// Unix timestamp (ms) when a `HalfOpen` probe is permitted.
        reopen_at_ms: i64,
    },
    /// Single trial request permitted to test recovery.
    HalfOpen,
}

/// Per-host Circuit Breaker registry preventing retry storms against overloaded servers (`P1-T17`).
#[derive(Debug, Clone)]
pub struct HostCircuitBreaker {
    clock: Arc<dyn Clock>,
    failure_threshold: u32,
    open_cooldown_ms: u64,
    hosts: Arc<Mutex<FxHashMap<String, CircuitState>>>,
}

impl HostCircuitBreaker {
    /// Creates a new [`HostCircuitBreaker`] driven by `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, failure_threshold: u32, open_cooldown_ms: u64) -> Self {
        Self {
            clock,
            failure_threshold: failure_threshold.max(1),
            open_cooldown_ms: open_cooldown_ms.max(100),
            hosts: Arc::new(Mutex::new(FxHashMap::default())),
        }
    }

    /// Checks whether a request to `host` is currently permitted.
    ///
    /// # Errors
    /// Returns transient `VTX-NET-002` with `retry_after_ms` if the circuit is `Open`.
    pub fn check_permitted(&self, host: &str, trace_id: TraceId) -> AppResult<()> {
        let now_ms = self.clock.now_unix_ms();
        let mut map = self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(&state) = map.get(host)
            && let CircuitState::Open { reopen_at_ms } = state
        {
            if now_ms >= reopen_at_ms {
                map.insert(host.to_owned(), CircuitState::HalfOpen);
                tracing::debug!(%trace_id, host = %host, "Circuit breaker transitioned Open -> HalfOpen");
            } else {
                let wait_ms = u64::try_from(reopen_at_ms.saturating_sub(now_ms)).unwrap_or(1_000);
                return Err(VortexError::new(
                    ApiError::new(ErrorCode::NetConnectionLost, trace_id)
                        .with_recoverability(Recoverability::Transient {
                            retry_after_ms: Some(wait_ms),
                        })
                        .with_technical_detail(format!(
                            "Circuit breaker OPEN for host `{host}` ({wait_ms}ms remaining)"
                        )),
                ));
            }
        }
        Ok(())
    }

    /// Records a successful response from `host`, resetting its circuit to `Closed(0)`.
    pub fn record_success(&self, host: &str) {
        let mut map = self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(
            host.to_owned(),
            CircuitState::Closed {
                consecutive_failures: 0,
            },
        );
    }

    /// Records a transient transport/5xx/429 failure on `host`, tripping `Open` on threshold or `HalfOpen` failure.
    pub fn record_transient_failure(&self, host: &str, trace_id: TraceId) -> CircuitState {
        let now_ms = self.clock.now_unix_ms();
        let cooldown_i64 = i64::try_from(self.open_cooldown_ms).unwrap_or(15_000);
        let mut map = self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let current = map.get(host).copied().unwrap_or(CircuitState::Closed {
            consecutive_failures: 0,
        });

        let next = match current {
            CircuitState::Closed {
                consecutive_failures,
            } => {
                let count = consecutive_failures.saturating_add(1);
                if count >= self.failure_threshold {
                    tracing::warn!(
                        error_code = %ErrorCode::NetConnectionLost,
                        %trace_id,
                        host = %host,
                        failures = count,
                        "Circuit breaker tripped OPEN"
                    );
                    CircuitState::Open {
                        reopen_at_ms: now_ms.saturating_add(cooldown_i64),
                    }
                } else {
                    CircuitState::Closed {
                        consecutive_failures: count,
                    }
                }
            }
            CircuitState::HalfOpen | CircuitState::Open { .. } => CircuitState::Open {
                reopen_at_ms: now_ms.saturating_add(cooldown_i64),
            },
        };

        map.insert(host.to_owned(), next);
        next
    }

    /// Inspects current [`CircuitState`] for `host`.
    #[must_use]
    pub fn state_of(&self, host: &str) -> CircuitState {
        self.hosts
            .lock()
            .ok()
            .and_then(|m| m.get(host).copied())
            .unwrap_or(CircuitState::Closed {
                consecutive_failures: 0,
            })
    }
}

/// Wraps an active [`ByteStream`] with a 30-second Stall Watchdog (`P1-T17` / `§8.6.2`).
pub struct WatchdogByteStream<S> {
    inner: S,
    idle_timeout: Duration,
    trace_id: TraceId,
}

impl<S> WatchdogByteStream<S> {
    /// Wraps `inner` so any `next_chunk` call exceeding `idle_timeout` fails cleanly with `VTX-NET-002`.
    #[must_use]
    pub const fn new(inner: S, idle_timeout: Duration, trace_id: TraceId) -> Self {
        Self {
            inner,
            idle_timeout,
            trace_id,
        }
    }
}

#[async_trait]
impl<S: ByteStream> ByteStream for WatchdogByteStream<S> {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        if let Ok(res) = tokio::time::timeout(self.idle_timeout, self.inner.next_chunk()).await {
            res
        } else {
            Err(VortexError::coded(
                ErrorCode::NetConnectionLost,
                self.trace_id,
                format!(
                    "Stall watchdog timed out after {}ms without receiving bytes",
                    self.idle_timeout.as_millis()
                ),
            ))
        }
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.inner.transport_metrics()
    }

    fn set_stop_offset(&self, stop_offset: u64) {
        self.inner.set_stop_offset(stop_offset);
    }
}
