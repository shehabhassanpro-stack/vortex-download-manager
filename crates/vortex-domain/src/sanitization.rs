//! Filename & URL sanitization (`TH-01`..`TH-03`) and automatic file categorization (`P1-T05`).
//!
//! Pure string/path logic (zero `std::fs` calls) protecting Windows and POSIX filesystems against:
//! - Path traversal (`../`, `..\`, absolute paths, drive letters)
//! - Forbidden schemes & UNC NTLM hash leaks (`file://`, `javascript:`, `\\server\share`)
//! - Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`, `LPT1`..`LPT9`, `COM¹`..`LPT³`)
//! - Right-to-Left Override (`U+202E`) extension spoofing and control characters
//! - Trailing dots/spaces and deterministic collision numbering (`file (1).zip`).

use std::path::{Component, Path, PathBuf};

use url::Url;
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::DomainResult;
use crate::value_objects::CategoryId;

/// Maximum UTF-8 byte length of a sanitized filename so `<save_dir>/<name>.vortex.meta` stays clean.
pub const MAX_FILENAME_BYTES: usize = 200;

/// Fallback filename when untrusted input reduces to an empty string.
pub const DEFAULT_FALLBACK_FILENAME: &str = "download.bin";

const WINDOWS_RESERVED_DEVICES: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{00B9}",
    "COM\u{00B2}",
    "COM\u{00B3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{00B9}",
    "LPT\u{00B2}",
    "LPT\u{00B3}",
];

/// Security sanitizer for URLs, untrusted `Content-Disposition` filenames, and target paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilenameSanitizer;

impl FilenameSanitizer {
    /// Extracts a normalized lowercase `scheme://host:port` origin key for an HTTP(S) URL (`TH-28`).
    #[must_use]
    pub fn origin_key_from_url(raw_url: &str) -> Option<String> {
        let parsed = Url::parse(raw_url.trim()).ok()?;
        let scheme = parsed.scheme().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?.to_ascii_lowercase();
        let port = parsed.port_or_known_default()?;
        Some(format!("{scheme}://{host}:{port}"))
    }

