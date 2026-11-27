//! Comprehensive Engine Integration, Bulkhead Panic Isolation, and **100× Kill-9 + Failpoint
//! Crash-Safety Torture Suite** (`P1-T18`, `P1-T19`, `P1-T20` / `ADR-004`, `ADR-008`, `ADR-009`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use vortex_application::{
    AppConfig, AppResult, BrowserIntegrationStatusDto, ByteSource, ByteStream, ConfigPort,
    DownloadRepository, EngineControlPort, EventBus, PlatformIntegration, ProbeOutcome,
    ResourceRequest, SleepInhibitorGuard, TransportMetrics,
};
use vortex_domain::{
    ByteRange, CategoryId, Checksum, Download, DownloadCommand, DownloadId, DownloadState,
    HashAlgorithm, Priority, QueueId, Resource, SystemClock,
};
use vortex_engine::{
    BroadcastEventBus, EngineSupervisor, MidCheckpointHook, PriorityQueueScheduler,
};
use vortex_error::{ErrorCode, TraceId, VortexError};
use vortex_io::{SparseFileSinkFactory, apply_mark_of_the_web_and_av_scan};
use vortex_net::{HttpByteSource, HttpClientConfig};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteDatabase, SqliteDownloadRepository,
    SqliteEventLogRepository,
};
use vortex_test_lab::{TestLabServer, deterministic_sha256};

struct DummySleepGuard;
impl SleepInhibitorGuard for DummySleepGuard {}

struct TestPlatform;

#[async_trait]
impl PlatformIntegration for TestPlatform {
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

/// Wrapper `ByteSource` that intentionally triggers a real Rust `panic!` on URLs containing `"/panic-me"`
/// to prove `EngineSupervisor` catches `JoinError::is_panic()` without affecting sibling downloads!
struct PanickingByteSource {
    inner: HttpByteSource,
}

#[async_trait]
impl ByteSource for PanickingByteSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        assert!(
            !req.url.contains("/panic-me"),
            "Simulated unexpected panic inside DownloadActor task!"
        );
        self.inner.probe_and_stream(req).await
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        self.inner.open_range(req, range).await
    }
}

fn sha256_file_hex(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    let mut h = Sha256::new();
    h.update(&bytes);
    hex::encode(h.finalize())
}

async fn wait_for_terminal_state(
    repo: &SqliteDownloadRepository,
    id: DownloadId,
    timeout: Duration,
) -> Download {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(dl) = repo.find(id, TraceId::new()).await.unwrap()
            && matches!(
                dl.state,
                DownloadState::Completed { .. }
                    | DownloadState::Failed { .. }
                    | DownloadState::NeedsAttention { .. }
            )
        {
            return dl;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for download {id} to reach terminal state"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Harness {
    supervisor: Arc<EngineSupervisor>,
    dl_repo: Arc<SqliteDownloadRepository>,
    cfg_mgr: Arc<ConfigManager>,
    event_bus: Arc<BroadcastEventBus>,
}

async fn build_harness(
    state_dir: &Path,
    downloads_dir: &Path,
    max_conns: u8,
    max_parallel: u8,
    checkpoint_secs: u64,
    custom_source: Option<Arc<dyn ByteSource>>,
    mid_hook: Option<MidCheckpointHook>,
) -> Harness {
    let trace = TraceId::new();
    let clock = Arc::new(SystemClock::new());
    let db_path = state_dir.join("vortex.db");
    let cfg_path = state_dir.join("config.toml");

    let (cfg_mgr_raw, _) = ConfigManager::load_or_init(&cfg_path, trace).unwrap();
    let cfg_mgr = Arc::new(cfg_mgr_raw);
    cfg_mgr
        .update(
            AppConfig {
                max_connections_per_download: max_conns,
                max_concurrent_downloads: max_parallel,
                checkpoint_interval_secs: checkpoint_secs.max(5),
                default_download_dir: downloads_dir.to_path_buf(),
                ..AppConfig::default()
            },
            trace,
        )
        .await
        .unwrap();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &db_path,
        &[downloads_dir.to_path_buf()],
        downloads_dir,
        clock.clone(),
        trace,
    )
    .unwrap();

    let dl_repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let evt_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secret_store = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let byte_source: Arc<dyn ByteSource> = custom_source.unwrap_or_else(|| {
        Arc::new(HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap())
    });
    let event_bus = Arc::new(BroadcastEventBus::default());

    let supervisor = EngineSupervisor::start(
        byte_source,
        sink_factory,
        dl_repo.clone(),
        evt_repo,
        secret_store,
        Arc::new(TestPlatform),
        cfg_mgr.clone(),
        event_bus.clone(),
        clock,
        mid_hook,
    );

    Harness {
        supervisor,
        dl_repo,
        cfg_mgr,
        event_bus,
    }
}

