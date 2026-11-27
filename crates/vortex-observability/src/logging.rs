//! Multi-layer `tracing` initialization, live dynamic reload (`INFO ↔ DEBUG` with automatic
//! 30-min revert timer), Flight Recorder integration, and Global Panic Hook (`P0-T11` / `ADR-016`).

use std::backtrace::Backtrace;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use vortex_error::TraceId;

use crate::flight_recorder::{
    DEFAULT_FLIGHT_RECORDER_CAPACITY, FlightRecorder, FlightRecorderLayer,
};
use crate::metrics::LocalMetrics;
use crate::redaction::redact_sensitive_text;
use crate::rotation::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_RETENTION_DAYS, LogWorkerGuard,
    NonBlockingLogSink, RotatingFileConfig,
};

/// Default automatic revert duration for live `DEBUG` diagnostic mode (`30 minutes`).
pub const DEFAULT_DEBUG_MODE_DURATION: Duration = Duration::from_mins(30);

/// Initialization errors returned by [`init_observability`].
#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    /// Failed to open or initialize rotating log files.
    #[error("failed to initialize log sink: {0}")]
    Io(#[from] std::io::Error),
    /// Invalid `tracing` filter directive.
    #[error("invalid log filter directive `{directive}`: {reason}")]
    InvalidFilter {
        /// Filter string that failed parsing.
        directive: String,
        /// Parser error description.
        reason: String,
    },
    /// Failed to install the global tracing subscriber (already initialized).
    #[error("tracing subscriber already initialized: {0}")]
    SubscriberInit(String),
}

/// Configuration for initializing Vortex observability in Core, Bridge, or CLI.
#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    /// Directory where rotating JSONL logs and flight-recorder dumps are stored.
    pub log_dir: PathBuf,
    /// Log file stem (e.g. `"core"`, `"bridge"`, `"cli"`).
    pub file_stem: String,
    /// Enables human-readable colorized stdout output for local development (`false` in production).
    pub dev_pretty_console: bool,
    /// Base file filter directive (default: `"info"`).
    pub default_filter: String,
    /// Per-file rotation size threshold (default: `10 MiB`).
    pub max_file_bytes: u64,
    /// Maximum rotated files retained including active file (default: `5`).
    pub max_files: usize,
    /// Retention days for rotated `.jsonl.gz` archives (default: `14`).
    pub retention_days: u64,
    /// Ring-buffer slot count for the in-memory [`FlightRecorder`] (default: `2_000`).
    pub flight_recorder_capacity: usize,
    /// Installs the global panic hook that dumps the Flight Recorder on fatal panic.
    pub install_panic_hook: bool,
}

impl ObservabilityConfig {
    /// Creates a production-ready JSONL observability configuration.
    #[must_use]
    pub fn production(log_dir: PathBuf, file_stem: impl Into<String>) -> Self {
        Self {
            log_dir,
            file_stem: file_stem.into(),
            dev_pretty_console: false,
            default_filter: "info".to_owned(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            retention_days: DEFAULT_RETENTION_DAYS,
            flight_recorder_capacity: DEFAULT_FLIGHT_RECORDER_CAPACITY,
            install_panic_hook: true,
        }
    }
}

type ReloadFn = Arc<dyn Fn(EnvFilter) -> Result<(), String> + Send + Sync>;

struct DebugTimerState {
    expires_at: Option<Instant>,
    shutdown: bool,
    timer_running: bool,
}

/// Live handle returned by [`init_observability`].
///
/// Must be kept alive for the duration of the process so logs are flushed cleanly on drop.
pub struct ObservabilityHandle {
    reload_filter: ReloadFn,
    default_filter: String,
    timer_state: Arc<(Mutex<DebugTimerState>, Condvar)>,
    flight_recorder: FlightRecorder,
    metrics: LocalMetrics,
    log_dir: PathBuf,
    active_log_path: PathBuf,
    guard: LogWorkerGuard,
}

impl Drop for ObservabilityHandle {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.timer_state;
        if let Ok(mut state) = lock.lock() {
            state.shutdown = true;
            state.expires_at = None;
            cvar.notify_all();
        }
    }
}

impl ObservabilityHandle {
    /// Dynamically reloads the file log filter without restarting the process.
    ///
    /// # Errors
    /// Returns [`ObservabilityError::InvalidFilter`] if `directive` is invalid.
    pub fn set_filter(&self, directive: &str) -> Result<(), ObservabilityError> {
        let parsed =
            EnvFilter::try_new(directive).map_err(|e| ObservabilityError::InvalidFilter {
                directive: directive.to_owned(),
                reason: e.to_string(),
            })?;
        (self.reload_filter)(parsed).map_err(|reason| ObservabilityError::InvalidFilter {
            directive: directive.to_owned(),
            reason,
        })?;
        Ok(())
    }

