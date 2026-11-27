//! Zero-I/O Magic-Byte, HTML-Trap, Double-Extension, and Cross-Origin Security Inspector
//! (`crates/vortex-domain/src/security.rs` — `P1.5-T02` / `Smart-Capture-And-Anti-Abuse-Spec.md` §3).
//!
//! Inspects up to the first `512` bytes already buffered by `probe_and_stream` (without consuming
//! the stream!) alongside filename, MIME type, and referrer/origin domains to protect users from:
//! - Fake HTML ad/countdown pages masquerading as `.zip`/`.rar`/`.mp4`/`.pdf` (`TH-21`)
//! - Windows PE executables (`MZ`) / CAB (`MSCF`) / MSI / ELF disguised as media/documents (`TH-21`)
//! - Double-extension spoofing (`Movie.2026.1080p.mp4.exe`, `Invoice.pdf.scr`)
//! - High-risk executable extensions downloaded via cross-origin ad redirects (`TH-20`).

use serde::{Deserialize, Serialize};

/// Overall security risk tier presented inside the Smart Capture Confirmation Dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Magic bytes positively matched the declared file extension (e.g. `PK..` for `.zip`).
    VerifiedMatch,
    /// Standard non-executable download with no anomalies detected.
    Safe,
    /// Requires user awareness (explicit executable, HTML trap, or cross-origin redirect).
    Caution,
    /// High-confidence deception/malware masquerade (`Enter` shortcut locked!).
    Danger,
}

/// Specific structured warning flags surfaced in localized Arabic/English UI banners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum SecurityWarningKind {
    /// Server returned an HTML web/ad/countdown page (`<!DOCTYPE html>` / `text/html`) for a non-HTML file.
    HtmlTrap,
    /// Executable binary (`MZ`/`MSCF`/`ELF`) or double-extension (`*.mp4.exe`) masquerading as media/docs.
    ExecutableMasquerade,
    /// Declared file extension is directly executable or scriptable on Windows (`.exe`, `.msi`, `.bat`, ...).
    HighRiskExecutable,
    /// Download redirected from an unrelated referrer page host onto a different origin host (common on ad sites).
    CrossOriginAdRedirect,
    /// Raw filename attempted Unicode Bidi (`RTLO U+202E`) or zero-width extension spoofing (`TH-30`).
    BidiFilenameSpoofing,
    /// Weaponized shortcut/help/ClickOnce/script smuggling vector (`.lnk`, `.chm`, `.application`, `<script` in `.svg` — `TH-30`).
    WeaponizedScriptOrShortcut,
    /// Mountable Windows virtual disk container (`.iso`, `.img`, `.vhd`, `.vhdx`) requiring extra caution (`TH-30`).
    MountableDiskImage,
}

/// Complete pre-download security assessment computed before writing a single byte to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[allow(clippy::struct_excessive_bools)]
pub struct ContentSecurityAssessment {
    /// Overall visual risk level (`verified_match` / `safe` / `caution` / `danger`).
    pub risk_level: RiskLevel,
    /// Ordered list of specific warnings triggered.
    pub warnings: Vec<SecurityWarningKind>,
    /// Human-readable detected magic signature (`"ZIP"`, `"MP4"`, `"PE_EXE"`, `"HTML"`, etc.).
    pub detected_signature: Option<String>,
    /// Ironclad rule (`P1.5-T01`): `true` forbids silent auto-start even if user enabled global auto-start!
    pub block_auto_start: bool,
    /// `true` disables quick `Enter` confirmation on `Danger` masquerades so users cannot approve by accident!
    pub block_enter_shortcut: bool,
    /// Hostname serving the actual download bytes (`final_url` host).
    pub origin_host: String,
    /// Hostname of the web page where the user clicked (`referrer` or `top_level_site` host).
    pub referrer_host: Option<String>,
    /// `true` when `referrer_host` and `origin_host` belong to different registrable sites.
    pub cross_origin_redirected: bool,
}

impl Default for ContentSecurityAssessment {
    fn default() -> Self {
        Self {
            risk_level: RiskLevel::Safe,
            warnings: Vec::new(),
            detected_signature: None,
            block_auto_start: false,
            block_enter_shortcut: false,
            origin_host: String::new(),
            referrer_host: None,
            cross_origin_redirected: false,
        }
    }
}

