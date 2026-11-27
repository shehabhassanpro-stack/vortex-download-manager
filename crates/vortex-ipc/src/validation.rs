//! Strict IPC input validation and Anti-DoS boundary guards (`Protocol-Spec.md` §5 / `TH-03`, `TH-05`, `TH-14`).

use vortex_error::{ErrorCode, TraceId, VortexError};
use vortex_observability::RedactedUrl;

use crate::proto;

/// Maximum URL / `final_url` / `referrer` length (`8,192` bytes).
pub const MAX_URL_BYTES: usize = 8_192;

/// Maximum suggested filename length (`512` bytes).
pub const MAX_SUGGESTED_NAME_BYTES: usize = 512;

/// Maximum number of custom HTTP headers per handoff (`64`).
pub const MAX_HEADER_COUNT: usize = 64;

/// Maximum length of any single HTTP header value (`8 KiB`).
pub const MAX_HEADER_VALUE_BYTES: usize = 8_192;

/// Maximum total size of transferred cookies string (`64 KiB`).
pub const MAX_COOKIES_BYTES: usize = 64 * 1024;

/// Maximum length of an HTTP header name (`256` bytes).
pub const MAX_HEADER_NAME_BYTES: usize = 256;

/// Maximum length of `Envelope` identifier strings (`message_id`, `reply_to`, `trace_id`).
pub const MAX_ENVELOPE_ID_BYTES: usize = 64;

/// Validates [`proto::Envelope`] header metadata before dispatching its body (`Protocol-Spec.md` §2 & §5).
///
/// # Errors
/// Returns `VTX-IPC-003` if `protocol_version == 0` or identifier fields are empty/oversized/malformed.
pub fn validate_envelope_header(envelope: &proto::Envelope) -> Result<TraceId, VortexError> {
    let fallback_trace = TraceId::new();
    if envelope.protocol_version == 0 {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            fallback_trace,
            "Envelope.protocol_version must be >= 1",
        ));
    }
    if envelope.message_id.is_empty()
        || envelope.message_id.len() > MAX_ENVELOPE_ID_BYTES
        || envelope.message_id.chars().any(char::is_control)
    {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            fallback_trace,
            "Envelope.message_id empty, oversized, or contains control characters",
        ));
    }
    if envelope.reply_to.len() > MAX_ENVELOPE_ID_BYTES
        || envelope.reply_to.chars().any(char::is_control)
    {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            fallback_trace,
            "Envelope.reply_to oversized or contains control characters",
        ));
    }
    let trace_id = envelope.trace_id.parse::<TraceId>().map_err(|_| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            fallback_trace,
            "Envelope.trace_id must be a valid UUID",
        )
    })?;
    if envelope.body.is_none() {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            "Envelope.body must be populated",
        ));
    }
    Ok(trace_id)
}