#[tokio::test]
async fn multi_worker_hybrid_leases_keepalive_priority_scheduler_and_bulkhead_panic_isolation() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let trace = TraceId::new();

    // Pure scheduler priority ordering check (`Critical > High > Normal > Low` + FIFO tie-breaker)
    let mut sched = PriorityQueueScheduler::new(1);
    let id_first = DownloadId::new();
    let id_low = DownloadId::new();
    let id_crit = DownloadId::new();
    assert!(sched.try_acquire_or_enqueue(id_first, Priority::Normal));
    assert!(!sched.try_acquire_or_enqueue(id_low, Priority::Low));
    assert!(!sched.try_acquire_or_enqueue(id_crit, Priority::Critical));
    assert_eq!(sched.active_count(), 1);
    let promoted = sched.release_and_promote(id_first);
    assert_eq!(promoted, vec![id_crit]);

    let http = HttpByteSource::new(
        Arc::new(SystemClock::new()),
        HttpClientConfig::for_local_testing(),
    )
    .unwrap();
    let panicking_source = Arc::new(PanickingByteSource { inner: http });
    let h = build_harness(&state_dir, &dl_dir, 8, 2, 5, Some(panicking_source), None).await;
    let mut sub = h.event_bus.subscribe();

    // Launch Download A (panics inside actor!) AND Download B (6 MiB multi-connection file with SHA-256!)
    let (bad_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(format!("{}/panic-me", lab.base_url())),
        format!("{}/panic-me", lab.base_url()),
        dl_dir.clone(),
        "crash.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let bad_id = bad_dl.id;
    h.dl_repo.save(&bad_dl, trace).await.unwrap();

    let six_mib: u64 = 6 * 1024 * 1024;
    let expected_hex = deterministic_sha256(99, six_mib);
    let expected_cs = Checksum::new(HashAlgorithm::Sha256, &expected_hex).unwrap();
    let (good_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(format!("{}/files/{six_mib}?seed=99", lab.base_url())),
        format!("{}/files/{six_mib}?seed=99", lab.base_url()),
        dl_dir.clone(),
        "ubuntu-6mb.iso".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::High,
        Some(expected_cs),
        1_001,
        trace,
    );
    let good_id = good_dl.id;

    h.supervisor
        .start_download(bad_dl, None, trace)
        .await
        .unwrap();
    h.supervisor
        .start_download(good_dl, None, trace)
        .await
        .unwrap();

    let bad_final = wait_for_terminal_state(&h.dl_repo, bad_id, Duration::from_secs(10)).await;
    let good_final = wait_for_terminal_state(&h.dl_repo, good_id, Duration::from_secs(15)).await;

    // Download A isolated to `Failed(VTX-BUG-001)`, while Download B completed 100% with verified SHA-256!
    match bad_final.state {
        DownloadState::Failed { error, .. } => {
            assert_eq!(error.code, ErrorCode::BugInternalIsolated);
        }
        other => panic!("Expected Failed(VTX-BUG-001), got {other:?}"),
    }
    assert_eq!(good_final.state.as_slug(), "completed");
    assert_eq!(
        sha256_file_hex(&dl_dir.join("ubuntu-6mb.iso")),
        expected_hex
    );
    assert!(sub.try_recv().is_ok());

    // Also test non-range (`200 OK`), `chunked` unknown-size, `429 Retry-After` recovery, and `Pause`/`Cancel`!
    let (chunked_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(format!("{}/chunked/131072?seed=5", lab.base_url())),
        format!("{}/chunked/131072?seed=5", lab.base_url()),
        dl_dir.clone(),
        "streamed.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        2_000,
        trace,
    );
    let cid = chunked_dl.id;
    h.supervisor
        .start_download(chunked_dl, None, trace)
        .await
        .unwrap();
    let chunked_done = wait_for_terminal_state(&h.dl_repo, cid, Duration::from_secs(10)).await;
    assert_eq!(chunked_done.state.as_slug(), "completed");
    assert_eq!(
        sha256_file_hex(&dl_dir.join("streamed.bin")),
        deterministic_sha256(5, 131_072)
    );

    let (rl_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(format!(
            "{}/rate-limit/65536?seed=11&fail_first=1&retry_after=1",
            lab.base_url()
        )),
        "https://lab/rate-limit".to_owned(),
        dl_dir.clone(),
        "ratelimited.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        2_100,
        trace,
    );
    let rlid = rl_dl.id;
    h.supervisor
        .start_download(rl_dl, None, trace)
        .await
        .unwrap();
    let rl_done = wait_for_terminal_state(&h.dl_repo, rlid, Duration::from_secs(10)).await;
    assert_eq!(rl_done.state.as_slug(), "completed");

    // Throttled download: test `pause_download`, `resume_download`, `pause_all`, `cancel_download`, and `shutdown_gracefully`
    let (throt_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(format!(
            "{}/throttled/1048576?bps=262144&seed=3",
            lab.base_url()
        )),
        "https://lab/throttled".to_owned(),
        dl_dir.clone(),
        "throttled.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        3_000,
        trace,
    );
    let tid = throt_dl.id;
    h.supervisor
        .start_download(throt_dl, None, trace)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(180)).await;
    let _snaps = h.supervisor.active_snapshots().await;
    h.supervisor.pause_download(tid, trace).await.unwrap();
    let paused = h.dl_repo.find(tid, trace).await.unwrap().unwrap();
    assert_eq!(paused.state.as_slug(), "paused");

    h.supervisor.resume_download(tid, trace).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    h.supervisor.pause_all(trace).await.unwrap();
    h.supervisor
        .cancel_download(tid, true, trace)
        .await
        .unwrap();
    let _ = h.cfg_mgr.current();
    h.supervisor.shutdown_gracefully(trace).await.unwrap();
}

