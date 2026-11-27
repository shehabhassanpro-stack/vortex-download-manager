//! Automated Phase 1 Performance, Memory, Sparse-I/O, and Throughput Budget Suite (`P1-T36` & Exit Gate).
//!
//! Quantitatively enforces all non-negotiable MVP budgets (`Vortex-Architecture-Design.md` §18 & `Tasks.md`):
//! 1. **20 GiB Sparse File Tail Write `< 50ms`** + physical allocation proof (`Zero NTFS Zero-Fill`).
//! 2. **10 GiB+ Multi-Segment Positional Sparse Download** + non-fatal `SetSparse = FALSE` finalization (`Exit Gate #1`).
//! 3. **`0ms` Add-Download Stream Start Waste** (`prepare_add_download` -> `add_download` single HTTP probe) + `< 80ms` warm staging.
//! 4. **`>= 95%` Multi-Connection Throughput Efficiency** against per-connection throttled `TestLabServer`.
//! 5. **Cold Start `< 300ms`** (`vortex-desktop.exe --headless-core` to live Named Pipe `HealthCheckResp`).
//! 6. **Idle RAM `< 60 MB`** (`WorkingSetSize` & `PrivateUsage` via Win32 `K32GetProcessMemoryInfo` with `WebView2` destroyed).
//! 7. **Thin Native Messaging Host `vortex-bridge.exe < 1 MB`** (when release binary is built).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    unsafe_code
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use vortex_application::{
    AddDownloadCommand, AppEvent, AppResult, ApplicationService, ByteSource, ByteStream,
    ConfigPort, EngineControlPort, EventBus, FileSinkFactory, PrepareAddInput, ProbeOutcome,
    ResourceRequest, SidecarMetadata, TransportMetrics,
};
use vortex_domain::{
    ByteRange, Checksum, DomainEvent, DownloadId, HashAlgorithm, Priority, RangeSet,
    ResumeValidators, SystemClock,
};
use vortex_engine::{BroadcastEventBus, EngineSupervisor};
use vortex_error::TraceId;
use vortex_io::SparseFileSinkFactory;
use vortex_net::{HttpByteSource, HttpClientConfig};
use vortex_observability::FlightRecorder;
use vortex_platform::{DiagnosticBundleBuilder, WindowsPlatform};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteCategoryRepository, SqliteDatabase,
    SqliteDownloadRepository, SqliteEventLogRepository,
};
use vortex_test_lab::{TestLabServer, deterministic_bytes, deterministic_sha256};

const ONE_MIB: usize = 1024 * 1024;
const TEN_GIB_PLUS: u64 = 10 * 1024 * 1024 * 1024 + 4 * 1024 * 1024;
const TWENTY_GIB: u64 = 20 * 1024 * 1024 * 1024;

#[cfg(windows)]
fn query_allocated_disk_bytes(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut high: u32 = 0;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; `high` points to stack `u32`.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    (u64::from(high) << 32) | u64::from(low)
}

