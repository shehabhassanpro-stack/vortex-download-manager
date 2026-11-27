//! `Supervisor` actor orchestration, Bulkhead panic isolation (`JoinError::is_panic`),
//! `<= 10Hz` UI Projection throttling, and `EngineControlPort` implementation (`P1-T18`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustc_hash::FxHashMap;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppEvent, AppResult, ByteSource, ByteStream, ConfigPort, DownloadRepository, EngineControlPort,
    EventBus, EventLogRepository, FileSinkFactory, PlatformIntegration, ProgressSnapshotDto,
    ResourceInfo, SecretStore,
};
use vortex_domain::{Clock, Download, DownloadCommand, DownloadId};
use vortex_error::{ApiError, ErrorCode, TraceId, VortexError};

use crate::actors::{PersistenceHandle, PriorityQueueScheduler};
use crate::host_capability::HostCapabilityCache;
use crate::worker::{ActorControlMsg, EngineContext, MidCheckpointHook, run_download_actor};

type PreopenedProbe = Option<(Box<ResourceInfo>, Box<dyn ByteStream>)>;
type ActorSpawnMsg = (
    Download,
    PreopenedProbe,
    mpsc::Receiver<ActorControlMsg>,
    watch::Sender<Option<ProgressSnapshotDto>>,
    TraceId,
);

struct ActiveActorEntry {
    ctrl_tx: mpsc::Sender<ActorControlMsg>,
    snapshot_rx: watch::Receiver<Option<ProgressSnapshotDto>>,
}

struct WaitingDownload {
    download: Download,
    preopened_probe: PreopenedProbe,
    trace_id: TraceId,
}

struct SupervisorInner {
    scheduler: PriorityQueueScheduler,
    active: FxHashMap<DownloadId, ActiveActorEntry>,
    waiting: FxHashMap<DownloadId, WaitingDownload>,
    sleep_guard: Option<Box<dyn vortex_application::SleepInhibitorGuard>>,
}

/// Top-level Engine Supervisor implementing [`EngineControlPort`] with Bulkhead panic isolation (`ADR-004`).
pub struct EngineSupervisor {
    inner: Arc<AsyncMutex<SupervisorInner>>,
    spawn_tx: mpsc::Sender<ActorSpawnMsg>,
    download_repo: Arc<dyn DownloadRepository>,
    ctx: EngineContext,
    root_cancel: CancellationToken,
    hard_kill_cancel: CancellationToken,
    reaper_done_rx: AsyncMutex<Option<oneshot::Receiver<()>>>,
}

impl std::fmt::Debug for EngineSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineSupervisor").finish_non_exhaustive()
    }
}

