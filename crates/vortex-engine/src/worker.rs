//! `DownloadActor`, concurrent overlapped `2 MiB` coalesced workers, `BBR/Gradient2 + AIMD`
//! dynamic auto-scaler, and `End-Game` Hedged Requests (`P1-T19`, `P1-T20`, `P1.6-T04`, `P1.7-T06`..`T08`).
//!
//! Architecture highlights (`ADR-008`, `ADR-009`, `ADR-021`):
//! - **Per-Worker `2 MiB` Write Coalescing + True Parallel Overlapped I/O (`P1.7-T08`):** each worker
//!   buffers contiguous bytes in a `2 MiB` `BytesMut` slab and calls `FileSink::write_at` directly
//!   in parallel under a global `32 MiB` byte-weighted `Semaphore`, reducing `WriteFile` syscalls
//!   by `128x` while `SparseFileSink`'s `RwLock` Flush Barrier preserves `ADR-008` durability.
//! - **Smart Staggered Launch + Decoupled Transient Drops (`P1.7-T06`):** proven fast hosts burst
//!   immediately at `t = 0ms`; unknown hosts probe with `W1 -> W2 (206 OK) -> full parallel burst`;
//!   transient `NetConnectionLost` respawns a replacement worker without poisoning `SmartHostProfileCache`.
//! - **BBR/Gradient2 + AIMD Auto-Scaler & End-Game Hedged Requests (`P1.7-T07`):** dynamically scales
//!   uncapped hosts up to `32` connections without Bufferbloat, and rescues slow tail stragglers in
//!   the final `<= min(2%, 16 MiB)` while unconditionally disabling hedging on `hard_cap == Some(1)` hosts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppConfig, AppResult, ByteSource, ByteStream, FileSink, FileSinkFactory, PlatformIntegration,
    ProbeOutcome, ProgressSnapshotDto, ResourceInfo, ResourceRequest, SecretStore,
    SpaceReservationId, TransportMetrics,
};
use vortex_domain::{
    AdaptiveConcurrencyConfig, AdaptiveConcurrencyController, ByteRange, Clock,
    ConcurrencyDecision, ConcurrencyObservation, DomainEvent, DomainEventEnvelope, Download,
    DownloadCommand, DownloadState, DualHorizonEwma, LeaseDecision, PostProcessStage, RangeSet,
    SegmentMap, SegmentationPolicy, WorkerId, extract_url_host, worth_hedging,
};
use vortex_error::{ApiError, ErrorCode, TraceId, VortexError};

use crate::actors::{PersistenceHandle, build_sidecar_snapshot};
use crate::host_capability::SmartHostProfileCache;

/// Worker event channel capacity (`P1.7-T08`).
const WORKER_EVENT_CAPACITY: usize = 256;

/// Per-worker contiguous disk write coalescing slab (`2 MiB` -> `128x` fewer `WriteFile` syscalls!).
pub const WORKER_COALESCE_BYTES: usize = 2 * 1024 * 1024;

/// Global byte-weighted in-flight write RAM budget (`32 MiB` across all workers).
const BYTE_BACKPRESSURE_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Semaphore permit quantum (`64 KiB` per permit -> `512` permits = `32 MiB`).
const PERMIT_QUANTUM_BYTES: usize = 64 * 1024;

/// Total semaphore permits backing the `32 MiB` in-flight write budget.
const TOTAL_WRITE_PERMITS: usize = BYTE_BACKPRESSURE_BUDGET_BYTES / PERMIT_QUANTUM_BYTES;

/// On very slow links (`< 350 KiB/s` per worker), flush partial `>= 256 KiB` slabs after `750ms`
/// so progress updates smoothly and Pause wastes near-zero bytes without harming fast `2 MiB` writes!
const SLOW_LINK_FLUSH_MIN_BYTES: usize = 256 * 1024;
const SLOW_LINK_FLUSH_MAX_MS: u64 = 750;

/// Minimum bytes delivered by Worker #1 before launching Probe Guard (`Worker #2`).
const SLOW_START_WAVE1_MIN_BYTES: u64 = 16 * 1024;

/// Minimum pacing delay (`ms`) between Wave 0 (`Worker #1`) and Probe Guard (`Worker #2`).
const SLOW_START_WAVE1_DELAY_MS: i64 = 10;

/// Live EWMA & UI snapshot refresh cadence (`50ms` = `20Hz`, reduced from `15ms` lock chatter).
const RATE_TICK_INTERVAL_MS: u64 = 50;

/// Periodic `BBR/Gradient2 + AIMD` concurrency evaluation cadence (`1.5s`).
const BBR_EVAL_INTERVAL_MS: u64 = 1_500;

/// Periodic `End-Game` straggler detection cadence (`500ms`).
const ENDGAME_POLL_INTERVAL_MS: u64 = 500;

/// Maximum concurrent `End-Game` hedged races active simultaneously.
const MAX_CONCURRENT_HEDGES: usize = 2;

/// Bounded transient worker respawn budget per session (`P1.7-T06`).
const MAX_TRANSIENT_WORKER_RESPAWNS: u16 = 16;

/// Early worker speed calibration thresholds (updates `worker_speed_bps` mid-lease on Lease #1!).
const EARLY_SPEED_MIN_BYTES: u64 = 1024 * 1024;
const EARLY_SPEED_MIN_MS: u64 = 250;

/// Optional test-only fault hook injected between `FlushFileBuffers` and `SQLite COMMIT` (`P1-T20`).
pub type MidCheckpointHook = Arc<dyn Fn() -> AppResult<()> + Send + Sync>;

/// Lock-free per-worker live telemetry used by `End-Game` straggler detection (`P1.7-T07` & `T08`).
#[derive(Debug, Default)]
struct WorkerLiveTracker {
    cursor: AtomicU64,
    lease_end: AtomicU64,
    lease_bytes: AtomicU64,
    lease_start_ms: AtomicI64,
}

impl WorkerLiveTracker {
    fn reset_lease(&self, start: u64, end: u64, now_ms: i64) {
        self.cursor.store(start, Ordering::Release);
        self.lease_end.store(end, Ordering::Release);
        self.lease_bytes.store(0, Ordering::Release);
        self.lease_start_ms.store(now_ms, Ordering::Release);
    }

