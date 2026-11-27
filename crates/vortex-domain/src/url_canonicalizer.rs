//! Pure, zero-I/O smart webpage-vs-direct-file URL canonicalizer (`P1.6-T02` / `UX-M01`).
//!
//! Protects end-users who accidentally paste a file-host viewer page URL (e.g.
//! `https://pixeldrain.com/u/8nHKszgt` or `https://github.com/org/repo/blob/main/file.zip`)
//! into `[+ Add URL]` instead of the direct binary link.

use url::Url;

/// Result of analyzing a user-supplied URL for known public viewer-to-direct transformations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalUrlResult {
    /// Original URL pasted by the user.
    pub original_url: String,
    /// Canonical direct download URL (identical to `original_url` if unchanged).
    pub resolved_url: String,
    /// `true` if Vortex automatically converted a known public viewer page to its direct file endpoint.
    pub transformed: bool,
    /// Optional provider slug (`"pixeldrain"`, `"github_blob"`, `"dropbox"`, `"interactive_viewer"`).
    pub provider_hint: Option<&'static str>,
    /// `true` if this URL belongs to an interactive viewer host (e.g. Google Drive / `MediaFire` / Mega)
    /// that typically serves HTML and should be opened in the browser if probing yields `text/html`.
    pub interactive_viewer_page: bool,
}

/// Deterministic, offline URL canonicalizer (`UX-M01`).
#[derive(Debug, Clone, Copy, Default)]
pub struct UrlCanonicalizer;

impl UrlCanonicalizer {
    /// Inspects `raw_url` and rewrites safe, deterministic public viewer patterns to direct download
    /// URLs while flagging interactive viewer pages that cannot be rewritten offline.
    #[must_use]
    pub fn canonicalize(raw_url: &str) -> CanonicalUrlResult {
        let trimmed = raw_url.trim();
        let Ok(parsed) = Url::parse(trimmed) else {
            return Self::unchanged(trimmed, None, false);
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return Self::unchanged(trimmed, None, false);
        }
        let Some(host_raw) = parsed.host_str() else {
            return Self::unchanged(trimmed, None, false);
        };
        let host = host_raw.to_ascii_lowercase();
        let segments: Vec<&str> = parsed
            .path_segments()
            .map_or_default(|s| s.filter(|p| !p.is_empty()).collect());

        // 1. Pixeldrain viewer page: `https://pixeldrain.com/u/<id>` -> `https://pixeldrain.com/api/file/<id>?download`
        if matches!(host.as_str(), "pixeldrain.com" | "www.pixeldrain.com")
            && segments.len() == 2
            && segments.first().copied() == Some("u")
            && let Some(file_id) = segments.get(1).copied()
            && is_safe_path_token(file_id)
        {
            return CanonicalUrlResult {
                original_url: trimmed.to_owned(),
                resolved_url: format!("https://pixeldrain.com/api/file/{file_id}?download"),
                transformed: true,
                provider_hint: Some("pixeldrain"),
                interactive_viewer_page: false,
            };
        }

        // 2. GitHub blob viewer: `https://github.com/<owner>/<repo>/blob/<ref>/<path...>` -> `raw.githubusercontent.com/...`
        if matches!(host.as_str(), "github.com" | "www.github.com")
            && segments.len() >= 5
            && segments.get(2).copied() == Some("blob")
            && let (Some(owner), Some(repo), Some(tail)) =
                (segments.first(), segments.get(1), segments.get(3..))
            && is_safe_path_token(owner)
            && is_safe_path_token(repo)
        {
            let rest = tail.join("/");
            return CanonicalUrlResult {
                original_url: trimmed.to_owned(),
                resolved_url: format!("https://raw.githubusercontent.com/{owner}/{repo}/{rest}"),
                transformed: true,
                provider_hint: Some("github_blob"),
                interactive_viewer_page: false,
            };
        }

        // 3. Dropbox share link with `dl=0` -> `dl=1`
        if matches!(host.as_str(), "dropbox.com" | "www.dropbox.com")
            && matches!(segments.first().copied(), Some("s" | "scl" | "sh"))
        {
            let mut rewritten = parsed.clone();
            let mut pairs: Vec<(String, String)> = parsed
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let mut had_dl = false;
            let mut changed = false;
            for (k, v) in &mut pairs {
                if k == "dl" {
                    had_dl = true;
                    if v != "1" {
                        "1".clone_into(v);
                        changed = true;
                    }
                }
            }
            if !had_dl {
                pairs.push(("dl".to_owned(), "1".to_owned()));
                changed = true;
            }
            if changed {
                rewritten.query_pairs_mut().clear().extend_pairs(pairs);
                return CanonicalUrlResult {
                    original_url: trimmed.to_owned(),
                    resolved_url: rewritten.into(),
                    transformed: true,
                    provider_hint: Some("dropbox"),
                    interactive_viewer_page: false,
                };
            }
        }

        // 4. Interactive file-host viewer pages requiring browser click/session
        let is_interactive_viewer = (host == "drive.google.com"
            && segments.first().copied() == Some("file"))
            || (matches!(host.as_str(), "mediafire.com" | "www.mediafire.com")
                && segments.first().copied() == Some("file"))
            || (matches!(host.as_str(), "mega.nz" | "www.mega.nz")
                && matches!(segments.first().copied(), Some("file" | "folder")))
            || (matches!(host.as_str(), "gofile.io" | "www.gofile.io")
                && segments.first().copied() == Some("d"))
            || (matches!(host.as_str(), "1fichier.com" | "www.1fichier.com")
                && parsed.query().is_some());

        Self::unchanged(
            trimmed,
            if is_interactive_viewer {
                Some("interactive_viewer")
            } else {
                None
            },
            is_interactive_viewer,
        )
    }

