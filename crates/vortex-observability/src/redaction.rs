//! Zero-Leakage secret redaction utilities (`P0-T10` / `ADR-016` / `Threat-Model.md` TH-08).
//!
//! Ensures credentials, cookies, signed query parameters, and personal user-profile paths
//! never appear in structured logs, Flight Recorder dumps, or diagnostic bundles — even if
//! a caller accidentally logs a raw string or field without wrapping in [`RedactedUrl`] or
//! [`secrecy::SecretString`] (Defense in Depth).

use std::fmt;

#[cfg(feature = "full")]
use url::Url;

/// Lowercase query parameter and structured field names whose values MUST always be redacted.
const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "auth",
    "authorization",
    "proxy_authorization",
    "proxy-authorization",
    "sig",
    "signature",
    "key",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "pwd",
    "pass",
    "session",
    "session_id",
    "sid",
    "cookie",
    "cookies",
    "set-cookie",
    "set_cookie",
    "jwt",
    "bearer",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
    "hdnts",
    "policy",
    "expires",
    "code",
];

/// Returns `true` if `key` (query parameter or structured log field name) holds sensitive data.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEYS.contains(&lower.as_str())
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("signature")
        || lower.contains("passwd")
        || lower.contains("password")
        || lower.contains("credential")
        || lower.contains("cookie")
        || lower.contains("apikey")
        || lower.contains("api_key")
}

/// Wrapper that eagerly strips `user:pass@` and sensitive query/fragment parameters from any URL.
///
/// Both [`fmt::Display`] and [`fmt::Debug`] emit ONLY the redacted representation so raw URLs
/// containing signed tokens or credentials cannot leak into logs even at `TRACE` level.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RedactedUrl {
    sanitized: String,
}

impl RedactedUrl {
    /// Parses and redacts a raw URL string. Malformed URLs are still defensively scrubbed.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        #[cfg(feature = "full")]
        let sanitized = Url::parse(raw).map_or_else(
            |_| fallback_redact_raw_url(raw),
            |url| redact_parsed_url(&url),
        );
        #[cfg(not(feature = "full"))]
        let sanitized = fallback_redact_raw_url(raw);
        Self { sanitized }
    }

    /// Creates a [`RedactedUrl`] from an already-parsed [`Url`].
    #[cfg(feature = "full")]
    #[must_use]
    pub fn from_url(url: &Url) -> Self {
        Self {
            sanitized: redact_parsed_url(url),
        }
    }

    /// Returns the redacted URL string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.sanitized
    }
}

#[cfg(feature = "full")]
impl From<&Url> for RedactedUrl {
    fn from(url: &Url) -> Self {
        Self::from_url(url)
    }
}

impl From<&str> for RedactedUrl {
    fn from(raw: &str) -> Self {
        Self::parse(raw)
    }
}

impl From<&String> for RedactedUrl {
    fn from(raw: &String) -> Self {
        Self::parse(raw)
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sanitized)
    }
}

impl fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RedactedUrl({:?})", self.sanitized)
    }
}

#[cfg(feature = "full")]
fn redact_parsed_url(url: &Url) -> String {
    let mut out = String::with_capacity(url.as_str().len());
    out.push_str(url.scheme());
    out.push(':');

    if url.has_authority() {
        out.push_str("//");
        // Intentionally omit username and password (`user:pass@`) completely.
        if let Some(host) = url.host_str() {
            out.push_str(host);
        }
        if let Some(port) = url.port() {
            out.push(':');
            out.push_str(&port.to_string());
        }
    }

    out.push_str(url.path());

    if url.query().is_some() {
        let mut first = true;
        for (key, value) in url.query_pairs() {
            out.push(if first { '?' } else { '&' });
            first = false;
            out.push_str(&key);
            out.push('=');
            if is_sensitive_key(&key) {
                out.push_str("REDACTED");
            } else {
                out.push_str(&value);
            }
        }
    }

    if let Some(fragment) = url.fragment() {
        out.push('#');
        if fragment.contains('=') {
            out.push_str("REDACTED");
        } else {
            out.push_str(fragment);
        }
    }

    out
}