    fn record_rx(&self, bytes: u64) {
        self.lease_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn advance_cursor(&self, new_cursor: u64) {
        self.cursor.store(new_cursor, Ordering::Release);
    }

    fn clear_lease(&self) {
        self.lease_end.store(0, Ordering::Release);
    }

    /// Returns `(cursor, lease_end, live_speed_bps)` if the worker currently has an active lease.
    #[allow(clippy::cast_precision_loss)]
    fn snapshot(&self, now_ms: i64) -> Option<(u64, u64, f64)> {
        let end = self.lease_end.load(Ordering::Acquire);
        let cur = self.cursor.load(Ordering::Acquire);
        if end == 0 || cur >= end {
            return None;
        }
        let start_ms = self.lease_start_ms.load(Ordering::Acquire);
        let elapsed_ms = now_ms.saturating_sub(start_ms).max(1);
        let rx = self.lease_bytes.load(Ordering::Relaxed);
        let speed_bps = (rx as f64) / (elapsed_ms as f64 / 1_000.0);
        Some((cur, end, speed_bps))
    }
}

#[derive(Debug)]
enum WorkerReport {
    /// Emitted once Worker #1 receives its first payload chunk or a secondary worker opens HTTP `206`.
    StreamOpened {
        worker_id: WorkerId,
        is_http2: bool,
        rtt_us: Option<u64>,
        min_rtt_us: Option<u64>,
    },
    /// Emitted after a coalesced slab (`<= 2 MiB`) has completed overlapped `FileSink::write_at`.
    ChunkWritten {
        worker_id: WorkerId,
        range: ByteRange,
        speed_bps: f64,
    },
    /// Emitted when a worker completes a bounded HTTP range lease.
    LeaseFinished {
        worker_id: WorkerId,
        rtt_us: Option<u64>,
        min_rtt_us: Option<u64>,
        is_http2: bool,
        reused_conn: bool,
        speed_bps: f64,
    },
    /// Emitted when a worker exits cleanly (no work remaining or graceful BBR step-down retirement).
    WorkerExited { worker_id: WorkerId },
    /// Emitted when an `End-Game` hedged worker completes racing a straggler's tail.
    HedgeFinished {
        victim_id: WorkerId,
        succeeded: bool,
    },
    /// Emitted when a worker encounters a network or disk error.
    WorkerError {
        worker_id: WorkerId,
        error: VortexError,
    },
}

/// Control messages sent from `Supervisor` to a running `DownloadActor`.
pub enum ActorControlMsg {
    /// Flush durable checkpoint, save `Paused` state in SQLite, and exit cleanly.
    Pause {
        /// Correlation trace ID.
        trace_id: TraceId,
        /// Completion reply.
        ack: oneshot::Sender<AppResult<()>>,
    },
    /// Abort immediately and optionally delete `.vortex` / `.vortex.meta`.
    Cancel {
        /// Delete partial files on disk.
        delete_partial: bool,
        /// Correlation trace ID.
        trace_id: TraceId,
        /// Completion reply.
        ack: oneshot::Sender<AppResult<()>>,
    },
}

/// Shared runtime dependencies injected into every `DownloadActor`.
#[derive(Clone)]
pub struct EngineContext {
    /// Network adapter (`HttpByteSource`).
    pub byte_source: Arc<dyn ByteSource>,
    /// Sparse disk sink factory + `VolumeSpaceLedger`.
    pub sink_factory: Arc<dyn FileSinkFactory>,
    /// Single-writer SQLite persistence actor handle.
    pub persistence: PersistenceHandle,
    /// Encrypted credential store (`DPAPI`).
    pub secret_store: Arc<dyn SecretStore>,
    /// OS integration (`MotW`, `IAttachmentExecute`, sleep inhibitor).
    pub platform: Arc<dyn PlatformIntegration>,
    /// Deterministic/system clock.
    pub clock: Arc<dyn Clock>,
    /// Live hot-reloaded configuration receiver.
    pub config_rx: watch::Receiver<Arc<AppConfig>>,
    /// Shared smart per-host profile cache (`P1.6-T04` & `P1.7-T06`).
    pub host_capabilities: Arc<SmartHostProfileCache>,
    /// Optional failpoint hook executed strictly between `FlushFileBuffers` and `SQLite COMMIT`.
    pub mid_checkpoint_hook: Option<MidCheckpointHook>,
}

/// Shared session state passed to every worker task (zero per-16KiB mutex overhead!).
#[derive(Clone)]
struct SharedWorkerEnv {
    req: ResourceRequest,
    byte_source: Arc<dyn ByteSource>,
    sink: Arc<dyn FileSink>,
    write_permits: Arc<Semaphore>,
    shared_map: Arc<AsyncMutex<SegmentMap>>,
    report_tx: mpsc::Sender<WorkerReport>,
    cancel: CancellationToken,
    preopened_holder: Arc<AsyncMutex<Option<Box<dyn ByteStream>>>>,
    first_stream_taken: Arc<AtomicBool>,
    probe_stop_offset: Arc<AtomicU64>,
    session_live_bytes: Arc<AtomicU64>,
    worker_one_live_bytes: Arc<AtomicU64>,
    target_workers_atomic: Arc<AtomicU16>,
    active_worker_count: Arc<AtomicU16>,
    clock: Arc<dyn Clock>,
    supports_range: bool,
    known_total: Option<u64>,
    trace_id: TraceId,
}

/// Runs the complete lifecycle of a single `DownloadActor` (`Single Writer Principle` — `§10.1`).
///
/// # Errors
/// Returns [`VortexError`] when an unrecoverable domain invariant or persistence failure occurs.
#[allow(clippy::too_many_lines)]
pub async fn run_download_actor(
    mut download: Download,
    preopened_probe: Option<(Box<ResourceInfo>, Box<dyn ByteStream>)>,
    mut ctrl_rx: mpsc::Receiver<ActorControlMsg>,
    snapshot_tx: watch::Sender<Option<ProgressSnapshotDto>>,
    shutdown_token: CancellationToken,
    ctx: EngineContext,
    initial_trace: TraceId,
) -> AppResult<()> {
    let creds = ctx
        .secret_store
        .load(download.id, initial_trace)
        .await
        .unwrap_or(None);

    let mut base_req = ResourceRequest::new(
        download.id,
        download.resource.effective_url(),
        initial_trace,
    );
    base_req.referrer.clone_from(&download.resource.referrer);
    base_req
        .top_level_site
        .clone_from(&download.resource.top_level_site);
    base_req
        .user_agent
        .clone_from(&download.resource.user_agent);
    base_req.headers.clone_from(&download.resource.headers);
    base_req
        .validators
        .clone_from(&download.resource.validators);
    if let Some(c) = creds {
        // Origin-Pinned Secret Vault (`TH-28`): never replay stored cookies/auth to a mismatched origin!
        if c.origin_matches(download.resource.effective_url()) {
            base_req.cookies = c.cookies;
            base_req.authorization = c.authorization;
        } else {
            tracing::warn!(
                error_code = %ErrorCode::SecPolicyViolation,
                download_id = %download.id,
                "Stored credentials origin mismatch detected; refusing to attach cookies/auth (TH-28)"
            );
        }
    }

    let mut attempt: u32 = 0;
    let mut preopened = preopened_probe;

    'retry_loop: loop {
        let cfg = Arc::clone(&ctx.config_rx.borrow());
        let now_ms = ctx.clock.now_unix_ms();

        // 1. Transition to `Probing` if not already in `Probing`
        if !matches!(download.state, DownloadState::Probing { .. }) {
            let evs = download.apply(DownloadCommand::StartProbe, now_ms, initial_trace)?;
            ctx.persistence
                .save_and_wait(download.clone(), evs, initial_trace)
                .await?;
        }

        // 2. Execute or reuse `probe_and_stream` (`0ms` effective start when `preopened` is `Some`!)
        let (probe_info, initial_stream_opt) = if let Some((info, stream)) = preopened.take() {
            (*info, Some(stream))
        } else {
            base_req.url = download.resource.effective_url().to_owned();
            base_req
                .validators
                .clone_from(&download.resource.validators);
            match ctx.byte_source.probe_and_stream(&base_req).await {
                Ok(ProbeOutcome::Transferable {
                    info,
                    initial_stream,
                }) => (*info, Some(initial_stream)),
                Ok(ProbeOutcome::FallbackToBrowser { detail, .. }) => {
                    let err = ApiError::new(ErrorCode::ResLinkExpired, initial_trace)
                        .with_technical_detail(detail);
                    let evs = download.apply(
                        DownloadCommand::Fail { error: err.clone() },
                        ctx.clock.now_unix_ms(),
                        initial_trace,
                    )?;
                    let _ = ctx
                        .persistence
                        .save_and_wait(download, evs, initial_trace)
                        .await;
                    let _ = snapshot_tx.send(None);
                    return Err(VortexError::new(err));
                }
                Err(probe_err) => {
                    if handle_error_or_schedule_retry(
                        &mut download,
                        &probe_err,
                        &mut attempt,
                        &cfg,
                        &ctx,
                        &mut ctrl_rx,
                        &shutdown_token,
                        &snapshot_tx,
                        initial_trace,
                    )
                    .await?
                    {
                        continue 'retry_loop;
                    }
                    return Ok(());
                }
            }
        };

        let initial_metrics = probe_info.transport_metrics;

        // 3. Apply `CompleteProbe` to `Download` aggregate (verifies RFC 9110 resume validators!)
        let redacted_final =
            Some(vortex_observability::RedactedUrl::parse(&probe_info.final_url).to_string());
        let probe_events = download.apply(
            DownloadCommand::CompleteProbe {
                total_size: probe_info.total_size,
                supports_range: probe_info.supports_range,
                validators: probe_info.validators.clone(),
                final_url: Some(probe_info.final_url.clone()),
                resolved_file_name: probe_info.suggested_filename.clone(),
                queue_full: false,
                final_url_redacted: redacted_final,
            },
            ctx.clock.now_unix_ms(),
            initial_trace,
        )?;
        ctx.persistence
            .save_and_wait(download.clone(), probe_events, initial_trace)
            .await?;

        // If resume validator mismatch moved `download` into `NeedsAttention`, stop cleanly!
        if matches!(download.state, DownloadState::NeedsAttention { .. }) {
            let _ = snapshot_tx.send(None);
            return Ok(());
        }

        base_req.url = download.resource.effective_url().to_owned();
        base_req
            .validators
            .clone_from(&download.resource.validators);

        // 3b. Anti-Hole Corruption Guard on Resume (`UX-M03`): if user deleted `.vortex` while paused
        //     and `completed` ranges were non-empty, reset ranges cleanly instead of writing a hollow sparse file!
        if !download.segments.completed.is_empty() && !download.temp_path().exists() {
            let missing_path = download.temp_path();
            tracing::warn!(
                error_code = %ErrorCode::DiskPartialFileMissing,
                download_id = %download.id,
                path = %missing_path.display(),
                "Partial .vortex file missing on resume; resetting progress to byte 0 to prevent sparse zero-hole corruption (UX-M03)"
            );
            download.segments.completed = RangeSet::new();
            download.segments.clear_active_workers();
            let _ = ctx
                .persistence
                .save_and_wait(download.clone(), vec![], initial_trace)
                .await;
        }

        // 4. Pre-flight FAT32 `4 GiB` filesystem ceiling check (`UX-M02`) + logical disk-space reserve (`ADR-007` & `TH-14`)
        let ledger = ctx.sink_factory.space_ledger();
        if let Some(total) = download.total_size
            && let Err(fs_err) =
                ledger.check_file_size_supported(&download.save_dir, total, initial_trace)
        {
            let evs = download.apply(
                DownloadCommand::MarkNeedsAttention {
                    reason_code: fs_err.code(),
                    detail: fs_err.as_api_error().technical_detail.clone(),
                },
                ctx.clock.now_unix_ms(),
                initial_trace,
            )?;
            let _ = ctx
                .persistence
                .save_and_wait(download, evs, initial_trace)
                .await;
            let _ = snapshot_tx.send(None);
            return Err(fs_err);
        }

        let remaining_needed = download
            .segments
            .remaining_bytes()
            .unwrap_or(16 * 1024 * 1024);
        let space_reservation = match ledger.reserve(
            &download.save_dir,
            remaining_needed,
            cfg.min_free_disk_bytes,
            initial_trace,
        ) {
            Ok(res_id) => res_id,
            Err(space_err) => {
                let evs = download.apply(
                    DownloadCommand::MarkNeedsAttention {
                        reason_code: space_err.code(),
                        detail: space_err.as_api_error().technical_detail.clone(),
                    },
                    ctx.clock.now_unix_ms(),
                    initial_trace,
                )?;
                let _ = ctx
                    .persistence
                    .save_and_wait(download, evs, initial_trace)
                    .await;
                let _ = snapshot_tx.send(None);
                return Err(space_err);
            }
        };

        // 5. Open `.vortex` overlapped sparse sink and run active transfer session
        let session_res = run_transfer_session(
            &mut download,
            &base_req,
            initial_stream_opt,
            initial_metrics,
            space_reservation,
            &cfg,
            &ctx,
            &mut ctrl_rx,
            &snapshot_tx,
            &shutdown_token,
            initial_trace,
        )
        .await;

        ledger.release(space_reservation);

        match session_res {
            Ok(SessionExit::Completed | SessionExit::PausedOrCancelled) => {
                let _ = snapshot_tx.send(None);
                return Ok(());
            }
            Err(transfer_err) => {
                if handle_error_or_schedule_retry(
                    &mut download,
                    &transfer_err,
                    &mut attempt,
                    &cfg,
                    &ctx,
                    &mut ctrl_rx,
                    &shutdown_token,
                    &snapshot_tx,
                    initial_trace,
                )
                .await?
                {
                    continue 'retry_loop;
                }
                let _ = snapshot_tx.send(None);
                return Ok(());
            }
        }
    }
}

