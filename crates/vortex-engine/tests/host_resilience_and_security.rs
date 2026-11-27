//! Phase 1.6 Integration Tests: Host Resilience (Slow-Start + Non-Fatal Step-Down + Continuous
//! Worker #1 Stream), Error Classification (`VTX-RES-006`, `VTX-AUTH-002`, `VTX-NET-005`),
//! User-Misuse Guards (`UX-M02` FAT32, `UX-M03` missing `.vortex`), and Security (`TH-27`, `TH-28`,
//! `TH-31`, `TH-32`) (`P1.6-T11` & `P1.6-T12`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use vortex_application::{
    AppConfig, AppResult, BrowserIntegrationStatusDto, ByteSource, ConfigPort, DownloadRepository,
    EngineControlPort, EventBus, PlatformIntegration, ProbeOutcome, ResourceRequest,
    SecretCredentials, SecretStore, SleepInhibitorGuard,
};
use vortex_domain::{CategoryId, Download, DownloadId, Priority, QueueId, Resource, SystemClock};
use vortex_engine::{BroadcastEventBus, EngineSupervisor};
use vortex_error::{ErrorCode, SuggestedAction, TraceId};
use vortex_io::{AlignedSlabPool, SparseFileSinkFactory, VolumeSpaceManager};
use vortex_net::{HttpByteSource, HttpClientConfig};
use vortex_observability::SecretString;
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteDatabase, SqliteDownloadRepository,
    SqliteEventLogRepository,
};
use vortex_test_lab::{TestLabServer, deterministic_sha256};

struct DummySleepGuard;
impl SleepInhibitorGuard for DummySleepGuard {}

