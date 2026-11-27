//! Exhaustive edge-case and resilience tests for `vortex-engine` (`P1-T18`..`P1-T20`),
//! verifying `0ms` pre-opened probe handoff, queue priority promotion, validator mismatch,
//! disk reservation exhaustion, `FallbackToBrowser`, backoff cancellation/shutdown, secret
//! auto-deletion on completion (`TH-08`), and premature-EOF recovery (`>= 90%` coverage gate).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppConfig, AppResult, BrowserIntegrationStatusDto, ByteSource, ByteStream, ConfigPort,
    DownloadRepository, EngineControlPort, FallbackReason, FileSink, FileSinkFactory,
    PlatformIntegration, ProbeOutcome, ResourceInfo, ResourceRequest, SecretCredentials,
    SecretStore, SleepInhibitorGuard, SpaceReservationId, TransportMetrics, VolumeSpaceLedger,
};
use vortex_domain::{
    ByteRange, CategoryId, Download, DownloadId, DownloadState, EtagValidator, Priority, QueueId,
    Resource, ResumeValidators, SystemClock,
};
use vortex_engine::{
    BroadcastEventBus, EngineSupervisor, PersistenceHandle, PriorityQueueScheduler,
    actors::child_token, layer_name,
};
use vortex_error::{ApiError, ErrorCode, Recoverability, TraceId, VortexError};
use vortex_io::{SparseFileSinkFactory, apply_mark_of_the_web_and_av_scan};
use vortex_observability::{ExposeSecret, SecretString};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteDatabase, SqliteDownloadRepository,
    SqliteEventLogRepository,
};

struct DummySleep;
impl SleepInhibitorGuard for DummySleep {}

struct StubPlatform;

#[async_trait]
impl PlatformIntegration for StubPlatform {
    fn current_user_sid(&self, _trace_id: TraceId) -> AppResult<String> {
        Ok("S-1-5-21-EDGE".to_owned())
    }

    fn acquire_sleep_inhibitor(
        &self,
        _trace_id: TraceId,
    ) -> AppResult<Box<dyn SleepInhibitorGuard>> {
        Ok(Box::new(DummySleep))
    }

    async fn apply_motw_and_av_scan(
        &self,
        file_path: &Path,
        source_url: &str,
        referrer_url: Option<&str>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        apply_mark_of_the_web_and_av_scan(file_path, source_url, referrer_url, trace_id).await
    }

    fn register_native_hosts(
        &self,
        _bridge_exe: &Path,
        _allowed_extension_ids: &[String],
        _trace_id: TraceId,
    ) -> AppResult<Vec<BrowserIntegrationStatusDto>> {
        Ok(vec![])
    }

    fn check_native_hosts(&self) -> Vec<BrowserIntegrationStatusDto> {
        vec![]
    }

    fn is_internet_available(&self) -> bool {
        true
    }
}

struct MemoryStream {
    chunks: VecDeque<AppResult<Bytes>>,
    metrics: TransportMetrics,
}

impl MemoryStream {
    fn from_bytes(data: Vec<u8>) -> Self {
        let mut chunks = VecDeque::new();
        if !data.is_empty() {
            chunks.push_back(Ok(Bytes::from(data)));
        }
        Self {
            chunks,
            metrics: TransportMetrics {
                rtt_us: Some(12_000),
                min_rtt_us: Some(10_000),
                is_proxied: false,
                reused_connection: true,
                is_http2: false,
            },
        }
    }
}

#[async_trait]
impl ByteStream for MemoryStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        match self.chunks.pop_front() {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.metrics
    }
}

enum ScriptedBehavior {
    FallbackWaf,
    AuthRequired,
    TransientAlways { retry_after_ms: u64 },
    PrematureEofOnce { payload: Vec<u8> },
    MutatedEtag { etag: String, payload: Vec<u8> },
    ZeroBytes,
    SlowInfinite,
    NormalPayload { payload: Vec<u8>, ranged: bool },
}

struct ScriptedByteSource {
    mode: Mutex<ScriptedBehavior>,
    seen_cookies: Mutex<Option<String>>,
}

impl ScriptedByteSource {
    fn new(mode: ScriptedBehavior) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            seen_cookies: Mutex::new(None),
        })
    }
}