/// Extensions that can execute code or alter Windows system state directly (`TH-21`, `TH-23`, `TH-30`).
const HIGH_RISK_EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe",
    "msi",
    "msix",
    "appx",
    "msp",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "psd1",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "hta",
    "scr",
    "cpl",
    "reg",
    "inf",
    "jar",
    "lnk",
    "scf",
    "url",
    "chm",
    "application",
    "appref-ms",
    "library-ms",
    "search-ms",
    "settingcontent-ms",
    "deskthemepack",
    "pif",
    "com",
    "dll",
    "sys",
    "iso",
    "img",
    "vhd",
    "vhdx",
];

/// Weaponized Windows shell/ClickOnce/help/script extensions highlighted specifically under `TH-30`.
const WEAPONIZED_SHORTCUT_OR_CONTAINER_EXTENSIONS: &[&str] = &[
    "lnk",
    "scf",
    "url",
    "chm",
    "hta",
    "wsf",
    "wsh",
    "jse",
    "vbe",
    "application",
    "appref-ms",
    "library-ms",
    "search-ms",
    "settingcontent-ms",
    "deskthemepack",
];

/// Mountable Windows virtual disk images (`TH-30`).
const MOUNTABLE_DISK_IMAGE_EXTENSIONS: &[&str] = &["iso", "img", "vhd", "vhdx"];

/// Non-executable bait extensions commonly abused in double-extension lures (`video.mp4.exe`).
const BAIT_INNER_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "mp3", "flac", "wav", "pdf", "doc", "docx", "xls", "xlsx",
    "ppt", "pptx", "txt", "jpg", "jpeg", "png", "gif", "webp", "zip", "rar", "7z", "epub", "apk",
];

/// Returns `true` if `file_name` ends with a Windows high-risk executable/script/disk-image extension.
#[must_use]
pub fn is_high_risk_executable_extension(file_name: &str) -> bool {
    let ext = final_lowercase_extension(file_name);
    HIGH_RISK_EXECUTABLE_EXTENSIONS.contains(&ext.as_str())
}

/// Extracts the normalized lowercase hostname from an HTTP(S) URL string (without port).
#[must_use]
pub fn extract_url_host(raw_url: &str) -> Option<String> {
    let trimmed = raw_url.trim();
    let after_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if host_port.starts_with('[') {
        host_port.split(']').next()?.trim_start_matches('[')
    } else {
        host_port.split(':').next()?
    };
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Returns `true` when `referrer_host` and `origin_host` do NOT share the same base site
/// (e.g., `watch-movies.example` -> `ad-cdn.other` is `true`, while `github.com` -> `objects.githubusercontent.com`
/// or `releases.ubuntu.com` -> `ubuntu.com` checks registrable suffix cleanly).
#[must_use]
pub fn is_cross_origin_host(referrer_host: &str, origin_host: &str) -> bool {
    let r = referrer_host.trim().to_ascii_lowercase();
    let o = origin_host.trim().to_ascii_lowercase();
    if r.is_empty() || o.is_empty() || r == o {
        return false;
    }
    let r_base = registrable_tail(&r);
    let o_base = registrable_tail(&o);
    r_base != o_base
}

fn registrable_tail(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return host.to_owned();
    }
    parts
        .get(parts.len() - 2..)
        .map_or_else(|| host.to_owned(), |s| s.join("."))
}

fn final_lowercase_extension(file_name: &str) -> String {
    let trimmed = file_name.trim().trim_end_matches('.');
    trimmed
        .rsplit_once('.')
        .map_or_default(|(_, ext)| ext.to_ascii_lowercase())
}

