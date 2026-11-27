//! Pure `BBR / Gradient2 + AIMD` Adaptive Concurrency Controller and `End-Game` Hedging
//! predicate (`P1.7-T01` / `ADR-009` / `ADR-021` / `Ultra-High-Throughput-Engine-Spec.md`).
//!
//! Zero I/O, deterministic math, and `proptest`-verified invariants.

#![allow(clippy::cast_precision_loss)]

/// Maximum remaining bytes threshold (`16 MiB`) for activating `End-Game` Hedged Requests (`§8.4`).
pub const ENDGAME_MAX_REMAINING_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum remaining fraction (`2%`) of total file size for activating `End-Game` mode (`§8.4`).
pub const ENDGAME_REMAINING_FRACTION: f64 = 0.02;

/// Minimum remaining bytes on a straggler worker (`64 KiB`) before hedging is worthwhile.
pub const ENDGAME_MIN_WORKER_REMAINING_BYTES: u64 = 64 * 1024;

/// Straggler speed ratio threshold (`30%` of peer median speed).
pub const ENDGAME_STRAGGLER_SPEED_RATIO: f64 = 0.30;

/// Minimum projected straggler stall time (`1.5s`) before issuing a hedged request.
pub const ENDGAME_MIN_STRAGGLER_ETA_SECS: f64 = 1.5;

/// Reason why [`AdaptiveConcurrencyController`] reduced the active worker target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDownReason {
    /// Queueing delay gradient dropped below Bufferbloat threshold (`g < 0.72`).
    Bufferbloat,
    /// Server emitted rate-limiting pressure (`429` / `503`) triggering AIMD multiplicative decrease.
    AimdRateLimit,
    /// Clamped to an explicit host concurrency ceiling (`SmartHostProfileCache::hard_cap`).
    HostHardCap,
}

/// Action returned by [`AdaptiveConcurrencyController::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConcurrencyDecision {
    /// Increase worker target to `target_workers` (line not saturated, positive throughput gain).
    ScaleUp {
        /// New desired worker count (`<= max_workers`).
        target_workers: u16,
        /// Evaluated utility score `U(N) = V * sqrt(g)`.
        utility: f64,
        /// Queueing delay gradient `g = RTT_min / RTT_ewma`.
        gradient: f64,
    },
    /// Reduce worker target cleanly.
    StepDown {
        /// New desired worker count (`>= 1`).
        target_workers: u16,
        /// Root cause for the step-down.
        reason: StepDownReason,
    },
    /// Hold steady at `target_workers`.
    Hold {
        /// Current desired worker count.
        target_workers: u16,
        /// Evaluated utility score `U(N)`.
        utility: f64,
        /// Queueing delay gradient `g`.
        gradient: f64,
    },
}

impl ConcurrencyDecision {
    /// Returns the resulting target worker count for any decision variant.
    #[must_use]
    pub const fn target_workers(self) -> u16 {
        match self {
            Self::ScaleUp { target_workers, .. }
            | Self::StepDown { target_workers, .. }
            | Self::Hold { target_workers, .. } => target_workers,
        }
    }
}

/// Configuration parameters for [`AdaptiveConcurrencyController`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveConcurrencyConfig {
    /// Lower bound on worker concurrency (always `>= 1`).
    pub min_workers: u16,
    /// Hard upper bound on worker concurrency (`1..=32`).
    pub max_workers: u16,
    /// Delay gradient floor below which Bufferbloat braking activates (default `0.72`).
    pub bufferbloat_gradient_floor: f64,
    /// Minimum delay gradient required before scaling up (default `0.80`).
    pub scale_up_gradient_min: f64,
    /// Minimum fractional throughput improvement (`0.08` = `+8%`) required to keep scaling up.
    pub min_throughput_gain_ratio: f64,
}

impl Default for AdaptiveConcurrencyConfig {
    fn default() -> Self {
        Self {
            min_workers: 1,
            max_workers: 32,
            bufferbloat_gradient_floor: 0.72,
            scale_up_gradient_min: 0.80,
            min_throughput_gain_ratio: 0.08,
        }
    }
}