#[async_trait]
impl ByteSource for ScriptedByteSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        if let Some(c) = &req.cookies {
            *self.seen_cookies.lock().unwrap() = Some(c.expose_secret().to_owned());
        }
        let res_opt: Option<AppResult<ProbeOutcome>> = {
            let mut guard = self.mode.lock().unwrap();
            match &mut *guard {
                ScriptedBehavior::FallbackWaf => Some(Ok(ProbeOutcome::FallbackToBrowser {
                    reason: FallbackReason::WafTlsBound,
                    status_code: 403,
                    detail: "cf-mitigated: challenge".to_owned(),
                })),
                ScriptedBehavior::AuthRequired => Some(Err(VortexError::new(
                    ApiError::new(ErrorCode::AuthRequired, req.trace_id)
                        .with_recoverability(Recoverability::UserActionRequired)
                        .with_technical_detail("401 Unauthorized"),
                ))),
                ScriptedBehavior::TransientAlways { retry_after_ms } => {
                    Some(Err(VortexError::new(
                        ApiError::new(ErrorCode::NetConnectionLost, req.trace_id)
                            .with_recoverability(Recoverability::Transient {
                                retry_after_ms: Some(*retry_after_ms),
                            })
                            .with_technical_detail("503 Service Unavailable"),
                    )))
                }
                ScriptedBehavior::PrematureEofOnce { payload } => {
                    let total = payload.len() as u64;
                    let info = ResourceInfo {
                        final_url: req.url.clone(),
                        total_size: Some(total),
                        supports_range: true,
                        mime_type: Some("application/octet-stream".to_owned()),
                        suggested_filename: None,
                        raw_suggested_filename: None,
                        validators: ResumeValidators {
                            etag: Some(EtagValidator::Strong("\"v1\"".to_owned())),
                            last_modified: None,
                            content_length: Some(total),
                        },
                        server_checksum: None,
                        transport_metrics: TransportMetrics::default(),
                    };
                    let half = payload[..(payload.len() / 2)].to_vec();
                    let full = payload.clone();
                    *guard = ScriptedBehavior::NormalPayload {
                        payload: full,
                        ranged: true,
                    };
                    Some(Ok(ProbeOutcome::Transferable {
                        info: Box::new(info),
                        initial_stream: Box::new(MemoryStream::from_bytes(half)),
                    }))
                }
                ScriptedBehavior::MutatedEtag { etag, payload } => {
                    let total = payload.len() as u64;
                    let info = ResourceInfo {
                        final_url: req.url.clone(),
                        total_size: Some(total),
                        supports_range: true,
                        mime_type: None,
                        suggested_filename: None,
                        raw_suggested_filename: None,
                        validators: ResumeValidators {
                            etag: Some(EtagValidator::Strong(etag.clone())),
                            last_modified: None,
                            content_length: Some(total),
                        },
                        server_checksum: None,
                        transport_metrics: TransportMetrics::default(),
                    };
                    Some(Ok(ProbeOutcome::Transferable {
                        info: Box::new(info),
                        initial_stream: Box::new(MemoryStream::from_bytes(payload.clone())),
                    }))
                }
                ScriptedBehavior::ZeroBytes => {
                    let info = ResourceInfo {
                        final_url: req.url.clone(),
                        total_size: Some(0),
                        supports_range: true,
                        mime_type: Some("text/plain".to_owned()),
                        suggested_filename: Some("empty.txt".to_owned()),
                        raw_suggested_filename: None,
                        validators: ResumeValidators::default(),
                        server_checksum: None,
                        transport_metrics: TransportMetrics::default(),
                    };
                    Some(Ok(ProbeOutcome::Transferable {
                        info: Box::new(info),
                        initial_stream: Box::new(MemoryStream::from_bytes(vec![])),
                    }))
                }
                ScriptedBehavior::SlowInfinite => None,
                ScriptedBehavior::NormalPayload { payload, ranged } => {
                    let total = payload.len() as u64;
                    let info = ResourceInfo {
                        final_url: req.url.clone(),
                        total_size: Some(total),
                        supports_range: *ranged,
                        mime_type: Some("application/octet-stream".to_owned()),
                        suggested_filename: None,
                        raw_suggested_filename: None,
                        validators: ResumeValidators {
                            etag: Some(EtagValidator::Strong("\"v1\"".to_owned())),
                            last_modified: None,
                            content_length: Some(total),
                        },
                        server_checksum: None,
                        transport_metrics: TransportMetrics::default(),
                    };
                    Some(Ok(ProbeOutcome::Transferable {
                        info: Box::new(info),
                        initial_stream: Box::new(MemoryStream::from_bytes(payload.clone())),
                    }))
                }
            }
        };
        if let Some(res) = res_opt {
            return res;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        Err(VortexError::coded(
            ErrorCode::NetConnectionLost,
            req.trace_id,
            "slow",
        ))
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        let slice_opt = {
            let guard = self.mode.lock().unwrap();
            if let ScriptedBehavior::NormalPayload { payload, .. } = &*guard {
                let s = usize::try_from(range.start()).unwrap();
                let e = usize::try_from(range.end()).unwrap().min(payload.len());
                Some(payload[s..e].to_vec())
            } else {
                None
            }
        };
        if let Some(slice) = slice_opt {
            let stream = MemoryStream::from_bytes(slice);
            let m = stream.transport_metrics();
            return Ok((Box::new(stream), m));
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        Err(VortexError::coded(
            ErrorCode::NetConnectionLost,
            req.trace_id,
            "range aborted",
        ))
    }
}