enum SessionExit {
    Completed,
    PausedOrCancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchPhase {
    /// Proven fast host or single-stream target already spawned in full.
    FullLaunched,
    /// Waiting for Worker #1 to deliver `>= 16 KiB` & `>= 10ms` before launching Probe Guard `W2`.
    AwaitingWave1Probe,
    /// Waiting for `W2` to confirm `206 Partial Content` before launching `3..=effective_max_workers` in one burst!
    AwaitingWave2Burst,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_transfer_session(
    download: &mut Download,
    base_req: &ResourceRequest,
    mut initial_stream_opt: Option<Box<dyn ByteStream>>,
    initial_metrics: TransportMetrics,
    space_reservation: SpaceReservationId,
    cfg: &AppConfig,
    ctx: &EngineContext,
    ctrl_rx: &mut mpsc::Receiver<ActorControlMsg>,
    snapshot_tx: &watch::Sender<Option<ProgressSnapshotDto>>,
    shutdown_token: &CancellationToken,
    trace_id: TraceId,
) -> AppResult<SessionExit> {
    let temp_path = download.temp_path();
    let meta_path = download.meta_sidecar_path();
    let sink = ctx
        .sink_factory
        .open(&temp_path, &meta_path, trace_id)
        .await?;
    let _is_sparse = sink.prepare(download.total_size, trace_id).await?;

    let (report_tx, mut report_rx) = mpsc::channel::<WorkerReport>(WORKER_EVENT_CAPACITY);
    let workers_cancel = shutdown_token.child_token();
    let ledger = ctx.sink_factory.space_ledger();

    let shared_map = Arc::new(AsyncMutex::new(download.segments.clone()));
    let write_permits = Arc::new(Semaphore::new(TOTAL_WRITE_PERMITS));
    let session_live_bytes = Arc::new(AtomicU64::new(0));
    let worker_one_live_bytes = Arc::new(AtomicU64::new(0));
    let probe_stop_offset = Arc::new(AtomicU64::new(u64::MAX));
    let active_worker_count = Arc::new(AtomicU16::new(0));

    // Determine initial target concurrency, hard ceiling (`<= 32`), and `SmartHostProfileCache` state (`P1.7-T06`)!
    let origin_host = extract_url_host(download.resource.effective_url()).unwrap_or_default();
    let max_by_size: u16 = match (download.supports_range, download.total_size) {
        (_, Some(0)) => 0,
        (true, Some(sz)) if sz > 0 => u16::try_from((sz / (1024 * 1024)).clamp(1, 32)).unwrap_or(1),
        _ => 1,
    };
    let user_initial_workers = u16::from(cfg.max_connections_per_download).min(max_by_size);
    let auto_scale_ceiling: u16 = if cfg.auto_scale_connections && download.supports_range {
        max_by_size.min(32)
    } else {
        user_initial_workers
    };

    let mut hard_cap_active: Option<u16> = if origin_host.is_empty() || max_by_size <= 1 {
        None
    } else {
        ctx.host_capabilities.hard_cap(&origin_host)
    };
    let proven_workers: Option<u16> = if origin_host.is_empty() || !download.supports_range {
        None
    } else {
        ctx.host_capabilities.proven_concurrency(&origin_host)
    };

    let mut effective_max_workers: u16 = if max_by_size == 0 {
        0
    } else if !download.supports_range || max_by_size == 1 {
        1
    } else if let Some(cap) = hard_cap_active {
        if let Some(probe_cap) = ctx
            .host_capabilities
            .try_claim_half_open_probe(&origin_host)
        {
            hard_cap_active = None;
            probe_cap.min(auto_scale_ceiling).max(1)
        } else {
            user_initial_workers.min(cap).max(1)
        }
    } else if let Some(proven) = proven_workers {
        proven
            .max(user_initial_workers)
            .min(auto_scale_ceiling)
            .max(1)
    } else {
        user_initial_workers.max(1)
    };

    let target_workers_atomic = Arc::new(AtomicU16::new(effective_max_workers.max(1)));

    // If resuming with existing bytes > 0, discard byte-0 `initial_stream` unless `[0, ...)` is still needed!
    let needs_offset_zero = download
        .segments
        .unassigned_gaps()
        .first()
        .is_some_and(|g| g.start() == 0)
        || download.total_size.is_none();
    if !needs_offset_zero {
        initial_stream_opt = None;
    }

    let first_stream_taken = Arc::new(AtomicBool::new(false));
    let preopened_holder = Arc::new(AsyncMutex::new(initial_stream_opt));

    let worker_env = SharedWorkerEnv {
        req: base_req.clone(),
        byte_source: Arc::clone(&ctx.byte_source),
        sink: Arc::clone(&sink),
        write_permits: Arc::clone(&write_permits),
        shared_map: Arc::clone(&shared_map),
        report_tx: report_tx.clone(),
        cancel: workers_cancel.clone(),
        preopened_holder: Arc::clone(&preopened_holder),
        first_stream_taken: Arc::clone(&first_stream_taken),
        probe_stop_offset: Arc::clone(&probe_stop_offset),
        session_live_bytes: Arc::clone(&session_live_bytes),
        worker_one_live_bytes: Arc::clone(&worker_one_live_bytes),
        target_workers_atomic: Arc::clone(&target_workers_atomic),
        active_worker_count: Arc::clone(&active_worker_count),
        clock: Arc::clone(&ctx.clock),
        supports_range: download.supports_range,
        known_total: download.total_size,
        trace_id,
    };

    let mut worker_joins = tokio::task::JoinSet::new();
    let mut next_worker_id: u16 = 1;
    let mut active_workers_set: BTreeSet<WorkerId> = BTreeSet::new();
    let mut workers_with_progress: BTreeSet<WorkerId> = BTreeSet::new();
    let mut worker_trackers: BTreeMap<WorkerId, Arc<WorkerLiveTracker>> = BTreeMap::new();
    let mut worker_speeds: BTreeMap<WorkerId, f64> = BTreeMap::new();
    let mut hedged_victims: BTreeSet<WorkerId> = BTreeSet::new();
    let mut active_hedges: usize = 0;

    let mut step_down_locked = false;
    let mut transient_respawns: u16 = 0;
    let mut peak_healthy_workers: u16 = 0;
    let mut saw_http2 = initial_metrics.is_http2;
    let mut latest_rtt_us: Option<u64> = initial_metrics.rtt_us;
    let mut min_rtt_us: Option<u64> = initial_metrics.min_rtt_us.or(initial_metrics.rtt_us);
    let mut interval_rate_limited = false;
    let session_start_ms = ctx.clock.now_unix_ms();

    let mut spawn_one_worker = |joins: &mut tokio::task::JoinSet<()>,
                                active_set: &mut BTreeSet<WorkerId>,
                                trackers: &mut BTreeMap<WorkerId, Arc<WorkerLiveTracker>>|
     -> WorkerId {
        let wid = WorkerId::new(next_worker_id);
        next_worker_id = next_worker_id.saturating_add(1);
        active_set.insert(wid);
        active_worker_count.store(
            u16::try_from(active_set.len()).unwrap_or(u16::MAX),
            Ordering::Release,
        );
        let tracker = Arc::new(WorkerLiveTracker::default());
        trackers.insert(wid, Arc::clone(&tracker));
        let env = worker_env.clone();
        joins.spawn(async move {
            run_worker_loop(wid, tracker, env).await;
        });
        wid
    };

    // Smart Staggered Launch (`P1.7-T06`):
    // - Proven fast host (`proven_workers.is_some()`): launch ALL `1..=effective_max_workers` at `t = 0ms`!
    // - Unknown host: launch `W1` -> Probe Guard `W2` -> burst `3..=effective_max_workers` on `W2` `206 OK`!
    let mut launch_phase = if effective_max_workers == 0 {
        LaunchPhase::FullLaunched
    } else if effective_max_workers == 1 {
        let _ = spawn_one_worker(
            &mut worker_joins,
            &mut active_workers_set,
            &mut worker_trackers,
        );
        LaunchPhase::FullLaunched
    } else if proven_workers.is_some() && hard_cap_active.is_none() {
        // Reserve Worker #1's byte-0 lease synchronously BEFORE launching the parallel burst so
        // `WorkerId(1)` ALWAYS owns `preopened_holder` (`bytes=0-`) even if `W2..WN` hit `403`!
        if download.supports_range && needs_offset_zero {
            let mut map = shared_map.lock().await;
            let _ = SegmentationPolicy::acquire_next_lease(
                &mut map,
                WorkerId::new(1),
                1_048_576.0,
                0.025,
                false,
            );
        }
        for _ in 0..effective_max_workers {
            let _ = spawn_one_worker(
                &mut worker_joins,
                &mut active_workers_set,
                &mut worker_trackers,
            );
        }
        LaunchPhase::FullLaunched
    } else {
        let _ = spawn_one_worker(
            &mut worker_joins,
            &mut active_workers_set,
            &mut worker_trackers,
        );
        LaunchPhase::AwaitingWave1Probe
    };

    let mut concurrency_ctrl = AdaptiveConcurrencyController::new(
        effective_max_workers.max(1),
        AdaptiveConcurrencyConfig {
            min_workers: 1,
            max_workers: auto_scale_ceiling.max(1),
            ..AdaptiveConcurrencyConfig::default()
        },
    );

    let mut ewma = DualHorizonEwma::new();
    let mut last_rate_ns = ctx.clock.monotonic_ns();
    let mut durable_ranges: RangeSet = download.segments.completed.clone();

    let mut checkpoint_ticker =
        tokio::time::interval(Duration::from_secs(cfg.checkpoint_interval_secs.max(5)));
    checkpoint_ticker.tick().await;
    let mut rate_ticker = tokio::time::interval(Duration::from_millis(RATE_TICK_INTERVAL_MS));
    let mut bbr_ticker = tokio::time::interval(Duration::from_millis(BBR_EVAL_INTERVAL_MS));
    bbr_ticker.tick().await;
    let mut endgame_ticker = tokio::time::interval(Duration::from_millis(ENDGAME_POLL_INTERVAL_MS));
    endgame_ticker.tick().await;

    let mut fatal_worker_err: Option<VortexError> = None;
    let mut pause_ack_opt: Option<(TraceId, oneshot::Sender<AppResult<()>>)> = None;
    let mut cancel_req_opt: Option<(bool, TraceId, oneshot::Sender<AppResult<()>>)> = None;
    let mut os_shutdown_triggered = false;

    loop {
        // Check completion when `total_size` is known
        if let Some(total) = download.total_size
            && download.segments.completed.is_complete(total)
        {
            break;
        }

        // All workers exited cleanly (unknown-size EOF or finished leases)
        if active_workers_set.is_empty() && active_hedges == 0 {
            break;
        }

        // Evaluate Wave 1 Probe Guard (`W1` delivered `>= 16 KiB` & `>= 10ms`) -> spawn `W2`!
        if !step_down_locked
            && launch_phase == LaunchPhase::AwaitingWave1Probe
            && download.supports_range
            && effective_max_workers >= 2
        {
            let w1_rx = worker_one_live_bytes.load(Ordering::Relaxed);
            let elapsed_ms = ctx.clock.now_unix_ms().saturating_sub(session_start_ms);
            if w1_rx >= SLOW_START_WAVE1_MIN_BYTES && elapsed_ms >= SLOW_START_WAVE1_DELAY_MS {
                let _ = spawn_one_worker(
                    &mut worker_joins,
                    &mut active_workers_set,
                    &mut worker_trackers,
                );
                launch_phase = if effective_max_workers > 2 {
                    LaunchPhase::AwaitingWave2Burst
                } else {
                    LaunchPhase::FullLaunched
                };
            }
        }

        tokio::select! {
            biased;

            () = shutdown_token.cancelled() => {
                os_shutdown_triggered = true;
                workers_cancel.cancel();
                break;
            }

            ctrl_opt = ctrl_rx.recv() => {
                if let Some(ctrl) = ctrl_opt {
                    match ctrl {
                        ActorControlMsg::Pause { trace_id: t, ack } => {
                            pause_ack_opt = Some((t, ack));
                            workers_cancel.cancel();
                            break;
                        }
                        ActorControlMsg::Cancel { delete_partial, trace_id: t, ack } => {
                            cancel_req_opt = Some((delete_partial, t, ack));
                            workers_cancel.cancel();
                            break;
                        }
                    }
                }
            }

            rep_opt = report_rx.recv() => {
                let Some(rep) = rep_opt else {
                    break;
                };
                match rep {
                    WorkerReport::StreamOpened {
                        worker_id,
                        is_http2,
                        rtt_us,
                        min_rtt_us: m_rtt,
                    } => {
                        workers_with_progress.insert(worker_id);
                        saw_http2 = saw_http2 || is_http2;
                        update_rtt_bounds(&mut latest_rtt_us, &mut min_rtt_us, rtt_us, m_rtt);

                        // Wave 2 Parallel Burst (`P1.7-T06`): once Probe Guard `W2` confirms `206 OK`,
                        // launch `3..=effective_max_workers` ALL AT ONCE in one burst!
                        if !step_down_locked
                            && launch_phase == LaunchPhase::AwaitingWave2Burst
                            && worker_id.get() >= 2
                        {
                            let missing = usize::from(effective_max_workers)
                                .saturating_sub(active_workers_set.len());
                            for _ in 0..missing {
                                let _ = spawn_one_worker(
                                    &mut worker_joins,
                                    &mut active_workers_set,
                                    &mut worker_trackers,
                                );
                            }
                            launch_phase = LaunchPhase::FullLaunched;
                        }
                    }
                    WorkerReport::ChunkWritten {
                        worker_id,
                        range,
                        speed_bps,
                    } => {
                        let len = range.len();
                        workers_with_progress.insert(worker_id);
                        if speed_bps.is_finite() && speed_bps > 0.0 {
                            worker_speeds.insert(worker_id, speed_bps);
                        }
                        let healthy_now = u16::try_from(workers_with_progress.len()).unwrap_or(1);
                        peak_healthy_workers = peak_healthy_workers.max(healthy_now);
                        ledger.record_written(space_reservation, len);
                        let _ = download.segments.record_written(worker_id, range);
                    }
                    WorkerReport::LeaseFinished {
                        worker_id,
                        rtt_us,
                        min_rtt_us: m_rtt,
                        is_http2,
                        reused_conn,
                        speed_bps,
                    } => {
                        saw_http2 = saw_http2 || is_http2;
                        update_rtt_bounds(&mut latest_rtt_us, &mut min_rtt_us, rtt_us, m_rtt);
                        if speed_bps.is_finite() && speed_bps > 0.0 {
                            worker_speeds.insert(worker_id, speed_bps);
                        }
                        tracing::trace!(
                            worker_id = worker_id.get(),
                            ?rtt_us,
                            is_http2,
                            reused_conn,
                            "Worker lease finished"
                        );
                        let map_snap = shared_map.lock().await;
                        download.segments.active.clone_from(&map_snap.active);
                    }
                    WorkerReport::WorkerExited { worker_id } => {
                        active_workers_set.remove(&worker_id);
                        worker_trackers.remove(&worker_id);
                        active_worker_count.store(
                            u16::try_from(active_workers_set.len()).unwrap_or(0),
                            Ordering::Release,
                        );
                    }
                    WorkerReport::HedgeFinished {
                        victim_id,
                        succeeded,
                    } => {
                        active_hedges = active_hedges.saturating_sub(1);
                        if !succeeded {
                            hedged_victims.remove(&victim_id);
                        }
                    }
                    WorkerReport::WorkerError { worker_id, error } => {
                        active_workers_set.remove(&worker_id);
                        worker_trackers.remove(&worker_id);
                        active_worker_count.store(
                            u16::try_from(active_workers_set.len()).unwrap_or(0),
                            Ordering::Release,
                        );

                        let is_concurrency_rejection = matches!(
                            error.code(),
                            ErrorCode::ResLinkExpired
                                | ErrorCode::ResConcurrencyCapped
                                | ErrorCode::ResQuotaOrCaptcha
                                | ErrorCode::NetRateLimited
                        );
                        let is_transient_socket_drop = matches!(
                            error.code(),
                            ErrorCode::NetConnectionLost | ErrorCode::NetDnsFailure
                        );
                        let has_healthy_sibling = !active_workers_set.is_empty()
                            && (active_workers_set.contains(&WorkerId::new(1))
                                || active_workers_set
                                    .iter()
                                    .any(|w| workers_with_progress.contains(w)));

                        if worker_id.get() > 1 && is_transient_socket_drop && has_healthy_sibling {
                            // DECOUPLED TRANSIENT DROP (`P1.7-T06`): NEVER poison `SmartHostProfileCache`!
                            // Reclaim unwritten tail and respawn a clean replacement worker.
                            {
                                let mut map = shared_map.lock().await;
                                let _ = map.abort_and_reclaim_worker(worker_id);
                            }
                            let _ = download.segments.abort_and_reclaim_worker(worker_id);
                            let not_done = download
                                .total_size
                                .is_none_or(|t| !download.segments.completed.is_complete(t));
                            if not_done
                                && transient_respawns < MAX_TRANSIENT_WORKER_RESPAWNS
                                && active_workers_set.len() < usize::from(effective_max_workers)
                            {
                                transient_respawns = transient_respawns.saturating_add(1);
                                let replacement_id = spawn_one_worker(
                                    &mut worker_joins,
                                    &mut active_workers_set,
                                    &mut worker_trackers,
                                );
                                tracing::info!(
                                    failed_worker = worker_id.get(),
                                    replacement_worker = replacement_id.get(),
                                    error_code = %error.code(),
                                    respawn_count = transient_respawns,
                                    "Transient secondary worker drop reclaimed and respawned without poisoning host profile"
                                );
                            }
                        } else if worker_id.get() > 1
                            && is_concurrency_rejection
                            && has_healthy_sibling
                        {
                            // GOLDEN SURVIVAL INVARIANT (`Host-Resilience Spec` §2.2 / `P1.6-T04` / `P1.7-T06`):
                            // Explicit `403`/`429`/`503` concurrency rejection steps down cleanly & caches `hard_cap`!
                            {
                                let mut map = shared_map.lock().await;
                                let _ = map.abort_and_reclaim_worker(worker_id);
                            }
                            let _ = download.segments.abort_and_reclaim_worker(worker_id);
                            if error.code() == ErrorCode::NetRateLimited {
                                interval_rate_limited = true;
                            }
                            step_down_locked = true;
                            launch_phase = LaunchPhase::FullLaunched;
                            let surviving =
                                u16::try_from(active_workers_set.len()).unwrap_or(1).max(1);
                            let safe_cap = if error.code() == ErrorCode::NetRateLimited
                                && surviving > 2
                            {
                                (surviving / 2).max(1)
                            } else {
                                surviving
                            };
                            effective_max_workers = effective_max_workers.min(safe_cap).max(1);
                            hard_cap_active = Some(effective_max_workers);
                            target_workers_atomic.store(effective_max_workers, Ordering::Release);
                            if !origin_host.is_empty() {
                                ctx.host_capabilities
                                    .record_concurrency_cap(&origin_host, effective_max_workers);
                            }
                            tracing::info!(
                                error_code = %ErrorCode::ResConcurrencyCapped,
                                worker_id = worker_id.get(),
                                original_code = %error.code(),
                                new_max_workers = effective_max_workers,
                                host = %origin_host,
                                "Non-fatal concurrency step-down applied; reclaimed aborted lease and continuing seamlessly"
                            );
                        } else {
                            tracing::warn!(
                                worker_id = worker_id.get(),
                                error_code = %error.code(),
                                "Worker reported unrecoverable transfer error"
                            );
                            fatal_worker_err = Some(error);
                            workers_cancel.cancel();
                            break;
                        }
                    }
                }
            }

            // Periodic `BBR/Gradient2 + AIMD` Concurrency Auto-Scaler (`P1.7-T07`)
            _ = bbr_ticker.tick() => {
                if download.supports_range
                    && download.total_size.is_some()
                    && launch_phase == LaunchPhase::FullLaunched
                    && !step_down_locked
                    && effective_max_workers >= 1
                {
                    let active_cnt = u16::try_from(active_workers_set.len()).unwrap_or(1);
                    let obs = ConcurrencyObservation {
                        active_workers: active_cnt,
                        throughput_bps: ewma.speed_fast_bps(),
                        rtt_us: latest_rtt_us,
                        min_rtt_us,
                        rate_limited: interval_rate_limited,
                        hard_cap: hard_cap_active,
                        auto_scale_enabled: cfg.auto_scale_connections,
                    };
                    interval_rate_limited = false;

                    match concurrency_ctrl.evaluate(obs) {
                        ConcurrencyDecision::ScaleUp {
                            target_workers,
                            utility,
                            gradient,
                        } => {
                            let bounded = target_workers.min(auto_scale_ceiling);
                            if bounded > effective_max_workers {
                                let to_add = bounded.saturating_sub(effective_max_workers);
                                effective_max_workers = bounded;
                                target_workers_atomic.store(bounded, Ordering::Release);
                                for _ in 0..to_add {
                                    let _ = spawn_one_worker(
                                        &mut worker_joins,
                                        &mut active_workers_set,
                                        &mut worker_trackers,
                                    );
                                }
                                tracing::info!(
                                    new_target = bounded,
                                    utility,
                                    gradient,
                                    host = %origin_host,
                                    "BBR/Gradient2 scaled concurrency up cleanly"
                                );
                            }
                        }
                        ConcurrencyDecision::StepDown {
                            target_workers,
                            reason,
                        } => {
                            let bounded = target_workers.max(1);
                            if bounded < effective_max_workers {
                                effective_max_workers = bounded;
                                target_workers_atomic.store(bounded, Ordering::Release);
                                tracing::info!(
                                    new_target = bounded,
                                    ?reason,
                                    host = %origin_host,
                                    "BBR/Gradient2 stepped concurrency down cleanly"
                                );
                            }
                        }
                        ConcurrencyDecision::Hold { .. } => {}
                    }
                }
            }

            // Periodic `End-Game Mode` Straggler Detection & Hedged Requests (`P1.7-T07`)
            _ = endgame_ticker.tick() => {
                if download.supports_range
                    && hard_cap_active != Some(1)
                    && effective_max_workers >= 2
                    && active_hedges < MAX_CONCURRENT_HEDGES
                    && let Some(total) = download.total_size
                {
                    let total_rem = total.saturating_sub(download.downloaded_bytes());
                    let median_bps = compute_median_speed(&worker_speeds);
                    #[allow(clippy::cast_precision_loss)]
                    let rtt_secs = latest_rtt_us.map_or(0.05, |us| (us as f64) / 1_000_000.0);
                    let now_ms = ctx.clock.now_unix_ms();

                    for (&vid, tracker) in &worker_trackers {
                        if active_hedges >= MAX_CONCURRENT_HEDGES || hedged_victims.contains(&vid) {
                            continue;
                        }
                        let Some((cur, lease_end, live_speed)) = tracker.snapshot(now_ms) else {
                            continue;
                        };
                        let worker_rem = lease_end.saturating_sub(cur);
                        let effective_speed = worker_speeds
                            .get(&vid)
                            .copied()
                            .map_or(live_speed, |hist| hist.min(live_speed.max(1.0)));

                        if worth_hedging(
                            total,
                            total_rem,
                            worker_rem,
                            effective_speed,
                            median_bps,
                            rtt_secs,
                            hard_cap_active,
                        ) && let Ok(race_range) = ByteRange::new(cur, lease_end)
                        {
                            hedged_victims.insert(vid);
                            active_hedges += 1;
                            let h_env = worker_env.clone();
                            tracing::info!(
                                victim_worker = vid.get(),
                                start = race_range.start(),
                                end = race_range.end(),
                                worker_speed_bps = effective_speed,
                                median_speed_bps = median_bps,
                                "End-Game Mode: launching hedged request to rescue slow tail straggler"
                            );
                            worker_joins.spawn(async move {
                                run_hedged_race_worker(vid, race_range, h_env).await;
                            });
                        }
                    }
                }
            }

            _ = rate_ticker.tick() => {
                let now_ns = ctx.clock.monotonic_ns();
                #[allow(clippy::cast_precision_loss)]
                let dt_s = (now_ns.saturating_sub(last_rate_ns) as f64) / 1_000_000_000.0;
                if dt_s >= 0.045 {
                    let delta_bytes = session_live_bytes.swap(0, Ordering::Relaxed);
                    ewma.observe(delta_bytes, dt_s);
                    last_rate_ns = now_ns;
                }
                let map_snap = shared_map.lock().await;
                download.segments.active.clone_from(&map_snap.active);
                drop(map_snap);

                let active_w = u8::try_from(active_workers_set.len()).unwrap_or(1).max(1);
                let eta = ewma.eta_secs(download.segments.remaining_bytes());
                download.state = DownloadState::Downloading {
                    active_workers: active_w,
                    speed_fast_bps: ewma.speed_fast_bps(),
                    speed_slow_bps: ewma.speed_slow_bps(),
                    eta_secs: eta,
                    supports_range: download.supports_range,
                };
                let protocol_str = if saw_http2 { "HTTP/2" } else { "HTTP/1.1" };
                let mode_str = if hard_cap_active.is_some() {
                    "capped"
                } else if cfg.auto_scale_connections {
                    "auto"
                } else {
                    "fixed"
                };
                let _ = snapshot_tx.send(Some(ProgressSnapshotDto {
                    download_id: download.id.to_string(),
                    state_slug: "downloading".to_owned(),
                    downloaded_bytes: download.downloaded_bytes(),
                    total_size: download.total_size,
                    speed_fast_bps: ewma.speed_fast_bps(),
                    speed_slow_bps: ewma.speed_slow_bps(),
                    eta_secs: eta,
                    active_workers: active_w,
                    bins: download.segments.render_512_bins().to_vec(),
                    protocol: Some(protocol_str.to_owned()),
                    concurrency_mode: Some(mode_str.to_owned()),
                }));
            }

            // Periodic 20-second Two-Tier Durable Checkpoint (`ADR-008` & `P1-T20`)!
            _ = checkpoint_ticker.tick() => {
                let candidate = download.segments.completed.clone();
                if candidate.total_bytes() > durable_ranges.total_bytes() {
                    execute_durable_checkpoint(
                        download,
                        &candidate,
                        &sink,
                        ctx,
                        vec![],
                        trace_id,
                    ).await?;
                    durable_ranges = candidate;
                }
            }
        }
    }

    workers_cancel.cancel();
    while worker_joins.join_next().await.is_some() {}

    // Drain any remaining `ChunkWritten` notifications already written cleanly to `FileSink`
    while let Ok(rep) = report_rx.try_recv() {
        if let WorkerReport::ChunkWritten {
            worker_id, range, ..
        } = rep
        {
            let _ = download.segments.record_written(worker_id, range);
        }
    }

    // Handle explicit `Cancel`
    if let Some((delete_partial, _c_trace, ack)) = cancel_req_opt {
        drop(sink);
        if delete_partial {
            let _ = tokio::fs::remove_file(&temp_path).await;
            let _ = tokio::fs::remove_file(&meta_path).await;
        }
        let _ = ack.send(Ok(()));
        return Ok(SessionExit::PausedOrCancelled);
    }

    // Handle explicit `Pause` or OS `Graceful Shutdown` (`WM_QUERYENDSESSION`): MANDATORY Durable Checkpoint!
    if pause_ack_opt.is_some() || os_shutdown_triggered {
        let p_trace = pause_ack_opt.as_ref().map_or(trace_id, |(t, _)| *t);
        let now_ms = ctx.clock.now_unix_ms();
        let evs = download.apply(DownloadCommand::Pause, now_ms, p_trace)?;
        let candidate = download.segments.completed.clone();
        let res = execute_durable_checkpoint(download, &candidate, &sink, ctx, evs, p_trace).await;
        drop(sink);
        if let Some((_, ack)) = pause_ack_opt {
            let _ = ack.send(res);
        }
        return Ok(SessionExit::PausedOrCancelled);
    }

    // Handle worker error: flush whatever bytes already landed safely so zero progress is wasted!
    if let Some(err) = fatal_worker_err {
        let candidate = download.segments.completed.clone();
        if candidate.total_bytes() > durable_ranges.total_bytes() {
            let _ = execute_durable_checkpoint(download, &candidate, &sink, ctx, vec![], trace_id)
                .await;
        }
        drop(sink);
        return Err(err);
    }

    // Unknown-size stream finished cleanly: lock in its actual total byte length!
    if download.total_size.is_none() {
        let actual_total = download.downloaded_bytes();
        download.total_size = Some(actual_total);
        download.segments.total_size = Some(actual_total);
    }

    // Verify 100% range completeness before finalizing
    if let Some(total) = download.total_size
        && !download.segments.completed.is_complete(total)
    {
        drop(sink);
        return Err(VortexError::coded(
            ErrorCode::NetConnectionLost,
            trace_id,
            format!(
                "Incomplete transfer before finalize: {} of {total} bytes",
                download.downloaded_bytes()
            ),
        ));
    }

    // Record proven multi-stream concurrency in `SmartHostProfileCache` (`P1.7-T06`)!
    if !origin_host.is_empty()
        && download.supports_range
        && hard_cap_active.is_none()
        && peak_healthy_workers >= 2
    {
        ctx.host_capabilities.record_proven_concurrency(
            &origin_host,
            peak_healthy_workers,
            saw_http2,
        );
    }

    // MANDATORY 100% Completion Durable Checkpoint (`FlushFileBuffers` -> `SQLite COMMIT`) BEFORE rename!
    let full_completed = download.segments.completed.clone();
    let post_evs = download.apply(
        DownloadCommand::BeginPostProcessing {
            stage: PostProcessStage::FinalizeFile,
        },
        ctx.clock.now_unix_ms(),
        trace_id,
    )?;
    execute_durable_checkpoint(download, &full_completed, &sink, ctx, post_evs, trace_id).await?;

    // Finalize `.vortex` -> final path + optional cryptographic hash verification via exclusive Flush Barrier!
    let final_path = download.final_path();
    let hash_algo = download.expected_hash.as_ref().map(|c| c.algorithm);
    let computed_hash_opt = if let Some(algo) = hash_algo {
        Some(sink.verify_hash(algo, trace_id).await?)
    } else {
        None
    };
    let final_size_bytes = sink.finalize(&final_path, trace_id).await?;
    drop(sink);

    // Apply Windows Mark-of-the-Web (`:Zone.Identifier`) + `IAttachmentExecute` AV scan (`TH-07`)
    ctx.platform
        .apply_motw_and_av_scan(
            &final_path,
            download.resource.effective_url(),
            download.resource.referrer.as_deref(),
            trace_id,
        )
        .await?;

    // Advance FSM to `Verifying` / `Completed` (or `Failed` on hash mismatch!)
    let mut final_events = download.apply(
        DownloadCommand::AdvanceAfterPostProcessing { final_size_bytes },
        ctx.clock.now_unix_ms(),
        trace_id,
    )?;
    if let Some(actual_hex) = computed_hash_opt {
        let verify_evs = download.apply(
            DownloadCommand::CompleteVerification {
                actual_hex,
                final_size_bytes,
            },
            ctx.clock.now_unix_ms(),
            trace_id,
        )?;
        final_events.extend(verify_evs);
    }

    // Delete stored secret cookies once download has completed (`TH-08` least-retention!)
    if matches!(download.state, DownloadState::Completed { .. }) {
        let _ = ctx.secret_store.delete(download.id, trace_id).await;
    }

    ctx.persistence
        .save_and_wait(download.clone(), final_events, trace_id)
        .await?;

    Ok(SessionExit::Completed)
}

/// Executes the strict Two-Tier Durable Checkpoint Protocol (`ADR-008` / `P1-T20` / `P1.7-T02`):
/// 1. `FileSink::flush_durable` acquires exclusive `RwLock::write()` Flush Barrier (draining all
///    in-flight overlapped writes), calls OS `FlushFileBuffers`, and writes atomic `.vortex.meta`.
/// 2. Optional test failpoint hook runs strictly between `FlushFileBuffers` and `SQLite COMMIT`.
/// 3. `PersistenceActor` commits `completed` + `state` inside a single SQLite WAL transaction.
async fn execute_durable_checkpoint(
    download: &Download,
    durable_completed: &RangeSet,
    sink: &Arc<dyn FileSink>,
    ctx: &EngineContext,
    events: Vec<DomainEventEnvelope>,
    trace_id: TraceId,
) -> AppResult<()> {
    let now_ms = ctx.clock.now_unix_ms();
    let sidecar = build_sidecar_snapshot(download, durable_completed.clone(), now_ms);

    // STEP 1: Exclusive Flush Barrier + `FlushFileBuffers` FIRST!
    sink.flush_durable(&sidecar, trace_id).await?;

    // Optional fault-injection hook right between `FlushFileBuffers` and `SQLite COMMIT` (`P1-T20`)!
    if let Some(hook) = &ctx.mid_checkpoint_hook {
        hook()?;
    }

    // STEP 2: `SQLite COMMIT` ONLY AFTER `FlushFileBuffers` succeeded!
    ctx.persistence
        .commit_checkpoint(
            download.id,
            durable_completed.clone(),
            download.state.clone(),
            now_ms,
            events,
            trace_id,
        )
        .await
}

fn update_rtt_bounds(
    latest_rtt_us: &mut Option<u64>,
    min_rtt_us: &mut Option<u64>,
    sample_rtt: Option<u64>,
    sample_min: Option<u64>,
) {
    if let Some(r) = sample_rtt.filter(|&v| v > 0) {
        *latest_rtt_us = Some(r);
        *min_rtt_us = Some(min_rtt_us.map_or(r, |cur| cur.min(r)));
    }
    if let Some(m) = sample_min.filter(|&v| v > 0) {
        *min_rtt_us = Some(min_rtt_us.map_or(m, |cur| cur.min(m)));
    }
}

fn compute_median_speed(worker_speeds: &BTreeMap<WorkerId, f64>) -> f64 {
    let mut vals: Vec<f64> = worker_speeds
        .values()
        .copied()
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(f64::total_cmp);
    let mid = vals.len() / 2;
    vals.get(mid).copied().unwrap_or(0.0)
}

/// Flushes a contiguous coalesced slab (`<= 2 MiB`) via parallel overlapped `FileSink::write_at`
/// guarded by the global `32 MiB` byte-weighted `Semaphore` (`P1.7-T08`).
async fn flush_coalesced_slab(
    worker_id: WorkerId,
    slab_offset: u64,
    buf: &mut BytesMut,
    speed_bps: f64,
    update_shared_map: bool,
    env: &SharedWorkerEnv,
) -> AppResult<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let len = buf.len();
    let frozen = buf.split().freeze();
    let needed_permits = u32::try_from(len.div_ceil(PERMIT_QUANTUM_BYTES).max(1))
        .unwrap_or(1)
        .min(u32::try_from(TOTAL_WRITE_PERMITS).unwrap_or(512));

    let permit = env
        .write_permits
        .acquire_many(needed_permits)
        .await
        .map_err(|_| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                env.trace_id,
                "Write backpressure semaphore closed",
            )
        })?;

    env.sink.write_at(slab_offset, frozen, env.trace_id).await?;
    drop(permit);

    let end = slab_offset.saturating_add(len as u64);
    if let Ok(range) = ByteRange::new(slab_offset, end) {
        if update_shared_map {
            let mut map = env.shared_map.lock().await;
            let _ = map.record_written(worker_id, range);
            if speed_bps.is_finite()
                && speed_bps > 0.0
                && let Some(seg) = map.active.iter_mut().find(|s| s.worker_id == worker_id)
            {
                seg.speed_bps = speed_bps;
            }
        }
        let _ = env
            .report_tx
            .send(WorkerReport::ChunkWritten {
                worker_id,
                range,
                speed_bps,
            })
            .await;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run_worker_loop(
    worker_id: WorkerId,
    tracker: Arc<WorkerLiveTracker>,
    env: SharedWorkerEnv,
) {
    // Special path for non-range or unknown-size streams: Worker #1 drains `initial_stream` from 0 to EOF
    if !env.supports_range || env.known_total.is_none() {
        if worker_id.get() != 1 {
            let _ = env
                .report_tx
                .send(WorkerReport::WorkerExited { worker_id })
                .await;
            return;
        }
        let mut stream_opt = env.preopened_holder.lock().await.take();
        let Some(mut stream) = stream_opt.take() else {
            let _ = env
                .report_tx
                .send(WorkerReport::WorkerExited { worker_id })
                .await;
            return;
        };
        let mut coalesced = BytesMut::with_capacity(WORKER_COALESCE_BYTES);
        let mut slab_start = 0_u64;
        let mut last_flush = Instant::now();
        let mut announced_open = false;

        loop {
            let chunk_res = tokio::select! {
                biased;
                () = env.cancel.cancelled() => {
                    let _ = flush_coalesced_slab(worker_id, slab_start, &mut coalesced, 0.0, false, &env).await;
                    let _ = env.report_tx.send(WorkerReport::WorkerExited { worker_id }).await;
                    return;
                }
                r = stream.next_chunk() => r,
            };
            match chunk_res {
                Ok(Some(bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    let len = bytes.len() as u64;
                    env.session_live_bytes.fetch_add(len, Ordering::Relaxed);
                    env.worker_one_live_bytes.fetch_add(len, Ordering::Relaxed);
                    if !announced_open {
                        announced_open = true;
                        let m = stream.transport_metrics();
                        let _ = env.report_tx.try_send(WorkerReport::StreamOpened {
                            worker_id,
                            is_http2: m.is_http2,
                            rtt_us: m.rtt_us,
                            min_rtt_us: m.min_rtt_us,
                        });
                    }
                    if coalesced.is_empty() {
                        last_flush = Instant::now();
                    }
                    coalesced.extend_from_slice(&bytes);
                    let should_flush = coalesced.len() >= WORKER_COALESCE_BYTES
                        || (coalesced.len() >= SLOW_LINK_FLUSH_MIN_BYTES
                            && last_flush.elapsed()
                                >= Duration::from_millis(SLOW_LINK_FLUSH_MAX_MS));
                    if should_flush {
                        let written_len = coalesced.len() as u64;
                        if let Err(err) = flush_coalesced_slab(
                            worker_id,
                            slab_start,
                            &mut coalesced,
                            0.0,
                            false,
                            &env,
                        )
                        .await
                        {
                            let _ = env
                                .report_tx
                                .send(WorkerReport::WorkerError {
                                    worker_id,
                                    error: err,
                                })
                                .await;
                            return;
                        }
                        slab_start = slab_start.saturating_add(written_len);
                        last_flush = Instant::now();
                    }
                }
                Ok(None) => {
                    if let Err(err) = flush_coalesced_slab(
                        worker_id,
                        slab_start,
                        &mut coalesced,
                        0.0,
                        false,
                        &env,
                    )
                    .await
                    {
                        let _ = env
                            .report_tx
                            .send(WorkerReport::WorkerError {
                                worker_id,
                                error: err,
                            })
                            .await;
                        return;
                    }
                    let _ = env
                        .report_tx
                        .send(WorkerReport::WorkerExited { worker_id })
                        .await;
                    return;
                }
                Err(err) => {
                    let _ = flush_coalesced_slab(
                        worker_id,
                        slab_start,
                        &mut coalesced,
                        0.0,
                        false,
                        &env,
                    )
                    .await;
                    let _ = env
                        .report_tx
                        .send(WorkerReport::WorkerError {
                            worker_id,
                            error: err,
                        })
                        .await;
                    return;
                }
            }
        }
    }

    // Range-capable path: Hybrid Adaptive Bounded Chunk Leases (`2–64 MiB`) + Work Stealing (`ADR-009` / `ADR-021`).
    // Start Lease #1 conservatively (`3 MiB` at `1 MiB/s` initial estimate) so medium `8–16 MiB` files
    // split across all workers immediately, while early speed calibration (`250ms` + `1 MiB`) scales
    // Lease #2+ dynamically up to `64 MiB` on high-speed links!
    let mut worker_speed_bps = 1_048_576.0_f64;
    let mut rtt_secs = 0.025_f64;
    let mut warm_conn = false;
    let mut carried_preopened: Option<(Box<dyn ByteStream>, u64)> = None;
    let mut coalesced = BytesMut::with_capacity(WORKER_COALESCE_BYTES);

    loop {
        if env.cancel.is_cancelled() {
            break;
        }

        // Graceful BBR/AIMD step-down retirement at lease boundary (never retire Worker #1!)
        let desired = env.target_workers_atomic.load(Ordering::Acquire);
        let current_active = env.active_worker_count.load(Ordering::Acquire);
        if worker_id.get() > 1 && current_active > desired {
            let mut map = env.shared_map.lock().await;
            let _ = map.abort_and_reclaim_worker(worker_id);
            break;
        }

        // Acquire next bounded HTTP lease `[http_lease.start, http_lease.end)`
        let lease_opt: Option<(ByteRange, Option<u64>)> = {
            let mut map = env.shared_map.lock().await;
            if map.is_complete() {
                break;
            }
            match SegmentationPolicy::acquire_next_lease(
                &mut map,
                worker_id,
                worker_speed_bps,
                rtt_secs,
                warm_conn,
            ) {
                LeaseDecision::ContinuedExisting { http_lease }
                | LeaseDecision::ClaimedUnassigned { http_lease, .. } => Some((http_lease, None)),
                LeaseDecision::StoleUnleasedTail {
                    victim_worker,
                    stolen_range,
                    http_lease,
                } => {
                    // Defer setting `probe_stop_offset` until `open_range` actually returns `206 OK`!
                    // That guarantees single-stream capped hosts (`Pixeldrain` 403 on W2) NEVER cut
                    // Worker #1's continuous `bytes=0-` stream (`P1.6-T04` + `P1.7-T03`)!
                    let w1_split = (victim_worker.get() == 1).then_some(stolen_range.start());
                    Some((http_lease, w1_split))
                }
                LeaseDecision::NoWorkAvailable => None,
            }
        };

        let Some((http_lease, stolen_from_w1_start)) = lease_opt else {
            tokio::select! {
                () = env.cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(15)) => {
                    if env.shared_map.lock().await.is_complete() {
                        break;
                    }
                    continue;
                }
            }
        };

        // Reuse continuous `probe_and_stream` (`bytes=0-`) stream whenever `http_lease.start()`
        // matches our live stream cursor (zero extra HTTP requests on single-connection hosts like Pixeldrain!)
        let mut using_continuous_probe = false;
        let mut stream: Box<dyn ByteStream> = if let Some((carried, stream_pos)) =
            carried_preopened.take()
            && stream_pos == http_lease.start()
        {
            using_continuous_probe = true;
            warm_conn = true;
            carried
        } else if worker_id.get() == 1
            && http_lease.start() == 0
            && !env.first_stream_taken.swap(true, Ordering::SeqCst)
            && let Some(s) = env.preopened_holder.lock().await.take()
        {
            using_continuous_probe = true;
            let m = s.transport_metrics();
            let _ = env.report_tx.try_send(WorkerReport::StreamOpened {
                worker_id,
                is_http2: m.is_http2,
                rtt_us: m.rtt_us,
                min_rtt_us: m.min_rtt_us,
            });
            s
        } else {
            match tokio::select! {
                biased;
                () = env.cancel.cancelled() => break,
                r = env.byte_source.open_range(&env.req, http_lease) => r,
            } {
                Ok((s, metrics)) => {
                    // Now that `206 Partial Content` succeeded, cap Worker #1's background probe
                    // pump strictly at `split_start` (`Zero-Waste Probe Stream Stop` — `P1.7-T03`)!
                    if let Some(split_start) = stolen_from_w1_start {
                        env.probe_stop_offset
                            .fetch_min(split_start, Ordering::Release);
                    }
                    warm_conn = metrics.reused_connection;
                    if let Some(us) = metrics.rtt_us {
                        #[allow(clippy::cast_precision_loss)]
                        {
                            rtt_secs = (us as f64 / 1_000_000.0).clamp(0.000_1, 5.0);
                        }
                    }
                    let _ = env.report_tx.try_send(WorkerReport::StreamOpened {
                        worker_id,
                        is_http2: metrics.is_http2,
                        rtt_us: metrics.rtt_us,
                        min_rtt_us: metrics.min_rtt_us,
                    });
                    s
                }
                Err(err) => {
                    tracker.clear_lease();
                    let _ = env
                        .report_tx
                        .send(WorkerReport::WorkerError {
                            worker_id,
                            error: err,
                        })
                        .await;
                    return;
                }
            }
        };

        let lease_t0 = Instant::now();
        let mut last_flush = lease_t0;
        let mut cursor = http_lease.start();
        let mut slab_start = cursor;
        let lease_end = http_lease.end();
        let mut lease_bytes_read = 0_u64;
        let mut last_applied_stop = u64::MAX;
        let mut leftover_from_probe: Option<Bytes> = None;

        tracker.reset_lease(cursor, lease_end, env.clock.now_unix_ms());
        coalesced.clear();

        while cursor < lease_end {
            // Propagate Zero-Waste Probe Stream stop offset immediately if Worker #1's tail was stolen!
            if using_continuous_probe {
                let stop = env.probe_stop_offset.load(Ordering::Acquire);
                if stop < last_applied_stop {
                    stream.set_stop_offset(stop);
                    last_applied_stop = stop;
                }
            }

            let next_res = tokio::select! {
                biased;
                () = env.cancel.cancelled() => {
                    let _ = flush_coalesced_slab(
                        worker_id,
                        slab_start,
                        &mut coalesced,
                        worker_speed_bps,
                        true,
                        &env,
                    )
                    .await;
                    tracker.clear_lease();
                    let _ = env.report_tx.send(WorkerReport::WorkerExited { worker_id }).await;
                    return;
                }
                r = stream.next_chunk() => r,
            };

            match next_res {
                Ok(Some(mut chunk)) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    let max_take = usize::try_from(lease_end - cursor).unwrap_or(usize::MAX);
                    if chunk.len() > max_take {
                        if using_continuous_probe {
                            leftover_from_probe = Some(chunk.slice(max_take..));
                        }
                        chunk = chunk.slice(..max_take);
                    }
                    let chunk_len = chunk.len() as u64;
                    env.session_live_bytes
                        .fetch_add(chunk_len, Ordering::Relaxed);
                    if worker_id.get() == 1 {
                        env.worker_one_live_bytes
                            .fetch_add(chunk_len, Ordering::Relaxed);
                    }
                    tracker.record_rx(chunk_len);
                    lease_bytes_read = lease_bytes_read.saturating_add(chunk_len);

                    // Early worker speed update after first `1 MiB` & `250ms` (`P1.7-T08`)!
                    if lease_bytes_read >= EARLY_SPEED_MIN_BYTES
                        && lease_t0.elapsed() >= Duration::from_millis(EARLY_SPEED_MIN_MS)
                    {
                        #[allow(clippy::cast_precision_loss)]
                        {
                            worker_speed_bps = (lease_bytes_read as f64)
                                / lease_t0.elapsed().as_secs_f64().max(0.001);
                        }
                    }

                    if coalesced.is_empty() {
                        slab_start = cursor;
                    }
                    coalesced.extend_from_slice(&chunk);
                    cursor = cursor.saturating_add(chunk_len);
                    tracker.advance_cursor(cursor);

                    let at_lease_end = cursor >= lease_end;
                    let should_flush = at_lease_end
                        || coalesced.len() >= WORKER_COALESCE_BYTES
                        || (coalesced.len() >= SLOW_LINK_FLUSH_MIN_BYTES
                            && last_flush.elapsed()
                                >= Duration::from_millis(SLOW_LINK_FLUSH_MAX_MS));

                    if should_flush {
                        if let Err(err) = flush_coalesced_slab(
                            worker_id,
                            slab_start,
                            &mut coalesced,
                            worker_speed_bps,
                            true,
                            &env,
                        )
                        .await
                        {
                            tracker.clear_lease();
                            let _ = env
                                .report_tx
                                .send(WorkerReport::WorkerError {
                                    worker_id,
                                    error: err,
                                })
                                .await;
                            return;
                        }
                        slab_start = cursor;
                        last_flush = Instant::now();
                    }
                }
                Ok(None) => {
                    if !coalesced.is_empty() {
                        let _ = flush_coalesced_slab(
                            worker_id,
                            slab_start,
                            &mut coalesced,
                            worker_speed_bps,
                            true,
                            &env,
                        )
                        .await;
                    }
                    if cursor < lease_end {
                        tracker.clear_lease();
                        let _ = env
                            .report_tx
                            .send(WorkerReport::WorkerError {
                                worker_id,
                                error: VortexError::coded(
                                    ErrorCode::NetConnectionLost,
                                    env.trace_id,
                                    format!(
                                        "Worker {worker_id} premature EOF at {cursor} before lease_end {lease_end}"
                                    ),
                                ),
                            })
                            .await;
                        return;
                    }
                    break;
                }
                Err(err) => {
                    if !coalesced.is_empty() {
                        let _ = flush_coalesced_slab(
                            worker_id,
                            slab_start,
                            &mut coalesced,
                            worker_speed_bps,
                            true,
                            &env,
                        )
                        .await;
                    }
                    tracker.clear_lease();
                    let _ = env
                        .report_tx
                        .send(WorkerReport::WorkerError {
                            worker_id,
                            error: err,
                        })
                        .await;
                    return;
                }
            }
        }

        tracker.clear_lease();
        let elapsed_s = lease_t0.elapsed().as_secs_f64().max(0.001);
        #[allow(clippy::cast_precision_loss)]
        {
            worker_speed_bps = (http_lease.len() as f64) / elapsed_s;
        }
        let m = stream.transport_metrics();
        if using_continuous_probe
            && env.known_total.is_none_or(|tot| cursor < tot)
            && cursor < env.probe_stop_offset.load(Ordering::Acquire)
        {
            let carried: Box<dyn ByteStream> = if let Some(rem) = leftover_from_probe {
                Box::new(SinglePrefixStream {
                    prefix: Some(rem),
                    inner: stream,
                })
            } else {
                stream
            };
            carried_preopened = Some((carried, cursor));
        }
        let _ = env
            .report_tx
            .send(WorkerReport::LeaseFinished {
                worker_id,
                rtt_us: m.rtt_us,
                min_rtt_us: m.min_rtt_us,
                is_http2: m.is_http2,
                reused_conn: m.reused_connection,
                speed_bps: worker_speed_bps,
            })
            .await;
    }

    tracker.clear_lease();
    let _ = env
        .report_tx
        .send(WorkerReport::WorkerExited { worker_id })
        .await;
}