    /// Validates that `raw_url` uses an allowed protocol (`http` or `https`) and is NOT a UNC path
    /// (`\\server\share` / `//server/share`) or dangerous scheme (`file:`, `javascript:`, `data:` — `TH-03`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if `raw_url` is malformed, uses a forbidden scheme, or lacks a valid host.
    pub fn validate_download_url(raw_url: &str) -> DomainResult<Url> {
        let trimmed = raw_url.trim();
        if trimmed.is_empty()
            || trimmed.starts_with(r"\\")
            || trimmed.starts_with("//")
            || trimmed.contains('\0')
        {
            return Err(Self::sec_err(
                "errors.security.forbidden_url",
                "UNC network paths and null bytes are strictly prohibited (TH-03)",
            ));
        }

        let parsed = Url::parse(trimmed).map_err(|err| {
            Self::sec_err(
                "errors.security.invalid_url",
                format!("Malformed download URL: {err}"),
            )
        })?;

        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(Self::sec_err(
                    "errors.security.forbidden_scheme",
                    format!("URL scheme `{other}:` is prohibited; only http/https allowed"),
                ));
            }
        }

        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(Self::sec_err(
                "errors.security.invalid_url",
                "Download URL must include a non-empty host",
            ));
        }

        Ok(parsed)
    }

    /// Strictly validates a user- or IPC-supplied filename, rejecting (`VTX-SEC-001`) any path
    /// traversal (`..`), path separators (`/` or `\`), drive prefixes (`:`), or null bytes (`P1-T05`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` on any path traversal or directory escape attempt.
    pub fn validate_strict_filename(name: &str) -> DomainResult<String> {
        if name.is_empty()
            || name.contains('\0')
            || name.contains('/')
            || name.contains('\\')
            || name.contains(':')
            || name == "."
            || name == ".."
        {
            return Err(Self::sec_err(
                "errors.security.path_traversal",
                "Filename contains forbidden path traversal or separator characters (TH-01)",
            ));
        }
        Ok(Self::sanitize_basename(name))
    }

    /// Sanitizes an untrusted filename from `Content-Disposition` or URL path into a safe single
    /// basename (stripping directory prefixes, Win32 forbidden characters, Bidi spoofing overrides,
    /// trailing dots/spaces, and neutralizing `CON`..`LPT9`).
    #[must_use]
    pub fn sanitize_untrusted(raw: &str) -> String {
        // Extract basename after any `/` or `\` sent by broken/hostile HTTP servers (`TH-01`)
        let basename = raw
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty() && *part != "." && *part != "..")
            .unwrap_or("");
        Self::sanitize_basename(basename)
    }

    /// Derives a safe filename from a validated [`Url`] when `Content-Disposition` is absent.
    #[must_use]
    pub fn filename_from_url(url: &Url) -> String {
        let candidate = url
            .path_segments()
            .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
            .unwrap_or("");
        let decoded = percent_decode_utf8_lossy(candidate);
        Self::sanitize_basename(&decoded)
    }

    /// Joins `save_dir` and `file_name` after verifying `file_name` resolves to a single normal
    /// child entry with zero parent/root escapes (`TH-01`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if `file_name` is not a single safe `Component::Normal` child.
    pub fn safe_join(save_dir: &Path, file_name: &str) -> DomainResult<PathBuf> {
        let clean = Self::validate_strict_filename(file_name)?;
        let candidate_rel = Path::new(&clean);
        let mut comps = candidate_rel.components();
        let first = comps.next();
        let second = comps.next();
        if !matches!(first, Some(Component::Normal(_))) || second.is_some() {
            return Err(Self::sec_err(
                "errors.security.path_traversal",
                "Resolved filename must be a single child path component",
            ));
        }
        Ok(save_dir.join(clean))
    }

    /// Resolves filename collisions purely via `exists` predicate:
    /// `report.pdf` -> `report (1).pdf` -> `report (2).pdf` (preserving compound `.tar.gz`).
    #[must_use]
    pub fn resolve_collision<F>(sanitized_name: &str, mut exists: F) -> String
    where
        F: FnMut(&str) -> bool,
    {
        if !exists(sanitized_name) {
            return sanitized_name.to_owned();
        }
        let (stem, ext) = split_stem_and_extension(sanitized_name);
        for counter in 1_u32..=100_000 {
            let candidate = if ext.is_empty() {
                format!("{stem} ({counter})")
            } else {
                format!("{stem} ({counter}).{ext}")
            };
            if !exists(&candidate) {
                return candidate;
            }
        }
        format!("{stem} ({})", uuid::Uuid::now_v7())
    }

    /// Detects Unicode Bidi overrides (`RTLO U+202E`, `U+202A..U+202E`, `U+2066..U+2069`) or
    /// zero-width spoofing characters (`U+200B..U+200D`, `U+FEFF`) in an untrusted filename (`TH-30`).
    #[must_use]
    pub fn contains_bidi_or_spoofing_chars(raw: &str) -> bool {
        raw.chars().any(is_bidi_or_zero_width_spoof)
    }

    fn sanitize_basename(input: &str) -> String {
        let mut cleaned = String::with_capacity(input.len());
        for ch in input.chars() {
            if is_forbidden_char(ch) {
                cleaned.push('_');
            } else {
                cleaned.push(ch);
            }
        }

        // Trim leading whitespace and trailing dots/spaces (Win32 strips trailing dots/spaces silently!)
        let trimmed = cleaned
            .trim_start()
            .trim_end_matches(|c: char| c == '.' || c.is_whitespace());

        // Prevent hidden dot-only names or empty output
        let non_empty = if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
            DEFAULT_FALLBACK_FILENAME.to_owned()
        } else {
            trimmed.to_owned()
        };

        // Neutralize Windows reserved device names (`CON`, `CON.txt`, `NUL.tar.gz`, `COM¹` -> `_CON.txt`)
        let stem_before_dot = non_empty.split('.').next().unwrap_or(&non_empty).trim();
        let neutralized = if is_windows_reserved_device(stem_before_dot) {
            format!("_{non_empty}")
        } else {
            non_empty
        };

        truncate_preserving_extension(&neutralized, MAX_FILENAME_BYTES)
    }

    fn sec_err(_key: &'static str, detail: impl Into<String>) -> VortexError {
        VortexError::coded(ErrorCode::SecPolicyViolation, TraceId::new(), detail)
    }
}

fn is_bidi_or_zero_width_spoof(ch: char) -> bool {
    matches!(
        ch,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200B}'..='\u{200F}'
            | '\u{FEFF}'
            | '\u{061C}'
    )
}

fn is_forbidden_char(ch: char) -> bool {
    matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        || ch.is_control()
        // Block Unicode Bidi Override / Isolate & Zero-Width spoofing chars (`TH-01`, `TH-30`)
        || is_bidi_or_zero_width_spoof(ch)
}

