//! Interval set (`RangeSet`) backed by `BTreeMap<u64, u64>` and `SegmentMap` (`P1-T02`).
//!
//! Guarantees automatic coalescing of overlapping and adjacent `[start, end)` intervals,
//! `O(1)` total completed byte queries, exact gap calculation, and overflow-free `u128`
//! projection onto the 512-bin `<canvas>` state array (`[u8; 512]`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::DomainResult;
use crate::value_objects::{ByteRange, WorkerId};

/// Visual bin count used by the `<canvas>` segment progress bar (`Design-System.md` §4.2).
pub const VISUAL_BIN_COUNT: usize = 512;

/// Bin value indicating an uncompleted, idle byte region.
pub const BIN_REMAINING: u8 = 0;

/// Bin value indicating a completed byte region.
pub const BIN_COMPLETED: u8 = 1;

/// Coalescing set of disjoint, non-adjacent half-open byte intervals `[start, end)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeSet {
    /// Map of `start -> end` maintaining strict invariant: `start_i < end_i < start_{i+1}`.
    intervals: BTreeMap<u64, u64>,
    /// Incrementally maintained sum of `(end - start)` across all intervals (`O(1)` read).
    total_bytes: u64,
}

impl RangeSet {
    /// Creates an empty [`RangeSet`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            intervals: BTreeMap::new(),
            total_bytes: 0,
        }
    }

    /// Inserts `range` (`[start, end)`), merging all overlapping or touching intervals.
    pub fn insert(&mut self, range: ByteRange) {
        let mut merged_start = range.start();
        let mut merged_end = range.end();
        let mut removed_bytes = 0_u64;
        let mut to_remove = SmallVec::<[u64; 8]>::new();

        // Check predecessor interval whose start <= range.start()
        if let Some((&prev_start, &prev_end)) = self.intervals.range(..=merged_start).next_back()
            && prev_end >= merged_start
        {
            merged_start = prev_start;
            if prev_end > merged_end {
                merged_end = prev_end;
            }
            removed_bytes = removed_bytes.saturating_add(prev_end - prev_start);
            to_remove.push(prev_start);
        }

        // Check all intervals starting in `(merged_start, merged_end]` (including adjacent at `merged_end`)
        for (&s, &e) in self.intervals.range(merged_start..=merged_end) {
            if !to_remove.contains(&s) {
                if e > merged_end {
                    merged_end = e;
                }
                removed_bytes = removed_bytes.saturating_add(e - s);
                to_remove.push(s);
            }
        }

        for key in to_remove {
            self.intervals.remove(&key);
        }

        self.intervals.insert(merged_start, merged_end);
        self.total_bytes = self
            .total_bytes
            .saturating_sub(removed_bytes)
            .saturating_add(merged_end - merged_start);
    }

    /// Total number of distinct bytes stored across all coalesced intervals (`O(1)`).
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of disjoint intervals currently stored (`K <= 32` in typical downloads).
    #[must_use]
    pub fn interval_count(&self) -> usize {
        self.intervals.len()
    }

    /// Returns `true` if no bytes have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// Iterates over coalesced [`ByteRange`] intervals in ascending byte order.
    pub fn iter(&self) -> impl Iterator<Item = ByteRange> + '_ {
        self.intervals
            .iter()
            .filter_map(|(&s, &e)| ByteRange::new(s, e).ok())
    }

    /// Returns `true` if `target` is completely covered by a single coalesced interval.
    #[must_use]
    pub fn contains_range(&self, target: ByteRange) -> bool {
        self.intervals
            .range(..=target.start())
            .next_back()
            .is_some_and(|(_, &end)| end >= target.end())
    }

    /// Returns `true` if `[0, total_size)` is 100% covered.
    #[must_use]
    pub fn is_complete(&self, total_size: u64) -> bool {
        if total_size == 0 {
            return true;
        }
        ByteRange::new(0, total_size).is_ok_and(|full| self.contains_range(full))
    }

    /// Total completed bytes strictly within `[0, total_size)`.
    #[must_use]
    pub fn covered_within(&self, total_size: u64) -> u64 {
        if total_size == 0 {
            return 0;
        }
        let mut sum = 0_u64;
        for (&s, &e) in &self.intervals {
            if s >= total_size {
                break;
            }
            let clamped_end = e.min(total_size);
            sum = sum.saturating_add(clamped_end - s);
        }
        sum
    }

    /// Exact number of uncompleted bytes remaining in `[0, total_size)`.
    #[must_use]
    pub fn remaining_bytes(&self, total_size: u64) -> u64 {
        total_size.saturating_sub(self.covered_within(total_size))
    }

    /// Computes all uncompleted `[a, b)` gaps inside `[0, total_size)` in ascending order.
    #[must_use]
    pub fn gaps(&self, total_size: u64) -> Vec<ByteRange> {
        let mut out = Vec::new();
        if total_size == 0 {
            return out;
        }
        let mut cursor = 0_u64;
        for (&s, &e) in &self.intervals {
            if s >= total_size {
                break;
            }
            if s > cursor
                && let Ok(gap) = ByteRange::new(cursor, s)
            {
                out.push(gap);
            }
            cursor = cursor.max(e.min(total_size));
        }
        if cursor < total_size
            && let Ok(tail_gap) = ByteRange::new(cursor, total_size)
        {
            out.push(tail_gap);
        }
        out
    }

    /// Projects completed intervals and active workers onto the 512-bin visual array (`[u8; 512]`).
    ///
    /// - `0`: remaining
    /// - `1`: completed
    /// - `2..=33`: active worker head/lease (`WorkerId::as_bin_marker`)
    #[must_use]
    pub fn render_512_bins(
        &self,
        total_size: Option<u64>,
        active: &[ActiveSegment],
    ) -> [u8; VISUAL_BIN_COUNT] {
        let mut bins = [BIN_REMAINING; VISUAL_BIN_COUNT];
        let Some(total) = total_size.filter(|&sz| sz > 0) else {
            if !self.is_empty() && active.is_empty() {
                bins.fill(BIN_COMPLETED);
            } else if let Some(first) = active.first() {
                let marker = first.worker_id.as_bin_marker();
                for slot in bins.iter_mut().take(64) {
                    *slot = marker;
                }
            }
            return bins;
        };

        let total_u128 = u128::from(total);
        let n_bins = VISUAL_BIN_COUNT as u128;

        for (idx, slot) in bins.iter_mut().enumerate() {
            let b_start = u64::try_from((idx as u128 * total_u128) / n_bins).unwrap_or(total);
            let mut b_end =
                u64::try_from(((idx as u128 + 1) * total_u128) / n_bins).unwrap_or(total);
            if b_end <= b_start {
                b_end = (b_start + 1).min(total);
            }
            if let Ok(bin_range) = ByteRange::new(b_start, b_end)
                && self.contains_range(bin_range)
            {
                *slot = BIN_COMPLETED;
            }
        }

        // Overlay active worker frontiers (`cursor..active_lease_end`) so parallel heads pulse cleanly
        for seg in active {
            if seg.cursor < seg.assigned_range.end() && seg.cursor < total {
                let head_bin = usize::try_from((u128::from(seg.cursor) * n_bins) / total_u128)
                    .unwrap_or(0)
                    .min(VISUAL_BIN_COUNT - 1);
                let lease_limit = seg.active_lease_end.max(seg.cursor + 1).min(total);
                let end_bin = usize::try_from(
                    (u128::from(lease_limit.saturating_sub(1)) * n_bins) / total_u128,
                )
                .unwrap_or(head_bin)
                .min(VISUAL_BIN_COUNT - 1);
                let marker = seg.worker_id.as_bin_marker();

                // Always highlight the exact frontier bin, plus remaining uncompleted bins in current lease
                for (bin_idx, slot) in bins.iter_mut().enumerate().take(end_bin + 1).skip(head_bin)
                {
                    if bin_idx == head_bin || *slot == BIN_REMAINING {
                        *slot = marker;
                    }
                }
            }
        }

        bins
    }
}