fn has_deceptive_double_extension(file_name: &str) -> bool {
    let trimmed = file_name.trim().trim_end_matches('.');
    let Some((stem, outer_ext)) = trimmed.rsplit_once('.') else {
        return false;
    };
    let outer = outer_ext.to_ascii_lowercase();
    if !HIGH_RISK_EXECUTABLE_EXTENSIONS.contains(&outer.as_str()) {
        return false;
    }
    let Some((_, inner_ext)) = stem.rsplit_once('.') else {
        return false;
    };
    let inner = inner_ext.to_ascii_lowercase();
    BAIT_INNER_EXTENSIONS.contains(&inner.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedMagic {
    Html,
    PeExecutable,
    CabArchive,
    CompoundMsi,
    ElfExecutable,
    WindowsShortcut,
    ChmHelp,
    SvgWithScript,
    Zip,
    Rar,
    SevenZip,
    Pdf,
    Mp4,
    MkvWebm,
    Png,
    Jpeg,
    Gzip,
    Zstd,
    Mp3Id3,
    Flac,
    Unknown,
}

impl DetectedMagic {
    const fn label(self) -> Option<&'static str> {
        match self {
            Self::Html => Some("HTML"),
            Self::PeExecutable => Some("PE_EXE"),
            Self::CabArchive => Some("CAB"),
            Self::CompoundMsi => Some("MSI"),
            Self::ElfExecutable => Some("ELF"),
            Self::WindowsShortcut => Some("LNK"),
            Self::ChmHelp => Some("CHM"),
            Self::SvgWithScript => Some("SVG_SCRIPT"),
            Self::Zip => Some("ZIP"),
            Self::Rar => Some("RAR"),
            Self::SevenZip => Some("7Z"),
            Self::Pdf => Some("PDF"),
            Self::Mp4 => Some("MP4"),
            Self::MkvWebm => Some("MKV"),
            Self::Png => Some("PNG"),
            Self::Jpeg => Some("JPEG"),
            Self::Gzip => Some("GZIP"),
            Self::Zstd => Some("ZSTD"),
            Self::Mp3Id3 => Some("MP3"),
            Self::Flac => Some("FLAC"),
            Self::Unknown => None,
        }
    }

    const fn is_executable_binary(self) -> bool {
        matches!(
            self,
            Self::PeExecutable | Self::ElfExecutable | Self::WindowsShortcut | Self::ChmHelp
        )
    }

    fn matches_extension(self, ext: &str) -> bool {
        match self {
            Self::Zip => matches!(
                ext,
                "zip" | "apk" | "jar" | "msix" | "docx" | "xlsx" | "pptx" | "epub"
            ),
            Self::Rar => ext == "rar",
            Self::SevenZip => ext == "7z",
            Self::Pdf => ext == "pdf",
            Self::Mp4 => matches!(ext, "mp4" | "m4a" | "m4v" | "mov" | "3gp"),
            Self::MkvWebm => matches!(ext, "mkv" | "webm"),
            Self::Png => ext == "png",
            Self::Jpeg => matches!(ext, "jpg" | "jpeg"),
            Self::Gzip => matches!(ext, "gz" | "tgz"),
            Self::Zstd => ext == "zst",
            Self::Mp3Id3 => ext == "mp3",
            Self::Flac => ext == "flac",
            Self::PeExecutable => matches!(ext, "exe" | "dll" | "scr" | "sys" | "cpl"),
            Self::CompoundMsi => matches!(ext, "msi" | "msp" | "doc" | "xls" | "ppt"),
            Self::CabArchive => matches!(ext, "cab" | "exe" | "msi"),
            Self::WindowsShortcut => ext == "lnk",
            Self::ChmHelp => ext == "chm",
            Self::Html => matches!(ext, "html" | "htm" | "xhtml"),
            Self::SvgWithScript | Self::ElfExecutable | Self::Unknown => false,
        }
    }
}

fn sniff_magic_bytes(header: &[u8]) -> DetectedMagic {
    if header.len() >= 2 && header.starts_with(b"MZ") {
        return DetectedMagic::PeExecutable;
    }
    if header.len() >= 8 && header.starts_with(&[0x4C, 0x00, 0x00, 0x00, 0x01, 0x14, 0x02, 0x00]) {
        return DetectedMagic::WindowsShortcut;
    }
    if header.len() >= 4 {
        if header.starts_with(b"ITSF") {
            return DetectedMagic::ChmHelp;
        }
        if header.starts_with(b"MSCF") {
            return DetectedMagic::CabArchive;
        }
        if header.starts_with(&[0x7F, b'E', b'L', b'F']) {
            return DetectedMagic::ElfExecutable;
        }
        if header.starts_with(&[0x50, 0x4B, 0x03, 0x04])
            || header.starts_with(&[0x50, 0x4B, 0x05, 0x06])
        {
            return DetectedMagic::Zip;
        }
        if header.starts_with(b"Rar!") {
            return DetectedMagic::Rar;
        }
        if header.starts_with(b"%PDF") {
            return DetectedMagic::Pdf;
        }
        if header.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            return DetectedMagic::MkvWebm;
        }
        if header.starts_with(&[0x89, b'P', b'N', b'G']) {
            return DetectedMagic::Png;
        }
        if header.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            return DetectedMagic::Zstd;
        }
        if header.starts_with(b"fLaC") {
            return DetectedMagic::Flac;
        }
    }
    if header.len() >= 8 && header.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return DetectedMagic::CompoundMsi;
    }
    if header.len() >= 6 && header.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return DetectedMagic::SevenZip;
    }
    if header.len() >= 12 && header.get(4..8) == Some(b"ftyp") {
        return DetectedMagic::Mp4;
    }
    if header.len() >= 3 {
        if header.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return DetectedMagic::Jpeg;
        }
        if header.starts_with(b"ID3") {
            return DetectedMagic::Mp3Id3;
        }
    }
    if header.len() >= 2 && header.starts_with(&[0x1F, 0x8B]) {
        return DetectedMagic::Gzip;
    }

    // Check UTF-8 / ASCII text prefix (skipping optional BOM & whitespace) for HTML traps & SVG script smuggling!
    let stripped = header.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(header);
    let full_window_len = stripped.len().min(512);
    if let Some(full_slice) = stripped.get(..full_window_len) {
        let lower_full: String = full_slice
            .iter()
            .copied()
            .map(|b| char::from(b.to_ascii_lowercase()))
            .collect();
        let trimmed = lower_full.trim_start();
        if trimmed.contains("<svg")
            && (trimmed.contains("<script")
                || trimmed.contains("onload=")
                || trimmed.contains("onerror=")
                || trimmed.contains("javascript:"))
        {
            return DetectedMagic::SvgWithScript;
        }
        if trimmed.starts_with("<!doctype html")
            || trimmed.starts_with("<html")
            || trimmed.starts_with("<head")
            || trimmed.starts_with("<script")
            || (trimmed.starts_with("<!--") && trimmed.contains("<html"))
        {
            return DetectedMagic::Html;
        }
    }

    DetectedMagic::Unknown
}