fn is_windows_reserved_device(stem: &str) -> bool {
    WINDOWS_RESERVED_DEVICES
        .iter()
        .any(|&dev| stem.eq_ignore_ascii_case(dev) || stem.to_uppercase() == dev)
}

fn split_stem_and_extension(name: &str) -> (&str, &str) {
    for compound in ["tar.gz", "tar.xz", "tar.bz2", "tar.zst"] {
        let suffix = format!(".{compound}");
        if name.len() > suffix.len()
            && name
                .get(name.len() - suffix.len()..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(&suffix))
            && let Some(stem) = name.get(..name.len() - suffix.len())
            && !stem.is_empty()
        {
            let ext = name.get(name.len() - compound.len()..).unwrap_or(compound);
            return (stem, ext);
        }
    }

    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 16
    {
        return (stem, ext);
    }
    (name, "")
}

fn truncate_preserving_extension(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }
    let (stem, ext) = split_stem_and_extension(name);
    let suffix = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    let budget = max_bytes.saturating_sub(suffix.len()).max(1);
    let mut cut = 0_usize;
    for (idx, ch) in stem.char_indices() {
        if idx + ch.len_utf8() > budget {
            break;
        }
        cut = idx + ch.len_utf8();
    }
    let truncated_stem = stem
        .get(..cut)
        .unwrap_or("download")
        .trim_end_matches(['.', ' ']);
    let safe_stem = if truncated_stem.is_empty() {
        "download"
    } else {
        truncated_stem
    };
    format!("{safe_stem}{suffix}")
}