/// Sink factory wrapper whose `VolumeSpaceLedger` rejects reservations to test `VTX-DISK-001` handling.
struct FullDiskLedger;

impl VolumeSpaceLedger for FullDiskLedger {
    fn reserve(
        &self,
        _target_dir: &Path,
        _bytes: u64,
        _min_free_reserve: u64,
        trace_id: TraceId,
    ) -> AppResult<SpaceReservationId> {
        Err(VortexError::new(
            ApiError::new(ErrorCode::DiskInsufficientSpace, trace_id)
                .with_recoverability(Recoverability::UserActionRequired)
                .with_technical_detail("Simulated ENOSPC in VolumeSpaceLedger"),
        ))
    }

    fn record_written(&self, _id: SpaceReservationId, _written_bytes: u64) {}
    fn release(&self, _id: SpaceReservationId) {}

    fn available_bytes(&self, _target_dir: &Path, _trace_id: TraceId) -> AppResult<u64> {
        Ok(0)
    }
}

struct FullDiskSinkFactory {
    inner: SparseFileSinkFactory,
}

#[async_trait]
impl FileSinkFactory for FullDiskSinkFactory {
    async fn open(
        &self,
        temp_path: &Path,
        meta_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<Arc<dyn FileSink>> {
        self.inner.open(temp_path, meta_path, trace_id).await
    }

    fn space_ledger(&self) -> Arc<dyn VolumeSpaceLedger> {
        Arc::new(FullDiskLedger)
    }
}

async fn setup_env(
    dir: &Path,
    source: Arc<dyn ByteSource>,
    sink_factory: Arc<dyn FileSinkFactory>,
    max_parallel: u8,
    max_retries: u32,
) -> (
    Arc<EngineSupervisor>,
    Arc<SqliteDownloadRepository>,
    Arc<DpapiSecretStore>,
) {
    let trace = TraceId::new();
    let clock = Arc::new(SystemClock::new());
    let state_dir = dir.join("state");
    let dl_dir = dir.join("dl");
    std::fs::create_dir_all(&dl_dir).unwrap();

    let (cfg_raw, _) = ConfigManager::load_or_init(&state_dir.join("config.toml"), trace).unwrap();
    let cfg = Arc::new(cfg_raw);
    cfg.update(
        AppConfig {
            max_concurrent_downloads: max_parallel,
            max_connections_per_download: 4,
            max_retries,
            default_download_dir: dl_dir.clone(),
            ..AppConfig::default()
        },
        trace,
    )
    .await
    .unwrap();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &state_dir.join("vortex.db"),
        std::slice::from_ref(&dl_dir),
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let dl_repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let bus = Arc::new(BroadcastEventBus::new(128));

    let sup = EngineSupervisor::start(
        source,
        sink_factory,
        dl_repo.clone(),
        ev_repo,
        secrets.clone(),
        Arc::new(StubPlatform),
        cfg,
        bus,
        clock,
        None,
    );
    (sup, dl_repo, secrets)
}

async fn wait_state(
    repo: &SqliteDownloadRepository,
    id: DownloadId,
    pred: impl Fn(&DownloadState) -> bool,
) -> Download {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(d) = repo.find(id, TraceId::new()).await.unwrap()
            && pred(&d.state)
        {
            return d;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timeout waiting for state on {id}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[test]
fn scheduler_priority_fifo_and_layer_helpers_work() {
    assert_eq!(layer_name(), "vortex-engine");
    let root = CancellationToken::new();
    let child = child_token(&root);
    root.cancel();
    assert!(child.is_cancelled());

    let mut sched = PriorityQueueScheduler::new(1);
    let a = DownloadId::new();
    let low = DownloadId::new();
    let high_1 = DownloadId::new();
    let high_2 = DownloadId::new();
    let cancelled = DownloadId::new();

    assert!(sched.try_acquire_or_enqueue(a, Priority::Normal));
    // Re-acquiring active ID is idempotent
    assert!(sched.try_acquire_or_enqueue(a, Priority::Normal));
    assert_eq!(sched.active_count(), 1);

    assert!(!sched.try_acquire_or_enqueue(low, Priority::Low));
    assert!(!sched.try_acquire_or_enqueue(cancelled, Priority::Critical));
    assert!(!sched.try_acquire_or_enqueue(high_1, Priority::High));
    assert!(!sched.try_acquire_or_enqueue(high_2, Priority::High));

    // Cancel waiting item before slot frees
    assert_eq!(
        sched.release_and_promote(cancelled),
        Vec::<DownloadId>::new()
    );

    // Release active `a` -> `high_1` promoted first (FIFO within `High`)!
    assert_eq!(sched.release_and_promote(a), vec![high_1]);
    sched.set_max_parallel(3);
    assert_eq!(sched.drain_available_promotions(), vec![high_2, low]);
    assert_eq!(sched.active_count(), 3);
}

#[tokio::test]
async fn preopened_probe_zero_byte_secret_cleanup_and_premature_eof_recovery() {
    let dir = tempdir().unwrap();
    let dl_dir = dir.path().join("dl");
    let trace = TraceId::new();

    // 1. Premature EOF on first attempt -> automatic transient retry -> 100% completion + DPAPI secret auto-cleanup (`TH-08`)!
    let payload = vec![0xAB_u8; 262_144];
    let source = ScriptedByteSource::new(ScriptedBehavior::PrematureEofOnce {
        payload: payload.clone(),
    });
    let (sup, repo, secrets) = setup_env(
        dir.path(),
        source.clone(),
        Arc::new(SparseFileSinkFactory::default()),
        2,
        3,
    )
    .await;
    assert!(format!("{sup:?}").contains("EngineSupervisor"));

    let (mut dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://cdn.example.com/recover.bin"),
        "https://cdn.example.com/recover.bin".to_owned(),
        dl_dir.clone(),
        "recover.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::High,
        None,
        1_000,
        trace,
    );
    dl.resource.has_secret_credentials = true;
    secrets
        .store(
            dl.id,
            &SecretCredentials {
                cookies: Some(SecretString::from("sid=topsecret123")),
                authorization: None,
                origin_key: None,
            },
            trace,
        )
        .await
        .unwrap();
    repo.save(&dl, trace).await.unwrap();

    let rec_id = dl.id;
    sup.start_download(dl, None, trace).await.unwrap();
    let done = wait_state(&repo, rec_id, |s| s.as_slug() == "completed").await;
    assert_eq!(done.downloaded_bytes(), 262_144);
    assert_eq!(
        source.seen_cookies.lock().unwrap().as_deref(),
        Some("sid=topsecret123")
    );
    // Secret cookies MUST be wiped once Completed (`TH-08` least retention)!
    assert!(secrets.load(rec_id, trace).await.unwrap().is_none());

    // 2. Pre-opened probe (`0ms` start) + Zero-byte file (`total_size = Some(0)`)
    let (zero_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://cdn.example.com/zero.txt"),
        "https://cdn.example.com/zero.txt".to_owned(),
        dl_dir.clone(),
        "zero.txt".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_100,
        trace,
    );
    let zid = zero_dl.id;
    repo.save(&zero_dl, trace).await.unwrap();
    let pre_info = ResourceInfo {
        final_url: "https://cdn.example.com/zero.txt".to_owned(),
        total_size: Some(0),
        supports_range: true,
        mime_type: Some("text/plain".to_owned()),
        suggested_filename: Some("zero.txt".to_owned()),
        raw_suggested_filename: None,
        validators: ResumeValidators::default(),
        server_checksum: None,
        transport_metrics: TransportMetrics::default(),
    };
    sup.start_download(
        zero_dl,
        Some((
            Box::new(pre_info),
            Box::new(MemoryStream::from_bytes(vec![])),
        )),
        trace,
    )
    .await
    .unwrap();
    let zero_done = wait_state(&repo, zid, |s| s.as_slug() == "completed").await;
    assert_eq!(zero_done.total_size, Some(0));
    assert!(dl_dir.join("zero.txt").exists());

    // 3. `resume_download` on missing ID returns `ResNotFound`
    let missing_err = sup
        .resume_download(DownloadId::new(), trace)
        .await
        .unwrap_err();
    assert_eq!(missing_err.code(), ErrorCode::ResNotFound);

    // 4. `pause_download` on inactive `Pending` persisted record transitions to `Paused`
    let (inactive_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://cdn.example.com/idle.bin"),
        "https://cdn.example.com/idle.bin".to_owned(),
        dl_dir,
        "idle.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Low,
        None,
        1_200,
        trace,
    );
    let idle_id = inactive_dl.id;
    repo.save(&inactive_dl, trace).await.unwrap();
    sup.pause_download(idle_id, trace).await.unwrap();
    assert_eq!(
        repo.find(idle_id, trace)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_slug(),
        "paused"
    );

    // 5. `PersistenceHandle` cleanly returns `DbCorruptOrIncompatible` (zero panic!) if actor task aborts
    let (closed_db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("closed_ev.db"),
        &[],
        dir.path(),
        Arc::new(SystemClock::new()),
        trace,
    )
    .unwrap();
    let ev_repo_closed = Arc::new(SqliteEventLogRepository::new(closed_db));
    let (closed_handle, p_join) = PersistenceHandle::spawn(
        repo.clone(),
        ev_repo_closed,
        Arc::new(BroadcastEventBus::default()),
    );
    p_join.abort();
    let _ = p_join.await;
    let dummy_dl = repo.find(idle_id, trace).await.unwrap().unwrap();
    assert_eq!(
        closed_handle
            .save_and_wait(dummy_dl.clone(), vec![], trace)
            .await
            .unwrap_err()
            .code(),
        ErrorCode::DbCorruptOrIncompatible
    );
    assert_eq!(
        closed_handle
            .commit_checkpoint(
                idle_id,
                dummy_dl.segments.completed,
                dummy_dl.state,
                2_000,
                vec![],
                trace,
            )
            .await
            .unwrap_err()
            .code(),
        ErrorCode::DbCorruptOrIncompatible
    );

    sup.shutdown_gracefully(trace).await.unwrap();
}

#[tokio::test]
async fn fallback_waf_auth_needs_attention_validator_mismatch_and_full_disk() {
    let trace = TraceId::new();

    // Case A: WAF FallbackToBrowser -> `Failed(ResLinkExpired)`
    {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::FallbackWaf),
            Arc::new(SparseFileSinkFactory::default()),
            2,
            1,
        )
        .await;
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://waf.example.com/f.iso"),
            "https://waf.example.com/f.iso".to_owned(),
            dir.path().join("dl"),
            "f.iso".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let id = dl.id;
        repo.save(&dl, trace).await.unwrap();
        sup.start_download(dl, None, trace).await.unwrap();
        let res = wait_state(&repo, id, |s| s.as_slug() == "failed").await;
        if let DownloadState::Failed { error, .. } = res.state {
            assert_eq!(error.code, ErrorCode::ResLinkExpired);
        }
        sup.shutdown_gracefully(trace).await.unwrap();
    }

    // Case B: `UserActionRequired` (`401`) -> `NeedsAttention`
    {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::AuthRequired),
            Arc::new(SparseFileSinkFactory::default()),
            2,
            1,
        )
        .await;
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://auth.example.com/f.iso"),
            "https://auth.example.com/f.iso".to_owned(),
            dir.path().join("dl"),
            "f.iso".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let id = dl.id;
        repo.save(&dl, trace).await.unwrap();
        sup.start_download(dl, None, trace).await.unwrap();
        let res = wait_state(&repo, id, |s| s.as_slug() == "needs_attention").await;
        assert_eq!(res.state.as_slug(), "needs_attention");
        sup.shutdown_gracefully(trace).await.unwrap();
    }

    // Case C: Mutated `ETag` on resume -> `NeedsAttention(ResResourceChanged)`
    {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::MutatedEtag {
                etag: "\"v2-mutated\"".to_owned(),
                payload: vec![1_u8; 65_536],
            }),
            Arc::new(SparseFileSinkFactory::default()),
            2,
            1,
        )
        .await;
        let (mut dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/mutated.bin"),
            "https://cdn.example.com/mutated.bin".to_owned(),
            dir.path().join("dl"),
            "mutated.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        dl.total_size = Some(65_536);
        dl.supports_range = true;
        dl.resource.validators.etag = Some(EtagValidator::Strong("\"v1-orig\"".to_owned()));
        dl.segments
            .completed
            .insert(ByteRange::new(0, 4_096).unwrap());
        dl.state = DownloadState::Paused {
            paused_at_ms: 200,
            downloaded_bytes: 4_096,
        };
        let id = dl.id;
        repo.save(&dl, trace).await.unwrap();
        sup.resume_download(id, trace).await.unwrap();
        let res = wait_state(&repo, id, |s| s.as_slug() == "needs_attention").await;
        if let DownloadState::NeedsAttention { reason_code, .. } = res.state {
            assert_eq!(reason_code, ErrorCode::ResResourceChanged);
        }
        sup.shutdown_gracefully(trace).await.unwrap();
    }

    // Case D: `VolumeSpaceLedger::reserve` fails (`ENOSPC`) -> `NeedsAttention(DiskInsufficientSpace)`
    {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::ZeroBytes),
            Arc::new(FullDiskSinkFactory {
                inner: SparseFileSinkFactory::default(),
            }),
            2,
            1,
        )
        .await;
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/big.iso"),
            "https://cdn.example.com/big.iso".to_owned(),
            dir.path().join("dl"),
            "big.iso".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let id = dl.id;
        repo.save(&dl, trace).await.unwrap();
        sup.start_download(dl, None, trace).await.unwrap();
        let res = wait_state(&repo, id, |s| s.as_slug() == "needs_attention").await;
        if let DownloadState::NeedsAttention { reason_code, .. } = res.state {
            assert_eq!(reason_code, ErrorCode::DiskInsufficientSpace);
        }
        sup.shutdown_gracefully(trace).await.unwrap();
    }
}

