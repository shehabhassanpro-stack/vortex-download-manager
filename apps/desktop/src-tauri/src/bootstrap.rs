//! Composition Root for Vortex Desktop (`apps/desktop/src-tauri/src/bootstrap.rs` — `P1-T30`).
//!
//! Wires all Hexagonal Level 3 adapters (`vortex-net`, `vortex-storage`, `vortex-io`,
//! `vortex-platform`, `vortex-ipc`, `vortex-engine`) into `ApplicationService` and executes the
//! full 7-step Startup & Self-Healing Sequence (`Vortex-Architecture-Design.md` §24.2).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppResult, ApplicationService, ConfigPort, EventBus, FileSinkFactory, PlatformIntegration,
};
use vortex_domain::SystemClock;
use vortex_engine::{BroadcastEventBus, EngineSupervisor};
use vortex_error::TraceId;
use vortex_io::{SparseFileSinkFactory, spawn_disk_space_monitor};
use vortex_ipc::{PipeServerInstanceFactory, spawn_ipc_server};
use vortex_net::{HttpByteSource, HttpClientConfig};
use vortex_observability::{
    FlightRecorder, ObservabilityConfig, ObservabilityHandle, init_observability,
};
use vortex_platform::{
    DiagnosticBundleBuilder, GracefulShutdownCoordinator, OFFICIAL_EXTENSION_ID, WindowsPlatform,
    create_owner_only_pipe_server, install_os_shutdown_listeners, query_current_user_sid,
    run_startup_self_healing, spawn_connectivity_monitor, user_named_pipe_path,
};
use vortex_storage::{
    ConfigManager, DpapiSecretStore, SqliteCategoryRepository, SqliteDatabase,
    SqliteDownloadRepository, SqliteEventLogRepository,
};

struct OwnerSidPipeFactory {
    pipe_name: String,
    user_sid: String,
}

impl PipeServerInstanceFactory for OwnerSidPipeFactory {
    fn create_instance(
        &self,
        first_instance: bool,
        trace_id: TraceId,
    ) -> AppResult<NamedPipeServer> {
        create_owner_only_pipe_server(&self.pipe_name, &self.user_sid, first_instance, trace_id)
    }
}

/// Live booted Vortex Core state shared with Tauri commands, System Tray, and Event Forwarder.
pub struct BootstrappedCore {
    /// Application CQRS-lite use case facade.
    pub app: Arc<ApplicationService>,
    /// Broadcast event bus (`<= 10Hz` progress deltas + domain events + `BringToFront`).
    pub event_bus: Arc<BroadcastEventBus>,
    /// Graceful shutdown coordinator (`<= 5s` flush + `PRAGMA wal_checkpoint(TRUNCATE)`).
    pub shutdown: Arc<GracefulShutdownCoordinator>,
    /// Observability handle kept alive until exit so `core.jsonl` flushes cleanly.
    pub observability: Option<Arc<ObservabilityHandle>>,
    /// Active Named Pipe path (`\\.\pipe\vortex-core-<SID>`).
    pub pipe_path: String,
}

impl std::fmt::Debug for BootstrappedCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrappedCore")
            .field("pipe_path", &self.pipe_path)
            .finish_non_exhaustive()
    }
}

/// Resolves `%LOCALAPPDATA%\Vortex` (or `VORTEX_DATA_DIR` override for isolated E2E/perf tests!).
#[must_use]
pub fn resolve_data_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("VORTEX_DATA_DIR") {
        return PathBuf::from(override_dir);
    }
    std::env::var_os("LOCALAPPDATA").map_or_else(
        || PathBuf::from(".vortex-data"),
        |base| PathBuf::from(base).join("Vortex"),
    )
}

/// Boots the complete Vortex Core runtime following `Vortex-Architecture-Design.md` §24.2.
///
/// # Errors
/// Returns an error if SQLite, HTTP TLS verifier, or Owner-SID Named Pipe initialization fails.
pub async fn boot_vortex_core(data_dir: &Path) -> AppResult<BootstrappedCore> {
    let env_sid = std::env::var("VORTEX_PIPE_SID_OVERRIDE")
        .ok()
        .filter(|s| !s.trim().is_empty());
    boot_vortex_core_with_sid(data_dir, env_sid.as_deref()).await
}