/// **P1-T20 TORTURE SUITE (Part 1 — 100× Abrupt Kills + Mid-Checkpoint `Flush <-> COMMIT` Faults):**
/// Interrupts a multi-connection (`8` workers) seeded download **100 times** across random mid-flight
/// intervals and between `FlushFileBuffers` and `SQLite COMMIT`, resuming from on-disk SQLite + `.vortex`
/// sparse file, and proves **100% SHA-256 integrity** at completion!
#[tokio::test]
async fn torture_100x_abrupt_kills_and_mid_checkpoint_faults_produce_exact_sha256() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let trace = TraceId::new();

    // 16 MiB file throttled per-TCP-socket so 100 abrupt kill/resume cycles hit active parallel transfers!
    let total_size: u64 = 16 * 1024 * 1024;
    let seed = 2026_u64;
    let expected_sha256 = deterministic_sha256(seed, total_size);
    let url = format!(
        "{}/throttled/{total_size}?bps=2097152&seed={seed}",
        lab.base_url()
    );

    let dl_id = DownloadId::new();
    let (initial_dl, _) = Download::new_pending(
        dl_id,
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "torture-16mb.bin".to_owned(),
        CategoryId::builtin(CategoryId::COMPRESSED),
        QueueId::main_queue(),
        Priority::High,
        Some(Checksum::new(HashAlgorithm::Sha256, &expected_sha256).unwrap()),
        1_000,
        trace,
    );

    let fault_counter = Arc::new(AtomicU32::new(0));
    {
        let h0 = build_harness(&state_dir, &dl_dir, 8, 1, 5, None, None).await;
        h0.dl_repo.save(&initial_dl, trace).await.unwrap();
    }

    // Execute 100 abrupt crash / failpoint cycles!
    for kill_round in 0..100_u32 {
        let inject_mid_commit_fault = kill_round % 5 == 0;
        let fc = Arc::clone(&fault_counter);
        let hook: Option<MidCheckpointHook> = if inject_mid_commit_fault {
            Some(Arc::new(move || {
                fc.fetch_add(1, Ordering::Relaxed);
                Err(VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    TraceId::new(),
                    "Simulated power loss right AFTER FlushFileBuffers and BEFORE SQLite COMMIT",
                ))
            }))
        } else {
            None
        };

        let h = build_harness(&state_dir, &dl_dir, 8, 1, 5, None, hook).await;
        let mut stored = h.dl_repo.find(dl_id, trace).await.unwrap().unwrap();
        if matches!(stored.state, DownloadState::Completed { .. }) {
            break;
        }
        // Simulate Startup Self-Healing reconciling dirty crashed state -> resumable `Paused`
        stored.segments.clear_active_workers();
        let _ = stored.apply(DownloadCommand::Pause, 2_000 + i64::from(kill_round), trace);
        h.dl_repo.save(&stored, trace).await.unwrap();

        let _ = h.supervisor.resume_download(dl_id, trace).await;
        let slice_us = 4_000 + u64::from((kill_round * 733) % 12_000);
        tokio::time::sleep(Duration::from_micros(slice_us)).await;

        if inject_mid_commit_fault {
            // Trigger checkpoint path via pause so the `Flush -> COMMIT` failpoint fires!
            let _ = h.supervisor.pause_download(dl_id, trace).await;
        }
        // Abruptly abort all actor tasks WITHOUT graceful checkpoint flush (simulates sudden `kill -9` / power cut!)
        h.supervisor.abort_abruptly().await;
        drop(h);
    }

    assert!(fault_counter.load(Ordering::Relaxed) > 0);

    // Final clean resume run to 100% completion and SHA-256 verification!
    let h_final = build_harness(&state_dir, &dl_dir, 8, 1, 5, None, None).await;
    let mut stored = h_final.dl_repo.find(dl_id, trace).await.unwrap().unwrap();
    if !matches!(stored.state, DownloadState::Completed { .. }) {
        stored.segments.clear_active_workers();
        let _ = stored.apply(DownloadCommand::Pause, 999_000, trace);
        h_final.dl_repo.save(&stored, trace).await.unwrap();
        h_final
            .supervisor
            .resume_download(dl_id, trace)
            .await
            .unwrap();
    }

    let completed = wait_for_terminal_state(&h_final.dl_repo, dl_id, Duration::from_secs(30)).await;
    assert_eq!(completed.state.as_slug(), "completed");
    assert_eq!(
        sha256_file_hex(&dl_dir.join("torture-16mb.bin")),
        expected_sha256
    );
    h_final.supervisor.shutdown_gracefully(trace).await.unwrap();
}