#[tokio::test]
async fn queueing_waiting_controls_and_retry_backoff_pause_cancel_and_exhaustion() {
    let trace = TraceId::new();

    // 1. Queueing (`max_parallel = 1`): active slow download + 2 waiting downloads (`pause` one, `cancel` one)
    {
        let dir = tempdir().unwrap();
        let dl_dir = dir.path().join("dl");
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::SlowInfinite),
            Arc::new(SparseFileSinkFactory::default()),
            1,
            1,
        )
        .await;

        let mk = |name: &str, prio: Priority| {
            let (d, _) = Download::new_pending(
                DownloadId::new(),
                Resource::new(format!("https://slow.example.com/{name}")),
                format!("https://slow.example.com/{name}"),
                dl_dir.clone(),
                name.to_owned(),
                CategoryId::default(),
                QueueId::main_queue(),
                prio,
                None,
                100,
                trace,
            );
            d
        };

        let d1 = mk("d1.bin", Priority::Normal);
        let d2 = mk("d2.bin", Priority::Low);
        let d3 = mk("d3.bin", Priority::High);
        let (id1, id2, id3) = (d1.id, d2.id, d3.id);
        repo.save(&d1, trace).await.unwrap();
        repo.save(&d2, trace).await.unwrap();
        repo.save(&d3, trace).await.unwrap();

        sup.start_download(d1, None, trace).await.unwrap();
        // Idempotent resume on active download
        sup.resume_download(id1, trace).await.unwrap();
        sup.start_download(d2, None, trace).await.unwrap();
        sup.start_download(d3, None, trace).await.unwrap();

        let q2 = wait_state(&repo, id2, |s| s.as_slug() == "queued").await;
        assert_eq!(q2.state.as_slug(), "queued");

        // Pause waiting `d2` and cancel waiting `d3`
        sup.pause_download(id2, trace).await.unwrap();
        assert_eq!(
            repo.find(id2, trace)
                .await
                .unwrap()
                .unwrap()
                .state
                .as_slug(),
            "paused"
        );
        sup.cancel_download(id3, true, trace).await.unwrap();
        sup.abort_abruptly().await;
    }

    // 2. Transient retry backoff: test `Pause`, `Cancel`, `Graceful Shutdown`, and retry exhaustion (`max_retries = 1`)!
    {
        let dir = tempdir().unwrap();
        let dl_dir = dir.path().join("dl");
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::TransientAlways {
                retry_after_ms: 5_000,
            }),
            Arc::new(SparseFileSinkFactory::default()),
            4,
            1,
        )
        .await;

        let mk = |name: &str| {
            let (d, _) = Download::new_pending(
                DownloadId::new(),
                Resource::new(format!("https://retry.example.com/{name}")),
                format!("https://retry.example.com/{name}"),
                dl_dir.clone(),
                name.to_owned(),
                CategoryId::default(),
                QueueId::main_queue(),
                Priority::Normal,
                None,
                100,
                trace,
            );
            d
        };

        let r_pause = mk("r_pause.bin");
        let r_cancel = mk("r_cancel.bin");
        let r_shut = mk("r_shut.bin");
        let (idp, idc, ids) = (r_pause.id, r_cancel.id, r_shut.id);
        repo.save(&r_pause, trace).await.unwrap();
        repo.save(&r_cancel, trace).await.unwrap();
        repo.save(&r_shut, trace).await.unwrap();

        sup.start_download(r_pause, None, trace).await.unwrap();
        sup.start_download(r_cancel, None, trace).await.unwrap();
        sup.start_download(r_shut, None, trace).await.unwrap();

        wait_state(&repo, idp, |s| s.as_slug() == "retrying").await;
        wait_state(&repo, idc, |s| s.as_slug() == "retrying").await;
        wait_state(&repo, ids, |s| s.as_slug() == "retrying").await;

        sup.pause_download(idp, trace).await.unwrap();
        assert_eq!(
            repo.find(idp, trace)
                .await
                .unwrap()
                .unwrap()
                .state
                .as_slug(),
            "paused"
        );
        sup.cancel_download(idc, true, trace).await.unwrap();
        sup.shutdown_gracefully(trace).await.unwrap();
        assert_eq!(
            repo.find(ids, trace)
                .await
                .unwrap()
                .unwrap()
                .state
                .as_slug(),
            "paused"
        );
    }

    // 3. Fast retry exhaustion (`retry_after_ms = 50`, `max_retries = 1`) -> `Failed`!
    {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::TransientAlways { retry_after_ms: 50 }),
            Arc::new(SparseFileSinkFactory::default()),
            2,
            1,
        )
        .await;
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://retry.example.com/exhaust.bin"),
            "https://retry.example.com/exhaust.bin".to_owned(),
            dir.path().join("dl"),
            "exhaust.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let id = dl.id;
        repo.save(&dl, trace).await.unwrap();
        sup.start_download(dl, None, trace).await.unwrap();
        let failed = wait_state(&repo, id, |s| s.as_slug() == "failed").await;
        assert_eq!(failed.state.as_slug(), "failed");
        sup.shutdown_gracefully(trace).await.unwrap();
    }
}