#[allow(clippy::too_many_lines)]
async fn boot_vortex_core_with_sid(
    data_dir: &Path,
    pipe_sid_override: Option<&str>,
) -> AppResult<BootstrappedCore> {
    let boot_trace = TraceId::new();
    std::fs::create_dir_all(data_dir).ok();

    let log_dir = data_dir.join("logs");
    let obs_cfg = ObservabilityConfig::production(log_dir.clone(), "core");
    let observability = init_observability(&obs_cfg).ok().map(Arc::new);
    let flight_recorder = observability.as_ref().map_or_else(
        || FlightRecorder::new(2_000, None),
        |o| o.flight_recorder().clone(),
    );

    let clock = Arc::new(SystemClock::new());
    let (cfg_mgr_raw, _recovered_cfg) =
        ConfigManager::load_or_init(&data_dir.join("config.toml"), boot_trace)?;
    let cfg_mgr = Arc::new(cfg_mgr_raw);
    let initial_cfg = cfg_mgr.current();
    let _ = std::fs::create_dir_all(&initial_cfg.default_download_dir);

    let (db, _db_recovered) = SqliteDatabase::open_with_self_healing(
        &data_dir.join("vortex.db"),
        std::slice::from_ref(&initial_cfg.default_download_dir),
        &initial_cfg.default_download_dir,
        clock.clone(),
        boot_trace,
    )?;

    let dl_repo = Arc::new(SqliteDownloadRepository::new(db.clone()));
    let cat_repo = Arc::new(SqliteCategoryRepository::new(db.clone()));
    let ev_repo = Arc::new(SqliteEventLogRepository::new(db.clone()));
    let secrets = Arc::new(DpapiSecretStore::new(db.clone(), clock.clone()));

    // Reconcile dirty states from any prior crash + import orphan `.vortex.meta` sidecars
    let _heal = run_startup_self_healing(
        dl_repo.as_ref(),
        std::slice::from_ref(&initial_cfg.default_download_dir),
        clock.as_ref(),
        boot_trace,
    )
    .await?;

    let platform = Arc::new(WindowsPlatform::new(data_dir.join("manifests")));
    // Best-effort auto-registration of sibling `vortex-bridge.exe` in `HKCU` (Brave + Chrome + Edge, zero Admin!)
    if let Ok(self_exe) = std::env::current_exe()
        && let Some(bin_dir) = self_exe.parent()
    {
        let bridge_exe = bin_dir.join("vortex-bridge.exe");
        if bridge_exe.exists() {
            let ext_ids = vec![OFFICIAL_EXTENSION_ID.to_owned()];
            let _ = platform.register_native_hosts(&bridge_exe, &ext_ids, boot_trace);
        }
    }

    let sink_factory = Arc::new(SparseFileSinkFactory::default());
    let allow_private_network = initial_cfg.allow_private_network
        || std::env::var("VORTEX_ALLOW_PRIVATE_NETWORK").is_ok_and(|v| v == "1");
    let byte_source = Arc::new(HttpByteSource::new(
        clock.clone(),
        HttpClientConfig {
            allow_private_network,
            enable_http2: initial_cfg.enable_http2,
            ..HttpClientConfig::default()
        },
    )?);
    let event_bus = Arc::new(BroadcastEventBus::default());
    let bus_dyn: Arc<dyn EventBus> = event_bus.clone();

    let supervisor = EngineSupervisor::start(
        byte_source.clone(),
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

    let pipe_active = Arc::new(AtomicBool::new(false));
    let diagnostics = Arc::new(DiagnosticBundleBuilder::new(
        cfg_mgr.clone(),
        dl_repo.clone(),
        Arc::new(db.clone()),
        sink_factory.space_ledger(),
        platform.clone(),
        flight_recorder,
        observability.clone(),
        log_dir,
        pipe_active.clone(),
        clock.clone(),
    ));

    let app = Arc::new(ApplicationService::new(
        byte_source,
        supervisor.clone(),
        dl_repo,
        cat_repo,
        ev_repo,
        secrets,
        cfg_mgr.clone(),
        platform.clone(),
        diagnostics,
        bus_dyn.clone(),
        clock,
    ));

    let root_cancel = CancellationToken::new();
    let shutdown = GracefulShutdownCoordinator::new(supervisor, Arc::new(db), root_cancel.clone());
    install_os_shutdown_listeners(Arc::clone(&shutdown));

    // Spawn 3-second live disk space monitor (`< 256 MiB` reserve protection)
    let (disk_stop_tx, disk_stop_rx) = tokio::sync::watch::channel(false);
    let disk_cancel = root_cancel.child_token();
    tokio::spawn(async move {
        disk_cancel.cancelled().await;
        let _ = disk_stop_tx.send(true);
    });
    let _disk_mon = spawn_disk_space_monitor(
        sink_factory.space_ledger(),
        initial_cfg.default_download_dir.clone(),
        initial_cfg.min_free_disk_bytes,
        bus_dyn.clone(),
        disk_stop_rx,
    );

    // Spawn connectivity monitor
    let _conn_mon = spawn_connectivity_monitor(
        platform.online_flag(),
        Arc::new(|| true),
        bus_dyn.clone(),
        Duration::from_secs(10),
        root_cancel.child_token(),
    );

    // Active 15-second staged-probe TTL eviction reaper (`TH-31`: zeroize idle RAM secrets & close idle sockets)
    let reaper_app = Arc::clone(&app);
    let reaper_cancel = root_cancel.child_token();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                biased;
                () = reaper_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let evicted = reaper_app.evict_expired_staged_probes().await;
                    if evicted > 0 {
                        tracing::info!(
                            evicted,
                            "Evicted expired staged probes and zeroized RAM secrets (TH-31)"
                        );
                    }
                }
            }
        }
    });

    // Resolve `<USER_SID>` (or explicit override in isolated E2E/unit tests) and start Named Pipe IPC Server!
    let real_os_sid = query_current_user_sid(boot_trace)?;
    let user_sid = pipe_sid_override.map_or_else(|| real_os_sid.clone(), str::to_owned);
    let pipe_path = user_named_pipe_path(&user_sid);

    let pipe_factory = Arc::new(OwnerSidPipeFactory {
        pipe_name: pipe_path.clone(),
        user_sid: real_os_sid,
    });
    let _ipc_join = spawn_ipc_server(
        pipe_factory,
        Arc::clone(&app),
        bus_dyn,
        pipe_active,
        root_cancel.child_token(),
        boot_trace,
    )?;

    // Enforce Owner-Only Protected NTFS DACL on `%LOCALAPPDATA%\Vortex` asynchronously off the
    // `< 300ms` Cold Start critical path (`TH-31`).
    let acl_platform = Arc::clone(&platform);
    let acl_dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Err(err) = acl_platform.enforce_owner_only_dir_acl(&acl_dir, boot_trace) {
            tracing::warn!(
                error_code = %err.code(),
                dir = %acl_dir.display(),
                "Could not enforce Owner-Only NTFS DACL on data_dir"
            );
        }
    });

    Ok(BootstrappedCore {
        app,
        event_bus,
        shutdown,
        observability,
        pipe_path,
    })
}