impl EngineSupervisor {
    /// Boots the complete Actor Supervision Tree (`PersistenceActor`, `Supervisor` reaper with
    /// `JoinError::is_panic()` Bulkhead isolation, and `10Hz` UI Projection publisher).
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::needless_pass_by_value
    )]
    pub fn start(
        byte_source: Arc<dyn ByteSource>,
        sink_factory: Arc<dyn FileSinkFactory>,
        download_repo: Arc<dyn DownloadRepository>,
        event_repo: Arc<dyn EventLogRepository>,
        secret_store: Arc<dyn SecretStore>,
        platform: Arc<dyn PlatformIntegration>,
        config_port: Arc<dyn ConfigPort>,
        event_bus: Arc<dyn EventBus>,
        clock: Arc<dyn Clock>,
        mid_checkpoint_hook: Option<MidCheckpointHook>,
    ) -> Arc<Self> {
        let (persistence, _persist_join) = PersistenceHandle::spawn(
            Arc::clone(&download_repo),
            event_repo,
            Arc::clone(&event_bus),
        );

        let initial_cfg = config_port.current();
        let config_rx = config_port.subscribe();

        let host_capabilities = Arc::new(HostCapabilityCache::new(Arc::clone(&clock)));
        let ctx = EngineContext {
            byte_source,
            sink_factory,
            persistence,
            secret_store,
            platform,
            clock,
            config_rx,
            host_capabilities,
            mid_checkpoint_hook,
        };

        let inner = Arc::new(AsyncMutex::new(SupervisorInner {
            scheduler: PriorityQueueScheduler::new(usize::from(
                initial_cfg.max_concurrent_downloads,
            )),
            active: FxHashMap::default(),
            waiting: FxHashMap::default(),
            sleep_guard: None,
        }));

        let root_cancel = CancellationToken::new();
        let hard_kill_cancel = CancellationToken::new();
        let (spawn_tx, mut spawn_rx) = mpsc::channel(128);
        let (reaper_done_tx, reaper_done_rx) = oneshot::channel();

        let sup = Arc::new(Self {
            inner: Arc::clone(&inner),
            spawn_tx,
            download_repo,
            ctx: ctx.clone(),
            root_cancel: root_cancel.clone(),
            hard_kill_cancel: hard_kill_cancel.clone(),
            reaper_done_rx: AsyncMutex::new(Some(reaper_done_tx_pair_rx(reaper_done_rx))),
        });

        // 1. Spawn Supervised Bulkhead Reaper Loop (`JoinSet<(DownloadId, TraceId)>`)
        let reaper_sup = Arc::clone(&sup);
        let reaper_cancel = root_cancel.clone();
        let reaper_hard_kill = hard_kill_cancel;
        tokio::spawn(async move {
            let mut join_set: JoinSet<(DownloadId, TraceId, AppResult<()>)> = JoinSet::new();
            let mut spawned_handles: FxHashMap<DownloadId, tokio::task::AbortHandle> =
                FxHashMap::default();

            loop {
                tokio::select! {
                    biased;

                    () = reaper_hard_kill.cancelled() => {
                        for (_, h) in spawned_handles.drain() {
                            h.abort();
                        }
                        join_set.abort_all();
                        while join_set.join_next().await.is_some() {}
                        break;
                    }

                    Some((dl, pre, ctrl_rx, snap_tx, tid)) = spawn_rx.recv() => {
                        let dl_id = dl.id;
                        let actor_ctx = reaper_sup.ctx.clone();
                        let child_cancel = reaper_cancel.child_token();
                        let inner_handle = tokio::spawn(run_download_actor(
                            dl,
                            pre,
                            ctrl_rx,
                            snap_tx,
                            child_cancel,
                            actor_ctx,
                            tid,
                        ));
                        spawned_handles.insert(dl_id, inner_handle.abort_handle());
                        join_set.spawn(async move {
                            // `JoinError::is_panic()` captures any panic inside `run_download_actor`!
                            match inner_handle.await {
                                Ok(res) => (dl_id, tid, res),
                                Err(join_err) if join_err.is_panic() => {
                                    tracing::error!(
                                        error_code = %ErrorCode::BugInternalIsolated,
                                        %tid,
                                        download_id = %dl_id,
                                        "BULKHEAD ISOLATION: Caught panic inside DownloadActor; sibling downloads unaffected"
                                    );
                                    let bug = VortexError::coded(
                                        ErrorCode::BugInternalIsolated,
                                        tid,
                                        "Isolated internal panic inside DownloadActor",
                                    );
                                    (dl_id, tid, Err(bug))
                                }
                                Err(join_err) => {
                                    let bug = VortexError::coded(
                                        ErrorCode::BugInternalIsolated,
                                        tid,
                                        format!("DownloadActor cancelled unexpectedly: {join_err}"),
                                    );
                                    (dl_id, tid, Err(bug))
                                }
                            }
                        });
                    }

                    Some(joined) = join_set.join_next() => {
                        if let Ok((dl_id, tid, outcome)) = joined {
                            spawned_handles.remove(&dl_id);
                            if let Err(err) = &outcome
                                && err.code() == ErrorCode::BugInternalIsolated
                            {
                                // Persist isolated panic failure so UI shows `VTX-BUG-001` on that row alone!
                                if let Ok(Some(mut persisted)) = reaper_sup.download_repo.find(dl_id, tid).await {
                                    let now = reaper_sup.ctx.clock.now_unix_ms();
                                    let api_err = ApiError::new(ErrorCode::BugInternalIsolated, tid)
                                        .with_technical_detail(err.as_api_error().technical_detail.clone());
                                    if let Ok(evs) = persisted.apply(DownloadCommand::Fail { error: api_err }, now, tid) {
                                        let _ = reaper_sup.ctx.persistence.save_and_wait(persisted, evs, tid).await;
                                    }
                                }
                            }
                            reaper_sup.on_actor_exited(dl_id).await;
                        }
                    }

                    () = reaper_cancel.cancelled(), if join_set.is_empty() => {
                        break;
                    }
                }
            }
            let _ = reaper_done_tx.send(());
        });

        // 2. Spawn Throttled `10Hz` (`100ms`) UI Projection Publisher (`§10.2.5`)
        let proj_inner = Arc::clone(&inner);
        let proj_bus = event_bus;
        let proj_cancel = root_cancel;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let batch = collect_snapshots(&proj_inner).await;
                        if !batch.is_empty() {
                            proj_bus.publish(AppEvent::ProgressBatch(batch));
                        }
                    }
                    () = proj_cancel.cancelled() => break,
                }
            }
        });

        sup
    }

    async fn spawn_actor_now(
        &self,
        download: Download,
        preopened_probe: Option<(Box<ResourceInfo>, Box<dyn ByteStream>)>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let id = download.id;
        let (ctrl_tx, ctrl_rx) = mpsc::channel(16);
        let (snap_tx, snap_rx) = watch::channel(None);

        {
            let mut guard = self.inner.lock().await;
            guard.active.insert(
                id,
                ActiveActorEntry {
                    ctrl_tx,
                    snapshot_rx: snap_rx,
                },
            );
            if guard.sleep_guard.is_none() && self.ctx.config_rx.borrow().prevent_sleep_while_active
            {
                guard.sleep_guard = self.ctx.platform.acquire_sleep_inhibitor(trace_id).ok();
            }
        }

        self.spawn_tx
            .send((download, preopened_probe, ctrl_rx, snap_tx, trace_id))
            .await
            .map_err(|_| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    "Supervisor spawn channel closed",
                )
            })
    }

    async fn on_actor_exited(&self, finished_id: DownloadId) {
        let to_promote = {
            let mut guard = self.inner.lock().await;
            guard.active.remove(&finished_id);
            let promoted_ids = guard.scheduler.release_and_promote(finished_id);
            let mut batch = Vec::new();
            for pid in promoted_ids {
                if let Some(waiting) = guard.waiting.remove(&pid) {
                    batch.push(waiting);
                }
            }
            if guard.active.is_empty() && batch.is_empty() {
                guard.sleep_guard = None; // Release `SetThreadExecutionState` when idle!
            }
            batch
        };

        for mut next in to_promote {
            let now_ms = self.ctx.clock.now_unix_ms();
            let _ = next
                .download
                .apply(DownloadCommand::StartProbe, now_ms, next.trace_id);
            let _ = self
                .spawn_actor_now(next.download, next.preopened_probe, next.trace_id)
                .await;
        }
    }

    /// Aborts all running actor tasks immediately **without** flushing buffers or committing SQLite
    /// checkpoints — used in crash-recovery torture tests (`P1-T20`) to simulate abrupt `SIGKILL` / power loss.
    pub async fn abort_abruptly(&self) {
        self.hard_kill_cancel.cancel();
        self.root_cancel.cancel();
        let rx_opt = self.reaper_done_rx.lock().await.take();
        if let Some(rx) = rx_opt {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
        }
        self.inner.lock().await.sleep_guard = None;
    }
}

