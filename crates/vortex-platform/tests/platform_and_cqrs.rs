//! Integration tests verifying `vortex-application` CQRS-lite use cases (`P1-T21`) and
//! `vortex-platform` Windows OS lifecycle & diagnostics (`P1-T22` & `P1-T23`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AddDownloadCommand, AppConfig, AppResult, ApplicationService, BrowserHandoffCommand,
    ByteSource, ByteStream, ConfigPort, DownloadFilterDto, DownloadRepository, EventBus,
    FallbackReason, FileSinkFactory, PlatformIntegration, PrepareAddInput, ProbeOutcome,
    ResourceInfo, ResourceRequest, SidecarMetadata, TransportMetrics,
};
use vortex_domain::{
    ByteRange, CategoryId, Download, DownloadId, DownloadState, FakeClock, Priority, QueueId,
    RangeSet, Resource, ResumeValidators,
};
use vortex_engine::{BroadcastEventBus, EngineSupervisor};
use vortex_error::TraceId;
use vortex_io::SparseFileSinkFactory;
use vortex_observability::{FlightEvent, FlightRecorder, SecretString, redact_sensitive_text};
use vortex_platform::{
    DiagnosticBundleBuilder, GracefulShutdownCoordinator, SingleInstanceGuard,
    SingleInstanceOutcome, WindowsPlatform, create_owner_only_pipe_server, layer_name,
    query_current_user_sid, run_startup_self_healing, spawn_connectivity_monitor,
    user_named_pipe_path,
};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteCategoryRepository, SqliteDatabase,
    SqliteDownloadRepository, SqliteEventLogRepository,
};

struct OneShotStream {
    payload: Option<Bytes>,
}

#[async_trait]
impl ByteStream for OneShotStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        Ok(self.payload.take())
    }

    fn transport_metrics(&self) -> TransportMetrics {
        TransportMetrics {
            rtt_us: Some(9_000),
            min_rtt_us: Some(8_500),
            is_proxied: false,
            reused_connection: true,
            is_http2: false,
        }
    }
}

struct MockSource {
    data: Bytes,
    probe_count: Mutex<u32>,
}

#[async_trait]
impl ByteSource for MockSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        *self.probe_count.lock().unwrap() += 1;
        if req.url.contains("/cloudflare-ja4") {
            return Ok(ProbeOutcome::FallbackToBrowser {
                reason: FallbackReason::WafTlsBound,
                status_code: 403,
                detail: "JA4 lock".to_owned(),
            });
        }
        if req.url.contains("/slow-2s") {
            tokio::time::sleep(Duration::from_millis(2_200)).await;
        }
        let info = ResourceInfo {
            final_url: req.url.clone(),
            total_size: Some(self.data.len() as u64),
            supports_range: true,
            mime_type: Some("application/x-iso9660-image".to_owned()),
            suggested_filename: None,
            raw_suggested_filename: None,
            validators: ResumeValidators::default(),
            server_checksum: None,
            transport_metrics: TransportMetrics::default(),
        };
        Ok(ProbeOutcome::Transferable {
            info: Box::new(info),
            initial_stream: Box::new(OneShotStream {
                payload: Some(self.data.clone()),
            }),
        })
    }

    async fn open_range(
        &self,
        _req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        let s = usize::try_from(range.start()).unwrap();
        let e = usize::try_from(range.end()).unwrap().min(self.data.len());
        Ok((
            Box::new(OneShotStream {
                payload: Some(self.data.slice(s..e)),
            }),
            TransportMetrics::default(),
        ))
    }
}