#[tokio::test]
async fn sparse_20gb_tail_write_under_50ms_and_10gb_positional_download_exit_gate() {
    let dir = TempDir::new().unwrap();
    let factory = SparseFileSinkFactory::default();
    let tid = TraceId::new();

    // 1. 20 GiB Sparse File Tail Write `< 50ms` + Zero NTFS Zero-Fill Proof (`P1-T36`)
    let temp_20g = dir.path().join("sparse-20gb.bin.vortex");
    let meta_20g = dir.path().join("sparse-20gb.bin.vortex.meta");
    let payload_1m = Bytes::from(deterministic_bytes(
        2026,
        TWENTY_GIB - ONE_MIB as u64,
        ONE_MIB,
    ));

    let t0 = Instant::now();
    let sink_20g = factory.open(&temp_20g, &meta_20g, tid).await.unwrap();
    let sparse_active = sink_20g.prepare(Some(TWENTY_GIB), tid).await.unwrap();
    sink_20g
        .write_at(TWENTY_GIB - ONE_MIB as u64, payload_1m.clone(), tid)
        .await
        .unwrap();
    let tail_write_elapsed = t0.elapsed();

    assert!(
        tail_write_elapsed < Duration::from_millis(50),
        "20 GiB sparse tail write exceeded 50ms budget: {tail_write_elapsed:?}"
    );

    #[cfg(windows)]
    if sparse_active {
        let final_20g = dir.path().join("sparse-20gb.bin");
        let logical_size = sink_20g.finalize(&final_20g, tid).await.unwrap();
        assert_eq!(logical_size, TWENTY_GIB);

        let physical_allocated = query_allocated_disk_bytes(&final_20g);
        assert!(
            physical_allocated <= 16 * 1024 * 1024,
            "NTFS Zero-Fill regression: physical allocation is {physical_allocated} bytes for 1 MiB tail write on 20 GiB sparse file"
        );
        let _ = std::fs::remove_file(&final_20g);
    }

    // 2. 10 GiB+ Positional Multi-Segment Sparse Download + Durable `.vortex.meta` + `SetSparse=FALSE` (`Exit Gate #1`)
    let temp_10g = dir.path().join("ubuntu-10gb.iso.vortex");
    let meta_10g = dir.path().join("ubuntu-10gb.iso.vortex.meta");
    let final_10g = dir.path().join("ubuntu-10gb.iso");

    let head_bytes = Bytes::from(deterministic_bytes(77, 0, ONE_MIB));
    let mid_offset = 5 * 1024 * 1024 * 1024_u64;
    let mid_bytes = Bytes::from(deterministic_bytes(77, mid_offset, ONE_MIB));
    let tail_offset = TEN_GIB_PLUS - ONE_MIB as u64;
    let tail_bytes = Bytes::from(deterministic_bytes(77, tail_offset, ONE_MIB));

    let sink_10g = factory.open(&temp_10g, &meta_10g, tid).await.unwrap();
    let _ = sink_10g.prepare(Some(TEN_GIB_PLUS), tid).await.unwrap();

    // Out-of-order parallel segment writes (tail -> middle -> head) with ZERO assembly step!
    sink_10g
        .write_at(tail_offset, tail_bytes.clone(), tid)
        .await
        .unwrap();
    sink_10g
        .write_at(mid_offset, mid_bytes.clone(), tid)
        .await
        .unwrap();
    sink_10g.write_at(0, head_bytes.clone(), tid).await.unwrap();

    let mut completed = RangeSet::new();
    completed.insert(ByteRange::new(0, ONE_MIB as u64).unwrap());
    completed.insert(ByteRange::new(mid_offset, mid_offset + ONE_MIB as u64).unwrap());
    completed.insert(ByteRange::new(tail_offset, TEN_GIB_PLUS).unwrap());

    let sidecar = SidecarMetadata {
        version: 1,
        download_id: DownloadId::new(),
        url: "https://cdn.example.com/ubuntu-10gb.iso".to_owned(),
        final_url: None,
        file_name: "ubuntu-10gb.iso".to_owned(),
        save_dir: dir.path().to_path_buf(),
        total_size: Some(TEN_GIB_PLUS),
        supports_range: true,
        validators: ResumeValidators::default(),
        completed_ranges: completed,
        expected_hash: None,
        updated_at_ms: 1_764_000_000_000,
    };
    sink_10g.flush_durable(&sidecar, tid).await.unwrap();
    assert!(meta_10g.exists(), "durable .vortex.meta sidecar must exist");

    // Verify exact positional reads before finalization
    assert_eq!(
        sink_10g.read_at(0, 4096, tid).await.unwrap().as_ref(),
        &head_bytes[..4096]
    );
    assert_eq!(
        sink_10g
            .read_at(mid_offset, 4096, tid)
            .await
            .unwrap()
            .as_ref(),
        &mid_bytes[..4096]
    );
    assert_eq!(
        sink_10g
            .read_at(tail_offset, 4096, tid)
            .await
            .unwrap()
            .as_ref(),
        &tail_bytes[..4096]
    );

    let final_len = sink_10g.finalize(&final_10g, tid).await.unwrap();
    assert_eq!(final_len, TEN_GIB_PLUS);
    assert!(final_10g.exists());
    assert!(!meta_10g.exists(), ".vortex.meta cleaned up on finalize");
    let _ = std::fs::remove_file(&final_10g);
}