fn percent_decode_utf8_lossy(input: &str) -> String {
    let raw = input.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw.get(i) == Some(&b'%')
            && let (Some(&hi), Some(&lo)) = (raw.get(i + 1), raw.get(i + 2))
            && let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        if let Some(&b) = raw.get(i) {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Built-in category definition mapping extensions to a default subfolder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryRule {
    /// Canonical category ID.
    pub id: CategoryId,
    /// Default subfolder under user's `Downloads` directory.
    pub default_subfolder: &'static str,
}

/// Automatic categorization policy (`P1-T05` / `Vortex-Architecture-Design.md` §15.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct CategorizationPolicy;

impl CategorizationPolicy {
    /// Classifies a file by its extension first, falling back to `mime_type` when extension is unknown.
    #[must_use]
    pub fn classify(file_name: &str, mime_type: Option<&str>) -> CategoryRule {
        let (_, ext_raw) = split_stem_and_extension(file_name);
        let ext = ext_raw
            .rsplit('.')
            .next()
            .unwrap_or(ext_raw)
            .to_ascii_lowercase();

        match ext.as_str() {
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "ts" => CategoryRule {
                id: CategoryId::builtin(CategoryId::VIDEO),
                default_subfolder: "Video",
            },
            "mp3" | "m4a" | "flac" | "wav" | "ogg" | "opus" | "aac" | "wma" => CategoryRule {
                id: CategoryId::builtin(CategoryId::AUDIO),
                default_subfolder: "Music",
            },
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" | "img" => {
                CategoryRule {
                    id: CategoryId::builtin(CategoryId::COMPRESSED),
                    default_subfolder: "Compressed",
                }
            }
            "exe" | "msi" | "msix" | "appx" | "dmg" | "pkg" | "deb" | "rpm" | "apk"
            | "appimage" => CategoryRule {
                id: CategoryId::builtin(CategoryId::PROGRAMS),
                default_subfolder: "Programs",
            },
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "epub"
            | "csv" => CategoryRule {
                id: CategoryId::builtin(CategoryId::DOCUMENTS),
                default_subfolder: "Documents",
            },
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" | "avif" | "bmp" | "ico" | "tiff" => {
                CategoryRule {
                    id: CategoryId::builtin(CategoryId::IMAGES),
                    default_subfolder: "Images",
                }
            }
            _ => Self::classify_by_mime(mime_type),
        }
    }

    fn classify_by_mime(mime_type: Option<&str>) -> CategoryRule {
        let mime = mime_type
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        if mime.starts_with("video/") {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::VIDEO),
                default_subfolder: "Video",
            }
        } else if mime.starts_with("audio/") {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::AUDIO),
                default_subfolder: "Music",
            }
        } else if mime.starts_with("image/") {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::IMAGES),
                default_subfolder: "Images",
            }
        } else if matches!(
            mime.as_str(),
            "application/zip"
                | "application/x-7z-compressed"
                | "application/vnd.rar"
                | "application/gzip"
                | "application/x-iso9660-image"
        ) {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::COMPRESSED),
                default_subfolder: "Compressed",
            }
        } else if matches!(
            mime.as_str(),
            "application/pdf" | "text/plain" | "text/csv" | "application/epub+zip"
        ) {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::DOCUMENTS),
                default_subfolder: "Documents",
            }
        } else if matches!(
            mime.as_str(),
            "application/x-msdownload" | "application/x-msi"
        ) {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::PROGRAMS),
                default_subfolder: "Programs",
            }
        } else {
            CategoryRule {
                id: CategoryId::builtin(CategoryId::OTHER),
                default_subfolder: "General",
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn blocks_path_traversal_unc_and_forbidden_schemes() {
        assert!(FilenameSanitizer::validate_strict_filename("../evil.dll").is_err());
        assert!(FilenameSanitizer::validate_strict_filename(r"..\..\Windows\evil.exe").is_err());
        assert!(FilenameSanitizer::validate_strict_filename("C:\\evil.exe").is_err());
        assert!(FilenameSanitizer::validate_download_url(r"\\attacker\share\a.exe").is_err());
        assert!(FilenameSanitizer::validate_download_url("file:////attacker/share/a").is_err());
        assert!(FilenameSanitizer::validate_download_url("javascript:alert(1)").is_err());
        assert!(FilenameSanitizer::validate_download_url("https://example.com/a.zip").is_ok());

        // Untrusted Content-Disposition strips directory prefix and preserves safe basename
        assert_eq!(
            FilenameSanitizer::sanitize_untrusted("../../Startup/evil.exe"),
            "evil.exe"
        );
    }

    #[test]
    fn neutralizes_all_windows_reserved_devices_and_rtlo_spoofing() {
        for dev in [
            "CON", "prn", "Aux", "NUL", "COM1", "COM9", "LPT1", "LPT9", "COM¹", "LPT³",
        ] {
            let clean = FilenameSanitizer::sanitize_untrusted(&format!("{dev}.txt"));
            assert!(
                clean.starts_with('_'),
                "Failed to prefix reserved name: {dev} -> {clean}"
            );
        }
        assert_eq!(
            FilenameSanitizer::sanitize_untrusted("invoice_\u{202E}fdp.exe"),
            "invoice__fdp.exe"
        );
        assert_eq!(
            FilenameSanitizer::sanitize_untrusted("report.pdf...   "),
            "report.pdf"
        );
    }

    #[test]
    fn resolves_collisions_and_categorizes_arabic_and_compound_files() {
        let mut existing = HashSet::from([
            "كتاب التصميم.pdf".to_owned(),
            "كتاب التصميم (1).pdf".to_owned(),
            "backup.tar.gz".to_owned(),
        ]);

        let next_pdf = FilenameSanitizer::resolve_collision("كتاب التصميم.pdf", |n| {
            existing.contains(n)
        });
        assert_eq!(next_pdf, "كتاب التصميم (2).pdf");
        existing.insert(next_pdf);

        let next_tar =
            FilenameSanitizer::resolve_collision("backup.tar.gz", |n| existing.contains(n));
        assert_eq!(next_tar, "backup (1).tar.gz");

        assert_eq!(
            CategorizationPolicy::classify("كتاب التصميم.pdf", None)
                .id
                .as_str(),
            CategoryId::DOCUMENTS
        );
        assert_eq!(
            CategorizationPolicy::classify("ubuntu-26.04.iso", None)
                .id
                .as_str(),
            CategoryId::COMPRESSED
        );
        assert_eq!(
            CategorizationPolicy::classify("stream", Some("video/mp4; codecs=av01"))
                .id
                .as_str(),
            CategoryId::VIDEO
        );
    }

    proptest! {
        #[test]
        fn proptest_sanitized_filename_is_always_safe_single_child_within_budget(
            raw in "[\\PC\\s/\\\\:<>\"|?*.\u{202E}]{0,350}"
        ) {
            let clean = FilenameSanitizer::sanitize_untrusted(&raw);
            prop_assert!(!clean.is_empty());
            prop_assert!(clean.len() <= MAX_FILENAME_BYTES);
            prop_assert!(!clean.ends_with('.'));
            prop_assert!(!clean.ends_with(' '));
            prop_assert!(!clean.contains('/'));
            prop_assert!(!clean.contains('\\'));
            prop_assert!(!clean.contains(':'));

            let joined = FilenameSanitizer::safe_join(Path::new("D:/Downloads"), &clean).unwrap();
            prop_assert_eq!(joined.parent(), Some(Path::new("D:/Downloads")));
        }
    }
}