    fn unchanged(
        url: &str,
        provider_hint: Option<&'static str>,
        interactive_viewer_page: bool,
    ) -> CanonicalUrlResult {
        CanonicalUrlResult {
            original_url: url.to_owned(),
            resolved_url: url.to_owned(),
            transformed: false,
            provider_hint,
            interactive_viewer_page,
        }
    }
}

fn is_safe_path_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_pixeldrain_github_blob_and_dropbox_viewer_urls() {
        let pd = UrlCanonicalizer::canonicalize("https://pixeldrain.com/u/8nHKszgt");
        assert!(pd.transformed);
        assert_eq!(pd.provider_hint, Some("pixeldrain"));
        assert_eq!(
            pd.resolved_url,
            "https://pixeldrain.com/api/file/8nHKszgt?download"
        );

        let gh = UrlCanonicalizer::canonicalize(
            "https://github.com/vortex/releases/blob/main/dist/vortex.zip",
        );
        assert!(gh.transformed);
        assert_eq!(gh.provider_hint, Some("github_blob"));
        assert_eq!(
            gh.resolved_url,
            "https://raw.githubusercontent.com/vortex/releases/main/dist/vortex.zip"
        );

        let db = UrlCanonicalizer::canonicalize("https://www.dropbox.com/s/abc123/file.zip?dl=0");
        assert!(db.transformed);
        assert_eq!(db.provider_hint, Some("dropbox"));
        assert!(db.resolved_url.ends_with("dl=1"));
    }

    #[test]
    fn identifies_interactive_viewers_without_mangling_direct_urls() {
        let gdrive = UrlCanonicalizer::canonicalize(
            "https://drive.google.com/file/d/123456/view?usp=sharing",
        );
        assert!(!gdrive.transformed);
        assert!(gdrive.interactive_viewer_page);

        let mf =
            UrlCanonicalizer::canonicalize("https://www.mediafire.com/file/xyz123/game.zip/file");
        assert!(!mf.transformed);
        assert!(mf.interactive_viewer_page);

        let direct =
            UrlCanonicalizer::canonicalize("https://pixeldrain.com/api/file/8nHKszgt?download");
        assert!(!direct.transformed);
        assert!(!direct.interactive_viewer_page);
        assert_eq!(
            direct.resolved_url,
            "https://pixeldrain.com/api/file/8nHKszgt?download"
        );
    }
}
