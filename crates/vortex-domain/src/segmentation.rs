//! Hybrid Lease-Based Dynamic Segmentation, Work Stealing, Split Worthiness Inequality,
//! and Dual-Horizon Continuous EWMA (`P1-T04` / `Vortex-Architecture-Design.md` §8.3 & §8.6).

#![allow(clippy::cast_precision_loss)]

use crate::range_set::{ActiveSegment, SegmentMap};
use crate::value_objects::{ByteRange, WorkerId};

/// Alignment quantum (`64 KiB`) for bounded chunk leases and work-stealing split points.
pub const LEASE_ALIGN_BYTES: u64 = 64 * 1024;

/// Minimum HTTP/1.1 bounded chunk lease (`2 MiB` — `P1.7-T01` / `ADR-021`).
pub const MIN_CHUNK_LEASE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum HTTP/1.1 bounded chunk lease (`64 MiB` for high-speed 200 Mbps–10 Gbps links — `P1.7-T01`).
pub const MAX_CHUNK_LEASE_BYTES: u64 = 64 * 1024 * 1024;

/// Target HTTP/1.1 chunk transfer duration (`3.0s`).
pub const TARGET_CHUNK_DURATION_SECS: f64 = 3.0;

/// Minimum remaining unleased bytes required to split an active worker (`2 MiB` — `§8.3.4`).
pub const MIN_SPLIT_REMAINING_BYTES: u64 = 2 * 1024 * 1024;

/// Minimum net time saved (`0.5s`) required to justify splitting an active segment (`§8.3.4`).
pub const MIN_SPLIT_TIME_SAVING_SECS: f64 = 0.5;

/// Fast EWMA horizon (`1.5s`) for responsive live speed display (`§8.6.1`).
pub const EWMA_TAU_FAST_SECS: f64 = 1.5;

/// Slow EWMA horizon (`8.0s`) for jitter-free ETA estimation (`§8.6.1`).
pub const EWMA_TAU_SLOW_SECS: f64 = 8.0;

/// Aligns `bytes` up to the nearest `64 KiB` boundary (saturating at `u64::MAX`).
#[must_use]
pub const fn align_up_64kib(bytes: u64) -> u64 {
    let mask = LEASE_ALIGN_BYTES - 1;
    bytes.saturating_add(mask) & !mask
}

/// Aligns `bytes` down to the nearest `64 KiB` boundary.
#[must_use]
pub const fn align_down_64kib(bytes: u64) -> u64 {
    bytes & !(LEASE_ALIGN_BYTES - 1)
}

