//! Typed application configuration (`AppConfig`), `CaptureRulesConfig`, and local `FeatureFlags`
//! (`P1-T06` / `P1-T09` / `Vortex-Architecture-Design.md` §24.4 & §30.2).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::AppResult;

/// Current configuration schema version (`config.toml`).
pub const CURRENT_CONFIG_SCHEMA_VERSION: u32 = 1;

/// Default safety reserve on target volumes (`256 MiB` — `§23.4` & `§30.2`).
pub const DEFAULT_MIN_FREE_DISK_BYTES: u64 = 256 * 1024 * 1024;

/// Default minimum browser-captured file size (`1 MiB` — `§30.2`).
pub const DEFAULT_MIN_CAPTURE_BYTES: u64 = 1024 * 1024;

/// UI color theme preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    /// Follow OS light/dark mode.
    #[default]
    System,
    /// Calm OKLCH Light mode.
    Light,
    /// Calm OKLCH Dark mode.
    Dark,
}

/// Local feature flags for Trunk-Based Development & kill-switches (`§24.4`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct FeatureFlags {
    /// Enable experimental HTTP/3 QUIC transport (Phase 3).
    pub enable_http3: bool,
    /// Enable polite gaming/call mode (`LEDBAT++` — Phase 4).
    pub enable_ledbat: bool,
    /// Enable WASM plugin sandbox (Phase 3).
    pub enable_wasm_plugins: bool,
    /// Enable YouTube/HLS/DASH hybrid pipeline (Phase 2).
    pub enable_youtube_hybrid: bool,
}

/// Browser extension capture rules pushed via `HelloAck` and `SettingsChanged` (`Protocol-Spec.md` §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(default)]
pub struct CaptureRulesConfig {
    /// Master toggle for automatic browser download interception.
    pub enabled: bool,
    /// Minimum file size in bytes before capturing (`1 MiB` default).
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub min_file_size_bytes: u64,
    /// Lowercase file extensions automatically captured.
    pub extensions: Vec<String>,
    /// MIME types automatically captured.
    pub mime_types: Vec<String>,
    /// Excluded host patterns (e.g., `"localhost"`, `"*.internal.corp"`).
    pub excluded_hosts: Vec<String>,
    /// Capture downloads started in Incognito/Private windows (`false` by default for privacy).
    pub capture_in_incognito: bool,
}

impl Default for CaptureRulesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_file_size_bytes: DEFAULT_MIN_CAPTURE_BYTES,
            extensions: [
                "zip", "rar", "7z", "iso", "img", "tar", "gz", "xz", "zst", "exe", "msi", "msix",
                "apk", "dmg", "pkg", "deb", "rpm", "mp4", "mkv", "webm", "avi", "mov", "mp3",
                "m4a", "flac", "wav", "opus", "pdf", "epub", "bin",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            mime_types: [
                "application/zip",
                "application/x-7z-compressed",
                "application/vnd.rar",
                "application/x-iso9660-image",
                "application/x-msdownload",
                "application/x-msi",
                "application/octet-stream",
                "application/pdf",
                "video/mp4",
                "video/x-matroska",
                "audio/mpeg",
                "audio/flac",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            excluded_hosts: vec!["localhost".to_owned(), "127.0.0.1".to_owned()],
            capture_in_incognito: false,
        }
    }
}

