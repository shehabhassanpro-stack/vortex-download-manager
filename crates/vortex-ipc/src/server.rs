//! Async Named Pipe IPC Server (`server` feature â€” `P1-T24` / `Protocol-Spec.md` Â§3â€“Â§7).
//!
//! Zero `unsafe` inside `vortex-ipc`: delegates Owner-SID DACL (`D:P(A;;GA;;;<USER_SID>)`) +
//! `PIPE_REJECT_REMOTE_CLIENTS` creation to [`PipeServerInstanceFactory`] implemented by `vortex-platform`!

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppConfig, AppEvent, AppResult, ApplicationService, BrowserHandoffCommand, CaptureRulesConfig,
    EventBus,
};
use vortex_error::{ApiError, ErrorCode, SuggestedAction, TraceId, VortexError};
use vortex_observability::SecretString;

use crate::framing::{
    MAX_PIPE_FRAME_BYTES, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION, encode_frame,
    negotiate_capabilities, negotiate_version, proto_error_from_api,
};
use crate::proto;
use crate::rate_limit::IpcRateLimiter;
use crate::validation::{validate_download_handoff, validate_envelope_header};

/// Core capability strings advertised during `HelloAck` negotiation (`Protocol-Spec.md` Â§3).
pub const CORE_CAPABILITIES: &[&str] = &["handoff", "diag-dump", "health-check", "bring-to-front"];

/// Maximum simultaneous Named Pipe client connections allowed (`TH-32` `DoS` defense).
pub const MAX_CONCURRENT_PIPE_CLIENTS: usize = 16;

/// Persistent flooder threshold: after this many consecutive `VTX-IPC-004` rejections, disconnect (`TH-32`).
pub const MAX_CONSECUTIVE_RATE_LIMIT_VIOLATIONS: u32 = 3;

/// Safe factory port producing Owner-SID-hardened [`NamedPipeServer`] instances (`ADR-006`).
pub trait PipeServerInstanceFactory: Send + Sync {
    /// Creates a new [`NamedPipeServer`] instance (`first_instance == true` on initial bind).
    ///
    /// # Errors
    /// Returns [`VortexError`] if SDDL creation or `CreateNamedPipeW` fails.
    fn create_instance(
        &self,
        first_instance: bool,
        trace_id: TraceId,
    ) -> AppResult<NamedPipeServer>;
}

/// Converts [`CaptureRulesConfig`] into wire [`proto::CaptureRules`].
#[must_use]
pub fn proto_capture_rules_from_config(cfg: &CaptureRulesConfig) -> proto::CaptureRules {
    proto::CaptureRules {
        enabled: cfg.enabled,
        min_file_size_bytes: cfg.min_file_size_bytes,
        extensions: cfg.extensions.clone(),
        mime_types: cfg.mime_types.clone(),
        excluded_hosts: cfg.excluded_hosts.clone(),
        capture_in_incognito: cfg.capture_in_incognito,
    }
}

/// Spawns the multi-client Named Pipe IPC server loop (`P1-T24`).
///
/// Sets `pipe_active_flag = true` while listening and resets it to `false` on Graceful Shutdown.
///
/// # Errors
/// Returns [`VortexError`] immediately if binding the initial `FILE_FLAG_FIRST_PIPE_INSTANCE` pipe fails.
pub fn spawn_ipc_server(
    pipe_factory: Arc<dyn PipeServerInstanceFactory>,
    app: Arc<ApplicationService>,
    event_bus: Arc<dyn EventBus>,
    pipe_active_flag: Arc<AtomicBool>,
    shutdown_token: CancellationToken,
    trace_id: TraceId,
) -> AppResult<tokio::task::JoinHandle<()>> {
    let initial_server = pipe_factory.create_instance(true, trace_id)?;
    pipe_active_flag.store(true, Ordering::SeqCst);

    let client_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PIPE_CLIENTS));

    let handle = tokio::spawn(async move {
        let mut current_server = initial_server;

        loop {
            let connect_res = tokio::select! {
                biased;
                () = shutdown_token.cancelled() => break,
                r = current_server.connect() => r,
            };

            // Immediately create the NEXT pipe instance BEFORE servicing this client so concurrent
            // callers (`bridge` + `cli` + secondary `vortex.exe`) never get `ERROR_FILE_NOT_FOUND`!
            let next_server = match pipe_factory.create_instance(false, TraceId::new()) {
                Ok(srv) => srv,
                Err(err) => {
                    tracing::error!(error_code = %err.code(), "Failed creating next Named Pipe instance");
                    break;
                }
            };

            let connected_stream = current_server;
            current_server = next_server;

            if let Err(err) = connect_res {
                tracing::warn!(detail = %err, "Named Pipe client connect aborted");
                continue;
            }

            let Ok(permit) = Arc::clone(&client_slots).try_acquire_owned() else {
                tracing::warn!(
                    error_code = %ErrorCode::IpcRateLimited,
                    max_clients = MAX_CONCURRENT_PIPE_CLIENTS,
                    "Rejecting excess Named Pipe client connection (TH-32 cap reached)"
                );
                drop(connected_stream);
                continue;
            };

            let client_app = Arc::clone(&app);
            let client_bus = Arc::clone(&event_bus);
            let client_cancel = shutdown_token.child_token();
            tokio::spawn(async move {
                let _permit = permit;
                let _ =
                    serve_ipc_stream(connected_stream, client_app, client_bus, client_cancel).await;
            });
        }

        pipe_active_flag.store(false, Ordering::SeqCst);
    });

    Ok(handle)
}

