//! Phase 1.7 Integration Tests: Per-Worker `2 MiB` Write Coalescing (`<= 64` syscalls for `64 MiB`),
//! Decoupled Transient Socket Drops (`SmartHostProfileCache` not poisoned), `BBR/Gradient2` Dynamic
//! Auto-Scaling on `/per-conn-cap`, `End-Game` Hedged Request Tail Straggler Rescue, and High-BDP
//! `HTTP/2` Transfer + `TH-27` Header-Bomb Protection (`P1.7-T11`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use vortex_application::{
    AppConfig, AppResult, BrowserIntegrationStatusDto, ByteSource, ConfigPort, DownloadRepository,
    EngineControlPort, EventBus, FileSink, FileSinkFactory, PlatformIntegration, ProbeOutcome,
    ResourceRequest, SidecarMetadata, SleepInhibitorGuard, VolumeSpaceLedger,
};
use vortex_domain::{
    CategoryId, Download, DownloadId, HashAlgorithm, Priority, QueueId, Resource, SystemClock,
};
use vortex_engine::{BroadcastEventBus, EngineSupervisor};
use vortex_error::{ErrorCode, TraceId};
use vortex_io::SparseFileSinkFactory;
use vortex_net::{HttpByteSource, HttpClientConfig};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteDatabase, SqliteDownloadRepository,
    SqliteEventLogRepository,
};
use vortex_test_lab::{TestLabServer, deterministic_sha256};

struct DummySleepGuard;
impl SleepInhibitorGuard for DummySleepGuard {}

#[derive(Default)]
struct MockPlatform;