/// Root strongly-typed configuration persisted atomically in `config.toml` (`§24.4` & `§30.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AppConfig {
    /// Schema version (`1`).
    pub schema_version: u32,
    /// Maximum concurrent connections per download (`1..=32`, default `8`; auto-scales up to `32` when enabled).
    pub max_connections_per_download: u8,
    /// Enable BBR/Gradient2 dynamic connection auto-scaling up to `32` on uncapped hosts (`P1.7-T07`).
    pub auto_scale_connections: bool,
    /// Enable High-BDP `HTTP/2` (`ALPN: ["h2", "http/1.1"]`) with automatic `HTTP/1.1` fallback (`P1.7-T04`).
    pub enable_http2: bool,
    /// Maximum simultaneous downloads in the main queue (`1..=16`, default `3`).
    pub max_concurrent_downloads: u8,
    /// Durable disk checkpoint interval in seconds (`15..=30`, default `20`).
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub checkpoint_interval_secs: u64,
    /// Stall watchdog idle socket timeout in seconds (`5..=300`, default `30`).
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub idle_timeout_secs: u64,
    /// Maximum transient retry attempts (`0..=50`, default `10`).
    pub max_retries: u32,
    /// Minimum free disk space reserve before auto-pausing (`>= 64 MiB`, default `256 MiB`).
    #[cfg_attr(feature = "specta", specta(type = f64))]
    pub min_free_disk_bytes: u64,
    /// Root destination directory for categorized downloads.
    pub default_download_dir: PathBuf,
    /// Start browser-handed-off downloads immediately without showing the Add Dialog.
    pub auto_start_handoff: bool,
    /// Keep a warm hidden `WebView2` instance on high-RAM machines (`false` by default so Idle < 45 MB).
    pub warm_instant_webview: bool,
    /// Call `SetThreadExecutionState` while active downloads are running.
    pub prevent_sleep_while_active: bool,
    /// Enable password-manager-safe clipboard URL detection.
    pub clipboard_monitor_enabled: bool,
    /// Active UI theme.
    pub theme: ThemePreference,
    /// UI language (`"ar"` or `"en"`).
    pub language: String,
    /// Optional global bandwidth cap in bytes/sec (`None` = unlimited).
    #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
    pub global_speed_limit_bps: Option<u64>,
    /// Allow direct downloads from private/loopback networks (`false` by default for `TH-26` SSRF defense;
    /// advanced users downloading from a local NAS or test harness may explicitly enable it).
    pub allow_private_network: bool,
    /// Browser extension capture rules.
    pub capture_rules: CaptureRulesConfig,
    /// Local feature flags.
    pub feature_flags: FeatureFlags,
}

impl Default for AppConfig {
    fn default() -> Self {
        let default_dir = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(
                || PathBuf::from("Downloads"),
                |home| PathBuf::from(home).join("Downloads"),
            );

        Self {
            schema_version: CURRENT_CONFIG_SCHEMA_VERSION,
            max_connections_per_download: 8,
            auto_scale_connections: true,
            enable_http2: true,
            max_concurrent_downloads: 3,
            checkpoint_interval_secs: 20,
            idle_timeout_secs: 30,
            max_retries: 10,
            min_free_disk_bytes: DEFAULT_MIN_FREE_DISK_BYTES,
            default_download_dir: default_dir,
            auto_start_handoff: false,
            warm_instant_webview: false,
            prevent_sleep_while_active: true,
            clipboard_monitor_enabled: true,
            theme: ThemePreference::System,
            language: "ar".to_owned(),
            global_speed_limit_bps: None,
            allow_private_network: false,
            capture_rules: CaptureRulesConfig::default(),
            feature_flags: FeatureFlags::default(),
        }
    }
}

impl AppConfig {
    /// Enforces all numeric and security bounds on `AppConfig` (`§24.4`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if any field falls outside permitted bounds.
    pub fn validate(&self) -> AppResult<()> {
        if !(1..=32).contains(&self.max_connections_per_download) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!(
                    "max_connections_per_download must be in 1..=32 (got {})",
                    self.max_connections_per_download
                ),
            ));
        }
        if !(1..=16).contains(&self.max_concurrent_downloads) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!(
                    "max_concurrent_downloads must be in 1..=16 (got {})",
                    self.max_concurrent_downloads
                ),
            ));
        }
        if !(5..=120).contains(&self.checkpoint_interval_secs) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!(
                    "checkpoint_interval_secs must be in 5..=120 (got {})",
                    self.checkpoint_interval_secs
                ),
            ));
        }
        if !(5..=300).contains(&self.idle_timeout_secs) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!(
                    "idle_timeout_secs must be in 5..=300 (got {})",
                    self.idle_timeout_secs
                ),
            ));
        }
        if self.max_retries > 50 {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                "max_retries must be <= 50",
            ));
        }
        if self.min_free_disk_bytes < 64 * 1024 * 1024 {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                "min_free_disk_bytes must be >= 64 MiB",
            ));
        }
        if !matches!(self.language.as_str(), "ar" | "en") {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!("Unsupported UI language `{}`", self.language),
            ));
        }
        Ok(())
    }
}