/// Periodic observation fed into [`AdaptiveConcurrencyController::evaluate`] every `1.5–2.0s`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConcurrencyObservation {
    /// Currently active streaming workers.
    pub active_workers: u16,
    /// Aggregate download throughput (`bytes/sec`).
    pub throughput_bps: f64,
    /// Smoothed/latest kernel or protocol RTT in microseconds (`WSAIoctl(SIO_TCP_INFO)`).
    pub rtt_us: Option<u64>,
    /// Windowed minimum baseline RTT in microseconds.
    pub min_rtt_us: Option<u64>,
    /// `true` if the host emitted `429` or `503` rate-limit pressure in this interval.
    pub rate_limited: bool,
    /// Optional discovered hard ceiling from `SmartHostProfileCache` (e.g. `Some(1)` on Pixeldrain).
    pub hard_cap: Option<u16>,
    /// Whether dynamic auto-scaling beyond the initial configured worker count is enabled.
    pub auto_scale_enabled: bool,
}

/// Sub-5ms (`5_000 µs`) kernel scheduling jitter floor ignored by `compute_delay_gradient` so
/// ultra-low-latency fiber/LAN/loopback links never trigger false Bufferbloat braking (`P1.7-T11`).
pub const MIN_QUEUEING_DELAY_JITTER_US: u64 = 5_000;

/// Computes the BBR / Gradient2 queueing delay gradient (`clamp(RTT_min / RTT_ewma, 0.1, 1.0)`).
///
/// Defaults cleanly to `1.0` when RTT samples are not yet available or when added queueing delay
/// (`cur_rtt - min_rtt`) is within the `5 ms` OS scheduling jitter floor.
#[must_use]
pub fn compute_delay_gradient(min_rtt_us: Option<u64>, rtt_us: Option<u64>) -> f64 {
    match (min_rtt_us, rtt_us) {
        (Some(min_r), Some(cur_r)) if min_r > 0 && cur_r > 0 => {
            if cur_r.saturating_sub(min_r) <= MIN_QUEUEING_DELAY_JITTER_US {
                return 1.0;
            }
            ((min_r as f64) / (cur_r as f64)).clamp(0.1, 1.0)
        }
        _ => 1.0,
    }
}

/// Computes the anti-Bufferbloat utility function (`U(N) = throughput_bps * sqrt(gradient)` — §8.5).
#[must_use]
pub fn compute_utility(throughput_bps: f64, gradient: f64) -> f64 {
    if !throughput_bps.is_finite() || throughput_bps <= 0.0 {
        return 0.0;
    }
    let g = if gradient.is_finite() {
        gradient.clamp(0.1, 1.0)
    } else {
        1.0
    };
    throughput_bps * g.sqrt()
}

/// Pure `BBR / Gradient2 + AIMD` concurrency controller (`P1.7-T01` / `ADR-021`).
#[derive(Debug, Clone)]
pub struct AdaptiveConcurrencyController {
    cfg: AdaptiveConcurrencyConfig,
    current_target: u16,
    baseline_throughput_bps: f64,
    baseline_utility: f64,
    has_probed_scale_up: bool,
    plateau_ticks: u8,
    cruise_remaining_ticks: u8,
}

impl AdaptiveConcurrencyController {
    /// Initializes a controller starting at `initial_workers` (clamped within `cfg`).
    #[must_use]
    pub fn new(initial_workers: u16, cfg: AdaptiveConcurrencyConfig) -> Self {
        let min_w = cfg.min_workers.max(1);
        let max_w = cfg.max_workers.clamp(min_w, 32);
        let normalized_cfg = AdaptiveConcurrencyConfig {
            min_workers: min_w,
            max_workers: max_w,
            ..cfg
        };
        Self {
            cfg: normalized_cfg,
            current_target: initial_workers.clamp(min_w, max_w),
            baseline_throughput_bps: 0.0,
            baseline_utility: 0.0,
            has_probed_scale_up: false,
            plateau_ticks: 0,
            cruise_remaining_ticks: 0,
        }
    }

    /// Returns the controller's current target worker count.
    #[must_use]
    pub const fn current_target(&self) -> u16 {
        self.current_target
    }