/// Pure domain inspector analyzing filename, MIME, referrer/origin hosts, and up to `512` header bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContentSecurityInspector;

impl ContentSecurityInspector {
    /// Produces a [`ContentSecurityAssessment`] in `O(1)` microseconds with zero I/O.
    #[must_use]
    pub fn inspect(
        file_name: &str,
        mime_type: Option<&str>,
        final_url: &str,
        referrer_or_site: Option<&str>,
        header_bytes: &[u8],
    ) -> ContentSecurityAssessment {
        Self::inspect_with_raw_name(
            file_name,
            file_name,
            mime_type,
            final_url,
            referrer_or_site,
            header_bytes,
        )
    }

    /// Produces a [`ContentSecurityAssessment`] inspecting BOTH the pre-sanitized raw server
    /// filename (to catch `RTLO` / zero-width spoofing before stripping — `TH-30`) and the
    /// sanitized filename + up to `512` header bytes.
    #[must_use]
    pub fn inspect_with_raw_name(
        raw_file_name: &str,
        file_name: &str,
        mime_type: Option<&str>,
        final_url: &str,
        referrer_or_site: Option<&str>,
        header_bytes: &[u8],
    ) -> ContentSecurityAssessment {
        let ext = final_lowercase_extension(file_name);
        let magic = sniff_magic_bytes(header_bytes);
        let mime_lower = mime_type.map_or_else(String::new, |m| {
            m.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });

        let origin_host = extract_url_host(final_url).unwrap_or_default();
        let referrer_host = referrer_or_site.and_then(extract_url_host);
        let cross_origin_redirected = referrer_host
            .as_deref()
            .is_some_and(|r| is_cross_origin_host(r, &origin_host));

        let mut warnings = Vec::new();

        // 0. Unicode Bidi (`RTLO U+202E`) / zero-width filename spoofing (`TH-30`)
        let has_bidi_spoof =
            crate::FilenameSanitizer::contains_bidi_or_spoofing_chars(raw_file_name);
        if has_bidi_spoof {
            warnings.push(SecurityWarningKind::BidiFilenameSpoofing);
        }

        // 1. HTML trap: server sent HTML web/ad/countdown page for a non-HTML download!
        let expects_html = matches!(ext.as_str(), "html" | "htm" | "xhtml");
        let is_html_payload = magic == DetectedMagic::Html || mime_lower == "text/html";
        if is_html_payload && !expects_html {
            warnings.push(SecurityWarningKind::HtmlTrap);
        }

        // 2. Masquerade detection: double-extension lure (`movie.mp4.exe`) OR executable/LNK/CHM binary bytes inside media/doc!
        let is_exec_ext = is_high_risk_executable_extension(file_name);
        let double_ext_lure = has_deceptive_double_extension(file_name);
        let binary_masquerade = magic.is_executable_binary() && !is_exec_ext && !ext.is_empty();
        if double_ext_lure || binary_masquerade {
            warnings.push(SecurityWarningKind::ExecutableMasquerade);
        }

        // 3. Weaponized Windows shortcuts / ClickOnce / CHM / SVG script smuggling (`TH-30`)
        let is_weaponized_container = WEAPONIZED_SHORTCUT_OR_CONTAINER_EXTENSIONS
            .contains(&ext.as_str())
            || matches!(
                magic,
                DetectedMagic::WindowsShortcut
                    | DetectedMagic::ChmHelp
                    | DetectedMagic::SvgWithScript
            );
        if is_weaponized_container {
            warnings.push(SecurityWarningKind::WeaponizedScriptOrShortcut);
        }

        // 4. Mountable virtual disk image awareness (`.iso`, `.img`, `.vhd`, `.vhdx`)
        if MOUNTABLE_DISK_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            warnings.push(SecurityWarningKind::MountableDiskImage);
        }

