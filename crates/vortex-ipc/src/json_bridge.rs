//! Bidirectional Channel A (Native Messaging UTF-8 JSON `camelCase`) ↔ Channel B (`proto::Envelope`)
//! codec and `stdio` binary length-delimited framing (`Protocol-Spec.md` §2.1 & §2.2 / `P1-T25`).

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::framing::{MAX_NATIVE_HOST_TO_EXT_BYTES, MAX_PIPE_FRAME_BYTES};
use crate::proto;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJsonEnvelope {
    v: u32,
    id: String,
    #[serde(default)]
    reply_to: String,
    trace_id: String,
    timestamp_ms: i64,
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRulesJson {
    enabled: bool,
    min_file_size_bytes: u64,
    extensions: Vec<String>,
    mime_types: Vec<String>,
    excluded_hosts: Vec<String>,
    capture_in_incognito: bool,
}

impl From<proto::CaptureRules> for CaptureRulesJson {
    fn from(p: proto::CaptureRules) -> Self {
        Self {
            enabled: p.enabled,
            min_file_size_bytes: p.min_file_size_bytes,
            extensions: p.extensions,
            mime_types: p.mime_types,
            excluded_hosts: p.excluded_hosts,
            capture_in_incognito: p.capture_in_incognito,
        }
    }
}

impl From<CaptureRulesJson> for proto::CaptureRules {
    fn from(j: CaptureRulesJson) -> Self {
        Self {
            enabled: j.enabled,
            min_file_size_bytes: j.min_file_size_bytes,
            extensions: j.extensions,
            mime_types: j.mime_types,
            excluded_hosts: j.excluded_hosts,
            capture_in_incognito: j.capture_in_incognito,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DownloadHandoffJson {
    url: String,
    final_url: String,
    referrer: String,
    suggested_name: String,
    mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    user_agent: String,
    cookies: String,
    headers: BTreeMap<String, String>,
    incognito: bool,
    source: String,
    auto_start: bool,
    top_level_site: String,
}

impl From<DownloadHandoffJson> for proto::DownloadHandoff {
    fn from(j: DownloadHandoffJson) -> Self {
        Self {
            url: j.url,
            final_url: j.final_url,
            referrer: j.referrer,
            suggested_name: j.suggested_name,
            mime_type: j.mime_type,
            size_bytes: j.size_bytes,
            user_agent: j.user_agent,
            cookies: j.cookies,
            headers: j.headers,
            incognito: j.incognito,
            source: j.source,
            auto_start: j.auto_start,
            top_level_site: j.top_level_site,
        }
    }
}

impl From<proto::DownloadHandoff> for DownloadHandoffJson {
    fn from(p: proto::DownloadHandoff) -> Self {
        Self {
            url: p.url,
            final_url: p.final_url,
            referrer: p.referrer,
            suggested_name: p.suggested_name,
            mime_type: p.mime_type,
            size_bytes: p.size_bytes,
            user_agent: p.user_agent,
            cookies: p.cookies,
            headers: p.headers,
            incognito: p.incognito,
            source: p.source,
            auto_start: p.auto_start,
            top_level_site: p.top_level_site,
        }
    }
}

/// Converts a Channel A UTF-8 JSON Native Messaging payload into a [`proto::Envelope`].
///
/// # Errors
/// Returns `VTX-IPC-003` (`IpcFrameInvalid`) if JSON syntax or message discriminant is malformed.
#[allow(clippy::too_many_lines)]
pub fn json_bytes_to_proto_envelope(json_bytes: &[u8]) -> Result<proto::Envelope, VortexError> {
    let fallback_tid = TraceId::new();
    let raw: RawJsonEnvelope = serde_json::from_slice(json_bytes).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            fallback_tid,
            format!("Invalid Channel A JSON envelope: {e}"),
        )
    })?;
    let tid = raw.trace_id.parse::<TraceId>().unwrap_or(fallback_tid);

    let body = match raw.msg_type.as_str() {
        "hello" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct H {
                client_kind: String,
                client_version: String,
                min_supported_v: u32,
                max_supported_v: u32,
                #[serde(default)]
                capabilities: Vec<String>,
            }
            let h: H = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::Hello(proto::HelloRequest {
                client_kind: h.client_kind,
                client_version: h.client_version,
                min_supported_v: h.min_supported_v,
                max_supported_v: h.max_supported_v,
                capabilities: h.capabilities,
            })
        }
        "hello.ack" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Ack {
                negotiated_version: u32,
                core_version: String,
                #[serde(default)]
                capabilities: Vec<String>,
                capture_rules: Option<CaptureRulesJson>,
            }
            let a: Ack = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::HelloAck(proto::HelloAck {
                negotiated_version: a.negotiated_version,
                core_version: a.core_version,
                capabilities: a.capabilities,
                capture_rules: a.capture_rules.map(Into::into),
            })
        }
        "ping" => {
            let sent = raw
                .payload
                .get("sentAtMs")
                .and_then(Value::as_i64)
                .unwrap_or(raw.timestamp_ms);
            proto::envelope::Body::Ping(proto::Ping { sent_at_ms: sent })
        }
        "pong" => {
            let echo = raw
                .payload
                .get("echoSentAtMs")
                .and_then(Value::as_i64)
                .unwrap_or(raw.timestamp_ms);
            proto::envelope::Body::Pong(proto::Pong {
                echo_sent_at_ms: echo,
            })
        }
        "download.handoff" => {
            let h: DownloadHandoffJson = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::DownloadHandoff(h.into())
        }
        "download.handoffAck" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", default)]
            #[derive(Default)]
            struct HAck {
                download_id: String,
                accepted: bool,
                duplicate_found: bool,
                fallback_to_browser: bool,
                fallback_reason: String,
            }
            let a: HAck = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::DownloadHandoffAck(proto::DownloadHandoffAck {
                download_id: a.download_id,
                accepted: a.accepted,
                duplicate_found: a.duplicate_found,
                fallback_to_browser: a.fallback_to_browser,
                fallback_reason: a.fallback_reason,
            })
        }
        "download.batch" => {
            #[derive(Deserialize)]
            struct B {
                items: Vec<DownloadHandoffJson>,
            }
            let b: B = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::DownloadBatch(proto::DownloadBatch {
                items: b.items.into_iter().map(Into::into).collect(),
            })
        }
        "settings.changed" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct S {
                capture_rules: Option<CaptureRulesJson>,
            }
            let s: S = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::SettingsChanged(proto::SettingsChanged {
                capture_rules: s.capture_rules.map(Into::into),
            })
        }
        "diagnostics.dumpReq" => {
            let max_entries = raw
                .payload
                .get("maxEntries")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(500);
            proto::envelope::Body::DiagnosticsReq(proto::DiagnosticsDumpReq { max_entries })
        }
        "diagnostics.dumpResp" => {
            let logs = raw
                .payload
                .get("extensionLogsJsonl")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            proto::envelope::Body::DiagnosticsResp(proto::DiagnosticsDumpResp {
                extension_logs_jsonl: logs,
            })
        }
        "health.checkReq" => proto::envelope::Body::HealthReq(proto::HealthCheckReq {}),
        "health.checkResp" => {
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "camelCase", default)]
            struct HResp {
                core_ok: bool,
                db_ok: bool,
                pipe_ok: bool,
                free_disk_bytes: u64,
            }
            let hr: HResp = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::HealthResp(proto::HealthCheckResp {
                core_ok: hr.core_ok,
                db_ok: hr.db_ok,
                pipe_ok: hr.pipe_ok,
                free_disk_bytes: hr.free_disk_bytes,
            })
        }
        "bringToFront" => {
            let open_url = raw
                .payload
                .get("openUrl")
                .and_then(Value::as_str)
                .map(str::to_owned);
            proto::envelope::Body::BringToFront(proto::BringToFront { open_url })
        }
        "error" => {
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "camelCase", default)]
            struct E {
                code: String,
                category: String,
                recoverability: String,
                retry_after_ms: Option<u64>,
                user_message_key: String,
                message_params: BTreeMap<String, String>,
                suggested_actions: Vec<String>,
                technical_detail: String,
                trace_id: String,
            }
            let e: E = parse_payload(raw.payload, tid)?;
            proto::envelope::Body::Error(proto::ApiError {
                code: e.code,
                category: e.category,
                recoverability: e.recoverability,
                retry_after_ms: e.retry_after_ms,
                user_message_key: e.user_message_key,
                message_params: e.message_params,
                suggested_actions: e.suggested_actions,
                technical_detail: e.technical_detail,
                trace_id: e.trace_id,
            })
        }
        other => {
            return Err(VortexError::coded(
                ErrorCode::IpcFrameInvalid,
                tid,
                format!("Unsupported Channel A message type `{other}`"),
            ));
        }
    };

    Ok(proto::Envelope {
        protocol_version: raw.v,
        message_id: raw.id,
        reply_to: raw.reply_to,
        trace_id: raw.trace_id,
        timestamp_ms: raw.timestamp_ms,
        body: Some(body),
    })
}