    /// Activates live `DEBUG` diagnostic mode for `duration` (default 30 min in Diagnostics UI)
    /// and schedules an automatic background revert back to `default_filter` (`INFO`) upon expiry.
    ///
    /// # Errors
    /// Returns [`ObservabilityError`] if filter reload fails.
    pub fn enable_debug_mode(&self, duration: Duration) -> Result<(), ObservabilityError> {
        self.set_filter("debug")?;
        let (lock, cvar) = &*self.timer_state;
        if let Ok(mut state) = lock.lock() {
            state.expires_at = Some(Instant::now() + duration);
            if !state.timer_running {
                state.timer_running = true;
                let timer_pair = Arc::clone(&self.timer_state);
                let reload_fn = Arc::clone(&self.reload_filter);
                let default_filter = self.default_filter.clone();
                let _ = thread::Builder::new()
                    .name("vortex-debug-revert".to_owned())
                    .spawn(move || {
                        run_debug_revert_timer(&timer_pair, &reload_fn, &default_filter);
                    });
            }
            cvar.notify_all();
        }
        Ok(())
    }

    /// Reverts the live filter back to `default_filter` (`INFO`) immediately if `now >= expires_at`.
    ///
    /// Returns `true` if a revert was performed.
    #[must_use]
    pub fn revert_debug_if_expired(&self, now: Instant) -> bool {
        let (lock, cvar) = &*self.timer_state;
        let expired = lock
            .lock()
            .ok()
            .and_then(|g| g.expires_at)
            .is_some_and(|deadline| now >= deadline);

        if expired && self.set_filter(&self.default_filter).is_ok() {
            if let Ok(mut state) = lock.lock() {
                state.expires_at = None;
                state.timer_running = false;
                cvar.notify_all();
            }
            return true;
        }
        false
    }

    /// Returns `true` while live debug diagnostic mode is active (until filter reload completes).
    #[must_use]
    pub fn is_debug_mode_active(&self) -> bool {
        self.timer_state
            .0
            .lock()
            .is_ok_and(|g| g.expires_at.is_some() || g.timer_running)
    }

    /// Accesses the in-memory [`FlightRecorder`].
    #[must_use]
    pub const fn flight_recorder(&self) -> &FlightRecorder {
        &self.flight_recorder
    }

    /// Accesses the local-only [`LocalMetrics`] registry.
    #[must_use]
    pub const fn metrics(&self) -> &LocalMetrics {
        &self.metrics
    }

    /// Path to the active JSONL log file.
    #[must_use]
    pub const fn active_log_path(&self) -> &PathBuf {
        &self.active_log_path
    }

    /// Directory containing all logs and flight-recorder dumps.
    #[must_use]
    pub const fn log_dir(&self) -> &PathBuf {
        &self.log_dir
    }

    /// Synchronously flushes buffered JSONL logs to disk.
    pub fn flush(&self) {
        self.guard.flush();
    }
}

fn run_debug_revert_timer(
    timer_pair: &Arc<(Mutex<DebugTimerState>, Condvar)>,
    reload_fn: &ReloadFn,
    default_filter: &str,
) {
    let (lock, cvar) = &**timer_pair;
    let Ok(mut state) = lock.lock() else {
        return;
    };

    loop {
        if state.shutdown {
            state.timer_running = false;
            return;
        }
        let Some(deadline) = state.expires_at else {
            state.timer_running = false;
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            // Reload subscriber filter FIRST before clearing `expires_at` / `timer_running`
            // so `is_debug_mode_active()` becomes false strictly AFTER `INFO` is restored!
            drop(state);
            if let Ok(parsed) = EnvFilter::try_new(default_filter) {
                let _ = reload_fn(parsed);
            }
            if let Ok(mut done) = lock.lock() {
                if done.expires_at.is_some_and(|d| Instant::now() >= d) {
                    done.expires_at = None;
                }
                if done.expires_at.is_none() {
                    done.timer_running = false;
                }
                cvar.notify_all();
            }
            return;
        }
        let wait_dur = deadline.saturating_duration_since(now);
        let Ok((next_state, _)) = cvar.wait_timeout(state, wait_dur) else {
            return;
        };
        state = next_state;
    }
}