struct MockPlatform {
    online: Arc<AtomicBool>,
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self {
            online: Arc::new(AtomicBool::new(true)),
        }
    }
}

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
        _file_path: &std::path::Path,
        _source_url: &str,
        _referrer_url: Option<&str>,
        _trace_id: TraceId,
    ) -> AppResult<()> {
        Ok(())
    }
    fn register_native_hosts(
        &self,
        _bridge_exe: &std::path::Path,
        _allowed_extension_ids: &[String],
        _trace_id: TraceId,
    ) -> AppResult<Vec<BrowserIntegrationStatusDto>> {
        Ok(vec![])
    }
    fn check_native_hosts(&self) -> Vec<BrowserIntegrationStatusDto> {
        vec![]
    }
    fn is_internet_available(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

fn sha256_file(path: &std::path::Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

async fn wait_download_state(
    repo: &SqliteDownloadRepository,
    id: DownloadId,
    pred: impl Fn(&str) -> bool,
) -> Download {
    for _ in 0..300 {
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

#[tokio::test]
async fn concurrency_capped_1_stream_host_completes_100_percent_with_exact_sha256_via_slow_start_step_down()
 {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("vortex.db"),
        &[],
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let (cfg_mgr, _) = ConfigManager::load_or_init(&dir.path().join("config.toml"), trace).unwrap();
    let cfg_mgr = Arc::new(cfg_mgr);
    cfg_mgr
        .update(
            AppConfig {
                max_connections_per_download: 8,
                default_download_dir: dl_dir.clone(),
                ..AppConfig::default()
            },
            trace,
        )
        .await
        .unwrap();

    let http = Arc::new(
        HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap(),
    );
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::default());
    let sup = EngineSupervisor::start(
        http,
        sink_factory,
        repo.clone(),
        ev_repo,
        secrets,
        Arc::new(MockPlatform::default()),
        cfg_mgr,
        bus,
        clock,
        None,
    );

    // 8 MiB leaves >= 5 MiB unleased after Worker #1's initial 3 MiB lease so Worker #2 genuinely
    // steals a tail, requests stream #2, receives `403`, triggers Non-Fatal Concurrency Step-Down,
    // reclaims its lease, and records the host ceiling in `HostCapabilityCache`!
    let size: u64 = 8 * 1024 * 1024;
    let seed: u64 = 77;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!(
        "{}/concurrency-capped/1/{size}?id=test-pd-1&bps=4194304&seed={seed}&name=pixeldrain-file.zip",
        lab.base_url()
    );

    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "pixeldrain-file.zip".to_owned(),
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
    let final_path = dl_dir.join("pixeldrain-file.zip");
    assert!(final_path.exists());
    assert_eq!(sha256_file(&final_path), expected_sha);

    // 2nd download on the same host should start at capped concurrency (`HostCapabilityCache`) and finish cleanly too!
    let url2 = format!(
        "{}/concurrency-capped/1/{size}?id=test-pd-2&bps=4194304&seed=78&name=pixeldrain-file-2.zip",
        lab.base_url()
    );
    let (dl2, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url2.clone()),
        url2,
        dl_dir.clone(),
        "pixeldrain-file-2.zip".to_owned(),
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
    assert_eq!(done2.downloaded_bytes(), size);
    assert_eq!(
        sha256_file(&dl_dir.join("pixeldrain-file-2.zip")),
        deterministic_sha256(78, size)
    );

    sup.shutdown_gracefully(trace).await.unwrap();
}

#[tokio::test]
async fn classifies_quota_captcha_clock_skew_header_bomb_and_over_delivery_under_th27() {
    let lab = TestLabServer::spawn().await.unwrap();
    let clock = Arc::new(SystemClock::new());
    let client = HttpByteSource::new(clock, HttpClientConfig::for_local_testing()).unwrap();
    let trace = TraceId::new();

    // 1. Pixeldrain quota / CAPTCHA JSON -> `VTX-RES-006` + `OpenReferrerInBrowser`
    let mut quota_req = ResourceRequest::new(
        DownloadId::new(),
        format!("{}/quota-captcha", lab.base_url()),
        trace,
    );
    quota_req.is_browser_handoff = false;
    let Err(quota_err) = client.probe_and_stream(&quota_req).await else {
        panic!("expected VTX-RES-006");
    };
    assert_eq!(quota_err.code(), ErrorCode::ResQuotaOrCaptcha);
    assert!(
        quota_err
            .as_api_error()
            .suggested_actions
            .contains(&SuggestedAction::OpenReferrerInBrowser)
    );

    // 2. Clock skew `403 RequestTimeTooSkewed` -> `VTX-AUTH-002` + `SyncSystemClock`
    let skew_req = ResourceRequest::new(
        DownloadId::new(),
        format!("{}/clock-skew", lab.base_url()),
        trace,
    );
    let Err(skew_err) = client.probe_and_stream(&skew_req).await else {
        panic!("expected VTX-AUTH-002");
    };
    assert_eq!(skew_err.code(), ErrorCode::AuthClockSkew);
    assert!(
        skew_err
            .as_api_error()
            .suggested_actions
            .contains(&SuggestedAction::SyncSystemClock)
    );

    // 3. Header bomb (> 100 headers) -> `VTX-NET-005` (`TH-27`)
    let bomb_req = ResourceRequest::new(
        DownloadId::new(),
        format!("{}/header-bomb", lab.base_url()),
        trace,
    );
    let Err(bomb_err) = client.probe_and_stream(&bomb_req).await else {
        panic!("expected VTX-NET-005 on header bomb");
    };
    assert_eq!(bomb_err.code(), ErrorCode::NetMalformedResponse);

    // 4. Over-delivery (> declared Content-Range length) -> `VTX-NET-005` (`TH-27`)
    let over_req = ResourceRequest::new(
        DownloadId::new(),
        format!("{}/over-deliver/2048", lab.base_url()),
        trace,
    );
    let outcome = client.probe_and_stream(&over_req).await.unwrap();
    let mut stream = match outcome {
        ProbeOutcome::Transferable { initial_stream, .. } => initial_stream,
        ProbeOutcome::FallbackToBrowser { .. } => panic!("unexpected fallback"),
    };
    let mut saw_over_deliver_err = false;
    loop {
        match stream.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(err) => {
                assert_eq!(err.code(), ErrorCode::NetMalformedResponse);
                saw_over_deliver_err = true;
                break;
            }
        }
    }
    assert!(
        saw_over_deliver_err,
        "HyperBodyStream must abort over-delivering response with VTX-NET-005 (TH-27)"
    );
}

#[tokio::test]
async fn missing_partial_vortex_file_on_resume_resets_cleanly_with_zero_sparse_holes_ux_m03() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("vortex.db"),
        &[],
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let (cfg_mgr, _) = ConfigManager::load_or_init(&dir.path().join("config.toml"), trace).unwrap();
    let cfg_mgr = Arc::new(cfg_mgr);
    cfg_mgr
        .update(
            AppConfig {
                max_connections_per_download: 4,
                default_download_dir: dl_dir.clone(),
                ..AppConfig::default()
            },
            trace,
        )
        .await
        .unwrap();

    let http = Arc::new(
        HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap(),
    );
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::default());
    let sup = EngineSupervisor::start(
        http,
        sink_factory,
        repo.clone(),
        ev_repo,
        secrets,
        Arc::new(MockPlatform::default()),
        cfg_mgr,
        bus,
        clock,
        None,
    );

    let size: u64 = 2 * 1024 * 1024;
    let seed: u64 = 55;
    let expected_sha = deterministic_sha256(seed, size);
    let url = format!("{}/throttled/{size}?bps=524288&seed={seed}", lab.base_url());

    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "resume-clean.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    // Wait until some bytes are written, then pause cleanly
    for _ in 0..80 {
        if let Some(snap) = sup
            .active_snapshots()
            .await
            .into_iter()
            .find(|s| s.download_id == id.to_string())
            && snap.downloaded_bytes >= 128 * 1024
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    sup.pause_download(id, trace).await.unwrap();
    let paused = wait_download_state(&repo, id, |s| s == "paused").await;
    assert!(paused.downloaded_bytes() > 0);

    // Simulate user / CCleaner deleting `.vortex` while paused (`UX-M03`)!
    let temp_file = dl_dir.join("resume-clean.bin.vortex");
    if temp_file.exists() {
        std::fs::remove_file(&temp_file).unwrap();
    }
    assert!(!temp_file.exists());

    // Resume MUST detect missing `.vortex`, reset completed ranges to byte 0, and produce 100% valid SHA-256!
    sup.resume_download(id, trace).await.unwrap();
    let done = wait_download_state(&repo, id, |s| s == "completed").await;
    assert_eq!(done.downloaded_bytes(), size);
    assert_eq!(sha256_file(&dl_dir.join("resume-clean.bin")), expected_sha);

    sup.shutdown_gracefully(trace).await.unwrap();
}

#[tokio::test]
async fn fat32_4gib_limit_fails_pre_flight_with_vtx_disk_005_ux_m02() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let dl_dir = dir.path().join("fat32_usb");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("vortex.db"),
        &[],
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let (cfg_mgr, _) = ConfigManager::load_or_init(&dir.path().join("config.toml"), trace).unwrap();

    let space_mgr = Arc::new(VolumeSpaceManager::new());
    space_mgr.set_simulated_fs_name(Some("FAT32".to_owned()));
    let sink_factory = Arc::new(SparseFileSinkFactory::new(
        space_mgr,
        Arc::new(AlignedSlabPool::default()),
    ));
    let http = Arc::new(
        HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap(),
    );
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::default());
    let sup = EngineSupervisor::start(
        http,
        sink_factory,
        repo.clone(),
        ev_repo,
        secrets,
        Arc::new(MockPlatform::default()),
        Arc::new(cfg_mgr),
        bus,
        clock,
        None,
    );

    // Virtual 5 GiB file on Test Lab -> blocked immediately before disk allocation!
    let five_gib: u64 = 5 * 1024 * 1024 * 1024;
    let url = format!("{}/files/{five_gib}", lab.base_url());
    let (dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "big-image.iso".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let id = dl.id;
    repo.save(&dl, trace).await.unwrap();
    sup.start_download(dl, None, trace).await.unwrap();

    let needs_attn = wait_download_state(&repo, id, |s| s == "needs_attention").await;
    match needs_attn.state {
        vortex_domain::DownloadState::NeedsAttention { reason_code, .. } => {
            assert_eq!(reason_code, ErrorCode::DiskFilesystemLimit);
        }
        other => panic!("expected NeedsAttention(VTX-DISK-005), got {other:?}"),
    }
    assert!(!dl_dir.join("big-image.iso.vortex").exists());

    sup.shutdown_gracefully(trace).await.unwrap();
}

