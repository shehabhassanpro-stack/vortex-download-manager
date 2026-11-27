//! Read-model DTOs crossing Tauri (`tauri-specta`) and IPC boundaries (`P1-T06`).

use serde::{Deserialize, Serialize};
use vortex_domain::{
    CategoryId, Checksum, ContentSecurityAssessment, Download, DownloadState, Priority, QueueId,
};
use vortex_error::ApiError;
use vortex_observability::RedactedUrl;

/// Compact read-model row rendered in the virtualized main downloads table (`60 FPS` at `10,000+` rows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DownloadSummaryDto {
    /// Hyphenated `UUIDv7` string.
    pub id: String,
    /// Sanitized file basename.
    pub file_name: String,
    /// Save directory path string.
    pub save_dir: String,
    /// Redacted origin/CDN domain or URL safe for UI & logs.
    pub display_url: String,
    /// Total file size in bytes (`None` if unknown).
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub total_size: Option<u64>,
    /// Downloaded bytes so far.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub downloaded_bytes: u64,
    /// Canonical state slug (`"downloading"`, `"paused"`, `"completed"`, `"needs_attention"`, ...).
    pub state_slug: String,
    /// Live fast-horizon speed (`bytes/sec`).
    pub speed_bps: f64,
    /// Stabilized ETA in seconds.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub eta_secs: Option<u64>,
    /// Active worker count (`0` when not downloading).
    pub active_workers: u8,
    /// Server range support flag.
    pub supports_range: bool,
    /// Category slug.
    pub category_id: CategoryId,
    /// Queue slug.
    pub queue_id: QueueId,
    /// Scheduling priority.
    pub priority: Priority,
    /// Created Unix timestamp (ms).
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub created_at_ms: i64,
    /// Completed Unix timestamp (ms).
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub completed_at_ms: Option<i64>,
    /// Structured error or attention reason when `failed` or `needs_attention`.
    pub error: Option<ApiError>,
}

impl DownloadSummaryDto {
    /// Projects a domain [`Download`] into a UI-ready [`DownloadSummaryDto`] (redacting URLs).
    #[must_use]
    pub fn from_domain(d: &Download) -> Self {
        let (speed_bps, eta_secs, active_workers, error) = match &d.state {
            DownloadState::Downloading {
                active_workers,
                speed_fast_bps,
                eta_secs,
                ..
            } => (*speed_fast_bps, *eta_secs, *active_workers, None),
            DownloadState::Failed { error, .. } => (0.0, None, 0, Some(error.clone())),
            DownloadState::NeedsAttention {
                reason_code,
                detail,
            } => {
                let err = ApiError::new(*reason_code, vortex_error::TraceId::new())
                    .with_technical_detail(detail.clone());
                (0.0, None, 0, Some(err))
            }
            _ => (0.0, None, 0, None),
        };

        Self {
            id: d.id.to_string(),
            file_name: d.file_name.clone(),
            save_dir: d.save_dir.display().to_string(),
            display_url: RedactedUrl::parse(d.resource.effective_url()).to_string(),
            total_size: d.total_size,
            downloaded_bytes: d.downloaded_bytes(),
            state_slug: d.state.as_slug().to_owned(),
            speed_bps,
            eta_secs,
            active_workers,
            supports_range: d.supports_range,
            category_id: d.category_id.clone(),
            queue_id: d.queue_id.clone(),
            priority: d.priority,
            created_at_ms: d.times.created_at_ms,
            completed_at_ms: d.times.completed_at_ms,
            error,
        }
    }
}

/// Throttled (`<= 10Hz`) live progress delta pushed to the UI & Progress Dialog (`§10.2.5`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ProgressSnapshotDto {
    /// Hyphenated `DownloadId`.
    pub download_id: String,
    /// Canonical state slug.
    pub state_slug: String,
    /// Completed bytes.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub downloaded_bytes: u64,
    /// Total expected bytes.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub total_size: Option<u64>,
    /// Fast EWMA speed (`tau = 1.5s`, bytes/sec).
    pub speed_fast_bps: f64,
    /// Slow EWMA speed (`tau = 8.0s`, bytes/sec).
    pub speed_slow_bps: f64,
    /// Stabilized ETA in seconds.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub eta_secs: Option<u64>,
    /// Active parallel workers.
    pub active_workers: u8,
    /// Active transport protocol (`"HTTP/2"` or `"HTTP/1.1"` — `P1.7-T09`).
    #[serde(default)]
    pub protocol: Option<String>,
    /// Live concurrency mode (`"auto"`, `"capped"`, or `"fixed"` — `P1.7-T09`).
    #[serde(default)]
    pub concurrency_mode: Option<String>,
    /// 512-bin visual segment bar state (`0 = rem`, `1 = done`, `2..=33 = worker k`).
    pub bins: Vec<u8>,
}

