//! Mandatory Zero-Leakage CI Test (`P0-T12` / `ADR-016` / `Threat-Model.md` TH-08).
//!
//! Verifies BOTH:
//! 1. Type-level protection (`SecretString` + `RedactedUrl` + `sanitize_path_for_export`), AND
//! 2. Sink-level Defense-in-Depth protection: even if future code accidentally logs raw URLs,
//!    raw `cookies = ...` fields, or raw `Bearer` tokens WITHOUT `RedactedUrl`/`SecretString`,
//!    neither `core.jsonl`, nor `FlightRecorder`, nor exported diagnostics ever leak a single secret!

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use vortex_error::{ErrorCode, TraceId};
use vortex_observability::{
    ExposeSecret, ObservabilityConfig, RedactedUrl, SecretString, redact_sensitive_text,
    sanitize_path_for_export,
};

#[test]
fn zero_secret_leakage_across_jsonl_flight_recorder_and_export() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = ObservabilityConfig::production(tmp.path().to_path_buf(), "core");
    cfg.install_panic_hook = false;

    let canary_cookie = "CANARY_CF_CLEARANCE_998177";
    let canary_session = "CANARY_SESSION_SID_442190";
    let canary_bearer = "CANARY_JWT_BEARER_883104";
    let canary_url_pass = "CANARY_URL_PASS_551923";
    let canary_amz_sig = "CANARY_AMZ_SIG_772819";
    let canary_query_tok = "CANARY_ACCESS_TOKEN_331942";
    let canary_accidental_raw = "CANARY_ACCIDENTAL_RAW_PW_112233";
    let canary_username = "CanaryPrivateUser";

    let raw_cookie = SecretString::from(format!(
        "cf_clearance={canary_cookie}; session={canary_session}"
    ));
    let raw_auth = SecretString::from(format!("Bearer {canary_bearer}"));
    let raw_signed_url = format!(
        "https://alice:{canary_url_pass}@cdn.example.com/dl/video.mkv?X-Amz-Signature={canary_amz_sig}&access_token={canary_query_tok}&itag=313"
    );
    let raw_local_path = format!(r"C:\Users\{canary_username}\Downloads\video.mkv");

    assert!(raw_cookie.expose_secret().contains(canary_cookie));
    assert!(raw_auth.expose_secret().contains(canary_bearer));

    let (subscriber, handle) = vortex_observability::build_subscriber(&cfg).unwrap();
    let trace_id = TraceId::new();

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "download.probe",
            trace_id = %trace_id,
            download_id = "0192f3a4-0000-7000-8000-000000000001"
        );
        let _entered = span.enter();

        // 1. Standard INFO log using RedactedUrl + SecretString
        tracing::info!(
            url = %RedactedUrl::parse(&raw_signed_url),
            cookie = ?raw_cookie,
            auth = ?raw_auth,
            "starting probe against remote server"
        );

        // 2. DEBUG event captured by Flight Recorder
        tracing::debug!(
            url = %RedactedUrl::parse(&raw_signed_url),
            header_dump = %redact_sensitive_text(&format!(
                "Cookie: {}\nAuthorization: {}",
                raw_cookie.expose_secret(),
                raw_auth.expose_secret()
            )),
            "prepared probe headers"
        );

        // 3. Defense-in-Depth test: simulate accidental raw developer mistake WITHOUT wrappers!
        tracing::error!(
            error_code = %ErrorCode::ResLinkExpired,
            cookies = %raw_cookie.expose_secret(),
            "probe failed for \"https://bob:{canary_accidental_raw}@cdn.example.com/raw.zip?token={canary_query_tok}\" with Authorization: Bearer {canary_bearer}"
        );
    });

    handle.flush();

    let jsonl_output = fs::read_to_string(handle.active_log_path()).unwrap();
    let flight_dump_path = tmp.path().join("core-flight-recorder.jsonl");
    assert!(
        flight_dump_path.is_file(),
        "ERROR event must flush Flight Recorder dump"
    );
    let flight_output = fs::read_to_string(&flight_dump_path).unwrap();

    let export_payload = sanitize_path_for_export(
        &format!("{jsonl_output}\n{flight_output}\nsave_path={raw_local_path}"),
        Some(&format!(r"C:\Users\{canary_username}")),
    );

    let all_canaries = [
        canary_cookie,
        canary_session,
        canary_bearer,
        canary_url_pass,
        canary_amz_sig,
        canary_query_tok,
        canary_accidental_raw,
        canary_username,
    ];

    for canary in all_canaries {
        assert!(
            !jsonl_output.contains(canary),
            "SECRET LEAK in core.jsonl: found `{canary}` in:\n{jsonl_output}"
        );
        assert!(
            !flight_output.contains(canary),
            "SECRET LEAK in flight-recorder dump: found `{canary}` in:\n{flight_output}"
        );
        assert!(
            !export_payload.contains(canary),
            "SECRET LEAK in diagnostic export: found `{canary}`"
        );
    }

    // Verify JSONL lines remain valid parseable JSON after sink-level Defense-in-Depth scrubbing!
    for line in jsonl_output.lines() {
        let _: serde_json::Value = serde_json::from_str(line).unwrap();
    }

    assert!(jsonl_output.contains(&trace_id.to_string()));
    assert!(jsonl_output.contains("VTX-RES-002"));
    assert!(jsonl_output.contains("itag=313"));
    assert!(export_payload.contains(r"<USERPROFILE>\Downloads\video.mkv"));
}