#[tokio::test]
async fn origin_pinned_secrets_enforce_th28() {
    let dir = tempdir().unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();
    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("vortex.db"),
        &[],
        dir.path(),
        clock.clone(),
        trace,
    )
    .unwrap();
    let secrets = DpapiSecretStore::new(db, clock);
    let dl_id = DownloadId::new();
    let creds = SecretCredentials {
        cookies: Some(SecretString::from("session=pinned_secret_token")),
        authorization: Some(SecretString::from("Bearer pinned_jwt")),
        origin_key: SecretCredentials::origin_key_from_url(
            "https://downloads.trusted.example/f.zip",
        ),
    };
    secrets.store(dl_id, &creds, trace).await.unwrap();
    let loaded = secrets.load(dl_id, trace).await.unwrap().unwrap();
    assert!(loaded.origin_matches("https://downloads.trusted.example/other.zip"));
    assert!(!loaded.origin_matches("https://ad-redirect.evil.example/f.zip"));
    assert!(!loaded.origin_matches("http://downloads.trusted.example/f.zip"));
}

#[tokio::test]
async fn worker_strips_cross_origin_secrets_and_freezes_retries_offline_ux_m10_th28() {
    let lab = TestLabServer::spawn().await.unwrap();
    let dir = tempdir().unwrap();
    let dl_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let clock = Arc::new(SystemClock::new());
    let trace = TraceId::new();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &dir.path().join("vortex.db"),
        &[],
        &dl_dir,
        clock.clone(),
        trace,
    )
    .unwrap();
    let repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db, clock.clone()));
    let (cfg_mgr, _) = ConfigManager::load_or_init(&dir.path().join("config.toml"), trace).unwrap();
    let cfg_mgr = Arc::new(cfg_mgr);
    cfg_mgr
        .update(
            AppConfig {
                max_connections_per_download: 2,
                max_retries: 1,
                default_download_dir: dl_dir.clone(),
                ..AppConfig::default()
            },
            trace,
        )
        .await
        .unwrap();

    let online = Arc::new(AtomicBool::new(true));
    let platform = Arc::new(MockPlatform {
        online: Arc::clone(&online),
    });
    let http = Arc::new(
        HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap(),
    );
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::default());
    let sup = EngineSupervisor::start(
        http,
        sink_factory,
        repo.clone(),
        ev_repo,
        secrets.clone(),
        platform,
        cfg_mgr,
        bus,
        clock,
        None,
    );

    // 1. Cross-Origin Secret Stripping in `worker.rs` (`TH-28`): stored creds belong to another origin
    let size: u64 = 65_536;
    let url = format!("{}/files/{size}?seed=13", lab.base_url());
    let (dl_sec, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(url.clone()),
        url,
        dl_dir.clone(),
        "origin-guard.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        1_000,
        trace,
    );
    let mismatched_creds = SecretCredentials {
        cookies: Some(SecretString::from("session=never_leak_me")),
        authorization: Some(SecretString::from("Bearer never_leak_me")),
        origin_key: SecretCredentials::origin_key_from_url("https://other.trusted.example/a.bin"),
    };
    secrets
        .store(dl_sec.id, &mismatched_creds, trace)
        .await
        .unwrap();
    let sec_id = dl_sec.id;
    repo.save(&dl_sec, trace).await.unwrap();
    sup.start_download(dl_sec, None, trace).await.unwrap();
    let done_sec = wait_download_state(&repo, sec_id, |s| s == "completed").await;
    assert_eq!(done_sec.downloaded_bytes(), size);

    // 2. Offline Network Freeze (`UX-M10`): allocate an unbound loopback port to trigger transient
    // `NetConnectionLost` while offline, then verify Pause, Cancel, and Online-Restoration!
    let unbound_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = unbound_listener.local_addr().unwrap().port();
    drop(unbound_listener);
    let dead_url = format!("http://127.0.0.1:{dead_port}/unreachable.bin");

    online.store(false, Ordering::SeqCst);

    // 2a. Pause while offline-frozen
    let (dl_pause, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(dead_url.clone()),
        dead_url.clone(),
        dl_dir.clone(),
        "offline-pause.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        2_000,
        trace,
    );
    let pause_id = dl_pause.id;
    repo.save(&dl_pause, trace).await.unwrap();
    sup.start_download(dl_pause, None, trace).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    sup.pause_download(pause_id, trace).await.unwrap();
    let paused = wait_download_state(&repo, pause_id, |s| s == "paused").await;
    assert_eq!(paused.state.as_slug(), "paused");

    // 2b. Cancel while offline-frozen
    let (dl_cancel, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(dead_url.clone()),
        dead_url.clone(),
        dl_dir.clone(),
        "offline-cancel.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        3_000,
        trace,
    );
    let cancel_id = dl_cancel.id;
    repo.save(&dl_cancel, trace).await.unwrap();
    sup.start_download(dl_cancel, None, trace).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    sup.cancel_download(cancel_id, true, trace).await.unwrap();

    // 2c. Online restoration wakes offline freeze and resumes retry lifecycle cleanly
    let (dl_wake, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new(dead_url.clone()),
        dead_url,
        dl_dir.clone(),
        "offline-wake.bin".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        4_000,
        trace,
    );
    let wake_id = dl_wake.id;
    repo.save(&dl_wake, trace).await.unwrap();
    sup.start_download(dl_wake, None, trace).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Must still be frozen (not Failed!) while offline
    let mid = repo.find(wake_id, trace).await.unwrap().unwrap();
    assert_ne!(mid.state.as_slug(), "failed");

    // Flip internet back online -> offline freeze exits (`Ok(true)`) and proceeds to normal retry/fail
    online.store(true, Ordering::SeqCst);
    let woken = wait_download_state(&repo, wake_id, |s| s == "retrying" || s == "failed").await;
    assert!(matches!(woken.state.as_slug(), "retrying" | "failed"));

    sup.shutdown_gracefully(trace).await.unwrap();
}