/// Converts a [`proto::Envelope`] into Channel A UTF-8 JSON bytes (`<= 1 MiB` enforced on write).
///
/// # Errors
/// Returns `VTX-IPC-003` if `env.body` is missing or JSON serialization fails.
#[allow(clippy::too_many_lines)]
pub fn proto_envelope_to_json_bytes(env: &proto::Envelope) -> Result<Vec<u8>, VortexError> {
    let tid = env
        .trace_id
        .parse::<TraceId>()
        .unwrap_or_else(|_| TraceId::new());
    let Some(body) = &env.body else {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            tid,
            "Cannot serialize empty Envelope.body to Channel A JSON",
        ));
    };

    let (msg_type, payload): (&str, Value) = match body {
        proto::envelope::Body::Hello(h) => (
            "hello",
            json!({
                "clientKind": h.client_kind,
                "clientVersion": h.client_version,
                "minSupportedV": h.min_supported_v,
                "maxSupportedV": h.max_supported_v,
                "capabilities": h.capabilities,
            }),
        ),
        proto::envelope::Body::HelloAck(ack) => (
            "hello.ack",
            json!({
                "negotiatedVersion": ack.negotiated_version,
                "coreVersion": ack.core_version,
                "capabilities": ack.capabilities,
                "captureRules": ack.capture_rules.clone().map(CaptureRulesJson::from),
            }),
        ),
        proto::envelope::Body::Ping(p) => ("ping", json!({ "sentAtMs": p.sent_at_ms })),
        proto::envelope::Body::Pong(p) => ("pong", json!({ "echoSentAtMs": p.echo_sent_at_ms })),
        proto::envelope::Body::DownloadHandoff(h) => (
            "download.handoff",
            serde_json::to_value(DownloadHandoffJson::from(h.clone())).unwrap_or(Value::Null),
        ),
        proto::envelope::Body::DownloadHandoffAck(a) => (
            "download.handoffAck",
            json!({
                "downloadId": a.download_id,
                "accepted": a.accepted,
                "duplicateFound": a.duplicate_found,
                "fallbackToBrowser": a.fallback_to_browser,
                "fallbackReason": a.fallback_reason,
            }),
        ),
        proto::envelope::Body::DownloadBatch(b) => {
            let items: Vec<DownloadHandoffJson> = b
                .items
                .iter()
                .cloned()
                .map(DownloadHandoffJson::from)
                .collect();
            ("download.batch", json!({ "items": items }))
        }
        proto::envelope::Body::SettingsChanged(s) => (
            "settings.changed",
            json!({
                "captureRules": s.capture_rules.clone().map(CaptureRulesJson::from),
            }),
        ),
        proto::envelope::Body::DiagnosticsReq(r) => (
            "diagnostics.dumpReq",
            json!({ "maxEntries": r.max_entries }),
        ),
        proto::envelope::Body::DiagnosticsResp(r) => (
            "diagnostics.dumpResp",
            json!({ "extensionLogsJsonl": r.extension_logs_jsonl }),
        ),
        proto::envelope::Body::HealthReq(_) => ("health.checkReq", json!({})),
        proto::envelope::Body::HealthResp(h) => (
            "health.checkResp",
            json!({
                "coreOk": h.core_ok,
                "dbOk": h.db_ok,
                "pipeOk": h.pipe_ok,
                "freeDiskBytes": h.free_disk_bytes,
            }),
        ),
        proto::envelope::Body::BringToFront(b) => (
            "bringToFront",
            json!({
                "openUrl": b.open_url,
            }),
        ),
        proto::envelope::Body::Error(e) => (
            "error",
            json!({
                "code": e.code,
                "category": e.category,
                "recoverability": e.recoverability,
                "retryAfterMs": e.retry_after_ms,
                "userMessageKey": e.user_message_key,
                "messageParams": e.message_params,
                "suggestedActions": e.suggested_actions,
                "technicalDetail": e.technical_detail,
                "traceId": e.trace_id,
            }),
        ),
        _ => {
            return Err(VortexError::coded(
                ErrorCode::IpcFrameInvalid,
                tid,
                "Unsupported Envelope.body variant for Phase 1 Channel A JSON",
            ));
        }
    };

    let wire = RawJsonEnvelope {
        v: env.protocol_version,
        id: env.message_id.clone(),
        reply_to: env.reply_to.clone(),
        trace_id: env.trace_id.clone(),
        timestamp_ms: env.timestamp_ms,
        msg_type: msg_type.to_owned(),
        payload,
    };

    serde_json::to_vec(&wire).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            tid,
            format!("Channel A JSON serialize error: {e}"),
        )
    })
}

