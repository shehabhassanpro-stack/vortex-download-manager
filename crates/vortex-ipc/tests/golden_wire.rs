//! Golden Wire Compatibility Tests (`P0-T14` / `Protocol-Spec.md` §6 / `ADR-010`).
//!
//! Decodes frozen binary (`.bin`) and Channel A JSON (`.json`) wire fixtures stored on disk in
//! `crates/vortex-ipc/tests/golden/` so ANY accidental Protobuf tag renumbering, wire-type
//! change, or framing drift immediately fails CI across `N / N-1 / N-2` versions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use prost::Message;
use vortex_error::{ApiError, ErrorCode, Recoverability, SuggestedAction, TraceId};
use vortex_ipc::{
    MAX_PIPE_FRAME_BYTES, PROTOCOL_VERSION, decode_frame, encode_frame, proto,
    proto_error_from_api, proto_error_to_api, validate_download_handoff, validate_envelope_header,
};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn canonical_handoff_envelope() -> proto::Envelope {
    let mut headers = BTreeMap::new();
    headers.insert("Accept-Language".to_owned(), "ar,en;q=0.9".to_owned());

    let handoff = proto::DownloadHandoff {
        url: "https://example.com/download?id=42".to_owned(),
        final_url: "https://cdn.example.com/release.iso".to_owned(),
        referrer: "https://example.com/downloads".to_owned(),
        suggested_name: "release.iso".to_owned(),
        mime_type: "application/octet-stream".to_owned(),
        size_bytes: Some(1_073_741_824),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Brave/1.0".to_owned(),
        cookies: String::new(),
        headers,
        incognito: false,
        source: "determining-filename".to_owned(),
        auto_start: false,
        top_level_site: "https://example.com".to_owned(),
    };

    proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: "0192f3a4-0000-7000-8000-000000000010".to_owned(),
        reply_to: String::new(),
        trace_id: "0192f3a4-0000-7000-8000-000000000001".to_owned(),
        timestamp_ms: 1_764_094_000_000,
        body: Some(proto::envelope::Body::DownloadHandoff(handoff)),
    }
}

fn canonical_hello_ack_envelope() -> proto::Envelope {
    proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: "0192f3a4-0000-7000-8000-000000000020".to_owned(),
        reply_to: "0192f3a4-0000-7000-8000-000000000019".to_owned(),
        trace_id: "0192f3a4-0000-7000-8000-000000000001".to_owned(),
        timestamp_ms: 1_764_094_000_050,
        body: Some(proto::envelope::Body::HelloAck(proto::HelloAck {
            negotiated_version: 1,
            core_version: "0.1.0".to_owned(),
            capabilities: vec![
                "diag-dump".to_owned(),
                "handoff".to_owned(),
                "link-refresh".to_owned(),
                "media".to_owned(),
            ],
            capture_rules: Some(proto::CaptureRules {
                enabled: true,
                min_file_size_bytes: 1_048_576,
                extensions: vec!["zip".to_owned(), "iso".to_owned(), "mkv".to_owned()],
                mime_types: vec!["application/octet-stream".to_owned()],
                excluded_hosts: vec!["localhost".to_owned()],
                capture_in_incognito: false,
            }),
        })),
    }
}

fn canonical_api_error_envelope() -> proto::Envelope {
    let trace_id: TraceId = "0192f3a4-0000-7000-8000-000000000001".parse().unwrap();
    let api_err = ApiError::new(ErrorCode::ResLinkExpired, trace_id)
        .with_recoverability(Recoverability::UserActionRequired)
        .with_actions(vec![SuggestedAction::RefreshLink])
        .with_param("host", "cdn.example.com")
        .with_technical_detail("HTTP 403 signed URL expired");

    proto::Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: "0192f3a4-0000-7000-8000-000000000030".to_owned(),
        reply_to: "0192f3a4-0000-7000-8000-000000000010".to_owned(),
        trace_id: trace_id.to_string(),
        timestamp_ms: 1_764_094_001_000,
        body: Some(proto::envelope::Body::Error(proto_error_from_api(&api_err))),
    }
}

