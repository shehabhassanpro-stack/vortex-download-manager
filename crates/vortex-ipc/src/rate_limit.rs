//! Per-client IPC token-bucket rate limiter (`Protocol-Spec.md` §5: `≤ 100 msg/s` → `VTX-IPC-004`).

use std::time::Instant;

use vortex_error::{ErrorCode, TraceId, VortexError};

/// Default maximum steady-state IPC message rate per connected client (`100 msg/s`).
pub const DEFAULT_IPC_RATE_LIMIT_PER_SEC: f64 = 100.0;

/// Tightened initial token-bucket burst capacity (`40` frames — `TH-32`).
pub const DEFAULT_IPC_BURST_CAPACITY: f64 = 40.0;

/// Token-bucket rate limiter protecting `Vortex Core` from runaway or malicious IPC clients.
#[derive(Debug)]
pub struct IpcRateLimiter {
    rate_per_sec: f64,
    burst_capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl IpcRateLimiter {
    /// Creates a new [`IpcRateLimiter`] initialized with equal burst capacity and steady-state rate.
    #[must_use]
    pub fn new(rate_per_sec: f64, now: Instant) -> Self {
        let clamped_rate = rate_per_sec.max(1.0);
        Self::with_burst(clamped_rate, clamped_rate, now)
    }

    /// Creates a new [`IpcRateLimiter`] with explicit steady-state rate and initial burst capacity.
    #[must_use]
    pub fn with_burst(rate_per_sec: f64, burst_capacity: f64, now: Instant) -> Self {
        let clamped_rate = rate_per_sec.max(1.0);
        let clamped_burst = burst_capacity.max(1.0);
        Self {
            rate_per_sec: clamped_rate,
            burst_capacity: clamped_burst,
            tokens: clamped_burst,
            last_refill: now,
        }
    }

    /// Creates a standard `Protocol-Spec.md` §5 + `TH-32` limiter (`100 msg/s`, burst `40`).
    #[must_use]
    pub fn protocol_default(now: Instant) -> Self {
        Self::with_burst(
            DEFAULT_IPC_RATE_LIMIT_PER_SEC,
            DEFAULT_IPC_BURST_CAPACITY,
            now,
        )
    }

    /// Consumes 1 message token at instant `now`, or rejects with `VTX-IPC-004` (`IpcRateLimited`).
    ///
    /// # Errors
    /// Returns `VTX-IPC-004` when the client exceeds its permitted message budget.
    pub fn check_and_consume(
        &mut self,
        now: Instant,
        trace_id: TraceId,
    ) -> Result<(), VortexError> {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = elapsed
                .mul_add(self.rate_per_sec, self.tokens)
                .min(self.burst_capacity);
            self.last_refill = now;
        }

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(VortexError::coded(
                ErrorCode::IpcRateLimited,
                trace_id,
                "IPC client exceeded 100 messages/second rate limit",
            ))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn enforces_40_burst_and_100_messages_per_second_refill_over_time() {
        let t0 = Instant::now();
        let tid = TraceId::new();
        let mut limiter = IpcRateLimiter::protocol_default(t0);

        for _ in 0..40 {
            limiter.check_and_consume(t0, tid).unwrap();
        }
        // 41st immediate message exceeds burst capacity (40) -> rejected with VTX-IPC-004 (`TH-32`)
        assert_eq!(
            limiter.check_and_consume(t0, tid).unwrap_err().code(),
            ErrorCode::IpcRateLimited
        );

        // After 50ms at 100 msg/s, 5 tokens refill deterministically
        let t1 = t0 + Duration::from_millis(50);
        for _ in 0..5 {
            limiter.check_and_consume(t1, tid).unwrap();
        }
        assert_eq!(
            limiter.check_and_consume(t1, tid).unwrap_err().code(),
            ErrorCode::IpcRateLimited
        );
    }
}