fn fallback_redact_raw_url(raw: &str) -> String {
    let without_userinfo = if let Some(scheme_pos) = raw.find("://") {
        let after_scheme = raw.get(scheme_pos + 3..).unwrap_or_default();
        let auth_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let authority = after_scheme.get(..auth_end).unwrap_or_default();
        if let Some(at_pos) = authority.rfind('@') {
            let mut rebuilt = String::with_capacity(raw.len());
            rebuilt.push_str(raw.get(..scheme_pos + 3).unwrap_or_default());
            rebuilt.push_str(authority.get(at_pos + 1..).unwrap_or_default());
            rebuilt.push_str(after_scheme.get(auth_end..).unwrap_or_default());
            rebuilt
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };

    if let Some((base, query_and_frag)) = without_userinfo.split_once('?') {
        let (query_part, frag_part) = query_and_frag
            .split_once('#')
            .map_or((query_and_frag, None), |(q, f)| (q, Some(f)));
        let mut out = String::with_capacity(without_userinfo.len());
        out.push_str(base);
        let mut first = true;
        for pair in query_part.split('&') {
            out.push(if first { '?' } else { '&' });
            first = false;
            if let Some((k, v)) = pair.split_once('=') {
                out.push_str(k);
                out.push('=');
                if is_sensitive_key(k) {
                    out.push_str("REDACTED");
                } else {
                    out.push_str(v);
                }
            } else {
                out.push_str(pair);
            }
        }
        if frag_part.is_some() {
            out.push_str("#REDACTED");
        }
        out
    } else {
        without_userinfo
    }
}

/// Scrubs URLs anywhere in text (even inside JSON quotes `"..."`, parentheses `(...)`, or `url=...`),
/// `Bearer`/`Basic` credentials, HTTP auth/cookie headers, and sensitive JSON object fields.
///
/// When `input` is valid JSON (such as a `core.jsonl` line), performs exact AST-level redaction
/// over [`serde_json::Value`] so JSON escaping (`\"`, `\n`, Unicode) can NEVER be corrupted.
#[must_use]
pub fn redact_sensitive_text(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && let Ok(mut json_val) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        redact_json_value(&mut json_val);
        if let Ok(serialized) = serde_json::to_string(&json_val) {
            return serialized;
        }
    }

    redact_plain_text(input)
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json_value(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_json_value(item);
            }
        }
        serde_json::Value::String(text) => {
            if might_contain_unredacted_secret(text.as_bytes()) {
                *text = redact_plain_text(text);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_plain_text(input: &str) -> String {
    let urls_scrubbed = redact_embedded_urls(input);
    redact_headers_and_tokens(&urls_scrubbed)
}

/// Fast-path check used by the non-blocking JSONL sink so clean log lines incur zero allocations.
#[must_use]
pub fn might_contain_unredacted_secret(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    lower.contains("://")
        || lower.contains("bearer ")
        || lower.contains("basic ")
        || lower.contains("cookie")
        || lower.contains("authorization")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("signature")
}

fn redact_embedded_urls(input: &str) -> String {
    const SCHEMES: &[&str] = &["https://", "http://", "sftp://", "ftp://"];
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while cursor < input.len() {
        let rest_lower = lower.get(cursor..).unwrap_or_default();
        let next_match = SCHEMES
            .iter()
            .filter_map(|scheme| rest_lower.find(scheme).map(|pos| cursor + pos))
            .min();

        let Some(url_start) = next_match else {
            out.push_str(input.get(cursor..).unwrap_or_default());
            break;
        };

        out.push_str(input.get(cursor..url_start).unwrap_or_default());
        let after_start = input.get(url_start..).unwrap_or_default();
        let raw_len = after_start
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '>' | '<' | '\\')
            })
            .unwrap_or(after_start.len());
        let candidate = after_start.get(..raw_len).unwrap_or_default();
        let trimmed = candidate.trim_end_matches([',', ';', '.']);
        out.push_str(RedactedUrl::parse(trimmed).as_str());
        cursor = url_start + trimmed.len();
    }

    out
}