struct WriteErrorSink;

#[async_trait]
impl FileSink for WriteErrorSink {
    async fn prepare(&self, _total_size: Option<u64>, _trace_id: TraceId) -> AppResult<bool> {
        Ok(true)
    }

    async fn write_at(&self, _offset: u64, _data: Bytes, trace_id: TraceId) -> AppResult<()> {
        Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            "Simulated EACCES during write_at",
        ))
    }

    async fn flush_durable(
        &self,
        _sidecar: &vortex_application::SidecarMetadata,
        _trace_id: TraceId,
    ) -> AppResult<()> {
        Ok(())
    }

    async fn read_at(&self, _offset: u64, _len: usize, _trace_id: TraceId) -> AppResult<Bytes> {
        Ok(Bytes::new())
    }

    async fn verify_hash(
        &self,
        _algorithm: vortex_domain::HashAlgorithm,
        _trace_id: TraceId,
    ) -> AppResult<String> {
        Ok(String::new())
    }

    async fn finalize(&self, _final_path: &Path, _trace_id: TraceId) -> AppResult<u64> {
        Ok(0)
    }
}

struct WriteErrorSinkFactory {
    inner: SparseFileSinkFactory,
}

#[async_trait]
impl FileSinkFactory for WriteErrorSinkFactory {
    async fn open(
        &self,
        _temp_path: &Path,
        _meta_path: &Path,
        _trace_id: TraceId,
    ) -> AppResult<Arc<dyn FileSink>> {
        Ok(Arc::new(WriteErrorSink))
    }