#[async_trait]
impl PlatformIntegration for MockPlatform {
    fn current_user_sid(&self, _trace_id: TraceId) -> AppResult<String> {
        Ok("S-1-5-21-TEST".to_owned())
    }
    fn acquire_sleep_inhibitor(
        &self,
        _trace_id: TraceId,
    ) -> AppResult<Box<dyn SleepInhibitorGuard>> {
        Ok(Box::new(DummySleepGuard))
    }
    async fn apply_motw_and_av_scan(
        &self,
        _file_path: &Path,
        _source_url: &str,
        _referrer_url: Option<&str>,
        _trace_id: TraceId,
    ) -> AppResult<()> {
        Ok(())
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

/// Thin spy wrapper around `SparseFileSinkFactory` recording exact `write_at` syscall & byte totals.
struct CountingSinkFactory {
    inner: SparseFileSinkFactory,
    total_write_syscalls: Arc<AtomicU64>,
    total_written_bytes: Arc<AtomicU64>,
}

impl CountingSinkFactory {
    fn new() -> Self {
        Self {
            inner: SparseFileSinkFactory::default(),
            total_write_syscalls: Arc::new(AtomicU64::new(0)),
            total_written_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    fn write_syscalls(&self) -> u64 {
        self.total_write_syscalls.load(Ordering::Relaxed)
    }

    fn written_bytes(&self) -> u64 {
        self.total_written_bytes.load(Ordering::Relaxed)
    }
}

struct CountingSink {
    inner: Arc<dyn FileSink>,
    syscalls: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

#[async_trait]
impl FileSink for CountingSink {
    async fn prepare(&self, total_size: Option<u64>, trace_id: TraceId) -> AppResult<bool> {
        self.inner.prepare(total_size, trace_id).await
    }
    async fn write_at(&self, offset: u64, data: Bytes, trace_id: TraceId) -> AppResult<()> {
        let len = data.len() as u64;
        self.inner.write_at(offset, data, trace_id).await?;
        self.syscalls.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }
    async fn read_at(&self, offset: u64, len: usize, trace_id: TraceId) -> AppResult<Bytes> {
        self.inner.read_at(offset, len, trace_id).await
    }
    async fn flush_durable(&self, sidecar: &SidecarMetadata, trace_id: TraceId) -> AppResult<()> {
        self.inner.flush_durable(sidecar, trace_id).await
    }
    async fn verify_hash(&self, algorithm: HashAlgorithm, trace_id: TraceId) -> AppResult<String> {
        self.inner.verify_hash(algorithm, trace_id).await
    }
    async fn finalize(&self, final_path: &Path, trace_id: TraceId) -> AppResult<u64> {
        self.inner.finalize(final_path, trace_id).await
    }
    fn write_syscall_count(&self) -> u64 {
        self.inner.write_syscall_count()
    }
    fn total_bytes_written(&self) -> u64 {
        self.inner.total_bytes_written()
    }
}

#[async_trait]
impl FileSinkFactory for CountingSinkFactory {
    async fn open(
        &self,
        temp_path: &Path,
        meta_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<Arc<dyn FileSink>> {
        let inner = self.inner.open(temp_path, meta_path, trace_id).await?;
        Ok(Arc::new(CountingSink {
            inner,
            syscalls: Arc::clone(&self.total_write_syscalls),
            bytes: Arc::clone(&self.total_written_bytes),
        }))
    }
    fn space_ledger(&self) -> Arc<dyn VolumeSpaceLedger> {
        self.inner.space_ledger()
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    hex::encode(Sha256::digest(&bytes))
}

async fn wait_download_state(
    repo: &SqliteDownloadRepository,
    id: DownloadId,
    pred: impl Fn(&str) -> bool,
) -> Download {
    for _ in 0..400 {
        if let Ok(Some(dl)) = repo.find(id, TraceId::new()).await
            && pred(dl.state.as_slug())
        {
            return dl;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let last = repo.find(id, TraceId::new()).await.unwrap();
    panic!(
        "Timed out waiting for download state; last={:?}",
        last.map(|d| d.state)
    );
}

async fn build_test_harness(
    dir: &Path,
    max_conns: u8,
    auto_scale: bool,
    http_cfg: HttpClientConfig,
    sink_factory: Arc<dyn FileSinkFactory>,
) -> (
    Arc<EngineSupervisor>,
    Arc<SqliteDownloadRepository>,
    Arc<HttpByteSource>,
    std::path::PathBuf,
) {
    let dl_dir = dir.join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.join("vortex.db"),
        &[],
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let (cfg_mgr, _) = ConfigManager::load_or_init(&dir.join("config.toml"), trace).unwrap();
    let cfg_mgr = Arc::new(cfg_mgr);
    cfg_mgr
        .update(
            AppConfig {
                max_connections_per_download: max_conns,
                auto_scale_connections: auto_scale,
                enable_http2: http_cfg.enable_http2,
                default_download_dir: dl_dir.clone(),
                ..AppConfig::default()
            },
            trace,
        )
        .await
        .unwrap();

    let http = Arc::new(HttpByteSource::new(clock.clone(), http_cfg).unwrap());
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::default());
    let sup = EngineSupervisor::start(
        http.clone(),
        sink_factory,
        repo.clone(),
        ev_repo,
        secrets,
        Arc::new(MockPlatform),
        cfg_mgr,
        bus,
        clock,
        None,
    );
    (sup, repo, http, dl_dir)
}

#[tokio::test]
async fn per_worker_coalescing_64mb_8_workers_executes_under_64_write_syscalls_and_exact_sha256() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let counting = Arc::new(CountingSinkFactory::new());
    let (sup, repo, _, dl_dir) = build_test_harness(
        dir.path(),
        8,
        false,
        HttpClientConfig::for_local_testing(),
        counting.clone(),
    )
    .await;

    let size: u64 = 64 * 1024 * 1024;
    let seed: u64 = 1701;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!("{}/files/{size}?seed={seed}", lab.base_url());
    let trace = TraceId::new();

    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "coalesce-64mb.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    assert_eq!(done.downloaded_bytes(), size);
    let final_path = dl_dir.join("coalesce-64mb.bin");
    assert_eq!(sha256_file(&final_path), expected_sha);

    let syscalls = counting.write_syscalls();
    let written = counting.written_bytes();
    assert_eq!(written, size);
    assert!(
        syscalls <= 64,
        "Expected <= 64 coalesced WriteFile syscalls for 64 MiB (2 MiB slabs), got {syscalls}"
    );
    let avg_write = written / syscalls.max(1);
    assert!(
        avg_write >= 1_572_864,
        "Expected average write size >= 1.5 MiB, got {avg_write} bytes ({syscalls} syscalls)"
    );

    // Follow-up download on the SAME proven fast host exercises `proven_workers` 0ms parallel burst!
    let burst_size: u64 = 4 * 1024 * 1024;
    let burst_url = format!("{}/files/{burst_size}?seed=1799", lab.base_url());
    let (dl2, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(burst_url.clone()),
        burst_url,
        dl_dir.clone(),
        "proven-burst-4mb.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        2_000,
        trace,
    );
    let id2 = dl2.id;
    repo.save(&dl2, trace).await.unwrap();
    sup.start_download(dl2, None, trace).await.unwrap();
    let done2 = wait_download_state(&repo, id2, |s| s == "completed").await;
    assert_eq!(done2.downloaded_bytes(), burst_size);
    assert_eq!(
        sha256_file(&dl_dir.join("proven-burst-4mb.bin")),
        deterministic_sha256(1799, burst_size)
    );

    // Non-range `3 MiB` file exercises `>= 2 MiB` coalesced slab flush in the sequential path!
    let seq_size: u64 = 3 * 1024 * 1024;
    let seq_url = format!("{}/no-range/{seq_size}?seed=1798", lab.base_url());
    let (dl3, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(seq_url.clone()),
        seq_url,
        dl_dir.clone(),
        "non-range-3mb.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        3_000,
        trace,
    );
    let id3 = dl3.id;
    repo.save(&dl3, trace).await.unwrap();
    sup.start_download(dl3, None, trace).await.unwrap();
    let done3 = wait_download_state(&repo, id3, |s| s == "completed").await;
    assert_eq!(done3.downloaded_bytes(), seq_size);
    assert_eq!(
        sha256_file(&dl_dir.join("non-range-3mb.bin")),
        deterministic_sha256(1798, seq_size)
    );

    let _ = sup.shutdown_gracefully(trace).await;
    lab.shutdown();
}

#[tokio::test]
async fn transient_net_connection_lost_does_not_poison_host_profile_or_collapse_concurrency() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let (sup, repo, _, dl_dir) = build_test_harness(
        dir.path(),
        4,
        false,
        HttpClientConfig::for_local_testing(),
        sink_factory,
    )
    .await;

    let size: u64 = 16 * 1024 * 1024;
    let seed: u64 = 1702;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!(
        "{}/transient-reset-once/{size}?id=tr-1&bps=8388608&seed={seed}",
        lab.base_url()
    );
    let trace = TraceId::new();

    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "transient-reset.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    assert_eq!(done.downloaded_bytes(), size);
    assert_eq!(
        sha256_file(&dl_dir.join("transient-reset.bin")),
        expected_sha
    );

    let stats = lab.telemetry();
    assert!(
        stats.transient_reset_drops >= 1,
        "expected simulated secondary ConnectionReset drop to fire"
    );
    assert!(
        stats.transient_reset_recoveries >= 1,
        "expected respawned secondary worker to recover tail without collapsing host to 1 stream"
    );

    let _ = sup.shutdown_gracefully(trace).await;
    lab.shutdown();
}

#[tokio::test]
async fn bbr_gradient2_auto_scales_up_on_per_conn_capped_host() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    // Start with ONLY 2 connections + `auto_scale_connections = true`:
    // `/per-conn-cap/1572864/25165824` caps each TCP socket to 1.5 MiB/s so BBR scales 2 -> 4 -> 6+!
    let (sup, repo, _, dl_dir) = build_test_harness(
        dir.path(),
        2,
        true,
        HttpClientConfig::for_local_testing(),
        sink_factory,
    )
    .await;

    let size: u64 = 24 * 1024 * 1024;
    let seed: u64 = 1703;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!("{}/per-conn-cap/1572864/{size}?seed={seed}", lab.base_url());
    let trace = TraceId::new();

    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "bbr-autoscale.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    assert_eq!(done.downloaded_bytes(), size);
    assert_eq!(sha256_file(&dl_dir.join("bbr-autoscale.bin")), expected_sha);

    let stats = lab.telemetry();
    assert!(
        stats.per_conn_cap_peak_streams >= 4,
        "expected BBR/Gradient2 to scale concurrency above initial 2 (peak observed={})",
        stats.per_conn_cap_peak_streams
    );

    // Follow-up download on the SAME proven fast host exercises `proven_workers` 0ms parallel burst!
    let burst_size: u64 = 4 * 1024 * 1024;
    let burst_url = format!("{}/files/{burst_size}?seed=1797", lab.base_url());
    let (dl2, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(burst_url.clone()),
        burst_url,
        dl_dir.clone(),
        "bbr-proven-burst.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        2_000,
        trace,
    );
    let id2 = dl2.id;
    repo.save(&dl2, trace).await.unwrap();
    sup.start_download(dl2, None, trace).await.unwrap();
    let done2 = wait_download_state(&repo, id2, |s| s == "completed").await;
    assert_eq!(done2.downloaded_bytes(), burst_size);
    assert_eq!(
        sha256_file(&dl_dir.join("bbr-proven-burst.bin")),
        deterministic_sha256(1797, burst_size)
    );

    let _ = sup.shutdown_gracefully(trace).await;
    lab.shutdown();
}

#[tokio::test]
async fn endgame_hedged_request_rescues_slow_tail_straggler_with_exact_sha256() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let (sup, repo, _, dl_dir) = build_test_harness(
        dir.path(),
        8,
        false,
        HttpClientConfig::for_local_testing(),
        sink_factory,
    )
    .await;

    // 16 MiB across 8 workers (`6 MiB/s` fast workers): final `<= 2 MiB` tail is throttled to
    // `16 KiB/s` (~128s alone!). End-Game Mode detects the slow straggler and rescues it via Hedged Request!
    let size: u64 = 16 * 1024 * 1024;
    let seed: u64 = 1704;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!(
        "{}/tail-straggler/{size}?id=hedge-1&bps=6291456&seed={seed}",
        lab.base_url()
    );
    let trace = TraceId::new();

    let t0 = Instant::now();
    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "tail-straggler.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    let elapsed = t0.elapsed();
    assert_eq!(done.downloaded_bytes(), size);
    assert_eq!(
        sha256_file(&dl_dir.join("tail-straggler.bin")),
        expected_sha
    );

    let stats = lab.telemetry();
    assert!(
        stats.tail_straggler_slow_hits >= 1,
        "expected tail straggler trap to trigger"
    );
    assert!(
        stats.tail_straggler_fast_rescues >= 1,
        "expected End-Game Hedged Request to rescue slow tail straggler"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "expected End-Game hedge to finish well under 10s (instead of 128s straggler), took {elapsed:?}"
    );

    let _ = sup.shutdown_gracefully(trace).await;
    lab.shutdown();
}

#[tokio::test]
async fn high_bdp_http2_transfer_and_header_bomb_rejection_th27() {
    let h2_lab = TestLabServer::spawn_h2().await.unwrap();
    let dir = tempdir().unwrap();
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let (sup, repo, http, dl_dir) = build_test_harness(
        dir.path(),
        8,
        false,
        HttpClientConfig::for_local_h2_testing(),
        sink_factory,
    )
    .await;

    let trace = TraceId::new();

    // 1. Verify `TH-27` Header-Bomb (`150 > 100` headers) is rejected cleanly over HTTP/2!
    let bomb_req = ResourceRequest::new(
        DownloadId::new(),
        format!("{}/header-bomb", h2_lab.base_url()),
        trace,
    );
    let bomb_err = http.probe_and_stream(&bomb_req).await.unwrap_err();
    assert_eq!(bomb_err.code(), ErrorCode::NetMalformedResponse);

    // 2. Verify `probe_and_stream` negotiates HTTP/2 (`is_http2 == true`)!
    let size: u64 = 16 * 1024 * 1024;
    let seed: u64 = 1705;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!("{}/files/{size}?seed={seed}", h2_lab.base_url());
    let probe_req = ResourceRequest::new(DownloadId::new(), url.clone(), trace);
    let ProbeOutcome::Transferable { info, .. } = http.probe_and_stream(&probe_req).await.unwrap()
    else {
        panic!("expected Transferable HTTP/2 probe");
    };
    assert!(
        info.transport_metrics.is_http2,
        "expected HTTP/2 transport metrics on h2c server"
    );

    // 3. Full 16 MiB multi-stream HTTP/2 download through `EngineSupervisor` with exact SHA-256!
    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "http2-16mb.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    assert_eq!(done.downloaded_bytes(), size);
    assert_eq!(sha256_file(&dl_dir.join("http2-16mb.bin")), expected_sha);

    let _ = sup.shutdown_gracefully(trace).await;
    h2_lab.shutdown();
}
