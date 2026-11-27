//! Lightweight local-only metrics registry (`P0-T11` / `ADR-014` / `ADR-016`).
//!
//! Strictly local — zero network export or telemetry. Tracks throughput counters,
//! error counts keyed by [`ErrorCode`], dropped log events, and lock-free latency histograms.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use vortex_error::ErrorCode;

/// Fixed upper bounds (in microseconds) for lock-free latency histograms.
const BUCKET_BOUNDS_US: [u64; 8] = [100, 500, 1_000, 5_000, 20_000, 100_000, 500_000, 2_000_000];

/// Lock-free fixed-bucket latency histogram (hot-path safe).
#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; 9],
    count: AtomicU64,
    sum_us: AtomicU64,
}

/// Serializable point-in-time snapshot of a [`LatencyHistogram`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySnapshot {
    /// Total number of recorded latency samples.
    pub count: u64,
    /// Mean latency in microseconds (`0` when empty).
    pub avg_us: u64,
    /// Per-bucket sample counts (8 bounded buckets + overflow bucket).
    pub buckets: [u64; 9],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    /// Creates an empty [`LatencyHistogram`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    /// Records a latency sample measured in microseconds.
    pub fn observe_us(&self, micros: u64) {
        let idx = BUCKET_BOUNDS_US
            .iter()
            .position(|&bound| micros <= bound)
            .unwrap_or(BUCKET_BOUNDS_US.len());
        if let Some(bucket) = self.buckets.get(idx) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(micros, Ordering::Relaxed);
    }

    /// Captures a point-in-time [`LatencySnapshot`].
    #[must_use]
    pub fn snapshot(&self) -> LatencySnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum_us.load(Ordering::Relaxed);
        let avg_us = sum.checked_div(count).unwrap_or(0);
        let mut out = [0_u64; 9];
        for (dst, src) in out.iter_mut().zip(self.buckets.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        LatencySnapshot {
            count,
            avg_us,
            buckets: out,
        }
    }
}

/// Process-wide local observability metrics registry.
#[derive(Clone, Debug)]
pub struct LocalMetrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    bytes_downloaded: AtomicU64,
    bytes_written_disk: AtomicU64,
    dropped_log_events: Arc<AtomicU64>,
    error_counts: Vec<(ErrorCode, AtomicU64)>,
    ipc_latency: LatencyHistogram,
    disk_write_latency: LatencyHistogram,
}

/// Serializable snapshot of all local metrics (used by Diagnostics Drawer & CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Total bytes downloaded across all workers.
    pub bytes_downloaded: u64,
    /// Total bytes written to disk.
    pub bytes_written_disk: u64,
    /// Number of log lines dropped under non-blocking queue saturation.
    pub dropped_log_events: u64,
    /// Non-zero error totals keyed by `ErrorCode`.
    pub errors_by_code: BTreeMap<ErrorCode, u64>,
    /// IPC request latency histogram.
    pub ipc_latency: LatencySnapshot,
    /// Disk write latency histogram.
    pub disk_write_latency: LatencySnapshot,
}

impl Default for LocalMetrics {
    fn default() -> Self {
        Self::new(Arc::new(AtomicU64::new(0)))
    }
}

impl LocalMetrics {
    /// Creates a new local metrics registry sharing `dropped_log_events` with the log sink.
    #[must_use]
    pub fn new(dropped_log_events: Arc<AtomicU64>) -> Self {
        let error_counts = ErrorCode::ALL
            .iter()
            .map(|&code| (code, AtomicU64::new(0)))
            .collect();
        Self {
            inner: Arc::new(MetricsInner {
                bytes_downloaded: AtomicU64::new(0),
                bytes_written_disk: AtomicU64::new(0),
                dropped_log_events,
                error_counts,
                ipc_latency: LatencyHistogram::new(),
                disk_write_latency: LatencyHistogram::new(),
            }),
        }
    }

    /// Adds downloaded byte count (hot-path atomic increment).
    pub fn add_bytes_downloaded(&self, bytes: u64) {
        self.inner
            .bytes_downloaded
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Adds disk-written byte count (hot-path atomic increment).
    pub fn add_bytes_written(&self, bytes: u64) {
        self.inner
            .bytes_written_disk
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Increments the counter for `code`.
    pub fn record_error(&self, code: ErrorCode) {
        if let Some((_, counter)) = self.inner.error_counts.iter().find(|(c, _)| *c == code) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records an IPC latency observation in microseconds.
    pub fn observe_ipc_latency_us(&self, micros: u64) {
        self.inner.ipc_latency.observe_us(micros);
    }

    /// Records a disk-write latency observation in microseconds.
    pub fn observe_disk_write_latency_us(&self, micros: u64) {
        self.inner.disk_write_latency.observe_us(micros);
    }

    /// Returns the shared `dropped_log_events` counter.
    #[must_use]
    pub fn dropped_log_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.inner.dropped_log_events)
    }

    /// Captures a serializable [`MetricsSnapshot`].
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let mut errors_by_code = BTreeMap::new();
        for (code, counter) in &self.inner.error_counts {
            let count = counter.load(Ordering::Relaxed);
            if count > 0 {
                errors_by_code.insert(*code, count);
            }
        }
        MetricsSnapshot {
            bytes_downloaded: self.inner.bytes_downloaded.load(Ordering::Relaxed),
            bytes_written_disk: self.inner.bytes_written_disk.load(Ordering::Relaxed),
            dropped_log_events: self.inner.dropped_log_events.load(Ordering::Relaxed),
            errors_by_code,
            ipc_latency: self.inner.ipc_latency.snapshot(),
            disk_write_latency: self.inner.disk_write_latency.snapshot(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn metrics_track_counters_errors_and_histograms() {
        let metrics = LocalMetrics::default();
        metrics.add_bytes_downloaded(1024);
        metrics.add_bytes_written(512);
        metrics.record_error(ErrorCode::NetRateLimited);
        metrics.record_error(ErrorCode::NetRateLimited);
        metrics.observe_ipc_latency_us(250);
        metrics.observe_disk_write_latency_us(1_500);

        let snap = metrics.snapshot();
        assert_eq!(snap.bytes_downloaded, 1024);
        assert_eq!(snap.bytes_written_disk, 512);
        assert_eq!(
            snap.errors_by_code.get(&ErrorCode::NetRateLimited),
            Some(&2)
        );
        assert_eq!(snap.ipc_latency.count, 1);
        assert_eq!(snap.ipc_latency.avg_us, 250);
        assert_eq!(snap.disk_write_latency.count, 1);
    }
}