        // 5. Explicit high-risk executable extension (`.exe`, `.msi`, `.bat`, `.scr`, ...)
        if is_exec_ext {
            warnings.push(SecurityWarningKind::HighRiskExecutable);
        }

        // 6. Cross-origin ad redirect warning whenever an executable, masquerade, weaponized container, or HTML trap jumped hosts!
        if cross_origin_redirected
            && (is_exec_ext
                || double_ext_lure
                || binary_masquerade
                || is_weaponized_container
                || is_html_payload)
        {
            warnings.push(SecurityWarningKind::CrossOriginAdRedirect);
        }

        let has_masquerade = warnings.contains(&SecurityWarningKind::ExecutableMasquerade);
        let has_html_trap = warnings.contains(&SecurityWarningKind::HtmlTrap);
        let has_exec = warnings.contains(&SecurityWarningKind::HighRiskExecutable);
        let is_svg_script = magic == DetectedMagic::SvgWithScript;

        let is_danger = has_masquerade || has_bidi_spoof || is_svg_script;

        let risk_level = if is_danger {
            RiskLevel::Danger
        } else if has_html_trap || has_exec || is_weaponized_container {
            RiskLevel::Caution
        } else if magic != DetectedMagic::Unknown && magic.matches_extension(&ext) {
            RiskLevel::VerifiedMatch
        } else {
            RiskLevel::Safe
        };

        let block_auto_start = is_danger || has_html_trap || has_exec || is_weaponized_container;
        let block_enter_shortcut = is_danger;

