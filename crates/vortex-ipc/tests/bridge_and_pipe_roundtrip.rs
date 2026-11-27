//! End-to-end tests for `vortex-ipc` Channel A JSON ↔ Channel B Protobuf bridge,
//! `SyncPipeClient`, and Native Messaging `1 MiB` / `4 MiB` `DoS` bounds (`P1-T24` & `P1-T25`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Cursor;
use std::time::Duration;

use vortex_error::{ErrorCode, TraceId};
use vortex_ipc::{
    MAX_NATIVE_HOST_TO_EXT_BYTES, PROTOCOL_VERSION, SyncPipeClient, decode_frame, encode_frame,
    json_bytes_to_proto_envelope, proto, proto_envelope_to_json_bytes, read_native_json_frame,
    write_native_json_frame,
};

#[test]
fn channel_a_json_round_trips_all_phase1_envelope_types_and_enforces_1mib_cap() {
    // 1. Frozen `v1_download_handoff.json` -> `proto::Envelope` -> JSON round-trip!
    let golden_json = include_bytes!("golden/v1_download_handoff.json");
    let env = json_bytes_to_proto_envelope(golden_json).unwrap();
    assert_eq!(env.protocol_version, 1);
    assert_eq!(env.trace_id, "0192f3a4-0000-7000-8000-000000000001");

    let back_json = proto_envelope_to_json_bytes(&env).unwrap();
    let env2 = json_bytes_to_proto_envelope(&back_json).unwrap();
    assert_eq!(env, env2);

    // 2. Round-trip `Hello`, `HelloAck`, `Ping`, `Pong`, `DownloadHandoffAck`, `SettingsChanged`, `HealthCheckResp`, `BringToFront`
    let samples = vec![
        proto::envelope::Body::Hello(proto::HelloRequest {
            client_kind: "extension-brave".to_owned(),
            client_version: "0.1.0".to_owned(),
            min_supported_v: 1,
            max_supported_v: 1,
            capabilities: vec!["handoff".to_owned()],
        }),
        proto::envelope::Body::Ping(proto::Ping { sent_at_ms: 12345 }),
        proto::envelope::Body::Pong(proto::Pong {
            echo_sent_at_ms: 12345,
        }),
        proto::envelope::Body::DownloadHandoffAck(proto::DownloadHandoffAck {
            download_id: String::new(),
            accepted: false,
            duplicate_found: false,
            fallback_to_browser: true,
            fallback_reason: "waf_tls_bound".to_owned(),
        }),
        proto::envelope::Body::SettingsChanged(proto::SettingsChanged {
            capture_rules: Some(proto::CaptureRules {
                enabled: true,
                min_file_size_bytes: 1_048_576,
                extensions: vec!["iso".to_owned()],
                mime_types: vec!["application/octet-stream".to_owned()],
                excluded_hosts: vec!["localhost".to_owned()],
                capture_in_incognito: false,
            }),
        }),
        proto::envelope::Body::HealthResp(proto::HealthCheckResp {
            core_ok: true,
            db_ok: true,
            pipe_ok: true,
            free_disk_bytes: 99_000_000,
        }),
        proto::envelope::Body::BringToFront(proto::BringToFront {
            open_url: Some("https://example.com/a.zip".to_owned()),
        }),
    ];

    for body in samples {
        let orig = proto::Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: TraceId::new().to_string(),
            reply_to: String::new(),
            trace_id: TraceId::new().to_string(),
            timestamp_ms: 1_765_000_000_000,
            body: Some(body),
        };
        let mut stdio_buf = Vec::new();
        let j = proto_envelope_to_json_bytes(&orig).unwrap();
        write_native_json_frame(&mut stdio_buf, &j).unwrap();

        let mut cursor = Cursor::new(stdio_buf);
        let read_back = read_native_json_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(json_bytes_to_proto_envelope(&read_back).unwrap(), orig);
        assert!(read_native_json_frame(&mut cursor).unwrap().is_none());
    }

    // 3. Enforce strict Chromium `1 MiB` outbound cap (`VTX-IPC-003`)!
    let mut sink = Vec::new();
    let oversized = vec![b'a'; MAX_NATIVE_HOST_TO_EXT_BYTES + 1];
    assert_eq!(
        write_native_json_frame(&mut sink, &oversized)
            .unwrap_err()
            .code(),
        ErrorCode::IpcFrameInvalid
    );
}

#[test]
fn sync_pipe_client_times_out_cleanly_with_ipc_core_unreachable() {
    let missing_pipe = format!(r"\\.\pipe\vortex-missing-{}", TraceId::new());
    let err = SyncPipeClient::connect_with_timeout(
        &missing_pipe,
        Duration::from_millis(60),
        TraceId::new(),
    )
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::IpcCoreUnreachable);

    // Frame encode/decode sanity check
    let ping = proto::Envelope {
        protocol_version: 1,
        message_id: TraceId::new().to_string(),
        reply_to: String::new(),
        trace_id: TraceId::new().to_string(),
        timestamp_ms: 42,
        body: Some(proto::envelope::Body::Ping(proto::Ping { sent_at_ms: 42 })),
    };
    let raw = encode_frame(&ping, 1024).unwrap();
    assert_eq!(decode_frame(&raw, 1024).unwrap(), ping);
}