/// Computes the adaptive HTTP/1.1 bounded chunk lease size (`§8.3.2`):
///
/// $$C = \operatorname{clamp}\!\big(\operatorname{align}_{64\text{KiB}}(\hat{v}_{\text{worker}} \cdot 3\text{s}),\; 1\text{ MiB},\; 32\text{ MiB}\big)$$
#[must_use]
pub fn compute_chunk_lease_bytes(worker_speed_bps: f64) -> u64 {
    if !worker_speed_bps.is_finite() || worker_speed_bps <= 0.0 {
        return MIN_CHUNK_LEASE_BYTES;
    }
    let target = worker_speed_bps * TARGET_CHUNK_DURATION_SECS;
    if target <= MIN_CHUNK_LEASE_BYTES as f64 {
        return MIN_CHUNK_LEASE_BYTES;
    }
    if target >= MAX_CHUNK_LEASE_BYTES as f64 {
        return MAX_CHUNK_LEASE_BYTES;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw_bytes = target as u64;
    align_up_64kib(raw_bytes).clamp(MIN_CHUNK_LEASE_BYTES, MAX_CHUNK_LEASE_BYTES)
}

/// Evaluates the Split Worthiness Inequality (`Vortex-Architecture-Design.md` §8.3.4):
///
/// $$\frac{B_{\text{rem}}}{\hat{v}_{\text{slow}}} - \left(T_{\text{overhead}} + \frac{B_{\text{rem}}}{\hat{v}_{\text{slow}} + \hat{v}_{\text{new}}}\right) > 0.5\text{ s}, \quad B_{\text{rem}} \ge 2\text{ MiB}$$
///
/// where $T_{\text{overhead}} = 1.0 \cdot \text{RTT}$ for warm Keep-Alive/H2 connections,
/// and $2.5 \cdot \text{RTT}$ for cold TCP + TLS 1.3 1-RTT connections.
#[must_use]
pub fn worth_splitting(
    rem_bytes: u64,
    slow_speed_bps: f64,
    expected_new_speed_bps: f64,
    rtt_secs: f64,
    warm_connection: bool,
) -> bool {
    if rem_bytes < MIN_SPLIT_REMAINING_BYTES {
        return false;
    }
    let clean_rtt = if rtt_secs.is_finite() && rtt_secs >= 0.0 {
        rtt_secs
    } else {
        0.05
    };
    let t_overhead = if warm_connection {
        1.0 * clean_rtt
    } else {
        2.5 * clean_rtt
    };

    let v_new = if expected_new_speed_bps.is_finite() && expected_new_speed_bps > 0.0 {
        expected_new_speed_bps
    } else {
        // Conservative cold-start estimate: 256 KiB/s
        262_144.0
    };

    if !slow_speed_bps.is_finite() || slow_speed_bps <= 1.0 {
        // Stalled or newly initialized worker holding >= 2 MiB unleased bytes: always worth helping
        return (rem_bytes as f64 / v_new) > MIN_SPLIT_TIME_SAVING_SECS;
    }

    let t_alone = (rem_bytes as f64) / slow_speed_bps;
    let t_together = t_overhead + (rem_bytes as f64) / (slow_speed_bps + v_new);
    (t_alone - t_together) > MIN_SPLIT_TIME_SAVING_SECS
}

/// Outcome of requesting the next bounded HTTP/1.1 chunk lease from [`SegmentationPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseDecision {
    /// Worker continued into its next bounded HTTP/1.1 chunk lease on its existing assignment
    /// (`100%` Keep-Alive socket reuse!).
    ContinuedExisting {
        /// Exact HTTP `Range` to fetch in this bounded request.
        http_lease: ByteRange,
    },
    /// Worker claimed a previously unassigned gap `[assigned_range.start, assigned_range.end)`.
    ClaimedUnassigned {
        /// Full logical assignment reserved for this worker.
        assigned_range: ByteRange,
        /// First bounded HTTP `Range` lease inside `assigned_range`.
        http_lease: ByteRange,
    },
    /// Fast worker stole the unleased tail of `victim_worker` without interrupting `victim_worker`'s
    /// in-flight HTTP/1.1 chunk (`100%` Keep-Alive preserved on BOTH workers!).
    StoleUnleasedTail {
        /// Slow worker whose unleased tail was halved.
        victim_worker: WorkerId,
        /// Logical interval stolen by this worker.
        stolen_range: ByteRange,
        /// First bounded HTTP `Range` lease inside `stolen_range`.
        http_lease: ByteRange,
    },
    /// No splittable or unassigned work remains for an additional worker right now.
    NoWorkAvailable,
}

/// Hybrid protocol-aware segmentation and work-stealing policy (`ADR-009`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentationPolicy;

fn compute_equal_work_split_point(
    victim: &ActiveSegment,
    unleased: ByteRange,
    fallback_mid: u64,
) -> u64 {
    let old_end = victim.assigned_range.end();
    let equal_work_mid = victim.cursor + (old_end.saturating_sub(victim.cursor) / 2);
    let target_mid = equal_work_mid.max(unleased.start());
    let aligned_mid = align_down_64kib(target_mid);
    if aligned_mid > unleased.start() && aligned_mid < unleased.end() {
        aligned_mid
    } else {
        let aligned_fallback = align_down_64kib(fallback_mid);
        if aligned_fallback > unleased.start() && aligned_fallback < unleased.end() {
            aligned_fallback
        } else {
            fallback_mid
        }
    }
}