        ContentSecurityAssessment {
            risk_level,
            warnings,
            detected_signature: magic.label().map(str::to_owned),
            block_auto_start,
            block_enter_shortcut,
            origin_host,
            referrer_host,
            cross_origin_redirected,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_ad_trap_disguised_as_archive_or_video() {
        let html = b"  \r\n<!DOCTYPE html><html><body>Wait 30 seconds...</body></html>";
        let res = ContentSecurityInspector::inspect(
            "Movie.2026.1080p.mkv",
            Some("text/html; charset=utf-8"),
            "https://ads-cdn.bad/download",
            Some("https://stream-site.example/watch"),
            html,
        );
        assert_eq!(res.risk_level, RiskLevel::Caution);
        assert!(res.warnings.contains(&SecurityWarningKind::HtmlTrap));
        assert!(
            res.warnings
                .contains(&SecurityWarningKind::CrossOriginAdRedirect)
        );
        assert_eq!(res.detected_signature.as_deref(), Some("HTML"));
        assert!(res.block_auto_start);
        assert!(!res.block_enter_shortcut);
    }

    #[test]
    fn detects_mz_executable_masquerading_as_mp4_and_locks_enter_shortcut() {
        let pe_bytes = b"MZ\x90\x00\x03\x00\x00\x00This program cannot be run in DOS mode";
        let res = ContentSecurityInspector::inspect(
            "SpiderMan.2026.mp4",
            Some("video/mp4"),
            "https://sketchy-cdn.net/f/123",
            Some("https://movies-hub.example/film"),
            pe_bytes,
        );
        assert_eq!(res.risk_level, RiskLevel::Danger);
        assert!(
            res.warnings
                .contains(&SecurityWarningKind::ExecutableMasquerade)
        );
        assert_eq!(res.detected_signature.as_deref(), Some("PE_EXE"));
        assert!(res.block_auto_start);
        assert!(res.block_enter_shortcut);
        assert!(res.cross_origin_redirected);
    }

    #[test]
    fn detects_double_extension_lure_even_before_magic_bytes() {
        let res = ContentSecurityInspector::inspect(
            "Invoice_2026.pdf.exe",
            Some("application/octet-stream"),
            "https://drop.example.org/file",
            None,
            &[],
        );
        assert_eq!(res.risk_level, RiskLevel::Danger);
        assert!(
            res.warnings
                .contains(&SecurityWarningKind::ExecutableMasquerade)
        );
        assert!(
            res.warnings
                .contains(&SecurityWarningKind::HighRiskExecutable)
        );
        assert!(res.block_auto_start);
        assert!(res.block_enter_shortcut);
    }

    #[test]
    fn verifies_legitimate_magic_signatures_across_formats() {
        let cases: &[(&str, &[u8], &str)] = &[
            ("archive.zip", &[0x50, 0x4B, 0x03, 0x04, 0, 0], "ZIP"),
            ("pack.rar", b"Rar!\x1A\x07\x01\x00", "RAR"),
            ("pack.7z", &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C], "7Z"),
            ("book.pdf", b"%PDF-1.7\n", "PDF"),
            ("clip.mp4", b"\x00\x00\x00\x20ftypisom\x00\x00", "MP4"),
            ("film.mkv", &[0x1A, 0x45, 0xDF, 0xA3, 0x01], "MKV"),
            ("photo.png", &[0x89, b'P', b'N', b'G', 0x0D, 0x0A], "PNG"),
            ("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0], "JPEG"),
            ("backup.gz", &[0x1F, 0x8B, 0x08, 0x00], "GZIP"),
            ("data.zst", &[0x28, 0xB5, 0x2F, 0xFD, 0x00], "ZSTD"),
            ("song.mp3", b"ID3\x03\x00\x00", "MP3"),
            ("lossless.flac", b"fLaC\x00\x00", "FLAC"),
        ];
        for &(name, header, expected_sig) in cases {
            let res = ContentSecurityInspector::inspect(
                name,
                Some("application/octet-stream"),
                "https://cdn.example.com/dl",
                Some("https://www.example.com/page"),
                header,
            );
            assert_eq!(res.risk_level, RiskLevel::VerifiedMatch, "for {name}");
            assert_eq!(res.detected_signature.as_deref(), Some(expected_sig));
            assert!(!res.block_auto_start);
            assert!(!res.block_enter_shortcut);
            assert!(!res.cross_origin_redirected);
        }

        // Explicit `.exe` from same site -> `Caution` (`block_auto_start = true`, `Enter` allowed)
        let exe = ContentSecurityInspector::inspect(
            "VortexSetup.exe",
            Some("application/x-msdownload"),
            "https://releases.vortexdm.io/VortexSetup.exe",
            Some("https://vortexdm.io/download"),
            b"MZ\x90\x00",
        );
        assert_eq!(exe.risk_level, RiskLevel::Caution);
        assert!(exe.block_auto_start);
        assert!(!exe.block_enter_shortcut);
        assert!(!exe.cross_origin_redirected);
    }

    #[test]
    fn host_extraction_and_other_binary_magic_signatures() {
        assert_eq!(
            extract_url_host("https://user:pass@Sub.Example.COM:8443/path?q=1").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(
            extract_url_host("http://[::1]:8080/a").as_deref(),
            Some("::1")
        );
        assert_eq!(extract_url_host("ftp://ignored/a"), None);
        assert!(!is_cross_origin_host("sub.example.com", "example.com"));
        assert!(is_cross_origin_host("movies.site", "adnetwork.biz"));

        let cab = ContentSecurityInspector::inspect(
            "setup.cab",
            None,
            "https://cdn.example.org/setup.cab",
            None,
            b"MSCF\x00\x00",
        );
        assert_eq!(cab.detected_signature.as_deref(), Some("CAB"));

        let msi = ContentSecurityInspector::inspect(
            "app.msi",
            None,
            "https://cdn.example.org/app.msi",
            None,
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
        );
        assert_eq!(msi.detected_signature.as_deref(), Some("MSI"));
        assert_eq!(msi.risk_level, RiskLevel::Caution);

        let elf = ContentSecurityInspector::inspect(
            "readme.pdf",
            None,
            "https://cdn.example.org/readme.pdf",
            None,
            &[0x7F, b'E', b'L', b'F', 0x02],
        );
        assert_eq!(elf.risk_level, RiskLevel::Danger);
        assert_eq!(elf.detected_signature.as_deref(), Some("ELF"));

        let def = ContentSecurityAssessment::default();
        assert_eq!(def.risk_level, RiskLevel::Safe);
    }

    #[test]
    fn detects_rtlo_lnk_chm_clickonce_svg_script_and_iso_under_th30() {
        // 1. RTLO Bidi Spoofing -> Danger + Enter locked
        let rtlo = ContentSecurityInspector::inspect_with_raw_name(
            "invoice_\u{202E}fdp.exe",
            "invoice__fdp.exe",
            Some("application/octet-stream"),
            "https://evil.example/a",
            None,
            b"MZ\x90\x00",
        );
        assert_eq!(rtlo.risk_level, RiskLevel::Danger);
        assert!(
            rtlo.warnings
                .contains(&SecurityWarningKind::BidiFilenameSpoofing)
        );
        assert!(rtlo.block_enter_shortcut);

        // 2. Windows `.lnk` binary masquerading as `.pdf` -> Danger + LNK signature
        let lnk = ContentSecurityInspector::inspect(
            "document.pdf",
            Some("application/pdf"),
            "https://drop.example/f",
            None,
            &[0x4C, 0x00, 0x00, 0x00, 0x01, 0x14, 0x02, 0x00, 0x00, 0x00],
        );
        assert_eq!(lnk.risk_level, RiskLevel::Danger);
        assert_eq!(lnk.detected_signature.as_deref(), Some("LNK"));
        assert!(
            lnk.warnings
                .contains(&SecurityWarningKind::WeaponizedScriptOrShortcut)
        );
        assert!(lnk.block_enter_shortcut);

        // 3. CHM Help file (`ITSF`) -> Caution + WeaponizedScriptOrShortcut
        let chm = ContentSecurityInspector::inspect(
            "manual.chm",
            None,
            "https://files.example/manual.chm",
            None,
            b"ITSF\x03\x00\x00\x00",
        );
        assert_eq!(chm.risk_level, RiskLevel::Caution);
        assert_eq!(chm.detected_signature.as_deref(), Some("CHM"));
        assert!(chm.block_auto_start);

        // 4. SVG with `<script>` smuggling -> Danger + Enter locked!
        let svg = ContentSecurityInspector::inspect(
            "logo.svg",
            Some("image/svg+xml"),
            "https://cdn.example/logo.svg",
            None,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
        );
        assert_eq!(svg.risk_level, RiskLevel::Danger);
        assert_eq!(svg.detected_signature.as_deref(), Some("SVG_SCRIPT"));
        assert!(svg.block_enter_shortcut);

        // 5. Mountable `.iso` container -> MountableDiskImage awareness warning
        let iso = ContentSecurityInspector::inspect(
            "ubuntu.iso",
            Some("application/x-iso9660-image"),
            "https://releases.ubuntu.com/ubuntu.iso",
            None,
            &[],
        );
        assert!(
            iso.warnings
                .contains(&SecurityWarningKind::MountableDiskImage)
        );
        assert!(iso.block_auto_start);
    }
}
