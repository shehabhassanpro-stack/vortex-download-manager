//! Startup Self-Healing (`§24.2`), Graceful Shutdown Coordinator (`<= 5s` flush +
//! `PRAGMA wal_checkpoint(TRUNCATE)` + `WM_QUERYENDSESSION`/`WM_ENDSESSION` listener — `§24.3`),
//! and Background Internet Connectivity Monitor (`P1-T22`).

#![allow(unsafe_code)]

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppEvent, AppResult, DatabaseHealthPort, DownloadRepository, EngineControlPort, EventBus,
    SidecarMetadata,
};
use vortex_domain::{
    CategorizationPolicy, Clock, Download, DownloadCommand, DownloadId, Priority, QueueId,
    Resource, SegmentMap, Timestamps,
};
use vortex_error::TraceId;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    RegisterClassW, TranslateMessage, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

/// Summary returned by [`run_startup_self_healing`] (`§24.2` & `P1-T22`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelfHealingSummary {
    /// Downloads that were active during a prior crash and cleanly reconciled to `Paused`.
    pub reconciled_dirty_ids: Vec<DownloadId>,
    /// Orphan `.vortex.meta` sidecars re-imported into SQLite.
    pub imported_sidecar_ids: Vec<DownloadId>,
    /// Unrecoverable zero-byte/orphan `.vortex` files cleaned up.
    pub cleaned_orphan_files: usize,
}

/// Reconciles prior dirty active states to resumable `Paused` and audits `.vortex` / `.vortex.meta`
/// files across `scan_dirs` (`§24.2` & `P1-T22`).
///
/// # Errors
/// Returns an error if querying or saving reconciled downloads in SQLite fails.
pub async fn run_startup_self_healing(
    download_repo: &dyn DownloadRepository,
    scan_dirs: &[PathBuf],
    clock: &dyn Clock,
    trace_id: TraceId,
) -> AppResult<SelfHealingSummary> {
    let mut summary = SelfHealingSummary::default();
    let existing = download_repo.list_all(trace_id).await?;
    let mut known_ids: HashSet<DownloadId> = HashSet::with_capacity(existing.len());
    let mut known_temp_paths: HashSet<PathBuf> = HashSet::with_capacity(existing.len());
    let now_ms = clock.now_unix_ms();

    // 1. Reconcile any download that crashed while `Probing`, `Downloading`, `Retrying`, `PostProcessing`, or `Verifying`
    for mut dl in existing {
        known_ids.insert(dl.id);
        known_temp_paths.insert(dl.temp_path());

        if dl.state.is_active() {
            dl.segments.clear_active_workers();
            // Force-reconcile to `Paused` preserving every durably committed `RangeSet` byte!
            dl.state = vortex_domain::DownloadState::Paused {
                paused_at_ms: now_ms,
                downloaded_bytes: dl.downloaded_bytes(),
            };
            dl.times.updated_at_ms = now_ms;
            let _ = dl.apply(DownloadCommand::Pause, now_ms, trace_id);
            download_repo.save(&dl, trace_id).await?;
            summary.reconciled_dirty_ids.push(dl.id);
        }
    }

    // 2. Audit download directories for orphan `.vortex.meta` sidecars or dangling `0`-byte `.vortex` files
    for dir in scan_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name_str) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if name_str.ends_with(".vortex.meta") {
                if let Ok(raw) = std::fs::read_to_string(&path)
                    && let Ok(meta) = serde_json::from_str::<SidecarMetadata>(&raw)
                    && !known_ids.contains(&meta.download_id)
                {
                    let temp_vortex = meta.save_dir.join(format!("{}.vortex", meta.file_name));
                    if temp_vortex.exists() {
                        let cat = CategorizationPolicy::classify(&meta.file_name, None).id;
                        let downloaded = meta.completed_ranges.total_bytes();
                        let mut segments = SegmentMap::new(meta.total_size);
                        segments.completed = meta.completed_ranges;
                        let mut resource = Resource::new(meta.url);
                        resource.final_url = meta.final_url;
                        resource.validators = meta.validators;

                        let recovered = Download {
                            id: meta.download_id,
                            resource,
                            save_dir: meta.save_dir,
                            file_name: meta.file_name,
                            total_size: meta.total_size,
                            state: vortex_domain::DownloadState::Paused {
                                paused_at_ms: now_ms,
                                downloaded_bytes: downloaded,
                            },
                            segments,
                            priority: Priority::Normal,
                            category_id: cat,
                            queue_id: QueueId::main_queue(),
                            supports_range: meta.supports_range,
                            expected_hash: meta.expected_hash,
                            times: Timestamps {
                                created_at_ms: meta.updated_at_ms,
                                started_at_ms: Some(meta.updated_at_ms),
                                updated_at_ms: now_ms,
                                completed_at_ms: None,
                            },
                        };
                        download_repo.save(&recovered, trace_id).await?;
                        known_ids.insert(recovered.id);
                        known_temp_paths.insert(recovered.temp_path());
                        summary.imported_sidecar_ids.push(recovered.id);
                    }
                }
            } else if std::path::Path::new(name_str)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("vortex"))
                && !known_temp_paths.contains(&path)
            {
                let companion_meta = PathBuf::from(format!("{}.meta", path.display()));
                let is_empty = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
                if !companion_meta.exists() && is_empty && std::fs::remove_file(&path).is_ok() {
                    summary.cleaned_orphan_files += 1;
                }
            }
        }
    }

    Ok(summary)
}