/// Runs an `End-Game Mode` hedged request racing on a slow straggler's remaining range (`P1.7-T07`).
///
/// Writes identical deterministic bytes via overlapped `FileSink::write_at` and commits them
/// idempotently into `SegmentMap` so whichever worker finishes first completes the download!
async fn run_hedged_race_worker(victim_id: WorkerId, race_range: ByteRange, env: SharedWorkerEnv) {
    let open_res = tokio::select! {
        biased;
        () = env.cancel.cancelled() => {
            let _ = env.report_tx.send(WorkerReport::HedgeFinished { victim_id, succeeded: false }).await;
            return;
        }
        r = env.byte_source.open_range(&env.req, race_range) => r,
    };

    let Ok((mut stream, _metrics)) = open_res else {
        let _ = env
            .report_tx
            .send(WorkerReport::HedgeFinished {
                victim_id,
                succeeded: false,
            })
            .await;
        return;
    };

    let mut coalesced = BytesMut::with_capacity(WORKER_COALESCE_BYTES);
    let mut cursor = race_range.start();
    let mut slab_start = cursor;
    let end = race_range.end();
    let t0 = Instant::now();

    while cursor < end {
        let chunk_res = tokio::select! {
            biased;
            () = env.cancel.cancelled() => {
                let _ = env.report_tx.send(WorkerReport::HedgeFinished { victim_id, succeeded: false }).await;
                return;
            }
            r = stream.next_chunk() => r,
        };
        if let Ok(Some(mut chunk)) = chunk_res {
            if chunk.is_empty() {
                continue;
            }
            let max_take = usize::try_from(end - cursor).unwrap_or(usize::MAX);
            if chunk.len() > max_take {
                chunk = chunk.slice(..max_take);
            }
            let len = chunk.len() as u64;
            env.session_live_bytes.fetch_add(len, Ordering::Relaxed);
            if coalesced.is_empty() {
                slab_start = cursor;
            }
            coalesced.extend_from_slice(&chunk);
            cursor = cursor.saturating_add(len);

            if cursor >= end || coalesced.len() >= WORKER_COALESCE_BYTES {
                #[allow(clippy::cast_precision_loss)]
                let spd = (cursor.saturating_sub(race_range.start()) as f64)
                    / t0.elapsed().as_secs_f64().max(0.001);
                if flush_coalesced_slab(victim_id, slab_start, &mut coalesced, spd, true, &env)
                    .await
                    .is_err()
                {
                    let _ = env
                        .report_tx
                        .send(WorkerReport::HedgeFinished {
                            victim_id,
                            succeeded: false,
                        })
                        .await;
                    return;
                }
                slab_start = cursor;
            }
        } else {
            if !coalesced.is_empty() {
                let _ =
                    flush_coalesced_slab(victim_id, slab_start, &mut coalesced, 0.0, true, &env)
                        .await;
            }
            let _ = env
                .report_tx
                .send(WorkerReport::HedgeFinished {
                    victim_id,
                    succeeded: cursor >= end,
                })
                .await;
            return;
        }
    }

    let _ = env
        .report_tx
        .send(WorkerReport::HedgeFinished {
            victim_id,
            succeeded: true,
        })
        .await;
}