impl Serialize for RangeSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let vec: Vec<ByteRange> = self.iter().collect();
        vec.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RangeSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let ranges = Vec::<ByteRange>::deserialize(deserializer)?;
        let mut set = Self::new();
        for r in ranges {
            set.insert(r);
        }
        Ok(set)
    }
}

/// Live progress and bounded HTTP/1.1 lease state of a single active connection worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveSegment {
    /// Owning worker ID.
    pub worker_id: WorkerId,
    /// Logical segment `[start, end)` assigned to this worker.
    pub assigned_range: ByteRange,
    /// Next unwritten byte offset inside `assigned_range` (`start <= cursor <= end`).
    pub cursor: u64,
    /// Exclusive end offset of the currently open HTTP/1.1 bounded chunk request (`cursor <= active_lease_end <= end`).
    pub active_lease_end: u64,
    /// Latest EWMA worker speed in bytes/sec.
    pub speed_bps: f64,
}

impl ActiveSegment {
    /// Creates a new [`ActiveSegment`] with an initial bounded lease ending at `lease_end`.
    #[must_use]
    pub fn new(worker_id: WorkerId, assigned_range: ByteRange, lease_end: u64) -> Self {
        let clamped_lease = lease_end
            .max(assigned_range.start() + 1)
            .min(assigned_range.end());
        Self {
            worker_id,
            assigned_range,
            cursor: assigned_range.start(),
            active_lease_end: clamped_lease,
            speed_bps: 0.0,
        }
    }