fn is_allowed_scheme(raw_url: &str) -> bool {
    let trimmed = raw_url.trim();
    // Explicitly reject Windows UNC paths (`\\server\share`) and non-http(s) schemes (TH-03)
    if trimmed.starts_with(r"\\") || trimmed.starts_with("//") {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Validates a [`proto::DownloadHandoff`] message against all `Protocol-Spec.md` §5 limits.
///
/// # Errors
/// Returns `VTX-SEC-001` if any protocol allowlist, UNC guard, or size cap is violated.
pub fn validate_download_handoff(
    handoff: &proto::DownloadHandoff,
    trace_id: TraceId,
) -> Result<(), VortexError> {
    if handoff.url.is_empty() || handoff.url.len() > MAX_URL_BYTES {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.url empty or exceeds 8192 bytes",
        ));
    }

    if !is_allowed_scheme(&handoff.url) {
        let safe = RedactedUrl::parse(&handoff.url);
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!("handoff.url scheme rejected by allowlist: {safe}"),
        ));
    }

    if !handoff.final_url.is_empty()
        && (handoff.final_url.len() > MAX_URL_BYTES || !is_allowed_scheme(&handoff.final_url))
    {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.final_url rejected by scheme allowlist or length limit",
        ));
    }

    if !handoff.referrer.is_empty()
        && (handoff.referrer.len() > MAX_URL_BYTES || !is_allowed_scheme(&handoff.referrer))
    {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.referrer rejected by scheme allowlist or length limit",
        ));
    }

    if handoff.suggested_name.len() > MAX_SUGGESTED_NAME_BYTES
        || handoff.suggested_name.contains('\0')
    {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.suggested_name exceeds 512 bytes or contains NUL byte",
        ));
    }

    if handoff.headers.len() > MAX_HEADER_COUNT {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.headers exceeds maximum of 64 entries",
        ));
    }

    for (name, value) in &handoff.headers {
        if name.is_empty()
            || name.len() > MAX_HEADER_NAME_BYTES
            || name.contains(['\r', '\n', '\0'])
            || value.len() > MAX_HEADER_VALUE_BYTES
            || value.contains(['\r', '\n', '\0'])
        {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                "handoff.headers entry violates CRLF/NUL hygiene or 8 KiB cap",
            ));
        }
    }

    if handoff.cookies.len() > MAX_COOKIES_BYTES || handoff.cookies.contains(['\r', '\n', '\0']) {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "handoff.cookies exceeds 64 KiB cap or contains control characters",
        ));
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn valid_handoff() -> proto::DownloadHandoff {
        proto::DownloadHandoff {
            url: "https://example.com/archive.zip".to_owned(),
            final_url: "https://cdn.example.com/archive.zip".to_owned(),
            referrer: "https://example.com/downloads".to_owned(),
            suggested_name: "archive.zip".to_owned(),
            mime_type: "application/zip".to_owned(),
            size_bytes: Some(1_048_576),
            user_agent: "Mozilla/5.0".to_owned(),
            cookies: String::new(),
            headers: BTreeMap::new(),
            incognito: false,
            source: "determining-filename".to_owned(),
            auto_start: false,
            top_level_site: "https://example.com".to_owned(),
        }
    }

    #[test]
    fn rejects_unc_file_and_javascript_schemes() {
        let tid = TraceId::new();
        for bad_url in [
            r"\\attacker\share\evil.exe",
            "file:///C:/Windows/System32/cmd.exe",
            "javascript:alert(1)",
            "data:text/plain;base64,SGVsbG8=",
            "smb://attacker/share/a.bin",
        ] {
            let mut h = valid_handoff();
            h.url = bad_url.to_owned();
            assert_eq!(
                validate_download_handoff(&h, tid).unwrap_err().code(),
                ErrorCode::SecPolicyViolation,
                "failed to reject `{bad_url}`"
            );
        }
    }

    #[test]
    fn rejects_oversized_headers_and_nul_filename() {
        let tid = TraceId::new();
        let mut h = valid_handoff();
        h.suggested_name = "bad\0name.zip".to_owned();
        assert_eq!(
            validate_download_handoff(&h, tid).unwrap_err().code(),
            ErrorCode::SecPolicyViolation
        );

        let mut h2 = valid_handoff();
        for i in 0..=MAX_HEADER_COUNT {
            h2.headers.insert(format!("X-H-{i}"), "ok".to_owned());
        }
        assert_eq!(
            validate_download_handoff(&h2, tid).unwrap_err().code(),
            ErrorCode::SecPolicyViolation
        );
    }

    #[test]
    fn accepts_valid_handoff_and_rejects_crlf_cookies_and_bad_final_or_referrer() {
        let tid = TraceId::new();
        assert!(validate_download_handoff(&valid_handoff(), tid).is_ok());

        let mut bad_final = valid_handoff();
        bad_final.final_url = "file:///etc/passwd".to_owned();
        assert_eq!(
            validate_download_handoff(&bad_final, tid)
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );

        let mut bad_ref = valid_handoff();
        bad_ref.referrer = "javascript:void(0)".to_owned();
        assert_eq!(
            validate_download_handoff(&bad_ref, tid).unwrap_err().code(),
            ErrorCode::SecPolicyViolation
        );

        let mut bad_hdr = valid_handoff();
        bad_hdr
            .headers
            .insert("X-Test".to_owned(), "bad\r\nInjected: 1".to_owned());
        assert_eq!(
            validate_download_handoff(&bad_hdr, tid).unwrap_err().code(),
            ErrorCode::SecPolicyViolation
        );

        let mut bad_cookie = valid_handoff();
        bad_cookie.cookies = "a=1\r\nb=2".to_owned();
        assert_eq!(
            validate_download_handoff(&bad_cookie, tid)
                .unwrap_err()
                .code(),
            ErrorCode::SecPolicyViolation
        );
    }
}