fn redact_headers_and_tokens(input: &str) -> String {
    let mut result = String::with_capacity(input.len());

    for (line_idx, line) in input.lines().enumerate() {
        if line_idx > 0 {
            result.push('\n');
        }

        let trimmed_lower = line.trim_start().to_ascii_lowercase();
        if (trimmed_lower.starts_with("cookie:")
            || trimmed_lower.starts_with("set-cookie:")
            || trimmed_lower.starts_with("authorization:")
            || trimmed_lower.starts_with("proxy-authorization:"))
            && let Some((header_name, _)) = line.split_once(':')
        {
            result.push_str(header_name);
            result.push_str(": [REDACTED]");
            continue;
        }

        let mut tokens = line.split_whitespace().peekable();
        let mut first_tok = true;
        while let Some(tok) = tokens.next() {
            if !first_tok {
                result.push(' ');
            }
            first_tok = false;

            let clean_scheme = tok
                .trim_start_matches(['"', '\'', '(', '['])
                .to_ascii_lowercase();
            if (clean_scheme == "bearer" || clean_scheme == "basic")
                && let Some(secret_tok) = tokens.next()
            {
                if secret_tok.starts_with("[REDACTED]") {
                    result.push_str(tok);
                    result.push(' ');
                    result.push_str(secret_tok);
                } else {
                    result.push_str(tok);
                    result.push_str(" [REDACTED]");
                    // Preserve any closing JSON quote / delimiter attached to the token!
                    let trailing_start = secret_tok
                        .rfind(|c: char| !matches!(c, '"' | '\'' | ')' | ']' | ',' | ';' | '}'))
                        .map_or(0, |idx| {
                            idx + secret_tok
                                .get(idx..)
                                .and_then(|s| s.chars().next())
                                .map_or(1, char::len_utf8)
                        });
                    result.push_str(secret_tok.get(trailing_start..).unwrap_or_default());
                }
            } else if !tok.contains("://")
                && let Some((k, v)) = tok.split_once('=')
            {
                let clean_key = k.trim_start_matches(['(', '[', '"', '\'']);
                if is_sensitive_key(clean_key) && !v.starts_with("[REDACTED]") {
                    result.push_str(k);
                    result.push_str("=[REDACTED]");
                    let trailing_start = v
                        .rfind(|c: char| !matches!(c, '"' | '\'' | ')' | ']' | ',' | ';' | '}'))
                        .map_or(0, |idx| {
                            idx + v
                                .get(idx..)
                                .and_then(|s| s.chars().next())
                                .map_or(1, char::len_utf8)
                        });
                    result.push_str(v.get(trailing_start..).unwrap_or_default());
                } else {
                    result.push_str(tok);
                }
            } else {
                result.push_str(tok);
            }
        }
    }

    result
}

/// Replaces personal OS user profile paths (e.g. `C:\Users\Alice`) with `<USERPROFILE>` for export.
#[must_use]
pub fn sanitize_path_for_export(input: &str, explicit_user_profile: Option<&str>) -> String {
    let mut out = input.to_owned();

    if let Some(profile) = explicit_user_profile {
        if !profile.is_empty() {
            out = replace_case_insensitive(&out, profile, "<USERPROFILE>");
            let forward = profile.replace('\\', "/");
            out = replace_case_insensitive(&out, &forward, "<USERPROFILE>");
        }
    } else if let Ok(env_profile) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))
        && !env_profile.is_empty()
    {
        out = replace_case_insensitive(&out, &env_profile, "<USERPROFILE>");
        let forward = env_profile.replace('\\', "/");
        out = replace_case_insensitive(&out, &forward, "<USERPROFILE>");
    }

    out = scrub_user_segment(&out, "\\Users\\", '\\');
    out = scrub_user_segment(&out, "/Users/", '/');
    out = scrub_user_segment(&out, "/home/", '/');
    out
}

fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let lower_haystack = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;

    while let Some(rel_idx) = lower_haystack
        .get(cursor..)
        .and_then(|s| s.find(&lower_needle))
    {
        let abs_idx = cursor + rel_idx;
        if let Some(prefix) = haystack.get(cursor..abs_idx) {
            out.push_str(prefix);
        }
        out.push_str(replacement);
        cursor = abs_idx + needle.len();
    }

    if let Some(tail) = haystack.get(cursor..) {
        out.push_str(tail);
    }
    out
}