    /// Evaluates one periodic telemetry window and returns the next [`ConcurrencyDecision`].
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(&mut self, obs: ConcurrencyObservation) -> ConcurrencyDecision {
        let ceiling = obs.hard_cap.map_or(self.cfg.max_workers, |cap| {
            cap.clamp(self.cfg.min_workers, self.cfg.max_workers)
        });

        // 1. Respect explicit host hard ceiling unconditionally (e.g. Pixeldrain `hard_cap = 1`)
        if self.current_target > ceiling {
            self.current_target = ceiling;
            self.cruise_remaining_ticks = 8;
            return ConcurrencyDecision::StepDown {
                target_workers: ceiling,
                reason: StepDownReason::HostHardCap,
            };
        }

        // 2. AIMD Multiplicative Decrease on `429` / `503` rate-limit pressure
        if obs.rate_limited {
            let reduced = (self.current_target / 2).max(self.cfg.min_workers);
            self.current_target = reduced;
            self.cruise_remaining_ticks = 6;
            self.plateau_ticks = 0;
            return ConcurrencyDecision::StepDown {
                target_workers: reduced,
                reason: StepDownReason::AimdRateLimit,
            };
        }

        let gradient = compute_delay_gradient(obs.min_rtt_us, obs.rtt_us);
        let throughput = if obs.throughput_bps.is_finite() && obs.throughput_bps > 0.0 {
            obs.throughput_bps
        } else {
            0.0
        };
        let utility = compute_utility(throughput, gradient);

        // 3. Bufferbloat brake (`g < 0.72` without strong compensating speed gain)
        let strong_gain =
            self.baseline_throughput_bps > 0.0 && throughput > self.baseline_throughput_bps * 1.10;
        if gradient < self.cfg.bufferbloat_gradient_floor
            && self.current_target > self.cfg.min_workers
            && !strong_gain
        {
            self.current_target = self
                .current_target
                .saturating_sub(1)
                .max(self.cfg.min_workers);
            self.baseline_throughput_bps = throughput;
            self.baseline_utility = utility;
            self.cruise_remaining_ticks = 4;
            return ConcurrencyDecision::StepDown {
                target_workers: self.current_target,
                reason: StepDownReason::Bufferbloat,
            };
        }

        if self.cruise_remaining_ticks > 0 {
            self.cruise_remaining_ticks -= 1;
            self.baseline_throughput_bps = self.baseline_throughput_bps.max(throughput);
            self.baseline_utility = self.baseline_utility.max(utility);
            return ConcurrencyDecision::Hold {
                target_workers: self.current_target,
                utility,
                gradient,
            };
        }

        // 4. Additive/progressive auto-scale up when host is uncapped, line has headroom, and speed grows!
        if obs.auto_scale_enabled
            && obs.hard_cap.is_none()
            && self.current_target < ceiling
            && obs.active_workers >= self.current_target
            && throughput > 0.0
            && gradient >= self.cfg.scale_up_gradient_min
        {
            let gain_ratio = if self.baseline_throughput_bps > 0.0 {
                (throughput - self.baseline_throughput_bps) / self.baseline_throughput_bps
            } else {
                0.0
            };
            let utility_improved =
                self.baseline_utility <= 0.0 || utility >= self.baseline_utility * 1.02;

            let should_probe = !self.has_probed_scale_up
                || (gain_ratio >= self.cfg.min_throughput_gain_ratio && utility_improved);

            if should_probe {
                let step: u16 = if self.current_target < 8 {
                    2
                } else if self.current_target < 16 {
                    4
                } else {
                    8
                };
                let next_target = self.current_target.saturating_add(step).min(ceiling);
                self.current_target = next_target;
                self.baseline_throughput_bps = throughput;
                self.baseline_utility = utility;
                self.has_probed_scale_up = true;
                self.plateau_ticks = 0;
                return ConcurrencyDecision::ScaleUp {
                    target_workers: next_target,
                    utility,
                    gradient,
                };
            }

            // Throughput plateaued: enter steady cruise for ~20-30s instead of thrashing connections
            self.plateau_ticks = self.plateau_ticks.saturating_add(1);
            if self.plateau_ticks >= 2 {
                self.cruise_remaining_ticks = 12;
                self.plateau_ticks = 0;
            }
        }

        if self.baseline_throughput_bps <= 0.0 && throughput > 0.0 {
            self.baseline_throughput_bps = throughput;
            self.baseline_utility = utility;
        }

        ConcurrencyDecision::Hold {
            target_workers: self.current_target,
            utility,
            gradient,
        }
    }
}