    /// Uncompleted bytes remaining in `assigned_range`.
    #[must_use]
    pub const fn remaining_bytes(&self) -> u64 {
        self.assigned_range.end().saturating_sub(self.cursor)
    }

    /// Portion `[active_lease_end, assigned_range.end)` not yet requested on the wire —
    /// 100% free to steal without closing the worker's HTTP/1.1 Keep-Alive socket!
    #[must_use]
    pub fn unleased_tail(&self) -> Option<ByteRange> {
        ByteRange::new(self.active_lease_end, self.assigned_range.end()).ok()
    }
}

/// Complete segmentation state of a `Download` (coalesced completed `RangeSet` + active workers).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SegmentMap {
    /// Coalesced completed byte ranges.
    pub completed: RangeSet,
    /// Stack-inline active workers (`SmallVec<[ActiveSegment; 32]>` — zero heap alloc up to 32 workers).
    pub active: SmallVec<[ActiveSegment; 32]>,
    /// Total file size in bytes when known.
    pub total_size: Option<u64>,
}

impl SegmentMap {
    /// Creates a new [`SegmentMap`] for `total_size`.
    #[must_use]
    pub fn new(total_size: Option<u64>) -> Self {
        Self {
            completed: RangeSet::new(),
            active: SmallVec::new(),
            total_size,
        }
    }

    /// Records a newly written `chunk` (`[start, end)`) for `worker_id` and retires finished workers.
    ///
    /// # Errors
    /// Returns an error only if `chunk` violates bounds when `total_size` is known.
    pub fn record_written(&mut self, worker_id: WorkerId, chunk: ByteRange) -> DomainResult<()> {
        if let Some(total) = self.total_size
            && chunk.end() > total
        {
            return Err(vortex_error::VortexError::coded(
                vortex_error::ErrorCode::NetMalformedResponse,
                vortex_error::TraceId::new(),
                format!("Written chunk {chunk} exceeds total size {total}"),
            ));
        }

        self.completed.insert(chunk);
        if let Some(seg) = self.active.iter_mut().find(|s| s.worker_id == worker_id)
            && chunk.end() > seg.cursor
        {
            seg.cursor = chunk.end().min(seg.assigned_range.end());
        }
        self.active.retain(|s| s.cursor < s.assigned_range.end());
        Ok(())
    }

    /// Computes unassigned gaps in `[0, total_size)` excluding both `completed` and `active` assignments.
    #[must_use]
    pub fn unassigned_gaps(&self) -> Vec<ByteRange> {
        let Some(total) = self.total_size else {
            return Vec::new();
        };
        let mut reserved = self.completed.clone();
        for seg in &self.active {
            if let Ok(rem) = ByteRange::new(seg.cursor, seg.assigned_range.end()) {
                reserved.insert(rem);
            }
        }
        reserved.gaps(total)
    }

    /// Remaining uncompleted bytes (`None` if total size is unknown).
    #[must_use]
    pub fn remaining_bytes(&self) -> Option<u64> {
        self.total_size.map(|sz| self.completed.remaining_bytes(sz))
    }