/// Verifies all layered workspace crates are reachable from the single Composition Root.
#[must_use]
pub fn wired_layers() -> (u32, [&'static str; 7]) {
    let trace = TraceId::new();
    let metrics = vortex_observability::LocalMetrics::default();
    metrics.add_bytes_downloaded(0);
    let negotiated = vortex_ipc::negotiate_version(
        vortex_ipc::PROTOCOL_VERSION,
        vortex_ipc::PROTOCOL_VERSION,
        vortex_ipc::MIN_SUPPORTED_PROTOCOL_VERSION,
        vortex_ipc::PROTOCOL_VERSION,
        trace,
    )
    .unwrap_or(vortex_ipc::PROTOCOL_VERSION);
    (
        negotiated,
        [
            vortex_domain::layer_name(),
            vortex_application::layer_name(),
            vortex_engine::layer_name(),
            vortex_net::layer_name(),
            vortex_storage::layer_name(),
            vortex_io::layer_name(),
            vortex_platform::layer_name(),
        ],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use vortex_ipc::{
        MAX_CONSECUTIVE_RATE_LIMIT_VIOLATIONS, MAX_PIPE_FRAME_BYTES, PROTOCOL_VERSION,
        decode_frame, encode_frame, proto, serve_ipc_stream,
    };

    use super::*;

    #[tokio::test]
    async fn ipc_server_disconnects_persistent_rate_limit_flooder_th32() {
        let root_dir = std::env::temp_dir().join(format!("vortex-flood-test-{}", TraceId::new()));
        let data_dir = root_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let unique_sid = format!("S-1-5-21-FLOOD-{}", TraceId::new());
        let core = boot_vortex_core_with_sid(&data_dir, Some(&unique_sid))
            .await
            .unwrap();

        let (mut client_io, server_io) = tokio::io::duplex(256 * 1024);
        let cancel = CancellationToken::new();
        let bus: Arc<dyn EventBus> = core.event_bus.clone();
        let srv = tokio::spawn(serve_ipc_stream(
            server_io,
            Arc::clone(&core.app),
            bus,
            cancel.clone(),
        ));

        let make_env = |body: proto::envelope::Body| proto::Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: TraceId::new().to_string(),
            reply_to: String::new(),
            trace_id: TraceId::new().to_string(),
            timestamp_ms: 1_000,
            body: Some(body),
        };

        let hello_raw = encode_frame(
            &make_env(proto::envelope::Body::Hello(proto::HelloRequest {
                client_kind: "cli".to_owned(),
                client_version: "0.1.0".to_owned(),
                min_supported_v: PROTOCOL_VERSION,
                max_supported_v: PROTOCOL_VERSION,
                capabilities: vec![],
            })),
            MAX_PIPE_FRAME_BYTES,
        )
        .unwrap();
        client_io.write_all(&hello_raw).await.unwrap();
        client_io.flush().await.unwrap();
        let mut hdr = [0_u8; 4];
        client_io.read_exact(&mut hdr).await.unwrap();
        let len = u32::from_le_bytes(hdr) as usize;
        let mut body = vec![0_u8; len];
        client_io.read_exact(&mut body).await.unwrap();
        let ack = decode_frame(&[&hdr[..], &body[..]].concat(), MAX_PIPE_FRAME_BYTES).unwrap();
        assert!(matches!(ack.body, Some(proto::envelope::Body::HelloAck(_))));

        let mut rate_limit_errors = 0_u32;
        let mut disconnected = false;
        for i in 0..80_i64 {
            let raw = encode_frame(
                &make_env(proto::envelope::Body::Ping(proto::Ping { sent_at_ms: i })),
                MAX_PIPE_FRAME_BYTES,
            )
            .unwrap();
            if client_io.write_all(&raw).await.is_err() {
                disconnected = true;
                break;
            }
            let mut rh = [0_u8; 4];
            if client_io.read_exact(&mut rh).await.is_err() {
                disconnected = true;
                break;
            }
            let rlen = u32::from_le_bytes(rh) as usize;
            let mut rbody = vec![0_u8; rlen];
            if client_io.read_exact(&mut rbody).await.is_err() {
                disconnected = true;
                break;
            }
            let reply =
                decode_frame(&[&rh[..], &rbody[..]].concat(), MAX_PIPE_FRAME_BYTES).unwrap();
            if let Some(proto::envelope::Body::Error(e)) = reply.body
                && e.code == "VTX-IPC-004"
            {
                rate_limit_errors += 1;
            }
        }
        assert_eq!(rate_limit_errors, MAX_CONSECUTIVE_RATE_LIMIT_VIOLATIONS);
        assert!(
            disconnected
                || tokio::time::timeout(Duration::from_secs(2), srv)
                    .await
                    .is_ok(),
            "IPC server must disconnect persistent flooder after 3 consecutive VTX-IPC-004 rejections"
        );
        cancel.cancel();
        core.shutdown.execute(TraceId::new()).await.unwrap();
        let _ = std::fs::remove_dir_all(&root_dir);
    }
}
