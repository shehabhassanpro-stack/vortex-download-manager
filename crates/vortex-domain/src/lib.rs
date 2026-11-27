//! Pure domain model for Vortex Download Manager (`crates/vortex-domain` — Level 1, zero I/O).
//!
//! Implements `P1-T01` → `P1-T05`:
//! - Value objects (`DownloadId`, `EventId`, `QueueId`, `CategoryId`, `WorkerId`, `ByteRange`,
//!   `Checksum`, RFC 9110 `ResumeValidators`, `Priority`, `Timestamps`) and deterministic [`Clock`]
//! - Coalescing [`RangeSet`] (`BTreeMap<u64, u64>`) + `512`-bin `<canvas>` state projection
//! - Explicit Type-State [`Download`] aggregate root & immutable [`DomainEvent`]s
//! - Hybrid Lease-Based [`SegmentationPolicy`] + Work Stealing + [`DualHorizonEwma`]
//! - Security-hardened [`FilenameSanitizer`] & [`CategorizationPolicy`].

#![forbid(unsafe_code)]

pub mod clock;
pub mod concurrency;
pub mod download;
pub mod events;
pub mod range_set;
pub mod sanitization;
pub mod security;
pub mod segmentation;
pub mod url_canonicalizer;
pub mod value_objects;

/// Canonical result alias for pure domain transitions.
pub type DomainResult<T> = Result<T, vortex_error::VortexError>;

/// Layer identifier used by Composition Root smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    "vortex-domain"
}

pub use clock::{Clock, FakeClock, SystemClock};
pub use concurrency::{
    AdaptiveConcurrencyConfig, AdaptiveConcurrencyController, ConcurrencyDecision,
    ConcurrencyObservation, ENDGAME_MAX_REMAINING_BYTES, ENDGAME_MIN_STRAGGLER_ETA_SECS,
    ENDGAME_MIN_WORKER_REMAINING_BYTES, ENDGAME_REMAINING_FRACTION, ENDGAME_STRAGGLER_SPEED_RATIO,
    StepDownReason, compute_delay_gradient, compute_utility, worth_hedging,
};
pub use download::{Download, DownloadCommand, DownloadState, Resource};
pub use events::{DomainEvent, DomainEventEnvelope, PostProcessStage};
pub use range_set::{
    ActiveSegment, BIN_COMPLETED, BIN_REMAINING, RangeSet, SegmentMap, VISUAL_BIN_COUNT,
};
pub use sanitization::{
    CategorizationPolicy, CategoryRule, DEFAULT_FALLBACK_FILENAME, FilenameSanitizer,
    MAX_FILENAME_BYTES,
};
pub use security::{
    ContentSecurityAssessment, ContentSecurityInspector, RiskLevel, SecurityWarningKind,
    extract_url_host, is_cross_origin_host, is_high_risk_executable_extension,
};
pub use segmentation::{
    DualHorizonEwma, EWMA_TAU_FAST_SECS, EWMA_TAU_SLOW_SECS, LEASE_ALIGN_BYTES, LeaseDecision,
    MAX_CHUNK_LEASE_BYTES, MIN_CHUNK_LEASE_BYTES, MIN_SPLIT_REMAINING_BYTES,
    MIN_SPLIT_TIME_SAVING_SECS, SegmentationPolicy, TARGET_CHUNK_DURATION_SECS, align_down_64kib,
    align_up_64kib, compute_chunk_lease_bytes, worth_splitting,
};
pub use url_canonicalizer::{CanonicalUrlResult, UrlCanonicalizer};
pub use value_objects::{
    ByteRange, CategoryId, Checksum, DownloadId, EtagValidator, EventId, HashAlgorithm,
    IfRangeDirective, LastModifiedValidator, Priority, QueueId, ResumeValidators, Timestamps,
    WorkerId,
};