#[tokio::test]
async fn windows_sid_single_instance_mutex_owner_pipe_and_hkcu_registry_work() {
    assert_eq!(layer_name(), "vortex-platform");
    let trace = TraceId::new();

    // 1. Real Windows Owner SID (`S-1-5-...`)
    let sid = query_current_user_sid(trace).unwrap();
    assert!(sid.starts_with("S-1-5-"));
    let pipe_path = user_named_pipe_path(&sid);
    assert!(pipe_path.starts_with(r"\\.\pipe\vortex-core-S-1-5-"));

    // 2. Single-Instance Named Mutex: 1st = Primary, 2nd = SecondaryAlreadyRunning, after Drop = Primary again!
    let unique_suffix = format!("{sid}_{}", TraceId::new());
    let first = SingleInstanceGuard::try_acquire(&unique_suffix, trace).unwrap();
    assert!(matches!(first, SingleInstanceOutcome::Primary(_)));
    let second = SingleInstanceGuard::try_acquire(&unique_suffix, trace).unwrap();
    assert!(matches!(
        second,
        SingleInstanceOutcome::SecondaryAlreadyRunning
    ));
    drop(first);
    let reacquired = SingleInstanceGuard::try_acquire(&unique_suffix, trace).unwrap();
    assert!(matches!(reacquired, SingleInstanceOutcome::Primary(_)));
    drop(reacquired);

    // 3. Owner-SID DACL + `PIPE_REJECT_REMOTE_CLIENTS` Named Pipe round-trip!
    let test_pipe = format!(r"\\.\pipe\vortex-test-{}-{}", sid, TraceId::new());
    let server = create_owner_only_pipe_server(&test_pipe, &sid, true, trace).unwrap();
    let server_join = tokio::spawn(async move {
        server.connect().await.unwrap();
        let mut srv = server;
        let mut buf = [0_u8; 4];
        srv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"PING");
        srv.write_all(b"PONG").await.unwrap();
    });
    let mut client = ClientOptions::new().open(&test_pipe).unwrap();
    client.write_all(b"PING").await.unwrap();
    let mut pong = [0_u8; 4];
    client.read_exact(&mut pong).await.unwrap();
    assert_eq!(&pong, b"PONG");
    server_join.await.unwrap();

    // 4. `HKCU` Native Host registration under isolated test registry prefix + `SetThreadExecutionState`
    let dir = tempdir().unwrap();
    let fake_bridge = dir.path().join("vortex-bridge.exe");
    std::fs::write(&fake_bridge, b"MZ-stub").unwrap();
    let test_reg_prefix = format!(r"Software\VortexAutomatedTest\{}", TraceId::new());
    let platform = WindowsPlatform::new(dir.path().join("manifests"))
        .with_registry_prefix_for_test(&test_reg_prefix);

    let before = platform.check_native_hosts();
    assert!(before.iter().all(|b| !b.registered));

    let registered = platform
        .register_native_hosts(
            &fake_bridge,
            &["abcdefghijklmnopabcdefghijklmnop".to_owned()],
            trace,
        )
        .unwrap();
    assert_eq!(registered.len(), 3);
    assert!(registered.iter().all(|b| b.registered));

    let after = platform.check_native_hosts();
    assert!(after.iter().all(|b| b.registered));

    let sleep_guard = platform.acquire_sleep_inhibitor(trace).unwrap();
    drop(sleep_guard);

    // Clean up our isolated test `HKCU\Software\VortexAutomatedTest` subtree via `reg delete`
    let _ = std::process::Command::new("reg")
        .args(["delete", &format!(r"HKCU\{test_reg_prefix}"), "/f"])
        .output();
}