    /// Returns `true` if `total_size` is known and `[0, total_size)` is completely written.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.total_size
            .is_some_and(|sz| self.completed.is_complete(sz))
    }

    /// Atomically removes an aborted worker (`worker_id`) and returns its unwritten sub-range
    /// `[cursor, assigned_range.end)` back to `unassigned_gaps()` for healthy sibling workers (`P1.6-T02`).
    ///
    /// Guarantees zero byte loss and zero overlap when stepping down concurrency on hosts like
    /// `pixeldrain.com` that return `403`/`429` on secondary connections!
    pub fn abort_and_reclaim_worker(&mut self, worker_id: WorkerId) -> Option<ByteRange> {
        let idx = self.active.iter().position(|s| s.worker_id == worker_id)?;
        let removed = self.active.remove(idx);
        ByteRange::new(removed.cursor, removed.assigned_range.end()).ok()
    }

    /// Clears all transient active worker assignments (e.g., on `Pause` or crash recovery).
    pub fn clear_active_workers(&mut self) {
        self.active.clear();
    }

    /// Renders the 512-bin visual progress state array.
    #[must_use]
    pub fn render_512_bins(&self) -> [u8; VISUAL_BIN_COUNT] {
        self.completed
            .render_512_bins(self.total_size, &self.active)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn merges_adjacent_and_overlapping_intervals_into_single_span() {
        let mut set = RangeSet::new();
        set.insert(ByteRange::new(0, 10).unwrap());
        set.insert(ByteRange::new(20, 30).unwrap());
        assert_eq!(set.interval_count(), 2);
        assert_eq!(set.total_bytes(), 20);

        // Bridge exact gap [10, 20)
        set.insert(ByteRange::new(10, 20).unwrap());
        assert_eq!(set.interval_count(), 1);
        assert_eq!(set.total_bytes(), 30);
        assert!(set.is_complete(30));
        assert_eq!(set.gaps(30).len(), 0);
    }

    #[test]
    fn render_512_bins_handles_multi_terabyte_sizes_without_overflow() {
        let total = 20_u64 * 1024 * 1024 * 1024 * 1024; // 20 TiB
        let half = total / 2;
        let mut map = SegmentMap::new(Some(total));
        map.completed.insert(ByteRange::new(0, half).unwrap());
        map.active.push(ActiveSegment::new(
            WorkerId::new(3),
            ByteRange::new(half, total).unwrap(),
            half + (total / 8),
        ));

        let bins = map.render_512_bins();
        assert_eq!(bins[0], BIN_COMPLETED);
        assert_eq!(bins[255], BIN_COMPLETED);
        assert_eq!(bins[256], WorkerId::new(3).as_bin_marker());
        assert_eq!(bins[511], BIN_REMAINING);
    }

    proptest! {
        #[test]
        fn proptest_rangeset_invariants_never_overlap_or_lose_bytes(
            ops in prop::collection::vec((0_u64..2048, 1_u64..256), 1..80),
            total_size in 512_u64..2500,
        ) {
            let mut set = RangeSet::new();
            let mut covered_bitmap = vec![false; 2500];

            for (start, len) in ops {
                let end = (start + len).min(2500);
                if start < end {
                    let range = ByteRange::new(start, end).unwrap();
                    set.insert(range);
                    for bit in &mut covered_bitmap[start as usize..end as usize] {
                        *bit = true;
                    }
                }
            }

            // 1. Strict disjoint & non-adjacent invariant: end_i < start_{i+1}
            let intervals: Vec<ByteRange> = set.iter().collect();
            for pair in intervals.windows(2) {
                prop_assert!(pair[0].end() < pair[1].start());
            }

            // 2. Exact byte count equality against ground-truth bitmap
            let expected_total = covered_bitmap.iter().filter(|&&b| b).count() as u64;
            prop_assert_eq!(set.total_bytes(), expected_total);

            // 3. Union of completed within [0, total_size) + gaps(total_size) == [0, total_size) with zero overlap
            let gaps = set.gaps(total_size);
            let gap_sum: u64 = gaps.iter().map(|g| g.len()).sum();
            prop_assert_eq!(set.covered_within(total_size) + gap_sum, total_size);
            prop_assert_eq!(set.remaining_bytes(total_size), gap_sum);

            // 4. Serde round-trip preserves exact RangeSet
            let json = serde_json::to_string(&set).unwrap();
            let restored: RangeSet = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(restored, set);
        }
    }
}