/// Coordinates the `<= 5s` Graceful Shutdown sequence (`§24.3` & `ADR-017`):
/// 1. Cancels IPC & background monitors (`shutdown_token`).
/// 2. Flushes all `DownloadActor` checkpoints (`EngineControlPort::shutdown_gracefully`).
/// 3. Executes `PRAGMA wal_checkpoint(TRUNCATE)` (`DatabaseHealthPort::wal_checkpoint_truncate`).
pub struct GracefulShutdownCoordinator {
    engine: Arc<dyn EngineControlPort>,
    db_health: Arc<dyn DatabaseHealthPort>,
    shutdown_token: CancellationToken,
    executed: AtomicBool,
}

impl std::fmt::Debug for GracefulShutdownCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GracefulShutdownCoordinator")
            .finish_non_exhaustive()
    }
}

impl GracefulShutdownCoordinator {
    /// Creates a new [`GracefulShutdownCoordinator`].
    #[must_use]
    pub fn new(
        engine: Arc<dyn EngineControlPort>,
        db_health: Arc<dyn DatabaseHealthPort>,
        shutdown_token: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            db_health,
            shutdown_token,
            executed: AtomicBool::new(false),
        })
    }

    /// Shared root [`CancellationToken`] cancelled when OS or user triggers shutdown.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Runs the complete idempotent Graceful Shutdown protocol within `<= 5 seconds`.
    ///
    /// # Errors
    /// Returns an error only if `wal_checkpoint(TRUNCATE)` fails after actor flush.
    pub async fn execute(&self, trace_id: TraceId) -> AppResult<()> {
        if self.executed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        tracing::info!(%trace_id, "Initiating Vortex Graceful Shutdown Protocol (<= 5s budget)");
        self.shutdown_token.cancel();

        // Flush all active actors & checkpoints within strict <= 4.5s budget
        let _ = tokio::time::timeout(
            Duration::from_millis(4_500),
            self.engine.shutdown_gracefully(trace_id),
        )
        .await;

        // Merge SQLite WAL cleanly into main DB file (`PRAGMA wal_checkpoint(TRUNCATE)`)
        self.db_health.wal_checkpoint_truncate(trace_id).await?;
        tracing::info!(%trace_id, "Graceful Shutdown completed cleanly (WAL truncated)");
        Ok(())
    }
}

static OS_SHUTDOWN_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

// SAFETY: Win32 `WNDPROC` callback invoked exclusively by the OS message pump on our dedicated
// message-only window thread; touches only lock-free `OS_SHUTDOWN_NOTIFY` or delegates to `DefWindowProcW`.
unsafe extern "system" fn shutdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_QUERYENDSESSION || msg == WM_ENDSESSION {
        if let Some(notify) = OS_SHUTDOWN_NOTIFY.get() {
            notify.notify_waiters();
        }
        return 1;
    }
    // SAFETY: Standard Win32 default window procedure forwarding.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Installs OS shutdown listeners (`WM_QUERYENDSESSION`, `WM_ENDSESSION`, and `Ctrl+C`) that
/// automatically invoke [`GracefulShutdownCoordinator::execute`] (`§24.3` & `P1-T22`).
pub fn install_os_shutdown_listeners(coordinator: Arc<GracefulShutdownCoordinator>) {
    let notify = Arc::clone(OS_SHUTDOWN_NOTIFY.get_or_init(|| Arc::new(Notify::new())));

    // Spawn dedicated lightweight Win32 hidden window thread for `WM_QUERYENDSESSION` / `WM_ENDSESSION`
    let _ = std::thread::Builder::new()
        .name("vortex-wm-endsession".to_owned())
        .spawn(|| {
            let class_name: Vec<u16> = OsStr::new("VortexShutdownListenerWindow")
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: Registers a minimal Win32 window class and pumps messages until thread exit.
            unsafe {
                let hinstance = GetModuleHandleW(ptr::null());
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(shutdown_wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinstance,
                    hIcon: ptr::null_mut(),
                    hCursor: ptr::null_mut(),
                    hbrBackground: ptr::null_mut(),
                    lpszMenuName: ptr::null(),
                    lpszClassName: class_name.as_ptr(),
                };
                let _ = RegisterClassW(&raw const wc);
                let hwnd = CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    ptr::null_mut(),
                    hinstance,
                    ptr::null(),
                );
                if !hwnd.is_null() {
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) > 0 {
                        let _ = TranslateMessage(&raw const msg);
                        let _ = DispatchMessageW(&raw const msg);
                    }
                }
            }
        });

    let cancel = coordinator.token();
    tokio::spawn(async move {
        tokio::select! {
            () = notify.notified() => {
                let _ = coordinator.execute(TraceId::new()).await;
            }
            Ok(()) = tokio::signal::ctrl_c() => {
                let _ = coordinator.execute(TraceId::new()).await;
            }
            () = cancel.cancelled() => {}
        }
    });
}

/// Spawns a lightweight connectivity monitor publishing [`AppEvent::ConnectivityChanged`] on transitions (`P1-T22`).
pub fn spawn_connectivity_monitor(
    online_flag: Arc<AtomicBool>,
    probe_fn: Arc<dyn Fn() -> bool + Send + Sync>,
    event_bus: Arc<dyn EventBus>,
    interval: Duration,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(100)));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let probe = Arc::clone(&probe_fn);
                    let now_online = tokio::task::spawn_blocking(move || probe())
                        .await
                        .unwrap_or(true);
                    let prev = online_flag.swap(now_online, Ordering::SeqCst);
                    if prev != now_online {
                        event_bus.publish(AppEvent::ConnectivityChanged { online: now_online });
                    }
                }
                () = cancel.cancelled() => break,
            }
        }
    })
}