#[tokio::test]
async fn cqrs_use_cases_nucleo_search_handoff_budgets_undo_self_healing_and_zip_diagnostics() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let dl_dir = dir.path().join("downloads");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&dl_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let trace = TraceId::new();
    let clock = Arc::new(FakeClock::new(1_760_000_000_000));

    let (cfg_raw, _) = ConfigManager::load_or_init(&state_dir.join("config.toml"), trace).unwrap();
    let cfg = Arc::new(cfg_raw);
    cfg.update(
        AppConfig {
            default_download_dir: dl_dir.clone(),
            auto_start_handoff: true,
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
    let cat_repo = Arc::new(SqliteCategoryRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db.clone(), clock.clone()));
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let bus = Arc::new(BroadcastEventBus::default());

    let fake_bridge = dir.path().join("vortex-bridge.exe");
    std::fs::write(&fake_bridge, b"MZ").unwrap();
    let reg_prefix = format!(r"Software\VortexAutomatedTest\{}", TraceId::new());
    let platform = Arc::new(
        WindowsPlatform::new(dir.path().join("manifests"))
            .with_registry_prefix_for_test(&reg_prefix),
    );

    // 1. Test Startup Self-Healing: dirty `Downloading` row + orphan `.vortex.meta` + empty orphan `.vortex`!
    let (mut dirty_dl, _) = Download::new_pending(
        DownloadId::new(),
        Resource::new("https://cdn.example.com/crashed.iso"),
        "https://cdn.example.com/crashed.iso".to_owned(),
        dl_dir.clone(),
        "crashed.iso".to_owned(),
        CategoryId::default(),
        QueueId::main_queue(),
        Priority::Normal,
        None,
        100,
        trace,
    );
    dirty_dl.state = DownloadState::Downloading {
        active_workers: 4,
        speed_fast_bps: 1000.0,
        speed_slow_bps: 900.0,
        eta_secs: Some(10),
        supports_range: true,
    };
    dl_repo.save(&dirty_dl, trace).await.unwrap();

    let sidecar_id = DownloadId::new();
    let mut completed = RangeSet::new();
    completed.insert(ByteRange::new(0, 8192).unwrap());
    let sidecar = SidecarMetadata {
        version: 1,
        download_id: sidecar_id,
        url: "https://cdn.example.com/recovered.zip".to_owned(),
        final_url: None,
        file_name: "recovered.zip".to_owned(),
        save_dir: dl_dir.clone(),
        total_size: Some(65_536),
        supports_range: true,
        validators: ResumeValidators::default(),
        completed_ranges: completed,
        expected_hash: None,
        updated_at_ms: 500,
    };
    std::fs::write(dl_dir.join("recovered.zip.vortex"), vec![0_u8; 8192]).unwrap();
    std::fs::write(
        dl_dir.join("recovered.zip.vortex.meta"),
        serde_json::to_string(&sidecar).unwrap(),
    )
    .unwrap();
    std::fs::write(dl_dir.join("empty_dangling.bin.vortex"), b"").unwrap();

    let heal = run_startup_self_healing(
        dl_repo.as_ref(),
        std::slice::from_ref(&dl_dir),
        clock.as_ref(),
        trace,
    )
    .await
    .unwrap();
    assert_eq!(heal.reconciled_dirty_ids, vec![dirty_dl.id]);
    assert_eq!(heal.imported_sidecar_ids, vec![sidecar_id]);
    assert_eq!(heal.cleaned_orphan_files, 1);

    // 2. Wire `EngineSupervisor`, `DiagnosticBundleBuilder`, and `ApplicationService`
    let mock_source = Arc::new(MockSource {
        data: Bytes::from(vec![0x42_u8; 32_768]),
        probe_count: Mutex::new(0),
    });
    let supervisor = EngineSupervisor::start(
        mock_source.clone(),
        sink_factory.clone(),
        dl_repo.clone(),
        ev_repo.clone(),
        secrets.clone(),
        platform.clone(),
        cfg.clone(),
        bus.clone(),
        clock.clone(),
        None,
    );

    let recorder = FlightRecorder::new(500, None);
    let mut fields = BTreeMap::new();
    fields.insert("trace_id".to_owned(), trace.to_string());
    recorder.record(FlightEvent {
        timestamp: "2026-11-26T12:00:00Z".to_owned(),
        level: "INFO".to_owned(),
        target: "vortex::test".to_owned(),
        message: "Connected with Authorization: Bearer supersecret_jwt_999".to_owned(),
        fields,
    });
    std::fs::write(
        log_dir.join("core.jsonl"),
        r#"{"msg":"Cookie: session=leak_me_not","path":"C:\\Users\\PC\\Downloads\\secret.iso"}"#,
    )
    .unwrap();

    let pipe_flag = Arc::new(AtomicBool::new(true));
    let diag = Arc::new(DiagnosticBundleBuilder::new(
        cfg.clone(),
        dl_repo.clone(),
        Arc::new(db.clone()),
        sink_factory.space_ledger(),
        platform.clone(),
        recorder,
        None,
        log_dir,
        pipe_flag,
        clock.clone(),
    ));

    let app = ApplicationService::new(
        mock_source.clone(),
        supervisor.clone(),
        dl_repo.clone(),
        cat_repo,
        ev_repo,
        secrets,
        cfg.clone(),
        platform.clone(),
        diag,
        bus.clone(),
        clock.clone(),
    );

    // 3. `prepare_add_download` + `add_download(staged_id)` reuses warm stream (`probe_count` stays `1` == `0ms` start!)
    let preview = app
        .prepare_add_download(
            PrepareAddInput {
                url: "https://releases.ubuntu.com/ubuntu-26.04-desktop.iso?token=secret77"
                    .to_owned(),
                referrer: None,
                user_agent: None,
                cookies: Some(SecretString::from("sid=abc")),
                authorization: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert_eq!(*mock_source.probe_count.lock().unwrap(), 1);
    assert!(!preview.display_url.contains("secret77"));

    let staged_uuid = DownloadId::parse(&preview.staged_id).unwrap();
    let added = app
        .add_download(
            AddDownloadCommand {
                staged_id: Some(staged_uuid),
                url: String::new(),
                referrer: None,
                user_agent: None,
                headers: BTreeMap::new(),
                cookies: None,
                authorization: None,
                file_name: Some("محاضرة-الشبكات-2026.pdf".to_owned()),
                save_dir: Some(dl_dir.clone()),
                category_id: None,
                queue_id: None,
                priority: Priority::High,
                expected_hash: None,
                auto_start: true,
                overwrite_existing: false,
            },
            trace,
        )
        .await
        .unwrap();
    // Wait briefly for completion and verify NO second probe occurred (`0ms` reuse)!
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*mock_source.probe_count.lock().unwrap(), 1);

    // 4. `AHashMap` Duplicate check + `nucleo-matcher` SIMD fuzzy search (Arabic & Latin)!
    let dup = app
        .check_duplicate(
            "https://releases.ubuntu.com/ubuntu-26.04-desktop.iso?token=secret77",
            None,
            trace,
        )
        .await
        .unwrap();
    assert!(dup.is_some());

    let arabic_hits = app.search_downloads("الشبكات", trace).await.unwrap();
    assert_eq!(arabic_hits[0].file_name, "محاضرة-الشبكات-2026.pdf");
    let fuzzy_hits = app.search_downloads("rcvrdzp", trace).await.unwrap();
    assert!(fuzzy_hits.iter().any(|d| d.file_name == "recovered.zip"));
    let filtered = app
        .list_downloads(
            Some(DownloadFilterDto {
                state_slug: Some("completed".to_owned()),
                category_id: None,
                queue_id: None,
            }),
            trace,
        )
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);

    // 5. `handoff_from_browser`: WAF JA4 `403` + `> 1400ms` Timeout -> `fallback_to_browser: true`, normal -> `accepted: true`!
    let waf_ack = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://protected.example.com/cloudflare-ja4".to_owned(),
                final_url: None,
                referrer: None,
                suggested_name: Some("protected.zip".to_owned()),
                mime_type: None,
                size_bytes: Some(1_048_576),
                user_agent: None,
                cookies: Some(SecretString::from("cf_clearance=xyz")),
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: true,
                top_level_site: Some("https://protected.example.com".to_owned()),
            },
            trace,
        )
        .await
        .unwrap();
    assert!(!waf_ack.accepted);
    assert!(waf_ack.fallback_to_browser);
    assert_eq!(waf_ack.fallback_reason.as_deref(), Some("waf_tls_bound"));

    let timeout_ack = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://slow.example.com/slow-2s".to_owned(),
                final_url: None,
                referrer: None,
                suggested_name: Some("slow.zip".to_owned()),
                mime_type: None,
                size_bytes: Some(2_097_152),
                user_agent: None,
                cookies: None,
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: true,
                top_level_site: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert!(!timeout_ack.accepted);
    assert!(timeout_ack.fallback_to_browser);
    assert_eq!(
        timeout_ack.fallback_reason.as_deref(),
        Some("probe_timeout")
    );

    // 6. `remove_download` + `undo_remove_download` within 6s + permanent purge after 6s!
    let added_id = DownloadId::parse(&added.id).unwrap();
    app.remove_download(added_id, false, trace).await.unwrap();
    assert!(dl_repo.find(added_id, trace).await.unwrap().is_none());
    let restored = app.undo_remove_download(added_id, trace).await.unwrap();
    assert_eq!(restored.file_name, "محاضرة-الشبكات-2026.pdf");

    app.remove_download(sidecar_id, true, trace).await.unwrap();
    clock.advance_ms(7_000);
    assert_eq!(app.purge_expired_deletions(trace).await, 1);

    // 7. Connectivity monitor + Self-Health + Zero-Leakage `.zip` Diagnostic Bundle verified by PowerShell `Expand-Archive`!
    let mut sub = bus.subscribe();
    let mon_cancel = CancellationToken::new();
    let _mon = spawn_connectivity_monitor(
        platform.online_flag(),
        Arc::new(|| false),
        bus.clone(),
        Duration::from_millis(100),
        mon_cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(160)).await;
    assert!(!platform.is_internet_available());
    assert!(sub.try_recv().is_ok());
    mon_cancel.cancel();

    let _ = app
        .repair_browser_integration(&fake_bridge, &["ext123".to_owned()], trace)
        .unwrap();
    app.set_debug_mode(true, trace).unwrap();
    let health = app.get_health_status(trace).await.unwrap();
    assert!(health.core_ok && health.db_ok && health.pipe_ok && health.debug_mode_active);
    assert_eq!(app.recent_logs(10).len(), 1);

    let preview_bundle = app.get_diagnostic_summary(trace).await.unwrap();
    assert!(preview_bundle.total_zip_bytes > 100);

    let zip_path = dir.path().join("vortex-diag.zip");
    let exported = app
        .export_diagnostic_bundle(&zip_path, trace)
        .await
        .unwrap();
    assert!(zip_path.exists());
    assert_eq!(exported.files.len(), 5);

    // Extract generated `.zip` using Windows `Expand-Archive` to prove 100% standard PKZIP validity AND Zero-Leakage!
    let unpack_dir = dir.path().join("unpacked_zip");
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                zip_path.display(),
                unpack_dir.display()
            ),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "Windows Expand-Archive must extract our PKZIP cleanly"
    );

    for rel in [
        "system_info.json",
        "config_redacted.json",
        "downloads_summary.json",
        "logs/flight_recorder.jsonl",
        "logs/core.jsonl",
    ] {
        let content = std::fs::read_to_string(unpack_dir.join(Path::new(rel))).unwrap();
        assert!(
            !content.contains("supersecret_jwt_999")
                && !content.contains("leak_me_not")
                && !content.contains("secret77")
                && redact_sensitive_text(&content) == content,
            "Zero-Leakage invariant violated in {rel}!"
        );
    }

    // 8. Graceful Shutdown Coordinator (`<= 5s` flush + `PRAGMA wal_checkpoint(TRUNCATE)`)
    let coordinator =
        GracefulShutdownCoordinator::new(supervisor, Arc::new(db), CancellationToken::new());
    coordinator.execute(trace).await.unwrap();

    let _ = std::process::Command::new("reg")
        .args(["delete", &format!(r"HKCU\{reg_prefix}"), "/f"])
        .output();
}