    fn space_ledger(&self) -> Arc<dyn VolumeSpaceLedger> {
        self.inner.space_ledger()
    }
}

struct HangingStream;

#[async_trait]
impl ByteStream for HangingStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(None)
    }

    fn transport_metrics(&self) -> TransportMetrics {
        TransportMetrics::default()
    }
}

struct StreamErrSource {
    hang_in_transfer: bool,
    ranged: bool,
}

#[async_trait]
impl ByteSource for StreamErrSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        let info = ResourceInfo {
            final_url: req.url.clone(),
            total_size: Some(4 * 1024 * 1024),
            supports_range: self.ranged,
            mime_type: None,
            suggested_filename: None,
            raw_suggested_filename: None,
            validators: ResumeValidators::default(),
            server_checksum: None,
            transport_metrics: TransportMetrics::default(),
        };
        if self.hang_in_transfer {
            return Ok(ProbeOutcome::Transferable {
                info: Box::new(info),
                initial_stream: Box::new(HangingStream),
            });
        }
        let mut chunks = VecDeque::new();
        chunks.push_back(Err(VortexError::coded(
            ErrorCode::ResNotFound,
            req.trace_id,
            "stream broken mid-body",
        )));
        Ok(ProbeOutcome::Transferable {
            info: Box::new(info),
            initial_stream: Box::new(MemoryStream {
                chunks,
                metrics: TransportMetrics::default(),
            }),
        })
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        _range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        if self.hang_in_transfer {
            return Ok((Box::new(HangingStream), TransportMetrics::default()));
        }
        Err(VortexError::coded(
            ErrorCode::ResNotFound,
            req.trace_id,
            "open_range rejected",
        ))
    }
}

