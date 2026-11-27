//! Immutable `DomainEvent` definitions (`P1-T03` / `Vortex-Architecture-Design.md` §7.4).

use serde::{Deserialize, Serialize};
use vortex_error::{ApiError, ErrorCode, TraceId};

use crate::value_objects::{
    ByteRange, CategoryId, Checksum, DownloadId, EventId, QueueId, WorkerId,
};

/// Sub-stage during `PostProcessing` (`FSCTL_SET_SPARSE` clear, MotW/AV scan, or Phase 2 media mux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum PostProcessStage {
    /// Clearing sparse attribute (non-fatal) and performing atomic `.vortex` -> final rename.
    FinalizeFile,
    /// Writing Windows `:Zone.Identifier` (`MotW`) and running `IAttachmentExecute` AV check.
    SecurityScan,
    /// Remuxing audio/video/subtitle streams (`FFmpeg -c copy`).
    MediaMux,
}

/// Immutable envelope carrying correlation IDs and timestamp for every domain transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainEventEnvelope {
    /// Unique time-sortable `UUIDv7` event ID.
    pub event_id: EventId,
    /// Aggregate root ID.
    pub download_id: DownloadId,
    /// Distributed correlation `TraceId`.
    pub trace_id: TraceId,
    /// Unix epoch timestamp in milliseconds.
    pub timestamp_ms: i64,
    /// Specific domain event payload.
    pub event: DomainEvent,
}

impl DomainEventEnvelope {
    /// Wraps `event` in a fresh [`DomainEventEnvelope`].
    #[must_use]
    pub fn new(
        download_id: DownloadId,
        trace_id: TraceId,
        timestamp_ms: i64,
        event: DomainEvent,
    ) -> Self {
        Self {
            event_id: EventId::new(),
            download_id,
            trace_id,
            timestamp_ms,
            event,
        }
    }
}

/// Canonical domain events emitted by the `Download` aggregate root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainEvent {
    /// New download added in `Pending` state.
    DownloadAdded {
        /// Redacted source URL safe for audit logs.
        url_redacted: String,
        /// Sanitized file basename.
        file_name: String,
        /// Assigned category.
        category_id: CategoryId,
        /// Assigned queue.
        queue_id: QueueId,
    },
    /// Probe started against remote origin.
    ProbeStarted,
    /// Initial `GET bytes=0-` probe succeeded.
    ProbeCompleted {
        /// Total byte length if reported by origin.
        total_size: Option<u64>,
        /// Whether `206 Partial Content` range requests are supported.
        supports_range: bool,
        /// Redacted final URL after redirects.
        final_url_redacted: Option<String>,
    },
    /// Placed into queue waiting for a concurrency slot.
    DownloadQueued {
        /// Target queue ID.
        queue_id: QueueId,
    },
    /// Worker opened or leased a byte segment.
    SegmentStarted {
        /// Worker ID.
        worker_id: WorkerId,
        /// Assigned byte range.
        range: ByteRange,
    },
    /// Fast worker stole the unleased tail of a slower worker.
    SegmentSplit {
        /// Victim slow worker.
        from_worker: WorkerId,
        /// Stealing worker.
        new_worker: WorkerId,
        /// Stolen byte interval.
        stolen_range: ByteRange,
    },
    /// Periodic aggregated transfer progress update.
    ProgressTick {
        /// Completed bytes so far.
        downloaded_bytes: u64,
        /// Total expected bytes.
        total_size: Option<u64>,
        /// Fast-horizon speed (`tau = 1.5s`) in bytes/sec.
        speed_fast_bps: f64,
        /// Slow-horizon speed (`tau = 8.0s`) in bytes/sec.
        speed_slow_bps: f64,
        /// Stabilized ETA in seconds.
        eta_secs: Option<u64>,
        /// Active worker count.
        active_workers: u8,
    },
    /// User or disk-space monitor paused the download after durable flush.
    DownloadPaused {
        /// Durable bytes persisted on disk.
        downloaded_bytes: u64,
    },
    /// Download resumed (transitioning to `Probing` for RFC 9110 re-validation).
    DownloadResumed,
    /// Transient failure scheduled for Decorrelated Full-Jitter retry.
    RetryScheduled {
        /// 1-based retry attempt counter.
        attempt: u32,
        /// Delay before next attempt in milliseconds.
        retry_after_ms: u64,
        /// Error code that triggered backoff.
        code: ErrorCode,
    },
    /// Signed link expired (`403`/`410`) or remote validator mutated (`VTX-RES-002`/`003`).
    LinkExpired {
        /// Stable `VTX-RES-*` or `VTX-AUTH-*` code.
        code: ErrorCode,
        /// Sanitized reason detail.
        detail: String,
    },
    /// Expired URL replaced via Link Refresh after 64 KiB prefix sanity check.
    LinkRefreshed {
        /// Redacted new URL.
        new_url_redacted: String,
    },
    /// All bytes received; post-processing started.
    PostProcessingStarted {
        /// Active post-processing stage.
        stage: PostProcessStage,
    },
    /// Cryptographic hash verification started.
    VerificationStarted,
    /// File hash matched expected [`Checksum`].
    VerificationPassed {
        /// Verified checksum.
        checksum: Checksum,
    },
    /// File hash mismatched expected [`Checksum`].
    VerificationFailed {
        /// Expected checksum.
        expected: Checksum,
        /// Computed lowercase hex digest.
        actual_hex: String,
    },
    /// Download finalized atomically and ready for user.
    DownloadCompleted {
        /// Final file size in bytes.
        final_size_bytes: u64,
    },
    /// Download terminated with an unrecovered [`ApiError`].
    DownloadFailed {
        /// Structured error contract.
        error: ApiError,
    },
}