struct MultiHeaderMockSource {
    zip: Bytes,
    html: Bytes,
    pe: Bytes,
}

#[async_trait]
impl ByteSource for MultiHeaderMockSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        let (payload, mime) = if req.url.contains("/fake-html-trap") {
            (self.html.clone(), "text/html; charset=utf-8")
        } else if req.url.contains("/masquerade-exe") {
            (self.pe.clone(), "video/mp4")
        } else {
            (self.zip.clone(), "application/zip")
        };
        let info = ResourceInfo {
            final_url: req.url.clone(),
            total_size: Some(payload.len() as u64),
            supports_range: true,
            mime_type: Some(mime.to_owned()),
            suggested_filename: None,
            raw_suggested_filename: None,
            validators: ResumeValidators::default(),
            server_checksum: None,
            transport_metrics: TransportMetrics::default(),
        };
        Ok(ProbeOutcome::Transferable {
            info: Box::new(info),
            initial_stream: Box::new(OneShotStream {
                payload: Some(payload),
            }),
        })
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        let payload = if req.url.contains("/fake-html-trap") {
            &self.html
        } else if req.url.contains("/masquerade-exe") {
            &self.pe
        } else {
            &self.zip
        };
        let s = usize::try_from(range.start()).unwrap();
        let e = usize::try_from(range.end()).unwrap().min(payload.len());
        Ok((
            Box::new(OneShotStream {
                payload: Some(payload.slice(s..e)),
            }),
            TransportMetrics::default(),
        ))
    }
}