/// Detailed view model for the Progress Dialog & Properties Drawer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DownloadDetailsDto {
    /// Summary fields.
    pub summary: DownloadSummaryDto,
    /// Redacted referrer URL.
    pub referrer: Option<String>,
    /// MIME type.
    pub mime_type: Option<String>,
    /// Expected or verified checksum.
    pub checksum: Option<Checksum>,
    /// 512-bin `<canvas>` array.
    pub bins: Vec<u8>,
}

impl DownloadDetailsDto {
    /// Projects a domain [`Download`] into [`DownloadDetailsDto`].
    #[must_use]
    pub fn from_domain(d: &Download) -> Self {
        let verified = match &d.state {
            DownloadState::Completed {
                verified_checksum, ..
            } => verified_checksum.clone(),
            _ => None,
        };
        Self {
            summary: DownloadSummaryDto::from_domain(d),
            referrer: d
                .resource
                .referrer
                .as_deref()
                .map(|r| RedactedUrl::parse(r).to_string()),
            mime_type: d.resource.mime_type.clone(),
            checksum: verified.or_else(|| d.expected_hash.clone()),
            bins: d.segments.render_512_bins().to_vec(),
        }
    }
}

/// Native Messaging host registration status for a specific browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BrowserIntegrationStatusDto {
    /// Browser name (`"Brave"`, `"Chrome"`, `"Edge"`, `"Firefox"`).
    pub browser: String,
    /// Whether `HKCU` registry key and manifest JSON exist and point to `vortex-bridge.exe`.
    pub registered: bool,
    /// Sanitized manifest file path.
    pub manifest_path: String,
}

/// System-wide Self-Health Check report (`Design-System.md` §4.4 & `P1-T23`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[allow(clippy::struct_excessive_bools)]
pub struct HealthStatusDto {
    /// Core actor supervisor healthy.
    pub core_ok: bool,
    /// SQLite `PRAGMA quick_check` passed.
    pub db_ok: bool,
    /// Named Pipe listener active.
    pub pipe_ok: bool,
    /// Free bytes on default download volume.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub free_disk_bytes: u64,
    /// Quarantined poison-pill records count.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub quarantined_records: u64,
    /// Whether runtime `DEBUG` mode is currently active.
    pub debug_mode_active: bool,
    /// Per-browser Native Messaging registration status.
    pub browsers: Vec<BrowserIntegrationStatusDto>,
}

/// Single file entry shown in the Diagnostic Bundle Preview Modal (`§22.4` & `P1-T23`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DiagnosticFilePreviewDto {
    /// Archive entry name (e.g., `"logs/core.jsonl"`, `"system_info.json"`).
    pub name: String,
    /// Uncompressed redacted byte length.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub size_bytes: u64,
    /// First sanitized preview lines so user sees exactly what is inside before saving.
    pub preview_excerpt: String,
}

/// Preview manifest returned before or upon generating a sanitized `.zip` diagnostic bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DiagnosticBundleReportDto {
    /// Output `.zip` path (if written) or suggested filename.
    pub bundle_path: String,
    /// Included redacted files.
    pub files: Vec<DiagnosticFilePreviewDto>,
    /// Total `.zip` size in bytes.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub total_zip_bytes: u64,
}

/// Duplicate detection match returned by `AHashMap` O(1) lookup (`P1-T21` & `P1-T33`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DuplicateMatchDto {
    /// Existing `DownloadId` string.
    pub existing_id: String,
    /// Existing sanitized filename.
    pub file_name: String,
    /// Save directory string.
    pub save_dir: String,
    /// Existing state slug (`"completed"`, `"downloading"`, `"paused"`, ...).
    pub state_slug: String,
    /// Whether the final file already exists on disk.
    pub file_exists_on_disk: bool,
}