const fn reaper_done_tx_pair_rx(rx: oneshot::Receiver<()>) -> oneshot::Receiver<()> {
    rx
}

async fn collect_snapshots(inner: &AsyncMutex<SupervisorInner>) -> Vec<ProgressSnapshotDto> {
    let guard = inner.lock().await;
    let mut out = Vec::with_capacity(guard.active.len());
    for entry in guard.active.values() {
        if let Some(snap) = entry.snapshot_rx.borrow().clone() {
            out.push(snap);
        }
    }
    out
}

#[async_trait]
impl EngineControlPort for EngineSupervisor {
    async fn start_download(
        &self,
        mut download: Download,
        preopened_probe: Option<(Box<ResourceInfo>, Box<dyn ByteStream>)>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let acquired = {
            let mut guard = self.inner.lock().await;
            let cfg_max = usize::from(self.ctx.config_rx.borrow().max_concurrent_downloads);
            guard.scheduler.set_max_parallel(cfg_max);
            guard
                .scheduler
                .try_acquire_or_enqueue(download.id, download.priority)
        };

        if acquired {
            self.spawn_actor_now(download, preopened_probe, trace_id)
                .await
        } else {
            let now_ms = self.ctx.clock.now_unix_ms();
            download.state = vortex_domain::DownloadState::Queued {
                queued_at_ms: now_ms,
            };
            self.ctx
                .persistence
                .save_and_wait(download.clone(), vec![], trace_id)
                .await?;
            self.inner.lock().await.waiting.insert(
                download.id,
                WaitingDownload {
                    download,
                    preopened_probe,
                    trace_id,
                },
            );
            Ok(())
        }
    }