#[derive(Clone)]
struct SpyByteSource {
    inner: Arc<HttpByteSource>,
    probe_calls: Arc<AtomicUsize>,
    range_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ByteSource for SpyByteSource {
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.probe_and_stream(req).await
    }

    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)> {
        self.range_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.open_range(req, range).await
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

#[tokio::test]
async fn zero_ms_add_download_stream_start_and_95_percent_test_lab_throughput() {
    let lab = TestLabServer::spawn().await.unwrap();
    let data_dir = TempDir::new().unwrap();
    let dl_dir = data_dir.path().join("downloads");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let tid = TraceId::new();

    let clock = Arc::new(SystemClock::new());
    let (cfg_raw, _) =
        ConfigManager::load_or_init(&data_dir.path().join("config.toml"), tid).unwrap();
    let cfg_mgr = Arc::new(cfg_raw);
    let mut updated_cfg = (*cfg_mgr.current()).clone();
    updated_cfg.default_download_dir.clone_from(&dl_dir);
    updated_cfg.max_connections_per_download = 8;
    updated_cfg.min_free_disk_bytes = 64 * 1024 * 1024;
    let _ = cfg_mgr.update(updated_cfg, tid).await.unwrap();

    let (db, _) = SqliteDatabase::open_with_self_healing(
        &data_dir.path().join("vortex.db"),
        std::slice::from_ref(&dl_dir),
        &dl_dir,
        clock.clone(),
        tid,
    )
    .unwrap();

    let dl_repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let cat_repo = Arc::new(SqliteCategoryRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db.clone(), clock.clone()));
    let platform = Arc::new(WindowsPlatform::new(data_dir.path().join("manifests")));
    let sink_factory = Arc::new(SparseFileSinkFactory::default());

    let http = Arc::new(
        HttpByteSource::new(clock.clone(), HttpClientConfig::for_local_testing()).unwrap(),
    );
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let range_calls = Arc::new(AtomicUsize::new(0));
    let spy = Arc::new(SpyByteSource {
        inner: http,
        probe_calls: Arc::clone(&probe_calls),
        range_calls: Arc::clone(&range_calls),
    });

    let event_bus = Arc::new(BroadcastEventBus::default());
    let bus_dyn: Arc<dyn EventBus> = event_bus.clone();

    let supervisor = EngineSupervisor::start(
        spy.clone(),
        sink_factory.clone(),
        dl_repo.clone(),
        ev_repo.clone(),
        secrets.clone(),
        platform.clone(),
        cfg_mgr.clone(),
        bus_dyn.clone(),
        clock.clone(),
        None,
    );

    let diagnostics = Arc::new(DiagnosticBundleBuilder::new(
        cfg_mgr.clone(),
        dl_repo.clone(),
        Arc::new(db),
        sink_factory.space_ledger(),
        platform.clone(),
        FlightRecorder::new(500, None),
        None,
        data_dir.path().join("logs"),
        Arc::new(AtomicBool::new(true)),
        clock.clone(),
    ));

    let app = ApplicationService::new(
        spy,
        supervisor.clone(),
        dl_repo,
        cat_repo,
        ev_repo,
        secrets,
        cfg_mgr,
        platform,
        diagnostics,
        bus_dyn.clone(),
        clock,
    );

    // ------------------------------------------------------------------------
    // Part A: `0ms` Add-Download Stream Start Waste + `< 80ms` Warm Dialog Staging (`P1-T36`)
    // ------------------------------------------------------------------------
    let warm_size = 4 * 1024 * 1024_u64;
    let warm_url = format!("{}/files/{warm_size}?seed=314", lab.base_url());
    let mut rx = bus_dyn.subscribe();

    let stage_t0 = Instant::now();
    let preview = app
        .prepare_add_download(
            PrepareAddInput {
                url: warm_url.clone(),
                referrer: None,
                user_agent: None,
                cookies: None,
                authorization: None,
            },
            tid,
        )
        .await
        .unwrap();
    let stage_elapsed = stage_t0.elapsed();
    assert!(
        stage_elapsed < Duration::from_millis(80),
        "Warm Add-Download probe staging exceeded 80ms budget: {stage_elapsed:?}"
    );
    assert_eq!(probe_calls.load(Ordering::SeqCst), 1);

    let added = app
        .add_download(
            AddDownloadCommand {
                staged_id: Some(preview.staged_id.parse().unwrap()),
                url: warm_url,
                referrer: None,
                user_agent: None,
                headers: std::collections::BTreeMap::new(),
                cookies: None,
                authorization: None,
                save_dir: Some(dl_dir.clone()),
                file_name: Some("warm-zero-waste.bin".to_owned()),
                category_id: None,
                queue_id: None,
                priority: Priority::Normal,
                expected_hash: Some(Checksum {
                    algorithm: HashAlgorithm::Sha256,
                    hex_digest: deterministic_sha256(314, warm_size),
                }),
                auto_start: true,
                overwrite_existing: true,
            },
            tid,
        )
        .await
        .unwrap();

    let warm_id: DownloadId = added.id.parse().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(ev) = rx.recv().await {
            if let AppEvent::Domain(env) = ev
                && env.download_id == warm_id
                && matches!(env.event, DomainEvent::DownloadCompleted { .. })
            {
                break;
            }
        }
    })
    .await
    .expect("warm staged download must complete");

    // Strictly ONE probe (`0` duplicate HTTP probes -> `0ms` wasted start!)
    assert_eq!(
        probe_calls.load(Ordering::SeqCst),
        1,
        "add_download with staged_id must reuse the warm 16 MiB stream with ZERO extra HTTP probes!"
    );
    assert_eq!(
        sha256_file(&dl_dir.join("warm-zero-waste.bin")),
        deterministic_sha256(314, warm_size)
    );

    // ------------------------------------------------------------------------
    // Part B: `>= 95%` Test Lab Multi-Connection Throughput Efficiency (`P1-T36`)
    // ------------------------------------------------------------------------
    // 64 MiB file across 8 parallel workers throttled at 2 MiB/s per connection -> 16 MiB/s aggregate ceiling (`4.000s` ideal).
    // Each worker claims an exact 8 MiB segment (`3 MiB` Lease #1 + `5 MiB` warm Keep-Alive Lease #2).
    let bench_size: u64 = 64 * 1024 * 1024;
    let per_conn_bps: u64 = 2 * 1024 * 1024;
    let worker_count: u64 = 8;
    let aggregate_ceiling_bps = (per_conn_bps * worker_count) as f64;
    let bench_url = format!(
        "{}/throttled/{bench_size}?bps={per_conn_bps}&seed=909&name=throughput-bench.bin",
        lab.base_url()
    );

    let range_before = range_calls.load(Ordering::SeqCst);
    let transfer_t0 = Instant::now();
    let bench_summary = app
        .add_download(
            AddDownloadCommand {
                staged_id: None,
                url: bench_url,
                referrer: None,
                user_agent: None,
                headers: std::collections::BTreeMap::new(),
                cookies: None,
                authorization: None,
                save_dir: Some(dl_dir.clone()),
                file_name: Some("throughput-bench.bin".to_owned()),
                category_id: None,
                queue_id: None,
                priority: Priority::High,
                expected_hash: None,
                auto_start: true,
                overwrite_existing: true,
            },
            tid,
        )
        .await
        .unwrap();
    let bench_id: DownloadId = bench_summary.id.parse().unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(ev) = rx.recv().await {
            if let AppEvent::Domain(env) = ev
                && env.download_id == bench_id
                && matches!(env.event, DomainEvent::DownloadCompleted { .. })
            {
                break;
            }
        }
    })
    .await
    .expect("throttled multi-connection download must complete");
    let transfer_secs = transfer_t0.elapsed().as_secs_f64();

    let range_used = range_calls.load(Ordering::SeqCst) - range_before;
    assert!(
        range_used >= 7,
        "expected parallel worker range leases (got {range_used})"
    );

    assert_eq!(
        sha256_file(&dl_dir.join("throughput-bench.bin")),
        deterministic_sha256(909, bench_size)
    );

    let achieved_bps = (bench_size as f64) / transfer_secs;
    let efficiency = achieved_bps / aggregate_ceiling_bps;
    assert!(
        efficiency >= 0.95,
        "Multi-connection throughput efficiency {:.2}% ({:.2} MiB/s vs {:.2} MiB/s ceiling) fell below 95% budget!",
        efficiency * 100.0,
        achieved_bps / (1024.0 * 1024.0),
        aggregate_ceiling_bps / (1024.0 * 1024.0),
    );

    let _ = supervisor.shutdown_gracefully(tid).await;
    lab.shutdown();
}