/// **P1-T20 TORTURE SUITE (Part 2 — Real OS `Child::kill()` = Windows `TerminateProcess` / `kill -9`):**
/// Spawns a real OS child process downloading via 8 parallel workers into an NTFS sparse `.vortex`
/// file, kills the child process mid-flight via `Child::kill()` (`kill -9`), then resumes in the
/// parent process and proves 100% SHA-256 integrity!
#[tokio::test]
async fn real_os_process_kill9_mid_download_resumes_and_matches_sha256() {
    if let Ok(work_dir_str) = std::env::var("VORTEX_KILL9_WORKDIR") {
        let url = std::env::var("VORTEX_KILL9_URL").unwrap();
        let id_str = std::env::var("VORTEX_KILL9_ID").unwrap();
        let work_dir = PathBuf::from(work_dir_str);
        let state_dir = work_dir.join("state");
        let dl_dir = work_dir.join("downloads");
        let dl_id = DownloadId::parse(&id_str).unwrap();
        let trace = TraceId::new();

        let h = build_harness(&state_dir, &dl_dir, 8, 1, 5, None, None).await;
        if let Some(mut existing) = h.dl_repo.find(dl_id, trace).await.unwrap() {
            existing.segments.clear_active_workers();
            let _ = existing.apply(DownloadCommand::Pause, 1_500, trace);
            h.dl_repo.save(&existing, trace).await.unwrap();
            h.supervisor.resume_download(dl_id, trace).await.unwrap();
        } else {
            let (dl, _) = Download::new_pending(
                dl_id,
                Resource::new(url.clone()),
                url,
                dl_dir,
                "kill9-real.bin".to_owned(),
                CategoryId::default(),
                QueueId::main_queue(),
                Priority::High,
                None,
                1_000,
                trace,
            );
            h.dl_repo.save(&dl, trace).await.unwrap();
            h.supervisor.start_download(dl, None, trace).await.unwrap();
        }
        // Stay alive transferring until parent sends `Child::kill()` (`TerminateProcess`)!
        tokio::time::sleep(Duration::from_secs(30)).await;
        return;
    }

    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();

    let size: u64 = 8 * 1024 * 1024;
    let seed = 777_u64;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!("{}/throttled/{size}?bps=524288&seed={seed}", lab.base_url());
    let dl_id = DownloadId::new();
    let current_test_bin = std::env::current_exe().unwrap();

    // Kill real child OS process with `Child::kill()` (`TerminateProcess` / `SIGKILL`) 3 times!
    for _ in 0..3 {
        let mut child = tokio::process::Command::new(&current_test_bin)
            .arg("--exact")
            .arg("real_os_process_kill9_mid_download_resumes_and_matches_sha256")
            .arg("--nocapture")
            .env("VORTEX_KILL9_WORKDIR", dir.path())
            .env("VORTEX_KILL9_URL", &url)
            .env("VORTEX_KILL9_ID", dl_id.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(280)).await;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    // Resume in parent process from on-disk SQLite + `.vortex` sparse file and verify SHA-256!
    let trace = TraceId::new();
    let h = build_harness(&state_dir, &dl_dir, 8, 1, 5, None, None).await;
    let mut stored = h.dl_repo.find(dl_id, trace).await.unwrap().unwrap();
    if !matches!(stored.state, DownloadState::Completed { .. }) {
        stored.segments.clear_active_workers();
        let _ = stored.apply(DownloadCommand::Pause, 5_000, trace);
        h.dl_repo.save(&stored, trace).await.unwrap();
        h.supervisor.resume_download(dl_id, trace).await.unwrap();
    }

    let finished = wait_for_terminal_state(&h.dl_repo, dl_id, Duration::from_secs(25)).await;
    assert_eq!(finished.state.as_slug(), "completed");
    assert_eq!(
        sha256_file_hex(&dl_dir.join("kill9-real.bin")),
        expected_sha
    );
    h.supervisor.shutdown_gracefully(trace).await.unwrap();
}