    async fn pause_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        let ctrl_opt = {
            let mut guard = self.inner.lock().await;
            if let Some(mut waiting) = guard.waiting.remove(&id) {
                let _ = guard.scheduler.release_and_promote(id);
                let now_ms = self.ctx.clock.now_unix_ms();
                let evs = waiting
                    .download
                    .apply(DownloadCommand::Pause, now_ms, trace_id)?;
                drop(guard);
                return self
                    .ctx
                    .persistence
                    .save_and_wait(waiting.download, evs, trace_id)
                    .await;
            }
            guard.active.get(&id).map(|e| e.ctrl_tx.clone())
        };

        if let Some(tx) = ctrl_opt {
            let (ack_tx, ack_rx) = oneshot::channel();
            if tx
                .send(ActorControlMsg::Pause {
                    trace_id,
                    ack: ack_tx,
                })
                .await
                .is_ok()
            {
                let _ = tokio::time::timeout(Duration::from_secs(5), ack_rx).await;
            }
            return Ok(());
        }

        if let Some(mut stored) = self.download_repo.find(id, trace_id).await? {
            let evs = stored.apply(
                DownloadCommand::Pause,
                self.ctx.clock.now_unix_ms(),
                trace_id,
            )?;
            self.ctx
                .persistence
                .save_and_wait(stored, evs, trace_id)
                .await?;
        }
        Ok(())
    }

    async fn pause_all(&self, trace_id: TraceId) -> AppResult<()> {
        let ids: Vec<DownloadId> = {
            let guard = self.inner.lock().await;
            guard
                .active
                .keys()
                .chain(guard.waiting.keys())
                .copied()
                .collect()
        };
        for id in ids {
            let _ = self.pause_download(id, trace_id).await;
        }
        Ok(())
    }

    async fn resume_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        let is_running = self.inner.lock().await.active.contains_key(&id);
        if is_running {
            return Ok(());
        }
        let Some(mut stored) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found"),
            ));
        };
        let now_ms = self.ctx.clock.now_unix_ms();
        let evs = match stored.state {
            vortex_domain::DownloadState::Paused { .. } => {
                stored.apply(DownloadCommand::Resume, now_ms, trace_id)?
            }
            vortex_domain::DownloadState::Failed { .. }
            | vortex_domain::DownloadState::Pending
            | vortex_domain::DownloadState::Queued { .. } => {
                stored.apply(DownloadCommand::StartProbe, now_ms, trace_id)?
            }
            _ => vec![],
        };
        self.ctx
            .persistence
            .save_and_wait(stored.clone(), evs, trace_id)
            .await?;
        self.start_download(stored, None, trace_id).await
    }

    async fn cancel_download(
        &self,
        id: DownloadId,
        delete_partial_files: bool,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let ctrl_opt = {
            let mut guard = self.inner.lock().await;
            if let Some(waiting) = guard.waiting.remove(&id) {
                let _ = guard.scheduler.release_and_promote(id);
                if delete_partial_files {
                    let _ = tokio::fs::remove_file(waiting.download.temp_path()).await;
                    let _ = tokio::fs::remove_file(waiting.download.meta_sidecar_path()).await;
                }
                return Ok(());
            }
            guard.active.get(&id).map(|e| e.ctrl_tx.clone())
        };

        if let Some(tx) = ctrl_opt {
            let (ack_tx, ack_rx) = oneshot::channel();
            if tx
                .send(ActorControlMsg::Cancel {
                    delete_partial: delete_partial_files,
                    trace_id,
                    ack: ack_tx,
                })
                .await
                .is_ok()
            {
                let _ = tokio::time::timeout(Duration::from_secs(3), ack_rx).await;
            }
        }
        Ok(())
    }

    async fn active_snapshots(&self) -> Vec<ProgressSnapshotDto> {
        collect_snapshots(&self.inner).await
    }

    async fn shutdown_gracefully(&self, _trace_id: TraceId) -> AppResult<()> {
        self.root_cancel.cancel();
        let rx_opt = self.reaper_done_rx.lock().await.take();
        if let Some(rx) = rx_opt {
            // Enforce strict `<= 5s` Graceful Shutdown budget (`§24.3` & `ADR-017`)!
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
        self.inner.lock().await.sleep_guard = None;
        Ok(())
    }
}
