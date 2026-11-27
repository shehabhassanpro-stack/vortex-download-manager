//! Structured logging, secret redaction, Flight Recorder, dynamic level reload,
//! global panic hook, and local metrics for Vortex (`crates/vortex-observability` — Level 1).
//!
//! Built in Phase 0 (`P0-T10`, `P0-T11`, `P0-T12`, `ADR-016`) before any engine code so every
//! crate is observable, privacy-preserving, and Zero-Leakage verified from day one.

#![forbid(unsafe_code)]

#[cfg(feature = "full")]
mod flight_recorder;
#[cfg(feature = "full")]
mod logging;
mod metrics;
mod redaction;
mod rotation;

#[cfg(feature = "full")]
pub use flight_recorder::{FlightEvent, FlightRecorder, FlightRecorderLayer};
#[cfg(feature = "full")]
pub use logging::{
    DEFAULT_DEBUG_MODE_DURATION, ObservabilityConfig, ObservabilityError, ObservabilityHandle,
    build_subscriber, init_observability, install_panic_hook,
};
pub use metrics::{LatencyHistogram, LatencySnapshot, LocalMetrics, MetricsSnapshot};
pub use redaction::{
    RedactedUrl, is_sensitive_key, might_contain_unredacted_secret, redact_sensitive_text,
    sanitize_path_for_export,
};
pub use rotation::{NonBlockingLogSink, RotatingFileConfig, SizeRotatingFile};
pub use secrecy::{ExposeSecret, SecretBox, SecretString};
pub use zeroize::Zeroize;