fn scrub_user_segment(input: &str, marker: &str, sep: char) -> String {
    let lower_input = input.to_ascii_lowercase();
    let lower_marker = marker.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(rel_pos) = lower_input
        .get(cursor..)
        .and_then(|s| s.find(&lower_marker))
    {
        let marker_start = cursor + rel_pos;
        let user_start = marker_start + marker.len();
        let rest = input.get(user_start..).unwrap_or_default();
        let user_end_rel = rest
            .find(|c: char| {
                c == sep || c == '/' || c == '\\' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(rest.len());
        let user_name = rest.get(..user_end_rel).unwrap_or_default();

        let prefix_trim = if marker == "\\Users\\" && marker_start >= 2 {
            let maybe_drive = input
                .get(marker_start - 2..marker_start)
                .unwrap_or_default();
            if maybe_drive.len() == 2
                && maybe_drive
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphabetic)
                && maybe_drive.as_bytes().get(1) == Some(&b':')
            {
                2
            } else {
                0
            }
        } else {
            0
        };

        if user_name.is_empty() || user_name == "<USERPROFILE>" {
            if let Some(chunk) = input.get(cursor..user_start + user_end_rel) {
                out.push_str(chunk);
            }
        } else {
            if let Some(prefix) = input.get(cursor..marker_start - prefix_trim) {
                out.push_str(prefix);
            }
            out.push_str("<USERPROFILE>");
        }
        cursor = user_start + user_end_rel;
    }

    if let Some(tail) = input.get(cursor..) {
        out.push_str(tail);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn redacted_url_removes_credentials_and_sensitive_params_while_keeping_safe_params() {
        let raw = "https://alice:TopSecret99@cdn.example.com:8443/dl/file.zip?X-Amz-Signature=deadbeef&token=abc123&page=2&lang=ar#access_token=fragsecret";
        let redacted = RedactedUrl::parse(raw);
        let disp = redacted.to_string();
        let dbg = format!("{redacted:?}");

        assert!(!disp.contains("alice"));
        assert!(!disp.contains("TopSecret99"));
        assert!(!disp.contains("deadbeef"));
        assert!(!disp.contains("abc123"));
        assert!(!disp.contains("fragsecret"));
        assert!(disp.contains("https://cdn.example.com:8443/dl/file.zip"));
        assert!(disp.contains("page=2"));
        assert!(disp.contains("lang=ar"));
        assert!(disp.contains("X-Amz-Signature=REDACTED"));
        assert!(disp.contains("token=REDACTED"));

        assert!(!dbg.contains("TopSecret99"));
        assert!(!dbg.contains("deadbeef"));
    }

    #[test]
    fn redact_sensitive_text_handles_quoted_json_urls_and_sensitive_fields() {
        let json_line = r#"{"level":"ERROR","fields":{"cookies":"cf_clearance=SECRET_CF_1","url":"https://bob:SECRET_PW_2@cdn.test/a.zip?sig=SECRET_SIG_3","auth":"Bearer SECRET_JWT_4","note":"(https://u:SECRET_PW_5@example.com/x)"}}"#;
        let clean = redact_sensitive_text(json_line);

        for secret in [
            "SECRET_CF_1",
            "SECRET_PW_2",
            "SECRET_SIG_3",
            "SECRET_JWT_4",
            "SECRET_PW_5",
        ] {
            assert!(!clean.contains(secret), "leaked `{secret}` in `{clean}`");
        }
        // Must remain valid JSON!
        let parsed: serde_json::Value = serde_json::from_str(&clean).unwrap();
        assert_eq!(parsed.get("level"), Some(&serde_json::json!("ERROR")));
    }

    #[test]
    fn sanitize_path_replaces_windows_and_unix_profiles_and_arabic_names() {
        let win =
            sanitize_path_for_export(r"C:\Users\أحمد\Downloads\ملف.zip", Some(r"C:\Users\أحمد"));
        assert_eq!(win, r"<USERPROFILE>\Downloads\ملف.zip");

        let generic_win = sanitize_path_for_export(r"D:\Users\OtherPerson\file.iso", None);
        assert_eq!(generic_win, r"<USERPROFILE>\file.iso");

        let unix = sanitize_path_for_export("/home/secretuser/Downloads/a.bin", None);
        assert_eq!(unix, "<USERPROFILE>/Downloads/a.bin");
    }

    proptest! {
        #[test]
        fn prop_redacted_url_never_leaks_credentials_or_tokens(
            user in "[a-zA-Z]{6,12}",
            pass in "[a-zA-Z0-9]{8,16}",
            tok in "[a-zA-Z0-9]{10,20}",
            sig in "[a-zA-Z0-9]{10,20}",
        ) {
            let raw = format!(
                "https://{user}:{pass}@cdn.example.com/archive.bin?token={tok}&X-Amz-Signature={sig}&safe=ok"
            );
            let redacted = RedactedUrl::parse(&raw);
            let display = redacted.to_string();
            let debug = format!("{redacted:?}");

            prop_assert!(!display.contains(&pass));
            prop_assert!(!display.contains(&tok));
            prop_assert!(!display.contains(&sig));
            prop_assert!(!debug.contains(&pass));
            prop_assert!(!debug.contains(&tok));
            prop_assert!(!debug.contains(&sig));
            prop_assert!(display.contains("safe=ok"));
        }
    }
}