impl SegmentationPolicy {
    /// Allocates the next bounded HTTP/1.1 chunk lease for `worker_id`, preserving Keep-Alive
    /// and guaranteeing 100% disjoint coverage of `[0, total_size)`.
    pub fn acquire_next_lease(
        map: &mut SegmentMap,
        worker_id: WorkerId,
        worker_speed_bps: f64,
        rtt_secs: f64,
        warm_connection: bool,
    ) -> LeaseDecision {
        let lease_span = compute_chunk_lease_bytes(worker_speed_bps);

        // 1. If this worker already holds an active assignment with unleased bytes after `active_lease_end`
        //    (and its previous lease has completed: `cursor >= active_lease_end`), advance its lease!
        if let Some(existing) = map.active.iter_mut().find(|s| s.worker_id == worker_id) {
            existing.speed_bps = worker_speed_bps.max(0.0);
            if existing.cursor < existing.active_lease_end {
                if let Ok(rem_in_lease) = ByteRange::new(existing.cursor, existing.active_lease_end)
                {
                    return LeaseDecision::ContinuedExisting {
                        http_lease: rem_in_lease,
                    };
                }
            } else if existing.active_lease_end < existing.assigned_range.end() {
                let next_start = existing.active_lease_end;
                let next_end = next_start
                    .saturating_add(lease_span)
                    .min(existing.assigned_range.end());
                existing.active_lease_end = next_end;
                if let Ok(http_lease) = ByteRange::new(next_start, next_end) {
                    return LeaseDecision::ContinuedExisting { http_lease };
                }
            }
        }

        // Clean up any exhausted slot for `worker_id`
        map.active
            .retain(|s| s.worker_id != worker_id || s.cursor < s.assigned_range.end());

        // 2. Claim largest unassigned gap in `[0, total_size)`
        let gaps = map.unassigned_gaps();
        if let Some(largest_gap) = gaps.into_iter().max_by_key(|g| g.len()) {
            let first_end = largest_gap
                .start()
                .saturating_add(lease_span)
                .min(largest_gap.end());
            if let Ok(http_lease) = ByteRange::new(largest_gap.start(), first_end) {
                let mut seg = ActiveSegment::new(worker_id, largest_gap, first_end);
                seg.speed_bps = worker_speed_bps.max(0.0);
                map.active.push(seg);
                return LeaseDecision::ClaimedUnassigned {
                    assigned_range: largest_gap,
                    http_lease,
                };
            }
        }

        // 3. Work Stealing: inspect active workers with an unleased tail `[active_lease_end, assigned_end)`
        //    and steal from the worker with the longest true remaining ETA (`assigned_end - cursor`),
        //    equalizing remaining work (`(cursor + assigned_end) / 2`) while never crossing `active_lease_end`!
        let mut best_victim_idx: Option<usize> = None;
        let mut worst_eta = 0.0_f64;

        for (idx, candidate) in map.active.iter().enumerate() {
            if candidate.worker_id == worker_id {
                continue;
            }
            let Some(unleased) = candidate.unleased_tail() else {
                continue;
            };
            if !worth_splitting(
                unleased.len(),
                candidate.speed_bps,
                worker_speed_bps,
                rtt_secs,
                warm_connection,
            ) {
                continue;
            }
            let rem_work = candidate
                .assigned_range
                .end()
                .saturating_sub(candidate.cursor);
            let denom = candidate.speed_bps.max(1.0);
            let eta = (rem_work as f64) / denom;
            if eta > worst_eta {
                worst_eta = eta;
                best_victim_idx = Some(idx);
            }
        }

        if let Some(v_idx) = best_victim_idx
            && let Some(victim) = map.active.get_mut(v_idx)
            && let Some(unleased) = victim.unleased_tail()
            && let Some(fallback_mid) = unleased.midpoint()
        {
            let old_end = victim.assigned_range.end();
            let split_point = compute_equal_work_split_point(victim, unleased, fallback_mid);
            let victim_id = victim.worker_id;
            if let (Ok(new_victim_range), Ok(stolen_range)) = (
                ByteRange::new(victim.assigned_range.start(), split_point),
                ByteRange::new(split_point, old_end),
            ) {
                victim.assigned_range = new_victim_range;
                let first_end = stolen_range
                    .start()
                    .saturating_add(lease_span)
                    .min(stolen_range.end());
                if let Ok(http_lease) = ByteRange::new(stolen_range.start(), first_end) {
                    let mut thief = ActiveSegment::new(worker_id, stolen_range, first_end);
                    thief.speed_bps = worker_speed_bps.max(0.0);
                    map.active.push(thief);
                    return LeaseDecision::StoleUnleasedTail {
                        victim_worker: victim_id,
                        stolen_range,
                        http_lease,
                    };
                }
            }
        }

        LeaseDecision::NoWorkAvailable
    }
}

/// Dual-Horizon Time-Continuous EWMA speed & ETA estimator (`Vortex-Architecture-Design.md` §8.6.1):
///
/// $$\alpha(\Delta t, \tau) = 1 - e^{-\Delta t / \tau}, \qquad \hat{v}_t = \alpha \frac{\Delta B}{\Delta t} + (1 - \alpha)\hat{v}_{t-1}$$
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DualHorizonEwma {
    fast_bps: f64,
    slow_bps: f64,
    initialized: bool,
}