/// Builds a composable tracing subscriber + [`ObservabilityHandle`] without binding globally.
///
/// # Errors
/// Returns [`ObservabilityError`] if log files or filters cannot be initialized.
pub fn build_subscriber(
    config: &ObservabilityConfig,
) -> Result<(impl Subscriber + Send + Sync, ObservabilityHandle), ObservabilityError> {
    let rotating_cfg = RotatingFileConfig {
        log_dir: config.log_dir.clone(),
        file_stem: config.file_stem.clone(),
        max_file_bytes: config.max_file_bytes,
        max_files: config.max_files,
        retention_days: config.retention_days,
    };
    let active_log_path = rotating_cfg.active_path();

    let dropped_counter = Arc::new(AtomicU64::new(0));
    let metrics = LocalMetrics::new(Arc::clone(&dropped_counter));
    let (sink, guard) = NonBlockingLogSink::spawn(rotating_cfg, dropped_counter)?;

    let flight_dump_path = config
        .log_dir
        .join(format!("{}-flight-recorder.jsonl", config.file_stem));
    let flight_recorder =
        FlightRecorder::new(config.flight_recorder_capacity, Some(flight_dump_path));
    let flight_layer = FlightRecorderLayer::new(flight_recorder.clone());

    let env_filter = EnvFilter::try_new(&config.default_filter).map_err(|e| {
        ObservabilityError::InvalidFilter {
            directive: config.default_filter.clone(),
            reason: e.to_string(),
        }
    })?;
    let (reloadable_filter, reload_handle) = reload::Layer::new(env_filter);

    let json_file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_writer(sink);

    let filtered_file_layer = json_file_layer.with_filter(reloadable_filter);

    let console_layer = config
        .dev_pretty_console
        .then(|| tracing_subscriber::fmt::layer().pretty().with_target(true));

    let subscriber = tracing_subscriber::registry()
        .with(flight_layer)
        .with(filtered_file_layer)
        .with(console_layer);

    let reload_fn: ReloadFn = Arc::new(move |new_filter| {
        reload_handle
            .reload(new_filter)
            .map_err(|err| err.to_string())
    });

    let timer_state = Arc::new((
        Mutex::new(DebugTimerState {
            expires_at: None,
            shutdown: false,
            timer_running: false,
        }),
        Condvar::new(),
    ));

    let handle = ObservabilityHandle {
        reload_filter: reload_fn,
        default_filter: config.default_filter.clone(),
        timer_state,
        flight_recorder,
        metrics,
        log_dir: config.log_dir.clone(),
        active_log_path,
        guard,
    };

    Ok((subscriber, handle))
}

/// Initializes global structured logging, Flight Recorder, dynamic reload, and optional panic hook.
///
/// # Errors
/// Returns [`ObservabilityError`] if initialization or global subscriber registration fails.
pub fn init_observability(
    config: &ObservabilityConfig,
) -> Result<ObservabilityHandle, ObservabilityError> {
    let (subscriber, handle) = build_subscriber(config)?;
    subscriber
        .try_init()
        .map_err(|e| ObservabilityError::SubscriberInit(e.to_string()))?;

    if config.install_panic_hook {
        install_panic_hook(handle.flight_recorder.clone(), config.log_dir.clone());
    }

    Ok(handle)
}

/// Installs a global panic hook recording a redacted backtrace + `trace_id` and dumping the
/// [`FlightRecorder`] to disk before terminating (`P0-T11` / `ADR-015`).
pub fn install_panic_hook(flight_recorder: FlightRecorder, log_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let trace_id = TraceId::new();
        let location = panic_info.location().map_or_else(
            || "unknown".to_owned(),
            |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        );
        let raw_msg = panic_info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| panic_info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        let safe_msg = redact_sensitive_text(&raw_msg);
        let backtrace = Backtrace::force_capture();

        let _ = fs::create_dir_all(&log_dir);
        let panic_log_path = log_dir.join("panic-report.jsonl");
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log_path)
        {
            let record = serde_json::json!({
                "level": "FATAL_PANIC",
                "error_code": "VTX-BUG-001",
                "trace_id": trace_id.to_string(),
                "location": location,
                "message": safe_msg,
                "backtrace": backtrace.to_string(),
            });
            if let Ok(line) = serde_json::to_string(&record) {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }

        let dump_path = log_dir.join(format!("panic-flight-{trace_id}.jsonl"));
        let _ = flight_recorder.dump_jsonl_to(&dump_path);

        previous(panic_info);
    }));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_reload_enables_and_automatically_reverts_debug_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = ObservabilityConfig::production(tmp.path().to_path_buf(), "core");
        cfg.install_panic_hook = false;

        let (subscriber, handle) = build_subscriber(&cfg).unwrap();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("hidden-at-info");
            tracing::info!("visible-at-info");

            // Enable debug mode with a short 25ms duration to prove automatic background revert!
            handle.enable_debug_mode(Duration::from_millis(25)).unwrap();
            assert!(handle.is_debug_mode_active());
            tracing::debug!("visible-during-debug-window");

            // Wait for automatic Condvar timer revert
            let deadline = Instant::now() + Duration::from_secs(2);
            while handle.is_debug_mode_active() && Instant::now() < deadline {
                thread::yield_now();
            }
            assert!(!handle.is_debug_mode_active());
            tracing::debug!("hidden-after-auto-revert");
        });

        handle.flush();
        let content = fs::read_to_string(handle.active_log_path()).unwrap();
        assert!(!content.contains("hidden-at-info"));
        assert!(content.contains("visible-at-info"));
        assert!(content.contains("visible-during-debug-window"));
        assert!(!content.contains("hidden-after-auto-revert"));

        // FlightRecorder still captured all DEBUG events in RAM!
        let snap = handle.flight_recorder().snapshot();
        assert!(snap.iter().any(|e| e.message.contains("hidden-at-info")));
        assert!(
            snap.iter()
                .any(|e| e.message.contains("hidden-after-auto-revert"))
        );
    }
}