fn parse_payload<T: serde::de::DeserializeOwned>(
    val: Value,
    tid: TraceId,
) -> Result<T, VortexError> {
    serde_json::from_value(val).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            tid,
            format!("Invalid Channel A payload fields: {e}"),
        )
    })
}

/// Reads a single 4-byte LE length-prefixed Native Messaging JSON frame from `reader` (`Ok(None)` on clean EOF).
///
/// Enforces `1 <= len <= 4 MiB` (`Protocol-Spec.md` §1).
///
/// # Errors
/// Returns `VTX-IPC-003` on oversized/zero frame or truncated stream.
pub fn read_native_json_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, VortexError> {
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => {
            return Err(VortexError::coded(
                ErrorCode::IpcCoreUnreachable,
                TraceId::new(),
                format!("Native Messaging stdin read failed: {e}"),
            ));
        }
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_PIPE_FRAME_BYTES {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!(
                "Inbound Native Messaging frame size {len} out of bounds [1, {MAX_PIPE_FRAME_BYTES}]"
            ),
        ));
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("Truncated Native Messaging payload: {e}"),
        )
    })?;
    Ok(Some(payload))
}

/// Writes a single 4-byte LE length-prefixed Native Messaging JSON frame to `writer`,
/// enforcing Chromium's strict **`1 MiB` (`MAX_NATIVE_HOST_TO_EXT_BYTES`)** Host → Extension cap!
///
/// # Errors
/// Returns `VTX-IPC-003` if `json_payload` is empty or exceeds `1 MiB`, or on I/O failure.
pub fn write_native_json_frame<W: Write>(
    writer: &mut W,
    json_payload: &[u8],
) -> Result<(), VortexError> {
    let len = json_payload.len();
    if len == 0 || len > MAX_NATIVE_HOST_TO_EXT_BYTES {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!(
                "Outbound Host->Ext JSON frame {len} out of bounds [1, {MAX_NATIVE_HOST_TO_EXT_BYTES}]"
            ),
        ));
    }
    let len_u32 = u32::try_from(len).unwrap_or(0);
    writer.write_all(&len_u32.to_le_bytes()).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            TraceId::new(),
            format!("Native Messaging stdout header write failed: {e}"),
        )
    })?;
    writer.write_all(json_payload).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            TraceId::new(),
            format!("Native Messaging stdout payload write failed: {e}"),
        )
    })?;
    writer.flush().map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            TraceId::new(),
            format!("Native Messaging stdout flush failed: {e}"),
        )
    })
}