#[cfg(windows)]
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[cfg(windows)]
fn locate_compiled_exe(bin_name: &str) -> Option<PathBuf> {
    let root = workspace_root();
    let rel_release = root.join("target").join("release").join(bin_name);
    if rel_release.exists() {
        return Some(rel_release);
    }
    let rel_debug = root.join("target").join("debug").join(bin_name);
    if rel_debug.exists() {
        return Some(rel_debug);
    }
    None
}

#[cfg(windows)]
fn query_process_memory_bytes(pid: u32) -> (usize, usize) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: Queries memory counters of our own child process `pid`.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        assert!(!handle.is_null(), "OpenProcess failed for child pid {pid}");

        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap();
        let ok = K32GetProcessMemoryInfo(handle, (&raw mut counters).cast(), counters.cb);
        let _ = CloseHandle(handle);
        assert!(ok != 0, "K32GetProcessMemoryInfo failed");
        (counters.WorkingSetSize, counters.PrivateUsage)
    }
}

#[cfg(windows)]
#[test]
fn cold_start_under_300ms_idle_ram_under_60mb_and_bridge_under_1mb() {
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};
    use vortex_ipc::{PROTOCOL_VERSION, SyncPipeClient, proto};
    use vortex_platform::user_named_pipe_path;

    let root = workspace_root();
    let release_bridge = root
        .join("target")
        .join("release")
        .join("vortex-bridge.exe");
    if release_bridge.exists() {
        let bridge_bytes = std::fs::metadata(&release_bridge).unwrap().len();
        assert!(
            bridge_bytes < 1_000_000,
            "vortex-bridge.exe release binary ({bridge_bytes} bytes) exceeded 1 MB ceiling!"
        );
    }

    let desktop_exe = locate_compiled_exe("vortex-desktop.exe").unwrap_or_else(|| {
        let status = Command::new("cargo")
            .args(["build", "-p", "vortex-desktop"])
            .current_dir(&root)
            .status()
            .expect("cargo build vortex-desktop");
        assert!(status.success());
        locate_compiled_exe("vortex-desktop.exe").expect("vortex-desktop.exe")
    });

    let sandbox = TempDir::new().unwrap();
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let test_sid = format!("S-1-5-21-BUDGET-{now_ns}");
    let pipe_path = user_named_pipe_path(&test_sid);
    let tid = TraceId::new();

    // Measure true Cold Start (`spawn` -> Named Pipe `Hello` -> `HealthCheckResp { core_ok: true }`)
    let boot_t0 = Instant::now();
    let mut child = Command::new(&desktop_exe)
        .arg("--headless-core")
        .env("VORTEX_DATA_DIR", sandbox.path())
        .env("VORTEX_PIPE_SID_OVERRIDE", &test_sid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vortex-desktop --headless-core");

    let mut client =
        SyncPipeClient::connect_with_timeout(&pipe_path, Duration::from_millis(290), tid)
            .unwrap_or_else(|e| {
                let _ = child.kill();
                panic!("Cold Start Named Pipe failed to become ready within 290ms: {e}");
            });

    let hello = proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: TraceId::new().to_string(),
        reply_to: String::new(),
        trace_id: tid.to_string(),
        timestamp_ms: 1_764_000_000_000,
        body: Some(proto::envelope::Body::Hello(proto::HelloRequest {
            client_kind: "bench-probe".to_owned(),
            client_version: "0.1.0".to_owned(),
            min_supported_v: PROTOCOL_VERSION,
            max_supported_v: PROTOCOL_VERSION,
            capabilities: vec!["health-check".to_owned()],
        })),
    };
    client.send_envelope(&hello).unwrap();
    let ack = client.recv_envelope().unwrap().expect("HelloAck");
    assert!(matches!(ack.body, Some(proto::envelope::Body::HelloAck(_))));

    let health_req = proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: TraceId::new().to_string(),
        reply_to: String::new(),
        trace_id: tid.to_string(),
        timestamp_ms: 1_764_000_000_001,
        body: Some(proto::envelope::Body::HealthReq(proto::HealthCheckReq {})),
    };
    client.send_envelope(&health_req).unwrap();
    let health_resp = client.recv_envelope().unwrap().expect("HealthCheckResp");
    let cold_start_elapsed = boot_t0.elapsed();

    let Some(proto::envelope::Body::HealthResp(h)) = health_resp.body else {
        let _ = child.kill();
        panic!("expected HealthResp");
    };
    assert!(h.core_ok && h.db_ok && h.pipe_ok);

    // Idle RAM measurement (`WebView2` destroyed / absent -> strictly `< 60 MiB`)
    std::thread::sleep(Duration::from_millis(80));
    let (working_set, private_usage) = query_process_memory_bytes(child.id());

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        cold_start_elapsed < Duration::from_millis(300),
        "Cold Start ({cold_start_elapsed:?}) exceeded 300ms budget!"
    );

    let max_idle_bytes: usize = 60 * 1024 * 1024;
    assert!(
        working_set < max_idle_bytes && private_usage < max_idle_bytes,
        "Idle RAM exceeded 60 MiB ceiling: WorkingSet={:.2} MiB, PrivateUsage={:.2} MiB",
        working_set as f64 / (1024.0 * 1024.0),
        private_usage as f64 / (1024.0 * 1024.0),
    );
}
