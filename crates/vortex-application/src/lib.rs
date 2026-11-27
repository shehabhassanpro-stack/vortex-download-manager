//! Application layer ports (traits), configuration contracts, DTOs, and CQRS-lite use cases
//! (`crates/vortex-application` — Level 2, `P1-T06` & `P1-T21`).
//!
//! Strictly depends only on Level 0 (`vortex-error`) and Level 1 (`vortex-domain`, `vortex-observability`).

#![forbid(unsafe_code)]

pub mod config;
pub mod dto;
pub mod ports;
pub mod use_cases;

/// Canonical result alias for application use cases and ports.
pub type AppResult<T> = Result<T, vortex_error::VortexError>;

/// Layer identifier used by downstream adapter smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_domain::layer_name();
    "vortex-application"
}

pub use config::{
    AppConfig, CURRENT_CONFIG_SCHEMA_VERSION, CaptureRulesConfig, DEFAULT_MIN_CAPTURE_BYTES,
    DEFAULT_MIN_FREE_DISK_BYTES, FeatureFlags, ThemePreference,
};
pub use dto::{
    AddDownloadPreviewDto, BrowserHandoffResultDto, BrowserIntegrationStatusDto,
    DiagnosticBundleReportDto, DiagnosticFilePreviewDto, DownloadDetailsDto, DownloadSummaryDto,
    DuplicateMatchDto, HealthStatusDto, LogEntryDto, ProgressSnapshotDto, StagedDownloadDto,
};
pub use ports::{
    AppEvent, ByteSource, ByteStream, CategoryRecord, CategoryRepository, ConfigPort,
    DatabaseHealthPort, DiagnosticsPort, DownloadRepository, EngineControlPort, EventBus,
    EventLogRepository, FallbackReason, FileSink, FileSinkFactory, PlatformIntegration,
    ProbeOutcome, QueueRecord, QueueRepository, ResourceInfo, ResourceRequest, SecretCredentials,
    SecretStore, SidecarMetadata, SleepInhibitorGuard, SpaceReservationId, TransportMetrics,
    VolumeSpaceLedger,
};
pub use use_cases::{
    AddDownloadCommand, ApplicationService, BROWSER_PROBE_BUDGET, BURST_WINDOW_MS,
    BrowserHandoffCommand, DownloadFilterDto, MAX_STAGED_PROBES, PrepareAddInput,
    UNDO_DELETE_WINDOW_MS,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use vortex_domain::{CategoryId, Download, DownloadId, Priority, QueueId, Resource};
    use vortex_error::TraceId;

    use super::*;

    #[test]
    fn default_app_config_validates_and_rejects_out_of_range_mutations() {
        let cfg = AppConfig::default();
        cfg.validate().unwrap();

        let bad_conn = AppConfig {
            max_connections_per_download: 64,
            ..cfg.clone()
        };
        assert!(bad_conn.validate().is_err());

        let bad_disk = AppConfig {
            min_free_disk_bytes: 1024,
            ..cfg.clone()
        };
        assert!(bad_disk.validate().is_err());

        let bad_lang = AppConfig {
            language: "fr".to_owned(),
            ..cfg
        };
        assert!(bad_lang.validate().is_err());
    }

    #[test]
    fn dto_projection_redacts_sensitive_urls_and_preserves_512_bins() {
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://user:pass@cdn.example.com/file.iso?token=secret99&v=1"),
            "https://cdn.example.com/file.iso?token=[REDACTED]&v=1".to_owned(),
            PathBuf::from("D:/Downloads"),
            "file.iso".to_owned(),
            CategoryId::builtin(CategoryId::COMPRESSED),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            1_000,
            TraceId::new(),
        );

        let details = DownloadDetailsDto::from_domain(&dl);
        assert!(!details.summary.display_url.contains("secret99"));
        assert!(!details.summary.display_url.contains("pass"));
        assert_eq!(details.bins.len(), 512);
        assert_eq!(FallbackReason::WafTlsBound.as_str(), "waf_tls_bound");
        assert_eq!(FallbackReason::SingleUseToken.as_str(), "single_use_token");
    }
}