/// Evaluates the `End-Game Mode` Hedged Request predicate (`Vortex-Architecture-Design.md` §8.4 / `P1.7-T01`):
///
/// $$B_{\text{total\_rem}} \le \min(0.02 \cdot S,\; 16\text{ MiB}) \;\land\; \hat{v}_i < 0.30 \cdot \operatorname{median}_j(\hat{v}_j) \;\land\; \frac{B_{\text{rem}, i}}{\hat{v}_i} > \max(1.5\text{ s},\; 3 \cdot \text{RTT}_{\text{ewma}})$$
///
/// Unconditionally returns `false` when `host_hard_cap == Some(1)` (protects single-stream hosts like Pixeldrain!).
#[must_use]
pub fn worth_hedging(
    total_size: u64,
    total_remaining: u64,
    worker_remaining: u64,
    worker_speed_bps: f64,
    median_speed_bps: f64,
    rtt_secs: f64,
    host_hard_cap: Option<u16>,
) -> bool {
    if host_hard_cap.is_some_and(|cap| cap <= 1) {
        return false;
    }
    if total_size < 2 * 1024 * 1024
        || total_remaining == 0
        || worker_remaining < ENDGAME_MIN_WORKER_REMAINING_BYTES
    {
        return false;
    }
    if !median_speed_bps.is_finite() || median_speed_bps <= 1.0 {
        return false;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let two_percent = ((total_size as f64) * ENDGAME_REMAINING_FRACTION).ceil() as u64;
    let endgame_window = two_percent.clamp(2 * 1024 * 1024, ENDGAME_MAX_REMAINING_BYTES);
    if total_remaining > endgame_window {
        return false;
    }

    let clean_worker_speed = if worker_speed_bps.is_finite() && worker_speed_bps > 0.0 {
        worker_speed_bps
    } else {
        1.0
    };
    if clean_worker_speed >= ENDGAME_STRAGGLER_SPEED_RATIO * median_speed_bps {
        return false;
    }

    let clean_rtt = if rtt_secs.is_finite() && rtt_secs >= 0.0 {
        rtt_secs
    } else {
        0.05
    };
    let min_eta = ENDGAME_MIN_STRAGGLER_ETA_SECS.max(3.0 * clean_rtt);
    let projected_eta = (worker_remaining as f64) / clean_worker_speed;
    projected_eta > min_eta
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn scales_up_on_uncapped_improving_host_and_brakes_on_bufferbloat_and_aimd() {
        let mut ctrl = AdaptiveConcurrencyController::new(4, AdaptiveConcurrencyConfig::default());
        assert_eq!(ctrl.current_target(), 4);

        // First probe scales 4 -> 6 when RTT is healthy (`g = 1.0`)
        let d1 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 4,
            throughput_bps: 4_000_000.0,
            rtt_us: Some(25_000),
            min_rtt_us: Some(25_000),
            rate_limited: false,
            hard_cap: None,
            auto_scale_enabled: true,
        });
        assert!(matches!(
            d1,
            ConcurrencyDecision::ScaleUp {
                target_workers: 6,
                ..
            }
        ));

        // +50% throughput gain scales 6 -> 8
        let d2 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 6,
            throughput_bps: 6_000_000.0,
            rtt_us: Some(26_000),
            min_rtt_us: Some(25_000),
            rate_limited: false,
            hard_cap: None,
            auto_scale_enabled: true,
        });
        assert!(matches!(
            d2,
            ConcurrencyDecision::ScaleUp {
                target_workers: 8,
                ..
            }
        ));

        // +50% throughput gain scales 8 -> 12
        let d3 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 8,
            throughput_bps: 9_000_000.0,
            rtt_us: Some(27_000),
            min_rtt_us: Some(25_000),
            rate_limited: false,
            hard_cap: None,
            auto_scale_enabled: true,
        });
        assert!(matches!(
            d3,
            ConcurrencyDecision::ScaleUp {
                target_workers: 12,
                ..
            }
        ));

        // Bufferbloat (`RTT` doubles: `g = 0.50 < 0.72`) brakes cleanly (`12 -> 11`)
        let d4 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 12,
            throughput_bps: 9_100_000.0,
            rtt_us: Some(50_000),
            min_rtt_us: Some(25_000),
            rate_limited: false,
            hard_cap: None,
            auto_scale_enabled: true,
        });
        assert_eq!(
            d4,
            ConcurrencyDecision::StepDown {
                target_workers: 11,
                reason: StepDownReason::Bufferbloat,
            }
        );

        // `429`/`503` rate limit triggers AIMD multiplicative decrease (`11 -> 5`)
        let d5 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 11,
            throughput_bps: 5_000_000.0,
            rtt_us: Some(25_000),
            min_rtt_us: Some(25_000),
            rate_limited: true,
            hard_cap: None,
            auto_scale_enabled: true,
        });
        assert_eq!(
            d5,
            ConcurrencyDecision::StepDown {
                target_workers: 5,
                reason: StepDownReason::AimdRateLimit,
            }
        );

        // Host hard cap (`Some(1)` e.g. Pixeldrain) clamps unconditionally
        let d6 = ctrl.evaluate(ConcurrencyObservation {
            active_workers: 5,
            throughput_bps: 5_000_000.0,
            rtt_us: Some(25_000),
            min_rtt_us: Some(25_000),
            rate_limited: false,
            hard_cap: Some(1),
            auto_scale_enabled: true,
        });
        assert_eq!(
            d6,
            ConcurrencyDecision::StepDown {
                target_workers: 1,
                reason: StepDownReason::HostHardCap,
            }
        );
        assert_eq!(d6.target_workers(), 1);
    }

    #[test]
    fn endgame_hedging_respects_pixeldrain_cap_and_straggler_inequality() {
        let total = 200 * 1024 * 1024;
        // Straggler at 100 KiB/s when peers run at 10 MiB/s with 2 MiB remaining -> hedge!
        assert!(worth_hedging(
            total,
            2 * 1024 * 1024,
            2 * 1024 * 1024,
            100_000.0,
            10_000_000.0,
            0.03,
            None,
        ));

        // Disabled on single-stream capped host (`Pixeldrain`)!
        assert!(!worth_hedging(
            total,
            2 * 1024 * 1024,
            2 * 1024 * 1024,
            100_000.0,
            10_000_000.0,
            0.03,
            Some(1),
        ));

        // Healthy worker at 50% median -> no hedge
        assert!(!worth_hedging(
            total,
            2 * 1024 * 1024,
            2 * 1024 * 1024,
            5_000_000.0,
            10_000_000.0,
            0.03,
            None,
        ));

        // Too much remaining (> 16 MiB) -> not in End-Game window
        assert!(!worth_hedging(
            2 * 1024 * 1024 * 1024,
            32 * 1024 * 1024,
            4 * 1024 * 1024,
            100_000.0,
            10_000_000.0,
            0.03,
            None,
        ));
    }

    proptest! {
        #[test]
        fn proptest_controller_never_exceeds_hard_cap_or_max_workers(
            init_w in 1_u16..=32,
            max_w in 1_u16..=32,
            hard_cap in prop::option::of(1_u16..=32),
            bps in 0.0_f64..1_000_000_000.0,
            min_rtt in 1_u64..500_000,
            cur_rtt in 1_u64..1_000_000,
            rate_limited in any::<bool>(),
            auto_scale in any::<bool>(),
        ) {
            let cfg = AdaptiveConcurrencyConfig {
                min_workers: 1,
                max_workers: max_w,
                ..AdaptiveConcurrencyConfig::default()
            };
            let mut ctrl = AdaptiveConcurrencyController::new(init_w, cfg);
            let decision = ctrl.evaluate(ConcurrencyObservation {
                active_workers: ctrl.current_target(),
                throughput_bps: bps,
                rtt_us: Some(cur_rtt),
                min_rtt_us: Some(min_rtt),
                rate_limited,
                hard_cap,
                auto_scale_enabled: auto_scale,
            });
            let ceiling = hard_cap.map_or(max_w, |c| c.min(max_w).max(1));
            prop_assert!(decision.target_workers() >= 1);
            prop_assert!(decision.target_workers() <= ceiling);
            let g = compute_delay_gradient(Some(min_rtt), Some(cur_rtt));
            prop_assert!((0.1..=1.0).contains(&g));
            prop_assert!(compute_utility(bps, g) >= 0.0);
        }
    }
}
