//! RFC 9110 / RFC 6266 / RFC 5987 / RFC 2047 HTTP header parsers & SSRF IP classification
//! (`P1-T15` & `P1-T16` / `TH-01`, `TH-04`, `TH-10`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::UNIX_EPOCH;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::HeaderMap;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, DATE, ETAG, LAST_MODIFIED, RETRY_AFTER};
use url::Url;
use vortex_domain::{EtagValidator, FilenameSanitizer, LastModifiedValidator, ResumeValidators};
use vortex_error::{ErrorCode, TraceId, VortexError};

/// Parses an HTTP-date (`Wed, 25 Nov 2026 12:00:00 GMT`) into Unix epoch seconds.
#[must_use]
pub fn parse_http_date_epoch_secs(raw: &str) -> Option<i64> {
    let sys_time = httpdate::parse_http_date(raw.trim()).ok()?;
    let dur = sys_time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(dur.as_secs()).ok()
}

/// Parses `Retry-After` (either delta-seconds `"12"` or HTTP-date) into milliseconds (`§8.6.2`).
#[must_use]
pub fn parse_retry_after_ms(headers: &HeaderMap, now_unix_ms: i64) -> Option<u64> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs.saturating_mul(1_000));
    }
    let target_secs = parse_http_date_epoch_secs(raw)?;
    let target_ms = target_secs.saturating_mul(1_000);
    let diff_ms = target_ms.saturating_sub(now_unix_ms).max(0);
    u64::try_from(diff_ms).ok()
}

/// Extracts [`ResumeValidators`] (`ETag`, `Last-Modified` + origin `Date` strength, `Content-Length`).
#[must_use]
pub fn extract_resume_validators(headers: &HeaderMap, total_size: Option<u64>) -> ResumeValidators {
    let etag = headers
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .and_then(EtagValidator::parse);

    let date_secs = headers
        .get(DATE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date_epoch_secs);

    let last_modified = headers
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw_lm| LastModifiedValidator {
            raw_http_date: raw_lm.to_owned(),
            last_modified_epoch_secs: parse_http_date_epoch_secs(raw_lm),
            response_date_epoch_secs: date_secs,
        });

    let content_length = total_size.or_else(|| {
        headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    });

    ResumeValidators {
        etag,
        last_modified,
        content_length,
    }
}

/// Parsed HTTP `Content-Range` header (`bytes start-end/total` or `bytes */total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedContentRange {
    /// Satisfied byte interval `bytes start-end/total_opt`.
    ByteInterval {
        /// Inclusive start offset.
        start: u64,
        /// Inclusive end offset (`end >= start`).
        end_inclusive: u64,
        /// Total resource size (`None` if `*`).
        total: Option<u64>,
    },
    /// Unsatisfiable `416` range response `bytes */total`.
    Unsatisfied {
        /// Authoritative total size (`0` on strict 0-byte resources!).
        total: u64,
    },
}

/// Strictly parses RFC 9110 `Content-Range` (`TH-10` & `P1-T16`).
///
/// # Errors
/// Returns `VTX-NET-005` (`NetMalformedResponse`) if syntax or numeric invariants (`start <= end < total`) fail.
pub fn parse_content_range(
    raw: &str,
    trace_id: TraceId,
) -> Result<ParsedContentRange, VortexError> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed
        .strip_prefix("bytes ")
        .or_else(|| trimmed.strip_prefix("bytes="))
    else {
        return Err(VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Invalid Content-Range unit: `{trimmed}`"),
        ));
    };

    let Some((range_part, total_part)) = rest.trim().split_once('/') else {
        return Err(VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Malformed Content-Range missing '/': `{trimmed}`"),
        ));
    };

    if range_part.trim() == "*" {
        let total = total_part.trim().parse::<u64>().map_err(|_| {
            VortexError::coded(
                ErrorCode::NetMalformedResponse,
                trace_id,
                format!("Invalid unsatisfied Content-Range total: `{trimmed}`"),
            )
        })?;
        return Ok(ParsedContentRange::Unsatisfied { total });
    }

    let Some((start_s, end_s)) = range_part.trim().split_once('-') else {
        return Err(VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Invalid Content-Range bounds: `{trimmed}`"),
        ));
    };
    let start = start_s.trim().parse::<u64>().map_err(|_| {
        VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Invalid Content-Range start: `{trimmed}`"),
        )
    })?;
    let end_inclusive = end_s.trim().parse::<u64>().map_err(|_| {
        VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Invalid Content-Range end: `{trimmed}`"),
        )
    })?;
    if start > end_inclusive {
        return Err(VortexError::coded(
            ErrorCode::NetMalformedResponse,
            trace_id,
            format!("Content-Range start ({start}) > end ({end_inclusive})"),
        ));
    }

    let total = if total_part.trim() == "*" {
        None
    } else {
        let tot = total_part.trim().parse::<u64>().map_err(|_| {
            VortexError::coded(
                ErrorCode::NetMalformedResponse,
                trace_id,
                format!("Invalid Content-Range total: `{trimmed}`"),
            )
        })?;
        if end_inclusive >= tot {
            return Err(VortexError::coded(
                ErrorCode::NetMalformedResponse,
                trace_id,
                format!("Content-Range end ({end_inclusive}) >= total ({tot})"),
            ));
        }
        Some(tot)
    };

    Ok(ParsedContentRange::ByteInterval {
        start,
        end_inclusive,
        total,
    })
}