impl DualHorizonEwma {
    /// Creates an uninitialized [`DualHorizonEwma`] at `0 B/s`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fast_bps: 0.0,
            slow_bps: 0.0,
            initialized: false,
        }
    }

    /// Feeds `delta_bytes` transferred over `dt_secs` seconds.
    pub fn observe(&mut self, delta_bytes: u64, dt_secs: f64) {
        if !dt_secs.is_finite() || dt_secs <= 1e-6 {
            return;
        }
        let sample_bps = (delta_bytes as f64) / dt_secs;
        if !self.initialized {
            self.fast_bps = sample_bps;
            self.slow_bps = sample_bps;
            self.initialized = true;
            return;
        }
        let alpha_fast = 1.0 - (-dt_secs / EWMA_TAU_FAST_SECS).exp();
        let alpha_slow = 1.0 - (-dt_secs / EWMA_TAU_SLOW_SECS).exp();
        self.fast_bps = alpha_fast * sample_bps + (1.0 - alpha_fast) * self.fast_bps;
        self.slow_bps = alpha_slow * sample_bps + (1.0 - alpha_slow) * self.slow_bps;
    }

    /// Live speed estimate (`tau = 1.5s`, bytes/sec).
    #[must_use]
    pub const fn speed_fast_bps(&self) -> f64 {
        self.fast_bps
    }

    /// Stabilized speed estimate (`tau = 8.0s`, bytes/sec).
    #[must_use]
    pub const fn speed_slow_bps(&self) -> f64 {
        self.slow_bps
    }

    /// Stabilized ETA in seconds (`remaining / max(v_slow, 1 B/s)` — `§8.6.1`).
    #[must_use]
    pub fn eta_secs(&self, remaining_bytes: Option<u64>) -> Option<u64> {
        let rem = remaining_bytes?;
        if rem == 0 {
            return Some(0);
        }
        if !self.initialized || self.slow_bps < 1.0 {
            return None;
        }
        let secs = (rem as f64 / self.slow_bps.max(1.0)).ceil();
        if secs.is_finite() && secs >= 0.0 && secs <= (u64::MAX as f64) {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(secs as u64)
        } else {
            None
        }
    }
}

impl Default for DualHorizonEwma {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn adaptive_chunk_lease_clamps_and_aligns_to_64kib() {
        assert_eq!(compute_chunk_lease_bytes(0.0), MIN_CHUNK_LEASE_BYTES);
        assert_eq!(compute_chunk_lease_bytes(100_000.0), MIN_CHUNK_LEASE_BYTES);
        // 2.1 MiB/s * 3s = 6.3 MiB -> aligned up to 64 KiB
        let c = compute_chunk_lease_bytes(2_202_009.0);
        assert_eq!(c % LEASE_ALIGN_BYTES, 0);
        assert!((MIN_CHUNK_LEASE_BYTES..=MAX_CHUNK_LEASE_BYTES).contains(&c));
        assert_eq!(
            compute_chunk_lease_bytes(100_000_000.0),
            MAX_CHUNK_LEASE_BYTES
        );
    }

    #[test]
    fn split_worthiness_respects_2mib_floor_and_rtt_overhead() {
        // < 2 MiB is never split
        assert!(!worth_splitting(
            1_500_000,
            500_000.0,
            5_000_000.0,
            0.02,
            true
        ));
        // 20 MiB on slow 1 MiB/s worker (20s alone vs ~3.4s together) -> worth splitting!
        assert!(worth_splitting(
            20 * 1024 * 1024,
            1_048_576.0,
            5_242_880.0,
            0.05,
            false
        ));
        // 4 MiB on blazing 100 MiB/s worker (0.04s remaining < 0.5s threshold) -> NOT worth splitting!
        assert!(!worth_splitting(
            4 * 1024 * 1024,
            100_000_000.0,
            100_000_000.0,
            0.05,
            true
        ));
    }

    #[test]
    fn dual_horizon_ewma_converges_and_computes_stable_eta() {
        let mut ewma = DualHorizonEwma::new();
        assert_eq!(ewma.eta_secs(Some(1000)), None);
        ewma.observe(10_000_000, 1.0); // 10 MB/s
        assert!((ewma.speed_fast_bps() - 10_000_000.0).abs() < 1.0);
        assert_eq!(ewma.eta_secs(Some(25_000_000)), Some(3));
        assert_eq!(ewma.eta_secs(Some(0)), Some(0));
    }