#[tokio::test]
async fn phase_1_5_smart_capture_magic_bytes_burst_guard_and_discard_invariants() {
    use vortex_application::MAX_STAGED_PROBES;
    use vortex_domain::{RiskLevel, SecurityWarningKind};

    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let dl_dir = dir.path().join("downloads");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&dl_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let trace = TraceId::new();
    let clock = Arc::new(FakeClock::new(1_760_000_000_000));

    let (cfg_raw, _) = ConfigManager::load_or_init(&state_dir.join("config.toml"), trace).unwrap();
    let cfg = Arc::new(cfg_raw);
    // Keep `auto_start_handoff: false` (default safe confirmation-first policy!)
    cfg.update(
        AppConfig {
            default_download_dir: dl_dir.clone(),
            auto_start_handoff: false,
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
    let cat_repo = Arc::new(SqliteCategoryRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db.clone(), clock.clone()));
    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let bus = Arc::new(BroadcastEventBus::default());
    let platform = Arc::new(WindowsPlatform::new(dir.path().join("manifests")));

    let mut zip_bytes = vec![0x50_u8, 0x4B, 0x03, 0x04];
    zip_bytes.extend((0..16_380_u32).map(|i| (i % 251) as u8));

    let mock_source = Arc::new(MultiHeaderMockSource {
        zip: Bytes::from(zip_bytes.clone()),
        html: Bytes::from_static(
            b"<!DOCTYPE html><html><body>Fake ad countdown page</body></html>",
        ),
        pe: Bytes::from_static(
            b"MZ\x90\x00\x03\x00\x00\x00This program cannot be run in DOS mode!",
        ),
    });

    let supervisor = EngineSupervisor::start(
        mock_source.clone(),
        sink_factory.clone(),
        dl_repo.clone(),
        ev_repo.clone(),
        secrets.clone(),
        platform.clone(),
        cfg.clone(),
        bus.clone(),
        clock.clone(),
        None,
    );

    let diag = Arc::new(DiagnosticBundleBuilder::new(
        cfg.clone(),
        dl_repo.clone(),
        Arc::new(db.clone()),
        sink_factory.space_ledger(),
        platform.clone(),
        FlightRecorder::new(100, None),
        None,
        log_dir,
        Arc::new(AtomicBool::new(true)),
        clock.clone(),
    ));

    let app = ApplicationService::new(
        mock_source,
        supervisor.clone(),
        dl_repo.clone(),
        cat_repo,
        ev_repo,
        secrets,
        cfg.clone(),
        platform.clone(),
        diag,
        bus,
        clock.clone(),
    );

    // 1. Safe ZIP capture with `auto_start: false` -> staged cleanly (0 bytes written to disk!), VerifiedMatch!
    let zip_ack = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://cdn.trusted.example/archive.zip".to_owned(),
                final_url: None,
                referrer: Some("https://trusted.example/releases".to_owned()),
                suggested_name: Some("archive.zip".to_owned()),
                mime_type: Some("application/zip".to_owned()),
                size_bytes: Some(zip_bytes.len() as u64),
                user_agent: None,
                cookies: None,
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: false,
                top_level_site: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert!(zip_ack.accepted);
    assert_eq!(dl_repo.list_all(trace).await.unwrap().len(), 0);
    assert!(app.free_disk_bytes_for_dir(&dl_dir, trace).unwrap_or(0) > 0);
    assert!(
        platform
            .reveal_in_file_manager(&dl_dir.join("missing.bin"), trace)
            .is_err()
    );
    assert!(
        platform
            .open_file_safely(&dl_dir.join("missing.bin"), trace)
            .is_err()
    );

    let staged = app.list_staged_downloads().await;
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].security.risk_level, RiskLevel::VerifiedMatch);
    assert_eq!(
        staged[0].security.detected_signature.as_deref(),
        Some("ZIP")
    );
    assert!(!staged[0].burst_quarantined);

    // Confirming the staged ZIP reuses the peeked `PrependByteStream` with ZERO dropped bytes (`100%` SHA-256 match)!
    let zip_staged_id = DownloadId::parse(&staged[0].staged_id).unwrap();
    let confirmed = app
        .add_download(
            AddDownloadCommand {
                staged_id: Some(zip_staged_id),
                url: staged[0].url.clone(),
                referrer: None,
                user_agent: None,
                headers: BTreeMap::new(),
                cookies: None,
                authorization: None,
                file_name: Some("verified-archive.zip".to_owned()),
                save_dir: Some(dl_dir.clone()),
                category_id: None,
                queue_id: None,
                priority: Priority::Normal,
                expected_hash: None,
                auto_start: true,
                overwrite_existing: false,
            },
            trace,
        )
        .await
        .unwrap();
    assert_eq!(confirmed.file_name, "verified-archive.zip");
    assert_eq!(app.list_staged_downloads().await.len(), 0);

    // Wait for completion and verify exact byte-for-byte SHA-256!
    let mut completed_bytes = None;
    for _ in 0..40 {
        let p = dl_dir.join("verified-archive.zip");
        if p.exists() && std::fs::metadata(&p).unwrap().len() == zip_bytes.len() as u64 {
            completed_bytes = Some(std::fs::read(&p).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let disk_bytes = completed_bytes.expect("confirmed staged download must complete on disk");
    assert_eq!(disk_bytes, zip_bytes);

    // 2. Ironclad Rule: Even when `cmd.auto_start = true`, an MZ PE masquerading as `.mp4` OR an HTML trap
    // MUST NEVER auto-start; both are forced into staged confirmation with Danger / Caution shields!
    clock.advance_ms(5_000);
    let pe_ack = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://ad-drop.evil/masquerade-exe/movie.mp4".to_owned(),
                final_url: None,
                referrer: Some("https://free-films.example/watch".to_owned()),
                suggested_name: Some("movie.mp4".to_owned()),
                mime_type: Some("video/mp4".to_owned()),
                size_bytes: Some(64),
                user_agent: None,
                cookies: None,
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: true, // Attempted silent start MUST be blocked!
                top_level_site: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert!(pe_ack.accepted);

    let html_ack = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://ad-drop.evil/fake-html-trap/pack.zip".to_owned(),
                final_url: None,
                referrer: Some("https://free-films.example/watch".to_owned()),
                suggested_name: Some("pack.zip".to_owned()),
                mime_type: Some("text/html".to_owned()),
                size_bytes: Some(64),
                user_agent: None,
                cookies: None,
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: true, // Attempted silent start MUST be blocked!
                top_level_site: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert!(html_ack.accepted);

    let staged_threats = app.list_staged_downloads().await;
    assert_eq!(staged_threats.len(), 2);
    assert_eq!(staged_threats[0].security.risk_level, RiskLevel::Danger);
    assert!(staged_threats[0].security.block_enter_shortcut);
    assert!(
        staged_threats[0]
            .security
            .warnings
            .contains(&SecurityWarningKind::ExecutableMasquerade)
    );
    assert_eq!(staged_threats[1].security.risk_level, RiskLevel::Caution);
    assert!(
        staged_threats[1]
            .security
            .warnings
            .contains(&SecurityWarningKind::HtmlTrap)
    );
    // 2nd rapid capture within 4s window is flagged as burst-quarantined!
    assert!(staged_threats[1].burst_quarantined);

    // 3. One-click `block_capture_host_and_discard` adds `ad-drop.evil` to `excluded_hosts` AND purges BOTH matching captures!
    let pe_id = DownloadId::parse(&staged_threats[0].staged_id).unwrap();
    let updated_cfg = app
        .block_capture_host_and_discard(pe_id, "ad-drop.evil".to_owned(), trace)
        .await
        .unwrap();
    assert!(
        updated_cfg
            .capture_rules
            .excluded_hosts
            .iter()
            .any(|h| h == "ad-drop.evil")
    );
    assert_eq!(app.list_staged_downloads().await.len(), 0);

    // 4. Ad-Burst Storm Cap: pushing `MAX_STAGED_PROBES + 1` rapid captures accepts `MAX_STAGED_PROBES` and rejects the 9th!
    for i in 0..MAX_STAGED_PROBES {
        let ack = app
            .handoff_from_browser(
                BrowserHandoffCommand {
                    url: format!("https://spam.example/file-{i}.zip"),
                    final_url: None,
                    referrer: None,
                    suggested_name: Some(format!("file-{i}.zip")),
                    mime_type: Some("application/zip".to_owned()),
                    size_bytes: Some(16_384),
                    user_agent: None,
                    cookies: None,
                    headers: BTreeMap::new(),
                    incognito: false,
                    source: "determining-filename".to_owned(),
                    auto_start: false,
                    top_level_site: None,
                },
                trace,
            )
            .await
            .unwrap();
        assert!(
            ack.accepted,
            "probe {i} should fit inside MAX_STAGED_PROBES"
        );
    }
    let overflow = app
        .handoff_from_browser(
            BrowserHandoffCommand {
                url: "https://spam.example/file-overflow.zip".to_owned(),
                final_url: None,
                referrer: None,
                suggested_name: Some("file-overflow.zip".to_owned()),
                mime_type: Some("application/zip".to_owned()),
                size_bytes: Some(16_384),
                user_agent: None,
                cookies: None,
                headers: BTreeMap::new(),
                incognito: false,
                source: "determining-filename".to_owned(),
                auto_start: false,
                top_level_site: None,
            },
            trace,
        )
        .await
        .unwrap();
    assert!(!overflow.accepted);
    assert!(!overflow.fallback_to_browser);
    assert_eq!(
        overflow.fallback_reason.as_deref(),
        Some("burst_limit_reached")
    );

    // Discard one, then discard all!
    let all_burst = app.list_staged_downloads().await;
    assert_eq!(all_burst.len(), MAX_STAGED_PROBES);
    let first_id = DownloadId::parse(&all_burst[0].staged_id).unwrap();
    assert!(app.discard_staged_download(first_id, trace).await.unwrap());
    assert_eq!(
        app.list_staged_downloads().await.len(),
        MAX_STAGED_PROBES - 1
    );
    assert_eq!(
        app.discard_all_staged_downloads(trace).await.unwrap(),
        MAX_STAGED_PROBES - 1
    );
    assert_eq!(app.list_staged_downloads().await.len(), 0);

    let coordinator =
        GracefulShutdownCoordinator::new(supervisor, Arc::new(db), CancellationToken::new());
    coordinator.execute(trace).await.unwrap();
}

#[cfg(windows)]
#[test]
fn owner_only_dir_acl_hardens_data_directory_th31() {
    let dir = tempdir().unwrap();
    let protected = dir.path().join("vortex_protected_data");
    let trace = TraceId::new();
    let platform = WindowsPlatform::new(dir.path().join("manifests"));
    platform
        .enforce_owner_only_dir_acl(&protected, trace)
        .unwrap();
    assert!(protected.exists());
    // Must allow the owner process to read/write files inside the ACL-protected directory
    let probe_file = protected.join("probe.txt");
    std::fs::write(&probe_file, b"ok").unwrap();
    assert_eq!(std::fs::read(&probe_file).unwrap(), b"ok");
}