/// Pre-probed metadata returned to the "Add Download" dialog while the `16 MiB` stream stays warm (`0ms` start — `§15.4` & `P1-T33`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AddDownloadPreviewDto {
    /// Staged `DownloadId` holding the pre-opened `ByteStream` in RAM.
    pub staged_id: String,
    /// Redacted origin/CDN URL.
    pub display_url: String,
    /// Suggested sanitized filename.
    pub suggested_file_name: String,
    /// Suggested category ID.
    pub category_id: CategoryId,
    /// Suggested save directory.
    pub suggested_save_dir: String,
    /// Discovered file size in bytes.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub total_size: Option<u64>,
    /// Whether `206 Partial Content` is supported.
    pub supports_range: bool,
    /// Free bytes available on `suggested_save_dir` volume (`None` if unknown).
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub free_disk_bytes: Option<u64>,
    /// Magic-byte, HTML-trap, double-extension, and cross-origin assessment (`P1.5-T02`).
    pub security: ContentSecurityAssessment,
    /// Optional duplicate warning (`Replace` / `Auto-Rename` / `Open Existing`).
    pub duplicate: Option<DuplicateMatchDto>,
    /// `true` if `UrlCanonicalizer` rewrote a public viewer page URL into a direct download link (`UX-M01`).
    pub canonical_url_transformed: bool,
    /// Optional provider hint (`"pixeldrain"`, `"github_blob"`, `"dropbox"`, `"interactive_viewer"`).
    pub url_provider_hint: Option<String>,
}

/// Pending staged browser/manual download awaiting explicit user confirmation (`P1.5-T03`).
///
/// Pulled via `cmd_list_staged_downloads` so waking `WebView2` from Tray NEVER loses a confirmation dialog!
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct StagedDownloadDto {
    /// Staged `DownloadId` holding the live `16 MiB` RAM stream.
    pub staged_id: String,
    /// Effective target URL (used when confirming via `cmd_add_download`).
    pub url: String,
    /// Redacted URL safe for display in the confirmation UI.
    pub display_url: String,
    /// Suggested sanitized filename.
    pub suggested_file_name: String,
    /// Auto-detected category ID.
    pub category_id: CategoryId,
    /// Auto-resolved save directory.
    pub suggested_save_dir: String,
    /// Discovered file size in bytes.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub total_size: Option<u64>,
    /// Whether `206 Partial Content` is supported.
    pub supports_range: bool,
    /// Free disk bytes on `suggested_save_dir`'s volume.
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub free_disk_bytes: Option<u64>,
    /// Magic-byte & anti-deception assessment (`P1.5-T02`).
    pub security: ContentSecurityAssessment,
    /// Optional duplicate match warning.
    pub duplicate: Option<DuplicateMatchDto>,
    /// Trigger origin (`"determining-filename"`, `"context-menu"`, `"manual"`).
    pub source: String,
    /// `true` if queued during a rapid burst (`> 1` unsolicited handoffs within `4s` — `TH-20`).
    pub burst_quarantined: bool,
    /// Unix timestamp (ms) when staged.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub created_at_ms: i64,
    /// Unix timestamp (ms) when the warm stream expires if untouched.
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub expires_at_ms: i64,
}

/// Result of `HandoffFromBrowser` (`Protocol-Spec.md` §2.2 & §4 — `Probe-Before-Commit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[allow(clippy::struct_excessive_bools)]
pub struct BrowserHandoffResultDto {
    /// Created or staged `DownloadId` (when `accepted == true`).
    pub download_id: Option<String>,
    /// `true` ONLY after `probe_and_stream` succeeded within the `1,400ms` budget!
    pub accepted: bool,
    /// Whether an identical URL or target filename was already recorded.
    pub duplicate_found: bool,
    /// `true` when Extension must call `suggest()` so browser finishes on its original open TCP stream!
    pub fallback_to_browser: bool,
    /// Canonical fallback reason (`"waf_tls_bound"`, `"single_use_token"`, `"probe_timeout"`, ...).
    pub fallback_reason: Option<String>,
}

/// Redacted live log line surfaced in the Diagnostics Drawer (`P1-T34`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct LogEntryDto {
    /// RFC-3339 timestamp.
    pub timestamp: String,
    /// Severity (`"INFO"`, `"WARN"`, `"ERROR"`, `"DEBUG"`).
    pub level: String,
    /// Module target.
    pub target: String,
    /// Redacted message text.
    pub message: String,
    /// Optional correlation `trace_id`.
    pub trace_id: Option<String>,
}