/// Services a single bidirectional framed Protobuf IPC connection (usable on `NamedPipeServer` or `DuplexStream` in tests!).
///
/// # Errors
/// Returns [`VortexError`] if framing or I/O fails.
#[allow(clippy::too_many_lines)]
pub async fn serve_ipc_stream<S>(
    stream: S,
    app: Arc<ApplicationService>,
    event_bus: Arc<dyn EventBus>,
    cancel: CancellationToken,
) -> AppResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut limiter = IpcRateLimiter::protocol_default(Instant::now());
    let mut cfg_rx = event_bus.subscribe();
    let mut handshake_done = false;
    let mut consecutive_rate_violations: u32 = 0;

    // Enforce 2.0-second `HelloRequest` handshake timeout (`Protocol-Spec.md` Â§3)
    let first_frame_opt = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(()),
        res = tokio::time::timeout(Duration::from_secs(2), read_async_frame(&mut reader)) => {
            if let Ok(r) = res {
                r?
            } else {
                tracing::warn!("IPC client failed to send HelloRequest within 2s handshake window");
                return Ok(());
            }
        }
    };

    let Some(first_env) = first_frame_opt else {
        return Ok(());
    };

    let (first_reply, disconnect_first) = handle_client_envelope(
        first_env,
        &mut handshake_done,
        &mut limiter,
        &mut consecutive_rate_violations,
        &app,
        &event_bus,
    )
    .await?;
    if let Some(reply) = first_reply {
        write_async_frame(&mut writer, &reply).await?;
    }
    if disconnect_first {
        return Ok(());
    }

    loop {
        tokio::select! {
            biased;

            () = cancel.cancelled() => break,

            // Push live `SettingsChanged` (`CaptureRules`) to connected Extension Bridges on Hot-Reload!
            Ok(bus_ev) = cfg_rx.recv(), if handshake_done => {
                if let AppEvent::ConfigChanged(new_cfg) = bus_ev {
                    let push = build_settings_changed_push(&new_cfg);
                    if write_async_frame(&mut writer, &push).await.is_err() {
                        break;
                    }
                }
            }

            frame_res = read_async_frame(&mut reader) => {
                match frame_res {
                    Ok(Some(env)) => {
                        let (reply_opt, should_disconnect) = handle_client_envelope(
                            env,
                            &mut handshake_done,
                            &mut limiter,
                            &mut consecutive_rate_violations,
                            &app,
                            &event_bus,
                        )
                        .await?;
                        if let Some(reply) = reply_opt {
                            write_async_frame(&mut writer, &reply).await?;
                        }
                        if should_disconnect {
                            tracing::warn!(
                                error_code = %ErrorCode::IpcRateLimited,
                                violations = consecutive_rate_violations,
                                "Disconnecting persistent IPC flooder after consecutive rate-limit violations (TH-32)"
                            );
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(error_code = %err.code(), "Closing IPC client on invalid frame");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_client_envelope(
    env: proto::Envelope,
    handshake_done: &mut bool,
    limiter: &mut IpcRateLimiter,
    consecutive_rate_violations: &mut u32,
    app: &ApplicationService,
    event_bus: &Arc<dyn EventBus>,
) -> AppResult<(Option<proto::Envelope>, bool)> {
    let trace_id = match validate_envelope_header(&env) {
        Ok(tid) => tid,
        Err(err) => {
            return Ok((
                Some(make_error_reply("", TraceId::new(), err.as_api_error())),
                false,
            ));
        }
    };

    if let Err(rate_err) = limiter.check_and_consume(Instant::now(), trace_id) {
        *consecutive_rate_violations = consecutive_rate_violations.saturating_add(1);
        let should_disconnect =
            *consecutive_rate_violations >= MAX_CONSECUTIVE_RATE_LIMIT_VIOLATIONS;
        return Ok((
            Some(make_error_reply(
                &env.message_id,
                trace_id,
                rate_err.as_api_error(),
            )),
            should_disconnect,
        ));
    }
    *consecutive_rate_violations = 0;

    let req_msg_id = env.message_id.clone();
    let span = tracing::info_span!(
        "ipc.dispatch",
        %trace_id,
        msg_id = %req_msg_id
    );
    let _enter = span.enter();

    let Some(body) = env.body else {
        return Ok((None, false));
    };

    // Allow `BringToFront` directly for fast Single-Instance secondary process exit, otherwise require `Hello` first
    if !*handshake_done
        && !matches!(
            body,
            proto::envelope::Body::Hello(_)
                | proto::envelope::Body::BringToFront(_)
                | proto::envelope::Body::HealthReq(_)
        )
    {
        let api = ApiError::new(ErrorCode::IpcFrameInvalid, trace_id)
            .with_technical_detail("HelloRequest handshake must precede RPC requests");
        return Ok((Some(make_error_reply(&req_msg_id, trace_id, &api)), false));
    }

    let reply = match body {
        proto::envelope::Body::Hello(hello) => {
            if let Ok(negotiated) = negotiate_version(
                hello.min_supported_v,
                hello.max_supported_v,
                MIN_SUPPORTED_PROTOCOL_VERSION,
                PROTOCOL_VERSION,
                trace_id,
            ) {
                *handshake_done = true;
                let caps = negotiate_capabilities(&hello.capabilities, CORE_CAPABILITIES);
                let cfg = app.current_config();
                let ack = proto::HelloAck {
                    negotiated_version: negotiated,
                    core_version: env!("CARGO_PKG_VERSION").to_owned(),
                    capabilities: caps,
                    capture_rules: Some(proto_capture_rules_from_config(&cfg.capture_rules)),
                };
                Some(make_reply(
                    &req_msg_id,
                    trace_id,
                    proto::envelope::Body::HelloAck(ack),
                ))
            } else {
                let api = ApiError::new(ErrorCode::IpcVersionMismatch, trace_id)
                    .with_actions(vec![SuggestedAction::UpdateVortex])
                    .with_technical_detail("Incompatible IPC protocol version");
                Some(make_error_reply(&req_msg_id, trace_id, &api))
            }
        }

        proto::envelope::Body::Ping(ping) => Some(make_reply(
            &req_msg_id,
            trace_id,
            proto::envelope::Body::Pong(proto::Pong {
                echo_sent_at_ms: ping.sent_at_ms,
            }),
        )),

        proto::envelope::Body::DownloadHandoff(handoff) => {
            if let Err(val_err) = validate_download_handoff(&handoff, trace_id) {
                return Ok((
                    Some(make_error_reply(
                        &req_msg_id,
                        trace_id,
                        val_err.as_api_error(),
                    )),
                    false,
                ));
            }

            // Immediately wrap `cookies` (`normal + CHIPS`) in zeroized `SecretString`!
            let secret_cookies =
                (!handoff.cookies.is_empty()).then(|| SecretString::from(handoff.cookies));

            let cmd = BrowserHandoffCommand {
                url: handoff.url,
                final_url: (!handoff.final_url.is_empty()).then_some(handoff.final_url),
                referrer: (!handoff.referrer.is_empty()).then_some(handoff.referrer),
                suggested_name: (!handoff.suggested_name.is_empty())
                    .then_some(handoff.suggested_name),
                mime_type: (!handoff.mime_type.is_empty()).then_some(handoff.mime_type),
                size_bytes: handoff.size_bytes,
                user_agent: (!handoff.user_agent.is_empty()).then_some(handoff.user_agent),
                cookies: secret_cookies,
                headers: handoff.headers,
                incognito: handoff.incognito,
                source: handoff.source,
                auto_start: handoff.auto_start,
                top_level_site: (!handoff.top_level_site.is_empty())
                    .then_some(handoff.top_level_site),
            };

            match app.handoff_from_browser(cmd, trace_id).await {
                Ok(res) => {
                    let ack = proto::DownloadHandoffAck {
                        download_id: res.download_id.unwrap_or_default(),
                        accepted: res.accepted,
                        duplicate_found: res.duplicate_found,
                        fallback_to_browser: res.fallback_to_browser,
                        fallback_reason: res.fallback_reason.unwrap_or_default(),
                    };
                    Some(make_reply(
                        &req_msg_id,
                        trace_id,
                        proto::envelope::Body::DownloadHandoffAck(ack),
                    ))
                }
                Err(err) => Some(make_error_reply(&req_msg_id, trace_id, err.as_api_error())),
            }
        }

        proto::envelope::Body::HealthReq(_) => {
            let health = app.get_health_status(trace_id).await?;
            Some(make_reply(
                &req_msg_id,
                trace_id,
                proto::envelope::Body::HealthResp(proto::HealthCheckResp {
                    core_ok: health.core_ok,
                    db_ok: health.db_ok,
                    pipe_ok: health.pipe_ok,
                    free_disk_bytes: health.free_disk_bytes,
                }),
            ))
        }

        proto::envelope::Body::BringToFront(b) => {
            event_bus.publish(AppEvent::BringToFront {
                open_url: b.open_url,
            });
            Some(make_reply(
                &req_msg_id,
                trace_id,
                proto::envelope::Body::Pong(proto::Pong {
                    echo_sent_at_ms: unix_now_ms(),
                }),
            ))
        }

        _ => {
            let api = ApiError::new(ErrorCode::IpcFrameInvalid, trace_id)
                .with_technical_detail("Unsupported RPC request variant in Phase 1");
            Some(make_error_reply(&req_msg_id, trace_id, &api))
        }
    };
    Ok((reply, false))
}

fn build_settings_changed_push(cfg: &AppConfig) -> proto::Envelope {
    proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: uuid::Uuid::now_v7().to_string(),
        reply_to: String::new(),
        trace_id: TraceId::new().to_string(),
        timestamp_ms: unix_now_ms(),
        body: Some(proto::envelope::Body::SettingsChanged(
            proto::SettingsChanged {
                capture_rules: Some(proto_capture_rules_from_config(&cfg.capture_rules)),
            },
        )),
    }
}

fn make_reply(reply_to: &str, trace_id: TraceId, body: proto::envelope::Body) -> proto::Envelope {
    proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: uuid::Uuid::now_v7().to_string(),
        reply_to: reply_to.to_owned(),
        trace_id: trace_id.to_string(),
        timestamp_ms: unix_now_ms(),
        body: Some(body),
    }
}

fn make_error_reply(reply_to: &str, trace_id: TraceId, api: &ApiError) -> proto::Envelope {
    make_reply(
        reply_to,
        trace_id,
        proto::envelope::Body::Error(proto_error_from_api(api)),
    )
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

async fn read_async_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> AppResult<Option<proto::Envelope>> {
    use prost::Message;

    let mut hdr = [0_u8; 4];
    match reader.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(VortexError::coded(
                ErrorCode::IpcCoreUnreachable,
                TraceId::new(),
                format!("IPC async header read failed: {e}"),
            ));
        }
    }

    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 || len > MAX_PIPE_FRAME_BYTES {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("IPC async frame size {len} outside [1, {MAX_PIPE_FRAME_BYTES}]"),
        ));
    }

    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf).await.map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("IPC async payload truncated: {e}"),
        )
    })?;

    let env = proto::Envelope::decode(buf.as_slice()).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("IPC async protobuf decode failed: {e}"),
        )
    })?;
    Ok(Some(env))
}

async fn write_async_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    env: &proto::Envelope,
) -> AppResult<()> {
    let bytes = encode_frame(env, MAX_PIPE_FRAME_BYTES)?;
    writer.write_all(&bytes).await.map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            TraceId::new(),
            format!("IPC async write failed: {e}"),
        )
    })?;
    writer.flush().await.map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            TraceId::new(),
            format!("IPC async flush failed: {e}"),
        )
    })
}