#[tokio::test]
async fn active_cancel_queue_promotion_disk_write_error_and_stream_errors() {
    let trace = TraceId::new();

    // 1. Queue Promotion (`max_parallel = 1`): `d1` finishes -> `d2` promoted from `WaitingDownload` -> `d2` completes!
    {
        let dir = tempdir().unwrap();
        let dl_dir = dir.path().join("dl");
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::NormalPayload {
                payload: vec![0x5A_u8; 2 * 1024 * 1024],
                ranged: true,
            }),
            Arc::new(SparseFileSinkFactory::default()),
            1,
            1,
        )
        .await;
        let (d1, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/p1.bin"),
            "https://cdn.example.com/p1.bin".to_owned(),
            dl_dir.clone(),
            "p1.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let (d2, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/p2.bin"),
            "https://cdn.example.com/p2.bin".to_owned(),
            dl_dir,
            "p2.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::High,
            None,
            101,
            trace,
        );
        let (id1, id2) = (d1.id, d2.id);
        repo.save(&d1, trace).await.unwrap();
        repo.save(&d2, trace).await.unwrap();
        sup.start_download(d1, None, trace).await.unwrap();
        sup.start_download(d2, None, trace).await.unwrap();

        wait_state(&repo, id1, |s| s.as_slug() == "completed").await;
        wait_state(&repo, id2, |s| s.as_slug() == "completed").await;
        sup.shutdown_gracefully(trace).await.unwrap();
    }

    // 2. Active in-flight `Cancel` AND active in-flight `shutdown_gracefully`!
    {
        let dir = tempdir().unwrap();
        let dl_dir = dir.path().join("dl");
        let (sup, repo, _) = setup_env(
            dir.path(),
            Arc::new(StreamErrSource {
                hang_in_transfer: true,
                ranged: true,
            }),
            Arc::new(SparseFileSinkFactory::default()),
            4,
            1,
        )
        .await;
        let (dc, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/hang_c.bin"),
            "https://cdn.example.com/hang_c.bin".to_owned(),
            dl_dir.clone(),
            "hang_c.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let (ds, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/hang_s.bin"),
            "https://cdn.example.com/hang_s.bin".to_owned(),
            dl_dir,
            "hang_s.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            101,
            trace,
        );
        let (idc, ids) = (dc.id, ds.id);
        repo.save(&dc, trace).await.unwrap();
        repo.save(&ds, trace).await.unwrap();
        sup.start_download(dc, None, trace).await.unwrap();
        sup.start_download(ds, None, trace).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;

        sup.cancel_download(idc, true, trace).await.unwrap();
        sup.shutdown_gracefully(trace).await.unwrap();
        assert_eq!(
            repo.find(ids, trace)
                .await
                .unwrap()
                .unwrap()
                .state
                .as_slug(),
            "paused"
        );
    }

    // 3. `FileSink::write_at` EACCES error + non-range & ranged stream errors -> `Failed`!
    {
        let dir = tempdir().unwrap();
        let dl_dir = dir.path().join("dl");
        let (sup, repo, _) = setup_env(
            dir.path(),
            ScriptedByteSource::new(ScriptedBehavior::NormalPayload {
                payload: vec![1_u8; 65_536],
                ranged: false,
            }),
            Arc::new(WriteErrorSinkFactory {
                inner: SparseFileSinkFactory::default(),
            }),
            2,
            0,
        )
        .await;
        let (dw, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/eacces.bin"),
            "https://cdn.example.com/eacces.bin".to_owned(),
            dl_dir,
            "eacces.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let idw = dw.id;
        repo.save(&dw, trace).await.unwrap();
        sup.start_download(dw, None, trace).await.unwrap();
        let attn = wait_state(&repo, idw, |s| s.as_slug() == "needs_attention").await;
        assert_eq!(attn.state.as_slug(), "needs_attention");
        sup.shutdown_gracefully(trace).await.unwrap();
    }

    for ranged in [false, true] {
        let dir = tempdir().unwrap();
        let (sup, repo, _) = setup_env(
            dir.path(),
            Arc::new(StreamErrSource {
                hang_in_transfer: false,
                ranged,
            }),
            Arc::new(SparseFileSinkFactory::default()),
            2,
            0,
        )
        .await;
        let (de, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/err.bin"),
            "https://cdn.example.com/err.bin".to_owned(),
            dir.path().join("dl"),
            "err.bin".to_owned(),
            CategoryId::default(),
            QueueId::main_queue(),
            Priority::Normal,
            None,
            100,
            trace,
        );
        let ide = de.id;
        repo.save(&de, trace).await.unwrap();
        sup.start_download(de, None, trace).await.unwrap();
        let failed = wait_state(&repo, ide, |s| s.as_slug() == "failed").await;
        assert_eq!(failed.state.as_slug(), "failed");
        sup.shutdown_gracefully(trace).await.unwrap();
    }
}