struct SinglePrefixStream {
    prefix: Option<Bytes>,
    inner: Box<dyn ByteStream>,
}

#[async_trait::async_trait]
impl ByteStream for SinglePrefixStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        if let Some(chunk) = self.prefix.take() {
            return Ok(Some(chunk));
        }
        self.inner.next_chunk().await
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.inner.transport_metrics()
    }

    fn set_stop_offset(&self, stop_offset: u64) {
        self.inner.set_stop_offset(stop_offset);
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn handle_error_or_schedule_retry(
    download: &mut Download,
    err: &VortexError,
    attempt: &mut u32,
    cfg: &AppConfig,
    ctx: &EngineContext,
    ctrl_rx: &mut mpsc::Receiver<ActorControlMsg>,
    shutdown_token: &CancellationToken,
    snapshot_tx: &watch::Sender<Option<ProgressSnapshotDto>>,
    trace_id: TraceId,
) -> AppResult<bool> {
    let api = err.as_api_error();

    // `UserActionRequired` (`403`/`410` expired link, `VTX-RES-003` validator mismatch, `VTX-DISK-001` full)
    if matches!(
        api.recoverability,
        vortex_error::Recoverability::UserActionRequired
    ) {
        let evs = download.apply(
            DownloadCommand::MarkNeedsAttention {
                reason_code: api.code,
                detail: api.technical_detail.clone(),
            },
            ctx.clock.now_unix_ms(),
            trace_id,
        )?;
        ctx.persistence
            .save_and_wait(download.clone(), evs, trace_id)
            .await?;
        let _ = snapshot_tx.send(None);
        return Ok(false);
    }

    // Offline Network Freeze (`UX-M10`): when internet connectivity is down, wait for network
    // restoration cleanly WITHOUT burning `max_retries`!
    if api.recoverability.is_retryable() && !ctx.platform.is_internet_available() {
        tracing::info!(
            download_id = %download.id,
            error_code = %api.code,
            "Network offline; freezing retry counter until connectivity returns (UX-M10)"
        );
        loop {
            if ctx.platform.is_internet_available() {
                return Ok(true);
            }
            tokio::select! {
                () = shutdown_token.cancelled() => {
                    let p_evs = download.apply(DownloadCommand::Pause, ctx.clock.now_unix_ms(), trace_id)?;
                    let _ = ctx.persistence.save_and_wait(download.clone(), p_evs, trace_id).await;
                    return Ok(false);
                }
                ctrl_opt = ctrl_rx.recv() => {
                    if let Some(ActorControlMsg::Pause { trace_id: pt, ack }) = ctrl_opt {
                        let p_evs = download.apply(DownloadCommand::Pause, ctx.clock.now_unix_ms(), pt)?;
                        let r = ctx.persistence.save_and_wait(download.clone(), p_evs, pt).await;
                        let _ = ack.send(r);
                        return Ok(false);
                    }
                    if let Some(ActorControlMsg::Cancel { delete_partial, ack, .. }) = ctrl_opt {
                        if delete_partial {
                            let _ = tokio::fs::remove_file(download.temp_path()).await;
                            let _ = tokio::fs::remove_file(download.meta_sidecar_path()).await;
                        }
                        let _ = ack.send(Ok(()));
                        return Ok(false);
                    }
                }
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    // `Transient` retry with Full-Jitter Backoff (`<= cfg.max_retries`)
    if api.recoverability.is_retryable() && *attempt < cfg.max_retries {
        *attempt += 1;
        let delay_ms = compute_jittered_backoff(
            *attempt,
            api.recoverability.retry_after_ms(),
            ctx.clock.monotonic_ns(),
        );
        let evs = download.apply(
            DownloadCommand::ScheduleRetry {
                attempt: *attempt,
                retry_after_ms: delay_ms,
                code: api.code,
            },
            ctx.clock.now_unix_ms(),
            trace_id,
        )?;
        ctx.persistence
            .save_and_wait(download.clone(), evs, trace_id)
            .await?;

        tokio::select! {
            () = shutdown_token.cancelled() => {
                let p_evs = download.apply(DownloadCommand::Pause, ctx.clock.now_unix_ms(), trace_id)?;
                let _ = ctx.persistence.save_and_wait(download.clone(), p_evs, trace_id).await;
                return Ok(false);
            }
            ctrl_opt = ctrl_rx.recv() => {
                if let Some(ActorControlMsg::Pause { trace_id: pt, ack }) = ctrl_opt {
                    let p_evs = download.apply(DownloadCommand::Pause, ctx.clock.now_unix_ms(), pt)?;
                    let r = ctx.persistence.save_and_wait(download.clone(), p_evs, pt).await;
                    let _ = ack.send(r);
                    return Ok(false);
                }
                if let Some(ActorControlMsg::Cancel { delete_partial, ack, .. }) = ctrl_opt {
                    if delete_partial {
                        let _ = tokio::fs::remove_file(download.temp_path()).await;
                        let _ = tokio::fs::remove_file(download.meta_sidecar_path()).await;
                    }
                    let _ = ack.send(Ok(()));
                    return Ok(false);
                }
            }
            () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
        }

        return Ok(true);
    }

    // Unrecoverable / retries exhausted -> `Failed`
    let evs = download.apply(
        DownloadCommand::Fail { error: api.clone() },
        ctx.clock.now_unix_ms(),
        trace_id,
    )?;
    ctx.persistence
        .save_and_wait(download.clone(), evs, trace_id)
        .await?;
    let _ = snapshot_tx.send(None);
    let _ = DomainEvent::ProbeStarted;
    Ok(false)
}

fn compute_jittered_backoff(attempt: u32, retry_after_ms: Option<u64>, seed: u64) -> u64 {
    if let Some(server_ms) = retry_after_ms {
        return server_ms.clamp(50, 300_000);
    }
    let cap = 500_u64
        .saturating_mul(1_u64 << attempt.min(16))
        .min(300_000);
    let mut z = seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let rnd = z ^ (z >> 31);
    (rnd % (cap + 1)).max(50)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;

    struct DummyStopStream {
        stop_seen: Arc<AtomicU64>,
        tail: Option<Bytes>,
    }

    #[async_trait::async_trait]
    impl ByteStream for DummyStopStream {
        async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
            Ok(self.tail.take())
        }

        fn transport_metrics(&self) -> TransportMetrics {
            TransportMetrics {
                rtt_us: Some(15_000),
                min_rtt_us: Some(12_000),
                is_proxied: false,
                reused_connection: true,
                is_http2: true,
            }
        }

        fn set_stop_offset(&self, stop_offset: u64) {
            self.stop_seen.store(stop_offset, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn helper_telemetry_prefix_stream_and_backoff_cover_all_edge_branches() {
        // 1. WorkerLiveTracker lifecycle
        let tracker = WorkerLiveTracker::default();
        assert!(tracker.snapshot(1_000).is_none());
        tracker.reset_lease(0, 2_097_152, 1_000);
        tracker.record_rx(1_048_576);
        tracker.advance_cursor(1_048_576);
        let (cur, end, spd) = tracker.snapshot(2_000).unwrap();
        assert_eq!(cur, 1_048_576);
        assert_eq!(end, 2_097_152);
        assert!(spd > 500_000.0);
        tracker.clear_lease();
        assert!(tracker.snapshot(3_000).is_none());

        // 2. Median speed & RTT bounds
        let mut speeds = BTreeMap::new();
        assert!(compute_median_speed(&speeds).abs() < f64::EPSILON);
        speeds.insert(WorkerId::new(1), 4_000_000.0);
        speeds.insert(WorkerId::new(2), 1_000_000.0);
        speeds.insert(WorkerId::new(3), 8_000_000.0);
        assert!((compute_median_speed(&speeds) - 4_000_000.0).abs() < 1.0);

        let mut latest = None;
        let mut min_r = None;
        update_rtt_bounds(&mut latest, &mut min_r, Some(30_000), Some(20_000));
        assert_eq!(latest, Some(30_000));
        assert_eq!(min_r, Some(20_000));
        update_rtt_bounds(&mut latest, &mut min_r, Some(15_000), None);
        assert_eq!(latest, Some(15_000));
        assert_eq!(min_r, Some(15_000));

        // 3. SinglePrefixStream forwards prefix, tail, metrics, and `set_stop_offset`
        let stop_seen = Arc::new(AtomicU64::new(u64::MAX));
        let mut wrapped = SinglePrefixStream {
            prefix: Some(Bytes::from_static(b"head-")),
            inner: Box::new(DummyStopStream {
                stop_seen: Arc::clone(&stop_seen),
                tail: Some(Bytes::from_static(b"tail")),
            }),
        };
        wrapped.set_stop_offset(65_536);
        assert_eq!(stop_seen.load(Ordering::Acquire), 65_536);
        assert!(wrapped.transport_metrics().is_http2);
        assert_eq!(
            wrapped.next_chunk().await.unwrap(),
            Some(Bytes::from_static(b"head-"))
        );
        assert_eq!(
            wrapped.next_chunk().await.unwrap(),
            Some(Bytes::from_static(b"tail"))
        );
        assert_eq!(wrapped.next_chunk().await.unwrap(), None);

        // 4. Backoff respects explicit `Retry-After` and clamps jitter cleanly
        assert_eq!(compute_jittered_backoff(1, Some(4_200), 99), 4_200);
        assert_eq!(compute_jittered_backoff(1, Some(10), 99), 50);
        let j = compute_jittered_backoff(3, None, 12345);
        assert!((50..=4_000).contains(&j));
    }

    struct FailingOrShortSource {
        fail_open: bool,
    }

    #[async_trait::async_trait]
    impl ByteSource for FailingOrShortSource {
        async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
            Err(VortexError::coded(
                ErrorCode::NetConnectionLost,
                req.trace_id,
                "unused",
            ))
        }

        async fn open_range(
            &self,
            req: &ResourceRequest,
            _range: ByteRange,
        ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
            if self.fail_open {
                return Err(VortexError::coded(
                    ErrorCode::NetConnectionLost,
                    req.trace_id,
                    "simulated hedge open error",
                ));
            }
            Ok((
                Box::new(DummyStopStream {
                    stop_seen: Arc::new(AtomicU64::new(u64::MAX)),
                    tail: Some(Bytes::from(vec![7_u8; 64])),
                }),
                TransportMetrics::default(),
            ))
        }
    }

    struct NoopSink;

    #[async_trait::async_trait]
    impl FileSink for NoopSink {
        async fn prepare(&self, _total_size: Option<u64>, _trace_id: TraceId) -> AppResult<bool> {
            Ok(true)
        }
        async fn write_at(&self, _offset: u64, _data: Bytes, _trace_id: TraceId) -> AppResult<()> {
            Ok(())
        }
        async fn read_at(&self, _offset: u64, _len: usize, _trace_id: TraceId) -> AppResult<Bytes> {
            Ok(Bytes::new())
        }
        async fn flush_durable(
            &self,
            _sidecar: &vortex_application::SidecarMetadata,
            _trace_id: TraceId,
        ) -> AppResult<()> {
            Ok(())
        }
        async fn verify_hash(
            &self,
            _algorithm: vortex_domain::HashAlgorithm,
            _trace_id: TraceId,
        ) -> AppResult<String> {
            Ok(String::new())
        }
        async fn finalize(
            &self,
            _final_path: &std::path::Path,
            _trace_id: TraceId,
        ) -> AppResult<u64> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn hedged_race_worker_handles_open_failure_cancel_and_short_eof_cleanly() {
        let trace = TraceId::new();
        let make_env = |fail_open: bool, cancel: CancellationToken| {
            let (report_tx, report_rx) = mpsc::channel(16);
            let env = SharedWorkerEnv {
                req: ResourceRequest::new(
                    vortex_domain::DownloadId::new(),
                    "http://127.0.0.1/test",
                    trace,
                ),
                byte_source: Arc::new(FailingOrShortSource { fail_open }),
                sink: Arc::new(NoopSink),
                write_permits: Arc::new(Semaphore::new(TOTAL_WRITE_PERMITS)),
                shared_map: Arc::new(AsyncMutex::new(SegmentMap::new(Some(1024)))),
                report_tx,
                cancel,
                preopened_holder: Arc::new(AsyncMutex::new(None)),
                first_stream_taken: Arc::new(AtomicBool::new(false)),
                probe_stop_offset: Arc::new(AtomicU64::new(u64::MAX)),
                session_live_bytes: Arc::new(AtomicU64::new(0)),
                worker_one_live_bytes: Arc::new(AtomicU64::new(0)),
                target_workers_atomic: Arc::new(AtomicU16::new(2)),
                active_worker_count: Arc::new(AtomicU16::new(2)),
                clock: Arc::new(vortex_domain::FakeClock::new(1_000)),
                supports_range: true,
                known_total: Some(1024),
                trace_id: trace,
            };
            (env, report_rx)
        };

        let range = ByteRange::new(0, 512).unwrap();

        // 1. Pre-cancelled token exits immediately with `succeeded: false`
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let (env1, mut rx1) = make_env(false, cancelled);
        run_hedged_race_worker(WorkerId::new(2), range, env1).await;
        assert!(matches!(
            rx1.recv().await,
            Some(WorkerReport::HedgeFinished {
                succeeded: false,
                ..
            })
        ));

        // 2. `open_range` failure emits `HedgeFinished { succeeded: false }`
        let (env2, mut rx2) = make_env(true, CancellationToken::new());
        run_hedged_race_worker(WorkerId::new(2), range, env2).await;
        assert!(matches!(
            rx2.recv().await,
            Some(WorkerReport::HedgeFinished {
                succeeded: false,
                ..
            })
        ));

        // 3. Premature short EOF (`64 < 512`) flushes partial coalesced slab and emits `succeeded: false`
        let (env3, mut rx3) = make_env(false, CancellationToken::new());
        run_hedged_race_worker(WorkerId::new(2), range, env3).await;
        let mut saw_finished = false;
        while let Some(ev) = rx3.recv().await {
            if matches!(
                ev,
                WorkerReport::HedgeFinished {
                    succeeded: false,
                    ..
                }
            ) {
                saw_finished = true;
            }
        }
        assert!(saw_finished);
    }
}