/// Parses `Content-Disposition` returning `(raw_decoded_filename, sanitized_filename)` so
/// callers can inspect raw names for Unicode `RTLO`/bidi spoofing (`TH-30`) before sanitization.
#[must_use]
pub fn parse_disposition_raw_and_sanitized(
    headers: &HeaderMap,
) -> (Option<String>, Option<String>) {
    let Some(raw_hdr) = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
    else {
        return (None, None);
    };
    let Some(raw_decoded) = parse_disposition_raw_str(raw_hdr) else {
        return (None, None);
    };
    let sanitized = FilenameSanitizer::sanitize_untrusted(&raw_decoded);
    (Some(raw_decoded), Some(sanitized))
}

/// Parses `Content-Disposition` supporting RFC 5987 (`filename*=UTF-8''...`), RFC 2047
/// (`=?UTF-8?B?...?=` / `=?UTF-8?Q?...?=`), and classic `filename="..."`, then sanitizes via
/// [`FilenameSanitizer::sanitize_untrusted`] (`P1-T15` & `TH-01`).
#[must_use]
pub fn parse_content_disposition_filename(headers: &HeaderMap) -> Option<String> {
    parse_disposition_raw_and_sanitized(headers).1
}

/// Parses raw `Content-Disposition` header text into a sanitized filename.
#[must_use]
pub fn parse_disposition_str(raw: &str) -> Option<String> {
    let chosen = parse_disposition_raw_str(raw)?;
    Some(FilenameSanitizer::sanitize_untrusted(&chosen))
}

fn parse_disposition_raw_str(raw: &str) -> Option<String> {
    let mut ext_filename_star: Option<String> = None;
    let mut plain_filename: Option<String> = None;

    for segment in raw.split(';').skip(1) {
        let trimmed = segment.trim();
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = unquote(v.trim());

        if key == "filename*" {
            if let Some(decoded) = decode_rfc5987(val) {
                ext_filename_star = Some(decoded);
            }
        } else if key == "filename" && plain_filename.is_none() {
            let decoded = decode_rfc2047_if_present(val);
            plain_filename = Some(decoded);
        }
    }

    // RFC 6266 §4.3: `filename*` MUST take precedence over `filename` when both are present!
    let chosen = ext_filename_star.or(plain_filename)?;
    if chosen.trim().is_empty() {
        return None;
    }
    Some(chosen)
}

fn unquote(val: &str) -> &str {
    if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        val.get(1..val.len() - 1).unwrap_or(val)
    } else {
        val
    }
}

/// Decodes RFC 5987 `charset'lang'%XX...` (e.g. `UTF-8''%D8%AA%D9%82%D8%B1%D9%8A%D8%B1-2026.pdf`).
fn decode_rfc5987(val: &str) -> Option<String> {
    let (_, after_charset) = val.split_once('\'')?;
    let (_, encoded) = after_charset.split_once('\'')?;
    Some(percent_decode_bytes(encoded))
}

/// Decodes RFC 2047 encoded-words (`=?UTF-8?B?...?=` or `=?UTF-8?Q?...?=`).
fn decode_rfc2047_if_present(val: &str) -> String {
    let trimmed = val.trim();
    if let Some(inner) = trimmed
        .strip_prefix("=?")
        .and_then(|s| s.strip_suffix("?="))
    {
        let mut parts = inner.splitn(3, '?');
        if let (Some(_charset), Some(enc), Some(payload)) =
            (parts.next(), parts.next(), parts.next())
        {
            if enc.eq_ignore_ascii_case("B")
                && let Ok(bytes) = BASE64.decode(payload.as_bytes())
            {
                return String::from_utf8_lossy(&bytes).into_owned();
            }
            if enc.eq_ignore_ascii_case("Q") {
                let replaced = payload.replace('_', " ");
                return quoted_printable_decode(&replaced);
            }
        }
    }
    trimmed.to_owned()
}

fn percent_decode_bytes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes.get(idx) == Some(&b'%')
            && let (Some(&h), Some(&l)) = (bytes.get(idx + 1), bytes.get(idx + 2))
            && let (Some(hv), Some(lv)) = (hex_nibble(h), hex_nibble(l))
        {
            out.push((hv << 4) | lv);
            idx += 3;
            continue;
        }
        if let Some(&b) = bytes.get(idx) {
            out.push(b);
        }
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn quoted_printable_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes.get(idx) == Some(&b'=')
            && let (Some(&h), Some(&l)) = (bytes.get(idx + 1), bytes.get(idx + 2))
            && let (Some(hv), Some(lv)) = (hex_nibble(h), hex_nibble(l))
        {
            out.push((hv << 4) | lv);
            idx += 3;
            continue;
        }
        if let Some(&b) = bytes.get(idx) {
            out.push(b);
        }
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Returns `true` if `a` and `b` share the exact same `(scheme, host, port)` origin.
#[must_use]
pub fn is_same_origin(a: &Url, b: &Url) -> bool {
    a.scheme().eq_ignore_ascii_case(b.scheme())
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Returns `true` if `ip` is loopback, private (RFC 1918 / ULA), link-local (`169.254.169.254` AWS/IMDS!),
/// CGNAT (`100.64.0.0/10`), unspecified, or multicast — forbidden as an automatic Cross-Origin Redirect
/// target (`TH-04` SSRF + DNS Rebinding protection).
#[must_use]
pub const fn is_forbidden_ssrf_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped_v4) = v6.to_ipv4_mapped() {
                return is_forbidden_v4(mapped_v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local_v6(v6)
                || is_unicast_link_local_v6(v6)
        }
    }
}

const fn is_forbidden_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        // Shared CGNAT space `100.64.0.0/10`
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
}

const fn is_unique_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

const fn is_unicast_link_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}
