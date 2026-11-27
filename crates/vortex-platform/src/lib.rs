//! Operating system integration behind `PlatformIntegration` & `DiagnosticsPort` traits
//! (`crates/vortex-platform` — Level 3, `P1-T22` → `P1-T23`).
//!
//! Provides:
//! - Genuine Windows Owner-SID resolution (`OpenProcessToken` + `ConvertSidToStringSidW`)
//! - Single-Instance Named Mutex (`Local\VortexDownloadManager_SingleInstance_<SID>`)
//! - Owner-SID Named Pipe Server Factory (`D:P(A;;GA;;;<SID>)` + `PIPE_REJECT_REMOTE_CLIENTS`)
//! - `HKCU` Native Messaging manifest registration (`Brave`, `Chrome`, `Edge` — zero Admin)
//! - Startup Self-Healing (`dirty active -> Paused` + `.vortex.meta` re-import)
//! - Graceful Shutdown Coordinator (`<= 5s` flush + `WM_QUERYENDSESSION`/`WM_ENDSESSION` + `wal_checkpoint(TRUNCATE)`)
//! - `SetThreadExecutionState` Sleep Prevention & Internet Connectivity Monitor
//! - Self-Health Check & Zero-Leakage `.zip` [`DiagnosticBundleBuilder`]

pub mod diagnostics;
pub mod lifecycle;
pub mod motw;
pub mod windows_os;

pub use diagnostics::{DiagnosticBundleBuilder, crc32_ieee};
pub use lifecycle::{
    GracefulShutdownCoordinator, SelfHealingSummary, install_os_shutdown_listeners,
    run_startup_self_healing, spawn_connectivity_monitor,
};
pub use windows_os::{
    NATIVE_HOST_NAME, OFFICIAL_EXTENSION_ID, SingleInstanceGuard, SingleInstanceOutcome,
    WindowsPlatform, WindowsSleepGuard, create_owner_only_pipe_server, query_current_user_sid,
    user_named_pipe_path,
};

/// Crate identifier verified by workspace smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_application::layer_name();
    "vortex-platform"
}