#[test]
fn golden_v1_frozen_binary_fixtures_decode_identically() {
    let dir = golden_dir();
    let fixtures = [
        ("v1_download_handoff.bin", canonical_handoff_envelope()),
        ("v1_hello_ack.bin", canonical_hello_ack_envelope()),
        ("v1_api_error.bin", canonical_api_error_envelope()),
    ];

    let update_golden = std::env::var("VORTEX_WRITE_GOLDEN").is_ok_and(|v| v == "1");

    for (file_name, expected_env) in fixtures {
        let path = dir.join(file_name);
        let expected_bytes = encode_frame(&expected_env, MAX_PIPE_FRAME_BYTES).unwrap();

        if update_golden {
            fs::create_dir_all(&dir).unwrap();
            fs::write(&path, &expected_bytes).unwrap();
        }

        let on_disk = fs::read(&path)
            .unwrap_or_else(|e| panic!("missing golden binary fixture `{}`: {e}", path.display()));

        assert_eq!(
            on_disk, expected_bytes,
            "wire bytes drifted from frozen v1 fixture `{file_name}` — check proto tag numbers!"
        );

        let decoded = decode_frame(&on_disk, MAX_PIPE_FRAME_BYTES).unwrap();
        assert_eq!(decoded, expected_env);
        let extracted_trace = validate_envelope_header(&decoded).unwrap();
        assert_eq!(
            extracted_trace.to_string(),
            "0192f3a4-0000-7000-8000-000000000001"
        );

        if let Some(proto::envelope::Body::DownloadHandoff(ref h)) = decoded.body {
            validate_download_handoff(h, extracted_trace).unwrap();
        }
        if let Some(proto::envelope::Body::Error(ref wire_err)) = decoded.body {
            let round_tripped = proto_error_to_api(wire_err).unwrap();
            assert_eq!(round_tripped.code, ErrorCode::ResLinkExpired);
        }
    }
}

#[test]
fn golden_v1_unknown_future_field_is_safely_ignored() {
    let env = proto::Envelope {
        protocol_version: 1,
        message_id: "0192f3a4-0000-7000-8000-000000000099".to_owned(),
        reply_to: String::new(),
        trace_id: "0192f3a4-0000-7000-8000-000000000001".to_owned(),
        timestamp_ms: 1_764_094_000_123,
        body: Some(proto::envelope::Body::Ping(proto::Ping {
            sent_at_ms: 1_764_094_000_123,
        })),
    };

    let mut payload = env.encode_to_vec();
    // Append unknown varint field tag #250: (250 << 3) | 0 = 2000 -> varint [0xD0, 0x0F], value 7 -> [0x07]
    payload.extend_from_slice(&[0xD0, 0x0F, 0x07]);

    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(u32::try_from(payload.len()).unwrap()).to_le_bytes());
    framed.extend_from_slice(&payload);

    let decoded = decode_frame(&framed, MAX_PIPE_FRAME_BYTES).unwrap();
    assert_eq!(decoded.protocol_version, 1);
    assert_eq!(
        decoded.body,
        Some(proto::envelope::Body::Ping(proto::Ping {
            sent_at_ms: 1_764_094_000_123
        }))
    );
}

#[test]
fn golden_v1_json_fixture_matches_channel_a_schema() {
    let raw = include_str!("golden/v1_download_handoff.json");
    let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();

    assert_eq!(parsed.get("v"), Some(&serde_json::json!(1)));
    assert_eq!(
        parsed.get("type"),
        Some(&serde_json::json!("download.handoff"))
    );
    let payload = parsed.get("payload").unwrap();
    assert_eq!(
        payload.get("mimeType"),
        Some(&serde_json::json!("application/octet-stream"))
    );
    assert_eq!(
        payload.get("sizeBytes"),
        Some(&serde_json::json!(1_073_741_824_u64))
    );
    assert_eq!(
        payload.get("topLevelSite"),
        Some(&serde_json::json!("https://example.com"))
    );
    assert_eq!(payload.get("autoStart"), Some(&serde_json::json!(false)));
}