    proptest! {
        #[test]
        fn proptest_hybrid_leases_and_work_stealing_cover_100_percent_with_zero_overlap(
            file_size in 1_u64..(64 * 1024 * 1024),
            worker_speeds in prop::collection::vec(256_000.0_f64..12_000_000.0, 2..=8),
        ) {
            let mut map = SegmentMap::new(Some(file_size));
            let mut leased_history: Vec<ByteRange> = Vec::new();

            // Simulate asynchronous multi-worker lease execution + work stealing until complete
            for step in 0..4096_usize {
                if map.is_complete() {
                    break;
                }
                let w_idx = step % worker_speeds.len();
                let worker_id = WorkerId::new(u16::try_from(w_idx + 1).unwrap());
                let speed = worker_speeds[w_idx];

                let decision = SegmentationPolicy::acquire_next_lease(
                    &mut map,
                    worker_id,
                    speed,
                    0.03,
                    step >= worker_speeds.len(),
                );

                match decision {
                    LeaseDecision::ContinuedExisting { http_lease }
                    | LeaseDecision::ClaimedUnassigned { http_lease, .. }
                    | LeaseDecision::StoleUnleasedTail { http_lease, .. } => {
                        // Verify ZERO overlap with any previously granted HTTP lease!
                        for prev in &leased_history {
                            prop_assert!(
                                !prev.overlaps(http_lease),
                                "Overlap detected between {prev} and {http_lease}"
                            );
                        }
                        leased_history.push(http_lease);
                        map.record_written(worker_id, http_lease).unwrap();
                    }
                    LeaseDecision::NoWorkAvailable => {
                        // Drain one remaining active worker's current in-flight lease
                        if let Some(in_flight) = map.active.first().cloned()
                            && let Ok(rem) = ByteRange::new(in_flight.cursor, in_flight.active_lease_end)
                        {
                            map.record_written(in_flight.worker_id, rem).unwrap();
                        }
                    }
                }
            }

            prop_assert!(map.is_complete());
            prop_assert_eq!(map.completed.total_bytes(), file_size);
            prop_assert_eq!(map.completed.interval_count(), 1);
            let total_leased: u64 = leased_history.iter().map(|r| r.len()).sum();
            prop_assert_eq!(total_leased, file_size);
        }

        #[test]
        fn proptest_abort_and_reclaim_worker_guarantees_100_percent_completion_on_step_down(
            file_size in (2 * 1024 * 1024_u64)..(48 * 1024 * 1024),
            abort_after_steps in 1_usize..12,
        ) {
            let mut map = SegmentMap::new(Some(file_size));
            let w1 = WorkerId::new(1);
            let w2 = WorkerId::new(2);
            let w3 = WorkerId::new(3);
            let mut w2_aborted = false;
            let mut w3_aborted = false;

            for step in 0..2048_usize {
                if map.is_complete() {
                    break;
                }
                if step == abort_after_steps && !w3_aborted {
                    let _reclaimed = map.abort_and_reclaim_worker(w3);
                    w3_aborted = true;
                }
                if step == abort_after_steps + 2 && !w2_aborted {
                    let _reclaimed = map.abort_and_reclaim_worker(w2);
                    w2_aborted = true;
                }

                let active_pool: &[WorkerId] = match (w2_aborted, w3_aborted) {
                    (false, false) => &[w1, w2, w3],
                    (false, true) => &[w1, w2],
                    (true, false) => &[w1, w3],
                    (true, true) => &[w1],
                };
                let wid = active_pool[step % active_pool.len()];
                let decision = SegmentationPolicy::acquire_next_lease(
                    &mut map,
                    wid,
                    4_000_000.0,
                    0.025,
                    true,
                );
                match decision {
                    LeaseDecision::ContinuedExisting { http_lease }
                    | LeaseDecision::ClaimedUnassigned { http_lease, .. }
                    | LeaseDecision::StoleUnleasedTail { http_lease, .. } => {
                        // Simulate partial write before an abort on secondary workers
                        let write_len = if wid != w1 && http_lease.len() > 4096 {
                            http_lease.len() / 2
                        } else {
                            http_lease.len()
                        };
                        let written = ByteRange::new(http_lease.start(), http_lease.start() + write_len).unwrap();
                        map.record_written(wid, written).unwrap();
                    }
                    LeaseDecision::NoWorkAvailable => {
                        if let Some(in_flight) = map.active.first().cloned()
                            && let Ok(rem) = ByteRange::new(in_flight.cursor, in_flight.active_lease_end)
                        {
                            map.record_written(in_flight.worker_id, rem).unwrap();
                        }
                    }
                }
            }

            prop_assert!(map.is_complete());
            prop_assert_eq!(map.completed.total_bytes(), file_size);
            prop_assert_eq!(map.completed.interval_count(), 1);
            prop_assert!(map.unassigned_gaps().is_empty());
        }
    }
}
