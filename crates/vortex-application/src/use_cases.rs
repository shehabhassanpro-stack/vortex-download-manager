//! CQRS-lite Commands & Queries (`ApplicationService` — `P1-T21` / `Vortex-Architecture-Design.md` §6.2 & §10.2).
//!
//! Coordinates:
//! - **Commands:** `prepare_add_download` (`0ms` warm stream staging), `add_download`,
//!   `handoff_from_browser` (`1,400ms` Probe-Before-Commit budget + Zero-Lost-Download fallback),
//!   `start_download`, `pause_download`, `pause_all`, `resume_download`, `cancel_download`,
//!   `remove_download` + `undo_remove_download` (`6s` Undo Trash Buffer), `update_settings`.
//! - **Queries:** `list_downloads`, `get_download_details`, `search_downloads` (SIMD `nucleo-matcher`),
//!   `check_duplicate` (`ahash::AHashMap`), `get_health_status`, `get_diagnostic_summary`, `recent_logs`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ahash::AHashMap;
use async_trait::async_trait;
use bytes::Bytes;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use vortex_domain::{
    CategorizationPolicy, CategoryId, Checksum, Clock, ContentSecurityAssessment,
    ContentSecurityInspector, Download, DownloadCommand, DownloadId, FilenameSanitizer, Priority,
    QueueId, Resource, UrlCanonicalizer,
};
use vortex_error::{ErrorCode, TraceId, VortexError};
use vortex_observability::{RedactedUrl, SecretString};

use crate::AppResult;
use crate::config::AppConfig;
use crate::dto::{
    AddDownloadPreviewDto, BrowserHandoffResultDto, DiagnosticBundleReportDto, DownloadDetailsDto,
    DownloadSummaryDto, DuplicateMatchDto, HealthStatusDto, LogEntryDto, ProgressSnapshotDto,
    StagedDownloadDto,
};
use crate::ports::{
    AppEvent, ByteSource, ByteStream, CategoryRepository, ConfigPort, DiagnosticsPort,
    DownloadRepository, EngineControlPort, EventBus, EventLogRepository, PlatformIntegration,
    ProbeOutcome, ResourceInfo, ResourceRequest, SecretCredentials, SecretStore, TransportMetrics,
};

/// Strict Core probe budget during live browser handoff (`1,400ms` inside Extension's `2,000ms` `SafeSuggestGuard` — `Protocol-Spec.md` §4).
pub const BROWSER_PROBE_BUDGET: Duration = Duration::from_millis(1_400);

/// Undo trash retention window (`6 seconds` — `Design-System.md` & `P1-T32`).
pub const UNDO_DELETE_WINDOW_MS: i64 = 6_000;

/// Maximum concurrent unconfirmed staged warm streams held in RAM (`Anti-Ad-Burst` cap — `P1.5-T03`).
pub const MAX_STAGED_PROBES: usize = 8;

/// Rapid burst detection window (`4 seconds`) flagging ad-storm multi-download traps (`P1.5-T03` / `TH-20`).
pub const BURST_WINDOW_MS: i64 = 4_000;

/// TTL for warm pre-probed streams staged while the user inspects the confirmation dialog (`120 seconds`).
const STAGED_PROBE_TTL_MS: i64 = 120_000;

/// Number of initial bytes inspected losslessly for Magic-Byte / HTML-trap / PE-masquerade verification (`P1.5-T02`).
const MAGIC_PEEK_BYTES: usize = 512;

/// Maximum time spent awaiting initial header bytes before proceeding with metadata-only assessment.
const MAGIC_PEEK_TIMEOUT: Duration = Duration::from_millis(120);

/// Internal source tag distinguishing manual Add-Dialog warm previews from queued browser captures.
const MANUAL_PREVIEW_SOURCE: &str = "manual_preview";

struct PrependByteStream {
    prefix: VecDeque<Bytes>,
    inner: Box<dyn ByteStream>,
}

#[async_trait]
impl ByteStream for PrependByteStream {
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        if let Some(chunk) = self.prefix.pop_front() {
            return Ok(Some(chunk));
        }
        self.inner.next_chunk().await
    }

    fn transport_metrics(&self) -> TransportMetrics {
        self.inner.transport_metrics()
    }

    fn set_stop_offset(&self, stop_offset: u64) {
        self.inner.set_stop_offset(stop_offset);
    }
}

/// Peeks up to `max_bytes` from `stream` without losing a single byte by re-prepending every read
/// [`Bytes`] chunk in FIFO order (`0` lost bytes -> `100%` `SHA-256` fidelity!).
async fn peek_stream_header(
    mut stream: Box<dyn ByteStream>,
    max_bytes: usize,
    timeout: Duration,
) -> (Vec<u8>, Box<dyn ByteStream>) {
    let mut prefix = VecDeque::new();
    let mut header = Vec::with_capacity(max_bytes);
    let deadline = tokio::time::Instant::now() + timeout;

    while header.len() < max_bytes {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next_chunk()).await {
            Ok(Ok(Some(chunk))) => {
                if !chunk.is_empty() {
                    let take = (max_bytes - header.len()).min(chunk.len());
                    if let Some(slice) = chunk.get(..take) {
                        header.extend_from_slice(slice);
                    }
                    prefix.push_back(chunk);
                }
            }
            Ok(Ok(None) | Err(_)) | Err(_) => break,
        }
    }

    let wrapped: Box<dyn ByteStream> = if prefix.is_empty() {
        stream
    } else {
        Box::new(PrependByteStream {
            prefix,
            inner: stream,
        })
    };
    (header, wrapped)
}

struct StagedProbeEntry {
    seq: u64,
    created_at_ms: i64,
    expires_at_ms: i64,
    url: String,
    display_url: String,
    suggested_file_name: String,
    category_id: CategoryId,
    suggested_save_dir: PathBuf,
    free_disk_bytes: Option<u64>,
    security: ContentSecurityAssessment,
    duplicate: Option<DuplicateMatchDto>,
    source: String,
    burst_quarantined: bool,
    referrer: Option<String>,
    top_level_site: Option<String>,
    user_agent: Option<String>,
    headers: BTreeMap<String, String>,
    creds: SecretCredentials,
    info: Box<ResourceInfo>,
    stream: Box<dyn ByteStream>,
}

impl StagedProbeEntry {
    fn is_browser_capture(&self) -> bool {
        self.source != MANUAL_PREVIEW_SOURCE
    }

    fn to_dto(&self, id: DownloadId) -> StagedDownloadDto {
        StagedDownloadDto {
            staged_id: id.to_string(),
            url: self.url.clone(),
            display_url: self.display_url.clone(),
            suggested_file_name: self.suggested_file_name.clone(),
            category_id: self.category_id.clone(),
            suggested_save_dir: self.suggested_save_dir.display().to_string(),
            total_size: self.info.total_size,
            supports_range: self.info.supports_range,
            free_disk_bytes: self.free_disk_bytes,
            security: self.security.clone(),
            duplicate: self.duplicate.clone(),
            source: self.source.clone(),
            burst_quarantined: self.burst_quarantined,
            created_at_ms: self.created_at_ms,
            expires_at_ms: self.expires_at_ms,
        }
    }
}

struct PendingDeletionEntry {
    download: Download,
    delete_completed_file: bool,
    expires_at_ms: i64,
}

/// Input payload for `prepare_add_download` (`0ms` warm probe while Add Dialog opens).
#[derive(Debug, Clone, Default)]
pub struct PrepareAddInput {
    /// Target `http://` or `https://` URL.
    pub url: String,
    /// Optional referrer URL.
    pub referrer: Option<String>,
    /// Optional user-agent override.
    pub user_agent: Option<String>,
    /// Optional cookies (`SecretString`).
    pub cookies: Option<SecretString>,
    /// Optional authorization header (`SecretString`).
    pub authorization: Option<SecretString>,
}

/// Input command for `add_download` (`P1-T21` & `P1-T33`).
#[derive(Debug, Clone)]
pub struct AddDownloadCommand {
    /// Optional `staged_id` returned by `prepare_add_download` to reuse the live open `ByteStream` (`0ms` start!).
    pub staged_id: Option<DownloadId>,
    /// Target URL.
    pub url: String,
    /// Optional referrer URL.
    pub referrer: Option<String>,
    /// Optional user-agent string.
    pub user_agent: Option<String>,
    /// Optional custom headers.
    pub headers: BTreeMap<String, String>,
    /// Optional cookies (`SecretString`).
    pub cookies: Option<SecretString>,
    /// Optional `Authorization` header (`SecretString`).
    pub authorization: Option<SecretString>,
    /// Optional explicit filename chosen in dialog.
    pub file_name: Option<String>,
    /// Optional explicit save directory chosen in dialog.
    pub save_dir: Option<PathBuf>,
    /// Optional category override.
    pub category_id: Option<CategoryId>,
    /// Optional queue override (defaults to `main`).
    pub queue_id: Option<QueueId>,
    /// Priority (defaults to `Normal`).
    pub priority: Priority,
    /// Optional expected cryptographic hash (`sha256:...` / `blake3:...`).
    pub expected_hash: Option<Checksum>,
    /// Start transferring immediately (`true`) vs enqueue paused (`false`).
    pub auto_start: bool,
    /// Overwrite existing file instead of collision auto-numbering (`file (1).iso`).
    pub overwrite_existing: bool,
}

/// Input command for `handoff_from_browser` (`Protocol-Spec.md` §2.2 & §4).
#[derive(Debug, Clone)]
pub struct BrowserHandoffCommand {
    /// Original URL.
    pub url: String,
    /// Final URL observed by browser (if redirected).
    pub final_url: Option<String>,
    /// Referrer URL.
    pub referrer: Option<String>,
    /// Browser-suggested filename.
    pub suggested_name: Option<String>,
    /// MIME type reported by browser.
    pub mime_type: Option<String>,
    /// Content-Length reported by browser.
    pub size_bytes: Option<u64>,
    /// Exact browser `User-Agent`.
    pub user_agent: Option<String>,
    /// Merged `normal + CHIPS` cookies wrapped immediately in [`SecretString`].
    pub cookies: Option<SecretString>,
    /// Additional safe request headers (`<= 64`).
    pub headers: BTreeMap<String, String>,
    /// Whether triggered from an Incognito window.
    pub incognito: bool,
    /// Trigger source (`"determining-filename"`, `"context-menu"`, ...).
    pub source: String,
    /// Start immediately without dialog if `true` or `silent_browser_capture == true`.
    pub auto_start: bool,
    /// CHIPS partition `top_level_site`.
    pub top_level_site: Option<String>,
}

/// Optional list query filter (`P1-T21` & `P1-T32`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DownloadFilterDto {
    /// Optional state slug (`"downloading"`, `"completed"`, `"paused"`, `"failed"`, ...).
    pub state_slug: Option<String>,
    /// Optional category ID string.
    pub category_id: Option<String>,
    /// Optional queue ID string.
    pub queue_id: Option<String>,
}

/// Primary CQRS-lite Application Service orchestrating Domain, Engine, Storage, and Diagnostics (`P1-T21`).
pub struct ApplicationService {
    byte_source: Arc<dyn ByteSource>,
    engine: Arc<dyn EngineControlPort>,
    download_repo: Arc<dyn DownloadRepository>,
    category_repo: Arc<dyn CategoryRepository>,
    event_repo: Arc<dyn EventLogRepository>,
    secret_store: Arc<dyn SecretStore>,
    config_port: Arc<dyn ConfigPort>,
    platform: Arc<dyn PlatformIntegration>,
    diagnostics: Arc<dyn DiagnosticsPort>,
    event_bus: Arc<dyn EventBus>,
    clock: Arc<dyn Clock>,
    staged_seq: AtomicU64,
    staged_probes: AsyncMutex<AHashMap<DownloadId, StagedProbeEntry>>,
    recent_handoff_timestamps: AsyncMutex<VecDeque<i64>>,
    undo_trash: AsyncMutex<AHashMap<DownloadId, PendingDeletionEntry>>,
}

impl std::fmt::Debug for ApplicationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplicationService").finish_non_exhaustive()
    }
}

impl ApplicationService {
    /// Constructs a new [`ApplicationService`] wiring all Hexagonal ports.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        byte_source: Arc<dyn ByteSource>,
        engine: Arc<dyn EngineControlPort>,
        download_repo: Arc<dyn DownloadRepository>,
        category_repo: Arc<dyn CategoryRepository>,
        event_repo: Arc<dyn EventLogRepository>,
        secret_store: Arc<dyn SecretStore>,
        config_port: Arc<dyn ConfigPort>,
        platform: Arc<dyn PlatformIntegration>,
        diagnostics: Arc<dyn DiagnosticsPort>,
        event_bus: Arc<dyn EventBus>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            byte_source,
            engine,
            download_repo,
            category_repo,
            event_repo,
            secret_store,
            config_port,
            platform,
            diagnostics,
            event_bus,
            clock,
            staged_seq: AtomicU64::new(1),
            staged_probes: AsyncMutex::new(AHashMap::new()),
            recent_handoff_timestamps: AsyncMutex::new(VecDeque::new()),
            undo_trash: AsyncMutex::new(AHashMap::new()),
        }
    }

    // ========================================================================
    // COMMANDS (`P1-T21` & `P1.5-T01`..`T05`)
    // ========================================================================

    /// Executes `probe_and_stream` immediately when opening the Add-Download dialog so up to `16 MiB`
    /// streams into RAM in parallel with user confirmation (`0ms` wasted start — `§18` & `P1-T33`),
    /// while losslessly peeking the first `512` bytes for Magic-Byte & HTML-trap verification (`P1.5-T02`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if URL validation fails (`VTX-SEC-001`) or initial probe fails.
    #[allow(clippy::too_many_lines)]
    pub async fn prepare_add_download(
        &self,
        input: PrepareAddInput,
        trace_id: TraceId,
    ) -> AppResult<AddDownloadPreviewDto> {
        let _ = FilenameSanitizer::validate_download_url(&input.url)?;
        self.evict_expired_staged_probes().await;

        // Smart webpage-vs-direct-file URL canonicalizer (`UX-M01`): e.g. `pixeldrain.com/u/<id>` -> `/api/file/<id>?download`
        let canonical = UrlCanonicalizer::canonicalize(&input.url);
        let probe_url = canonical.resolved_url.clone();
        let effective_referrer = input.referrer.clone().or_else(|| {
            canonical
                .transformed
                .then(|| canonical.original_url.clone())
        });

        let staged_id = DownloadId::new();
        let mut req = ResourceRequest::new(staged_id, probe_url.clone(), trace_id);
        req.referrer.clone_from(&effective_referrer);
        req.user_agent.clone_from(&input.user_agent);
        req.cookies.clone_from(&input.cookies);
        req.authorization.clone_from(&input.authorization);

        let outcome = self.byte_source.probe_and_stream(&req).await?;
        let (info, raw_stream) = match outcome {
            ProbeOutcome::Transferable {
                info,
                initial_stream,
            } => (info, initial_stream),
            ProbeOutcome::FallbackToBrowser { detail, .. } => {
                return Err(VortexError::coded(
                    ErrorCode::ResLinkExpired,
                    trace_id,
                    detail,
                ));
            }
        };

        let (header_bytes, stream) =
            peek_stream_header(raw_stream, MAGIC_PEEK_BYTES, MAGIC_PEEK_TIMEOUT).await;

        let raw_untrusted_name = info
            .raw_suggested_filename
            .clone()
            .or_else(|| info.suggested_filename.clone())
            .or_else(|| basename_from_url(&info.final_url))
            .unwrap_or_else(|| "download.bin".to_owned());
        let suggested_file_name = FilenameSanitizer::sanitize_untrusted(&raw_untrusted_name);
        let category_id =
            CategorizationPolicy::classify(&suggested_file_name, info.mime_type.as_deref()).id;
        let suggested_save_dir = self.resolve_category_dir(&category_id, trace_id).await;
        let free_disk_bytes = self.platform.free_disk_bytes(&suggested_save_dir, trace_id);
        let duplicate = self
            .check_duplicate(&probe_url, Some(&suggested_file_name), trace_id)
            .await?;

        let security = ContentSecurityInspector::inspect_with_raw_name(
            &raw_untrusted_name,
            &suggested_file_name,
            info.mime_type.as_deref(),
            &info.final_url,
            effective_referrer.as_deref(),
            &header_bytes,
        );

        let display_url = RedactedUrl::parse(&info.final_url).to_string();
        let total_size = info.total_size;
        let supports_range = info.supports_range;
        let now_ms = self.clock.now_unix_ms();
        let origin_key = SecretCredentials::origin_key_from_url(&info.final_url);

        {
            let mut guard = self.staged_probes.lock().await;
            if guard.len() >= MAX_STAGED_PROBES
                && let Some(oldest_manual_id) = guard
                    .iter()
                    .filter(|(_, e)| !e.is_browser_capture())
                    .min_by_key(|(_, e)| e.created_at_ms)
                    .map(|(k, _)| *k)
            {
                let _ = guard.remove(&oldest_manual_id);
            }
            guard.insert(
                staged_id,
                StagedProbeEntry {
                    seq: self.staged_seq.fetch_add(1, Ordering::Relaxed),
                    created_at_ms: now_ms,
                    expires_at_ms: now_ms.saturating_add(STAGED_PROBE_TTL_MS),
                    url: probe_url,
                    display_url: display_url.clone(),
                    suggested_file_name: suggested_file_name.clone(),
                    category_id: category_id.clone(),
                    suggested_save_dir: suggested_save_dir.clone(),
                    free_disk_bytes,
                    security: security.clone(),
                    duplicate: duplicate.clone(),
                    source: MANUAL_PREVIEW_SOURCE.to_owned(),
                    burst_quarantined: false,
                    referrer: effective_referrer,
                    top_level_site: None,
                    user_agent: input.user_agent,
                    headers: BTreeMap::new(),
                    creds: SecretCredentials {
                        cookies: input.cookies,
                        authorization: input.authorization,
                        origin_key,
                    },
                    info,
                    stream,
                },
            );
        }

        Ok(AddDownloadPreviewDto {
            staged_id: staged_id.to_string(),
            display_url,
            suggested_file_name,
            category_id,
            suggested_save_dir: suggested_save_dir.display().to_string(),
            total_size,
            supports_range,
            free_disk_bytes,
            security,
            duplicate,
            canonical_url_transformed: canonical.transformed,
            url_provider_hint: canonical.provider_hint.map(str::to_owned),
        })
    }

    /// Creates and persists a new [`Download`] (optionally consuming a warm `staged_id` stream for `0ms` start!).
    ///
    /// # Errors
    /// Returns [`VortexError`] on URL policy violation, persistence failure, or engine error.
    #[allow(clippy::too_many_lines)]
    pub async fn add_download(
        &self,
        cmd: AddDownloadCommand,
        trace_id: TraceId,
    ) -> AppResult<DownloadSummaryDto> {
        let (staged_opt, notify_staged_snapshot) = if let Some(sid) = cmd.staged_id {
            let mut guard = self.staged_probes.lock().await;
            let removed = guard.remove(&sid);
            let snap = removed
                .as_ref()
                .is_some_and(StagedProbeEntry::is_browser_capture)
                .then(|| summarize_browser_staged(&guard));
            (removed, snap)
        } else {
            (None, None)
        };

        if let Some((pending_count, latest_staged_id, burst_detected)) = notify_staged_snapshot {
            self.event_bus.publish(AppEvent::StagedDownloadsChanged {
                pending_count,
                latest_staged_id,
                burst_detected,
            });
        }

        let effective_url = staged_opt
            .as_ref()
            .map_or_else(|| cmd.url.clone(), |s| s.url.clone());
        let _ = FilenameSanitizer::validate_download_url(&effective_url)?;

        let id = cmd.staged_id.unwrap_or_default();
        let raw_name = cmd
            .file_name
            .clone()
            .or_else(|| {
                staged_opt
                    .as_ref()
                    .and_then(|s| s.info.suggested_filename.clone())
            })
            .or_else(|| basename_from_url(&effective_url))
            .unwrap_or_else(|| "download.bin".to_owned());
        let base_sanitized = FilenameSanitizer::sanitize_untrusted(&raw_name);

        let mime_hint = staged_opt
            .as_ref()
            .and_then(|s| s.info.mime_type.as_deref());
        let category_id = cmd
            .category_id
            .unwrap_or_else(|| CategorizationPolicy::classify(&base_sanitized, mime_hint).id);

        let save_dir = match cmd.save_dir {
            Some(dir) => dir,
            None => self.resolve_category_dir(&category_id, trace_id).await,
        };
        if let Err(e) = std::fs::create_dir_all(&save_dir) {
            return Err(VortexError::coded(
                ErrorCode::DiskPermissionDenied,
                trace_id,
                format!("Cannot create save directory {}: {e}", save_dir.display()),
            ));
        }

        let final_file_name = if cmd.overwrite_existing {
            base_sanitized
        } else {
            FilenameSanitizer::resolve_collision(&base_sanitized, |cand| {
                save_dir.join(cand).exists() || save_dir.join(format!("{cand}.vortex")).exists()
            })
        };
        let _ = FilenameSanitizer::safe_join(&save_dir, &final_file_name)?;

        let creds = SecretCredentials {
            cookies: cmd
                .cookies
                .or_else(|| staged_opt.as_ref().and_then(|s| s.creds.cookies.clone())),
            authorization: cmd.authorization.or_else(|| {
                staged_opt
                    .as_ref()
                    .and_then(|s| s.creds.authorization.clone())
            }),
            origin_key: staged_opt
                .as_ref()
                .and_then(|s| s.creds.origin_key.clone())
                .or_else(|| SecretCredentials::origin_key_from_url(&effective_url)),
        };
        let has_secrets = creds.cookies.is_some() || creds.authorization.is_some();

        let mut resource = Resource::new(effective_url.clone());
        resource.referrer = cmd
            .referrer
            .or_else(|| staged_opt.as_ref().and_then(|s| s.referrer.clone()));
        resource.top_level_site = staged_opt.as_ref().and_then(|s| s.top_level_site.clone());
        resource.user_agent = cmd
            .user_agent
            .or_else(|| staged_opt.as_ref().and_then(|s| s.user_agent.clone()));
        resource.headers = if cmd.headers.is_empty() {
            staged_opt
                .as_ref()
                .map_or_else(BTreeMap::new, |s| s.headers.clone())
        } else {
            cmd.headers
        };
        resource.has_secret_credentials = has_secrets;
        if let Some(st) = &staged_opt {
            resource.final_url = Some(st.info.final_url.clone());
            resource.mime_type.clone_from(&st.info.mime_type);
            resource.validators.clone_from(&st.info.validators);
        }

        let now_ms = self.clock.now_unix_ms();
        let redacted = RedactedUrl::parse(&effective_url).to_string();
        let (mut download, added_ev) = Download::new_pending(
            id,
            resource,
            redacted,
            save_dir,
            final_file_name,
            category_id,
            cmd.queue_id.unwrap_or_else(QueueId::main_queue),
            cmd.priority,
            cmd.expected_hash,
            now_ms,
            trace_id,
        );
        let mut events = vec![added_ev];

        if has_secrets {
            self.secret_store.store(id, &creds, trace_id).await?;
        }

        if !cmd.auto_start {
            let p_evs = download.apply(DownloadCommand::Pause, now_ms, trace_id)?;
            events.extend(p_evs);
        }

        self.download_repo.save(&download, trace_id).await?;
        self.event_repo.append(&events, trace_id).await?;
        for ev in events {
            self.event_bus.publish(AppEvent::Domain(ev));
        }

        let summary = DownloadSummaryDto::from_domain(&download);

        if cmd.auto_start {
            let preopened = staged_opt.map(|s| (s.info, s.stream));
            self.engine
                .start_download(download, preopened, trace_id)
                .await?;
        }

        Ok(summary)
    }

    /// Executes `Probe-Before-Commit` browser handoff (`Protocol-Spec.md` §4 & `P1-T21` / `P1-T28`).
    ///
    /// Enforces a hard **`1,400ms` probe timeout** so the MV3 Extension ALWAYS receives an answer
    /// inside its `2,000ms` `SafeSuggestGuard` window. On WAF `403`, single-use token `410`, or
    /// timeout, returns `fallback_to_browser: true` so the browser finishes seamlessly on its
    /// **original open TCP connection** via `suggest()`!
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` only when input fails security policy validation (e.g. `file://` or UNC).
    #[allow(clippy::too_many_lines)]
    pub async fn handoff_from_browser(
        &self,
        cmd: BrowserHandoffCommand,
        trace_id: TraceId,
    ) -> AppResult<BrowserHandoffResultDto> {
        let cfg = self.config_port.current();

        // Respect Incognito privacy rule (`capture_in_incognito == false` by default — `Protocol-Spec.md` §2.2)
        if cmd.incognito && !cfg.capture_rules.capture_in_incognito {
            return Ok(BrowserHandoffResultDto {
                download_id: None,
                accepted: false,
                duplicate_found: false,
                fallback_to_browser: true,
                fallback_reason: Some("incognito_disabled".to_owned()),
            });
        }

        let probe_target = cmd
            .final_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&cmd.url);
        let _ = FilenameSanitizer::validate_download_url(probe_target)?;

        self.evict_expired_staged_probes().await;

        let wants_auto_start = cmd.auto_start || cfg.auto_start_handoff;
        if !wants_auto_start {
            let pending_staged = self.staged_probes.lock().await.len();
            if pending_staged >= MAX_STAGED_PROBES {
                tracing::warn!(
                    %trace_id,
                    pending_staged,
                    max_staged = MAX_STAGED_PROBES,
                    "Browser handoff throttled: staged burst limit reached (`TH-20`)"
                );
                return Ok(BrowserHandoffResultDto {
                    download_id: None,
                    accepted: false,
                    duplicate_found: false,
                    fallback_to_browser: false,
                    fallback_reason: Some("burst_limit_reached".to_owned()),
                });
            }
        }

        let id = DownloadId::new();
        let mut probe_req = ResourceRequest::new(id, probe_target.to_owned(), trace_id);
        probe_req.referrer.clone_from(&cmd.referrer);
        probe_req.top_level_site.clone_from(&cmd.top_level_site);
        probe_req.user_agent.clone_from(&cmd.user_agent);
        probe_req.headers.clone_from(&cmd.headers);
        probe_req.cookies.clone_from(&cmd.cookies);
        probe_req.is_browser_handoff = true;

        // Enforce strict `1,400ms` Probe-Before-Commit budget!
        let probe_timed = tokio::time::timeout(
            BROWSER_PROBE_BUDGET,
            self.byte_source.probe_and_stream(&probe_req),
        )
        .await;

        let (info, raw_stream) = match probe_timed {
            Ok(Ok(ProbeOutcome::Transferable {
                info,
                initial_stream,
            })) => (info, initial_stream),
            Ok(Ok(ProbeOutcome::FallbackToBrowser {
                reason,
                status_code,
                detail,
            })) => {
                tracing::warn!(
                    %trace_id,
                    status_code,
                    reason = reason.as_str(),
                    detail = %detail,
                    "Browser handoff declined -> seamless browser suggest() fallback"
                );
                return Ok(BrowserHandoffResultDto {
                    download_id: None,
                    accepted: false,
                    duplicate_found: false,
                    fallback_to_browser: true,
                    fallback_reason: Some(reason.as_str().to_owned()),
                });
            }
            Ok(Err(probe_err)) => {
                tracing::warn!(
                    %trace_id,
                    error_code = %probe_err.code(),
                    "Browser handoff probe failed -> seamless browser suggest() fallback"
                );
                return Ok(BrowserHandoffResultDto {
                    download_id: None,
                    accepted: false,
                    duplicate_found: false,
                    fallback_to_browser: true,
                    fallback_reason: Some("probe_error".to_owned()),
                });
            }
            Err(_elapsed) => {
                tracing::warn!(
                    %trace_id,
                    budget_ms = BROWSER_PROBE_BUDGET.as_millis(),
                    "Browser handoff exceeded 1400ms budget -> seamless browser suggest() fallback"
                );
                return Ok(BrowserHandoffResultDto {
                    download_id: None,
                    accepted: false,
                    duplicate_found: false,
                    fallback_to_browser: true,
                    fallback_reason: Some("probe_timeout".to_owned()),
                });
            }
        };

        // Zero-loss `512`-byte Magic-Byte / HTML-trap / PE-masquerade inspection (`P1.5-T02`)!
        let (header_bytes, initial_stream) =
            peek_stream_header(raw_stream, MAGIC_PEEK_BYTES, MAGIC_PEEK_TIMEOUT).await;

        let raw_untrusted_name = info
            .raw_suggested_filename
            .clone()
            .or_else(|| info.suggested_filename.clone())
            .or_else(|| cmd.suggested_name.clone().filter(|s| !s.is_empty()))
            .or_else(|| basename_from_url(&info.final_url))
            .unwrap_or_else(|| "download.bin".to_owned());
        let clean_base = FilenameSanitizer::sanitize_untrusted(&raw_untrusted_name);
        let duplicate = self
            .check_duplicate(&cmd.url, Some(&clean_base), trace_id)
            .await?;
        let duplicate_found = duplicate.is_some();

        let mime_ref = info.mime_type.as_deref().or(cmd.mime_type.as_deref());
        let category_id = CategorizationPolicy::classify(&clean_base, mime_ref).id;
        let save_dir = self.resolve_category_dir(&category_id, trace_id).await;
        let free_disk_bytes = self.platform.free_disk_bytes(&save_dir, trace_id);
        let referrer_or_site = cmd.referrer.as_deref().or(cmd.top_level_site.as_deref());
        let security = ContentSecurityInspector::inspect_with_raw_name(
            &raw_untrusted_name,
            &clean_base,
            mime_ref,
            &info.final_url,
            referrer_or_site,
            &header_bytes,
        );
        let pinned_origin = SecretCredentials::origin_key_from_url(&info.final_url);

        // Ironclad rule (`P1.5-T01`): executables, masquerades, and HTML traps NEVER auto-start!
        let should_auto_start = wants_auto_start && !security.block_auto_start;

        if !should_auto_start {
            let now_ms = self.clock.now_unix_ms();
            let recent_burst_count = {
                let mut recent = self.recent_handoff_timestamps.lock().await;
                while let Some(&front) = recent.front() {
                    if now_ms.saturating_sub(front) > BURST_WINDOW_MS {
                        let _ = recent.pop_front();
                    } else {
                        break;
                    }
                }
                recent.push_back(now_ms);
                recent.len()
            };

            let display_url = RedactedUrl::parse(&info.final_url).to_string();
            let (inserted, snapshot) = {
                let mut guard = self.staged_probes.lock().await;
                if guard.len() >= MAX_STAGED_PROBES {
                    (false, None)
                } else {
                    let existing_browser_staged =
                        guard.values().filter(|e| e.is_browser_capture()).count();
                    let burst_quarantined = existing_browser_staged > 0 || recent_burst_count > 1;
                    guard.insert(
                        id,
                        StagedProbeEntry {
                            seq: self.staged_seq.fetch_add(1, Ordering::Relaxed),
                            created_at_ms: now_ms,
                            expires_at_ms: now_ms.saturating_add(STAGED_PROBE_TTL_MS),
                            url: cmd.url.clone(),
                            display_url,
                            suggested_file_name: clean_base,
                            category_id,
                            suggested_save_dir: save_dir,
                            free_disk_bytes,
                            security,
                            duplicate,
                            source: cmd.source,
                            burst_quarantined,
                            referrer: cmd.referrer,
                            top_level_site: cmd.top_level_site,
                            user_agent: cmd.user_agent,
                            headers: cmd.headers,
                            creds: SecretCredentials {
                                cookies: cmd.cookies,
                                authorization: None,
                                origin_key: pinned_origin.clone(),
                            },
                            info,
                            stream: initial_stream,
                        },
                    );
                    (true, Some(summarize_browser_staged(&guard)))
                }
            };

            if !inserted {
                return Ok(BrowserHandoffResultDto {
                    download_id: None,
                    accepted: false,
                    duplicate_found,
                    fallback_to_browser: false,
                    fallback_reason: Some("burst_limit_reached".to_owned()),
                });
            }

            if let Some((pending_count, latest_staged_id, burst_detected)) = snapshot {
                self.event_bus.publish(AppEvent::StagedDownloadsChanged {
                    pending_count,
                    latest_staged_id,
                    burst_detected,
                });
            }
            self.event_bus.publish(AppEvent::BringToFront {
                open_url: Some(format!("vortex://staged/{id}")),
            });
            return Ok(BrowserHandoffResultDto {
                download_id: Some(id.to_string()),
                accepted: true,
                duplicate_found,
                fallback_to_browser: false,
                fallback_reason: None,
            });
        }

        let _ = std::fs::create_dir_all(&save_dir);
        let final_name = FilenameSanitizer::resolve_collision(&clean_base, |cand| {
            save_dir.join(cand).exists() || save_dir.join(format!("{cand}.vortex")).exists()
        });

        // Start transfer immediately on Worker #1 using `initial_stream` (`0ms` wasted start)!
        let has_secrets = cmd.cookies.is_some();
        let mut resource = Resource::new(cmd.url.clone());
        resource.final_url = Some(info.final_url.clone());
        resource.referrer = cmd.referrer;
        resource.top_level_site = cmd.top_level_site;
        resource.user_agent = cmd.user_agent;
        resource.mime_type = info.mime_type.clone().or(cmd.mime_type);
        resource.headers = cmd.headers;
        resource.has_secret_credentials = has_secrets;
        resource.validators.clone_from(&info.validators);

        let now_ms = self.clock.now_unix_ms();
        let redacted = RedactedUrl::parse(&cmd.url).to_string();
        let (download, added_ev) = Download::new_pending(
            id,
            resource,
            redacted,
            save_dir,
            final_name,
            category_id,
            QueueId::main_queue(),
            Priority::Normal,
            info.server_checksum.clone(),
            now_ms,
            trace_id,
        );
        let events = vec![added_ev];

        if has_secrets {
            self.secret_store
                .store(
                    id,
                    &SecretCredentials {
                        cookies: cmd.cookies,
                        authorization: None,
                        origin_key: pinned_origin,
                    },
                    trace_id,
                )
                .await?;
        }
        self.download_repo.save(&download, trace_id).await?;
        self.event_repo.append(&events, trace_id).await?;
        for ev in events {
            self.event_bus.publish(AppEvent::Domain(ev));
        }

        self.engine
            .start_download(download, Some((info, initial_stream)), trace_id)
            .await?;

        Ok(BrowserHandoffResultDto {
            download_id: Some(id.to_string()),
            accepted: true,
            duplicate_found,
            fallback_to_browser: false,
            fallback_reason: None,
        })
    }

    /// Starts or resumes a persisted download by `id`.
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if `id` does not exist.
    pub async fn start_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        self.engine.resume_download(id, trace_id).await
    }

    /// Durably pauses an active or queued download.
    ///
    /// # Errors
    /// Returns [`VortexError`] if checkpointing fails.
    pub async fn pause_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        self.engine.pause_download(id, trace_id).await
    }

    /// Durably pauses all active and queued downloads.
    ///
    /// # Errors
    /// Returns [`VortexError`] if pausing fails.
    pub async fn pause_all(&self, trace_id: TraceId) -> AppResult<()> {
        self.engine.pause_all(trace_id).await
    }

    /// Resumes a paused or failed download.
    ///
    /// # Errors
    /// Returns [`VortexError`] if `id` is missing or cannot resume.
    pub async fn resume_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        self.engine.resume_download(id, trace_id).await
    }

    /// Cancels an active download and optionally deletes partial `.vortex` files.
    ///
    /// # Errors
    /// Returns [`VortexError`] on cancellation failure.
    pub async fn cancel_download(
        &self,
        id: DownloadId,
        delete_partial_files: bool,
        trace_id: TraceId,
    ) -> AppResult<()> {
        self.engine
            .cancel_download(id, delete_partial_files, trace_id)
            .await
    }

    /// Removes a download into the **`6-second` Undo Trash Buffer** (`P1-T21` & `P1-T32`).
    ///
    /// Calling [`Self::undo_remove_download`] within `6s` restores the record cleanly without
    /// losing partial `.vortex` or final files!
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if `id` is not found.
    pub async fn remove_download(
        &self,
        id: DownloadId,
        delete_completed_file: bool,
        trace_id: TraceId,
    ) -> AppResult<()> {
        self.purge_expired_deletions(trace_id).await;
        // Stop active actor WITHOUT deleting disk files yet so Undo within 6s is 100% lossless!
        let _ = self.engine.cancel_download(id, false, trace_id).await;

        let Some(existing) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found for removal"),
            ));
        };

        self.download_repo.delete(id, trace_id).await?;
        let expires_at_ms = self
            .clock
            .now_unix_ms()
            .saturating_add(UNDO_DELETE_WINDOW_MS);
        self.undo_trash.lock().await.insert(
            id,
            PendingDeletionEntry {
                download: existing,
                delete_completed_file,
                expires_at_ms,
            },
        );
        Ok(())
    }

    /// Restores a recently removed download from the `6-second` Undo buffer (`P1-T21` & `P1-T32`).
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if the Undo window has already expired.
    pub async fn undo_remove_download(
        &self,
        id: DownloadId,
        trace_id: TraceId,
    ) -> AppResult<DownloadSummaryDto> {
        let entry_opt = self.undo_trash.lock().await.remove(&id);
        let Some(mut entry) = entry_opt else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Undo window expired or download {id} not in trash"),
            ));
        };
        if entry.download.state.is_active() {
            let _ =
                entry
                    .download
                    .apply(DownloadCommand::Pause, self.clock.now_unix_ms(), trace_id);
        }
        self.download_repo.save(&entry.download, trace_id).await?;
        Ok(DownloadSummaryDto::from_domain(&entry.download))
    }

    /// Permanently purges any deleted downloads whose `6-second` Undo window has elapsed.
    pub async fn purge_expired_deletions(&self, trace_id: TraceId) -> usize {
        let now_ms = self.clock.now_unix_ms();
        let expired: Vec<(DownloadId, PendingDeletionEntry)> = {
            let mut guard = self.undo_trash.lock().await;
            let keys: Vec<DownloadId> = guard
                .iter()
                .filter_map(|(k, v)| (now_ms >= v.expires_at_ms).then_some(*k))
                .collect();
            keys.into_iter()
                .filter_map(|k| guard.remove(&k).map(|v| (k, v)))
                .collect()
        };

        let count = expired.len();
        for (id, entry) in expired {
            let _ = self.secret_store.delete(id, trace_id).await;
            let _ = tokio::fs::remove_file(entry.download.temp_path()).await;
            let _ = tokio::fs::remove_file(entry.download.meta_sidecar_path()).await;
            if entry.delete_completed_file {
                let _ = tokio::fs::remove_file(entry.download.final_path()).await;
            }
        }
        count
    }

    /// Validates, atomically saves, and hot-reloads `new_config` (`P1-T09` & `P1-T21`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if validation or atomic disk save fails.
    pub async fn update_settings(
        &self,
        new_config: AppConfig,
        trace_id: TraceId,
    ) -> AppResult<Arc<AppConfig>> {
        let updated = self.config_port.update(new_config, trace_id).await?;
        self.event_bus
            .publish(AppEvent::ConfigChanged(Arc::clone(&updated)));
        Ok(updated)
    }

    // ========================================================================
    // QUERIES (`P1-T21`)
    // ========================================================================

    /// Lists downloads with optional filtering, overlaying live `EngineControlPort` snapshots.
    ///
    /// # Errors
    /// Returns [`VortexError`] if SQLite query fails.
    pub async fn list_downloads(
        &self,
        filter: Option<DownloadFilterDto>,
        trace_id: TraceId,
    ) -> AppResult<Vec<DownloadSummaryDto>> {
        let _ = self.purge_expired_deletions(trace_id).await;
        let all = self.download_repo.list_all(trace_id).await?;
        let live_snaps = self.live_snapshot_map().await;

        let mut out = Vec::with_capacity(all.len());
        for dl in &all {
            let mut dto = DownloadSummaryDto::from_domain(dl);
            if let Some(snap) = live_snaps.get(&dto.id) {
                dto.downloaded_bytes = snap.downloaded_bytes;
                dto.total_size = snap.total_size.or(dto.total_size);
                dto.speed_bps = snap.speed_fast_bps;
                dto.eta_secs = snap.eta_secs;
                dto.active_workers = snap.active_workers;
                dto.state_slug.clone_from(&snap.state_slug);
            }
            if let Some(f) = &filter {
                if let Some(st) = &f.state_slug
                    && &dto.state_slug != st
                {
                    continue;
                }
                if let Some(cat) = &f.category_id
                    && dto.category_id.as_str() != cat
                {
                    continue;
                }
                if let Some(q) = &f.queue_id
                    && dto.queue_id.as_str() != q
                {
                    continue;
                }
            }
            out.push(dto);
        }
        Ok(out)
    }

    /// Retrieves full [`DownloadDetailsDto`] (including live 512-bin `<canvas>` map) for `id`.
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if `id` is not found.
    pub async fn get_download_details(
        &self,
        id: DownloadId,
        trace_id: TraceId,
    ) -> AppResult<DownloadDetailsDto> {
        let Some(dl) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found"),
            ));
        };
        let mut details = DownloadDetailsDto::from_domain(&dl);
        let id_str = id.to_string();
        for snap in self.engine.active_snapshots().await {
            if snap.download_id == id_str {
                details.summary.downloaded_bytes = snap.downloaded_bytes;
                details.summary.total_size = snap.total_size.or(details.summary.total_size);
                details.summary.speed_bps = snap.speed_fast_bps;
                details.summary.eta_secs = snap.eta_secs;
                details.summary.active_workers = snap.active_workers;
                details.summary.state_slug = snap.state_slug;
                details.bins = snap.bins;
                break;
            }
        }
        Ok(details)
    }

    /// Instant SIMD-accelerated fuzzy search across `file_name` and `display_url` via **`nucleo-matcher`** (`P1-T21`).
    ///
    /// Supports both Latin and Arabic UTF-32 codepoints with zero heap churn inside the matcher loop.
    ///
    /// # Errors
    /// Returns [`VortexError`] if listing downloads fails.
    pub async fn search_downloads(
        &self,
        query: &str,
        trace_id: TraceId,
    ) -> AppResult<Vec<DownloadSummaryDto>> {
        let all = self.list_downloads(None, trace_id).await?;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(all);
        }

        let mut matcher = Matcher::new(NucleoConfig::DEFAULT.match_paths());
        let pattern = Pattern::parse(trimmed, CaseMatching::Ignore, Normalization::Smart);
        let mut utf32_buf = Vec::new();

        let mut scored: Vec<(u32, DownloadSummaryDto)> = Vec::new();
        for item in all {
            let file_score =
                pattern.score(Utf32Str::new(&item.file_name, &mut utf32_buf), &mut matcher);
            utf32_buf.clear();
            let url_score = pattern.score(
                Utf32Str::new(&item.display_url, &mut utf32_buf),
                &mut matcher,
            );
            utf32_buf.clear();

            if let Some(best) = file_score.map(|s| s.saturating_mul(2)).max(url_score) {
                scored.push((best, item));
            }
        }

        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        Ok(scored.into_iter().map(|(_, dto)| dto).collect())
    }

    /// Checks whether `url` or `file_name` duplicates an existing download using **`ahash::AHashMap`** (`P1-T21`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if reading repository fails.
    pub async fn check_duplicate(
        &self,
        url: &str,
        file_name: Option<&str>,
        trace_id: TraceId,
    ) -> AppResult<Option<DuplicateMatchDto>> {
        let list = self.download_repo.list_all(trace_id).await?;
        let mut by_url: AHashMap<String, &Download> = AHashMap::with_capacity(list.len());
        let mut by_name: AHashMap<String, &Download> = AHashMap::with_capacity(list.len());

        for dl in &list {
            by_url.insert(normalize_url_key(&dl.resource.url), dl);
            if let Some(fin) = &dl.resource.final_url {
                by_url.insert(normalize_url_key(fin), dl);
            }
            by_name.insert(dl.file_name.to_lowercase(), dl);
        }

        let target_key = normalize_url_key(url);
        let matched = by_url
            .get(&target_key)
            .copied()
            .or_else(|| file_name.and_then(|n| by_name.get(&n.to_lowercase()).copied()));

        Ok(matched.map(|d| DuplicateMatchDto {
            existing_id: d.id.to_string(),
            file_name: d.file_name.clone(),
            save_dir: d.save_dir.display().to_string(),
            state_slug: d.state.as_slug().to_owned(),
            file_exists_on_disk: d.final_path().exists(),
        }))
    }

    /// Runs the system-wide Self-Health Check (`P1-T21` & `P1-T23`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if diagnostics fail.
    pub async fn get_health_status(&self, trace_id: TraceId) -> AppResult<HealthStatusDto> {
        self.diagnostics.check_health(trace_id).await
    }

    /// Builds a preview manifest of the sanitized `.zip` diagnostic bundle (`P1-T21` & `P1-T23`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if preview generation fails.
    pub async fn get_diagnostic_summary(
        &self,
        trace_id: TraceId,
    ) -> AppResult<DiagnosticBundleReportDto> {
        self.diagnostics.preview_bundle(trace_id).await
    }

    /// Exports the sanitized `.zip` diagnostic bundle to `dest_zip_path` (`P1-T23` & `P1-T34`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if bundle writing fails.
    pub async fn export_diagnostic_bundle(
        &self,
        dest_zip_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<DiagnosticBundleReportDto> {
        self.diagnostics
            .export_bundle(dest_zip_path, trace_id)
            .await
    }

    /// Returns recent redacted `FlightRecorder` log lines (`P1-T34`).
    #[must_use]
    pub fn recent_logs(&self, limit: usize) -> Vec<LogEntryDto> {
        self.diagnostics.recent_logs(limit)
    }

    /// Toggles 30-minute live `DEBUG` diagnostic mode (`P1-T34`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if filter reload fails.
    pub fn set_debug_mode(&self, enabled: bool, trace_id: TraceId) -> AppResult<bool> {
        self.diagnostics.set_debug_mode(enabled, trace_id)
    }

    /// Repairs `HKCU` Native Messaging manifests for Brave, Chrome, and Edge (`[🔧 إصلاح ربط المتصفح]` — `P1-T34`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if registry update fails.
    pub fn repair_browser_integration(
        &self,
        bridge_exe: &Path,
        allowed_extension_ids: &[String],
        trace_id: TraceId,
    ) -> AppResult<Vec<crate::dto::BrowserIntegrationStatusDto>> {
        self.platform
            .register_native_hosts(bridge_exe, allowed_extension_ids, trace_id)
    }

    /// Current immutable [`AppConfig`] snapshot.
    #[must_use]
    pub fn current_config(&self) -> Arc<AppConfig> {
        self.config_port.current()
    }

    // ========================================================================
    // STAGED CONFIRMATION QUEUE & NATIVE OS SHELL ACTIONS (`P1.5-T03`..`T05`)
    // ========================================================================

    /// Lists all pending browser-captured downloads awaiting explicit user confirmation in FIFO order.
    ///
    /// Pull-based design guarantees waking `WebView2` from Tray NEVER misses an event (`P1.5-T03`).
    pub async fn list_staged_downloads(&self) -> Vec<StagedDownloadDto> {
        self.evict_expired_staged_probes().await;
        let guard = self.staged_probes.lock().await;
        let mut items: Vec<(u64, StagedDownloadDto)> = guard
            .iter()
            .filter(|(_, entry)| entry.is_browser_capture())
            .map(|(id, entry)| (entry.seq, entry.to_dto(*id)))
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        items.into_iter().map(|(_, dto)| dto).collect()
    }

    /// Immediately discards a staged download in `0ms`: closes its TCP socket, releases its `16 MiB`
    /// RAM buffer, zeroizes in-memory cookies, and removes any persisted secrets (`P1.5-T03`).
    ///
    /// # Errors
    /// Returns [`VortexError`] only if secret store cleanup reports an unexpected I/O failure.
    pub async fn discard_staged_download(
        &self,
        staged_id: DownloadId,
        trace_id: TraceId,
    ) -> AppResult<bool> {
        let (removed, snapshot) = {
            let mut guard = self.staged_probes.lock().await;
            let entry = guard.remove(&staged_id);
            let snap = entry
                .as_ref()
                .is_some_and(StagedProbeEntry::is_browser_capture)
                .then(|| summarize_browser_staged(&guard));
            (entry.is_some(), snap)
        };

        let _ = self.secret_store.delete(staged_id, trace_id).await;

        if let Some((pending_count, latest_staged_id, burst_detected)) = snapshot {
            self.event_bus.publish(AppEvent::StagedDownloadsChanged {
                pending_count,
                latest_staged_id,
                burst_detected,
            });
        }
        Ok(removed)
    }

    /// Discards ALL pending browser-captured downloads at once (1-click Ad-Burst cleanup — `TH-20`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if cleanup encounters an unexpected error.
    pub async fn discard_all_staged_downloads(&self, trace_id: TraceId) -> AppResult<usize> {
        let removed_ids: Vec<DownloadId> = {
            let mut guard = self.staged_probes.lock().await;
            let keys: Vec<DownloadId> = guard
                .iter()
                .filter_map(|(k, v)| v.is_browser_capture().then_some(*k))
                .collect();
            for k in &keys {
                let _ = guard.remove(k);
            }
            keys
        };

        let count = removed_ids.len();
        for id in removed_ids {
            let _ = self.secret_store.delete(id, trace_id).await;
        }

        if count > 0 {
            self.event_bus.publish(AppEvent::StagedDownloadsChanged {
                pending_count: 0,
                latest_staged_id: None,
                burst_detected: false,
            });
        }
        Ok(count)
    }

    /// Adds `host` to `capture_rules.excluded_hosts` (immediately synced to Brave via Named Pipe!)
    /// and discards `staged_id` plus any other staged captures originating from `host` (`P1.5-T04`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if `host` is empty or persisting settings fails.
    pub async fn block_capture_host_and_discard(
        &self,
        staged_id: DownloadId,
        host: String,
        trace_id: TraceId,
    ) -> AppResult<Arc<AppConfig>> {
        let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                "Cannot block an empty hostname",
            ));
        }

        let mut next_cfg = (*self.config_port.current()).clone();
        if !next_cfg
            .capture_rules
            .excluded_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&normalized))
        {
            next_cfg
                .capture_rules
                .excluded_hosts
                .push(normalized.clone());
        }
        let updated = self.update_settings(next_cfg, trace_id).await?;

        let (removed_ids, snapshot) = {
            let mut guard = self.staged_probes.lock().await;
            let keys: Vec<DownloadId> = guard
                .iter()
                .filter_map(|(k, v)| {
                    let matches_host = v.security.origin_host.eq_ignore_ascii_case(&normalized)
                        || v.security
                            .referrer_host
                            .as_deref()
                            .is_some_and(|r| r.eq_ignore_ascii_case(&normalized));
                    (*k == staged_id || (v.is_browser_capture() && matches_host)).then_some(*k)
                })
                .collect();
            for k in &keys {
                let _ = guard.remove(k);
            }
            (keys, summarize_browser_staged(&guard))
        };

        for id in removed_ids {
            let _ = self.secret_store.delete(id, trace_id).await;
        }
        let (pending_count, latest_staged_id, burst_detected) = snapshot;
        self.event_bus.publish(AppEvent::StagedDownloadsChanged {
            pending_count,
            latest_staged_id,
            burst_detected,
        });

        Ok(updated)
    }

    /// Queries free bytes on the disk volume hosting `target_dir`.
    #[must_use]
    pub fn free_disk_bytes_for_dir(&self, target_dir: &Path, trace_id: TraceId) -> Option<u64> {
        self.platform.free_disk_bytes(target_dir, trace_id)
    }

    /// Opens the native OS folder picker dialog (`IFileOpenDialog` on Windows — `P1.5-T05`).
    ///
    /// # Errors
    /// Returns [`VortexError`] if the OS dialog fails unexpectedly.
    pub fn pick_folder(
        &self,
        initial_dir: Option<&Path>,
        trace_id: TraceId,
    ) -> AppResult<Option<PathBuf>> {
        self.platform.pick_folder(initial_dir, trace_id)
    }

    /// Reveals a download's final file, partial `.vortex` file, or save folder in the OS file manager (`P1.5-T05`).
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if the download does not exist or OS shell launch fails.
    pub async fn reveal_in_folder(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        let Some(dl) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found"),
            ));
        };
        let final_path = dl.final_path();
        let temp_path = dl.temp_path();
        let target = if final_path.exists() {
            final_path
        } else if temp_path.exists() {
            temp_path
        } else {
            dl.save_dir
        };
        self.platform.reveal_in_file_manager(&target, trace_id)
    }

    /// Opens a completed file safely via OS Shell (`ShellExecuteW`, respecting `MotW` `SmartScreen` — `P1.5-T05`).
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if the download or completed file on disk does not exist.
    pub async fn open_downloaded_file(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        let Some(dl) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found"),
            ));
        };
        let final_path = dl.final_path();
        if !final_path.exists() {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!(
                    "Completed file does not exist on disk: {}",
                    final_path.display()
                ),
            ));
        }
        self.platform.open_file_safely(&final_path, trace_id)
    }

    /// Opens the source/referrer webpage of `id` in the user's browser (used for `VTX-RES-006` CAPTCHA/Quota
    /// or `VTX-RES-002` link refresh — `P1.6-T06`).
    ///
    /// # Errors
    /// Returns `VTX-RES-001` if `id` is not found or `VTX-SEC-001` if the URL is not `http`/`https`.
    pub async fn open_download_referrer(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        let Some(dl) = self.download_repo.find(id, trace_id).await? else {
            return Err(VortexError::coded(
                ErrorCode::ResNotFound,
                trace_id,
                format!("Download {id} not found"),
            ));
        };
        let target = dl
            .resource
            .referrer
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(dl.resource.effective_url());
        self.open_external_url(target, trace_id)
    }

    /// Validates `url` (`http`/`https` or exact hardcoded safe Windows settings URI) and opens it (`UX-M01` / `VTX-RES-006`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if `url` violates the protocol allowlist or OS shell launch fails.
    pub fn open_external_url(&self, url: &str, trace_id: TraceId) -> AppResult<()> {
        let trimmed = url.trim();
        let is_safe_os_settings = matches!(
            trimmed,
            "ms-settings:dateandtime" | "windowsdefender://threat"
        );
        if !is_safe_os_settings {
            let _ = FilenameSanitizer::validate_download_url(trimmed)?;
        }
        self.platform.open_url_in_browser(trimmed, trace_id)
    }

    async fn resolve_category_dir(&self, category_id: &CategoryId, trace_id: TraceId) -> PathBuf {
        if let Ok(Some(rec)) = self.category_repo.find(category_id, trace_id).await
            && !rec.folder.as_os_str().is_empty()
        {
            return rec.folder;
        }
        let base = &self.config_port.current().default_download_dir;
        let sub = match category_id.as_str() {
            CategoryId::VIDEO => "Video",
            CategoryId::AUDIO => "Audio",
            CategoryId::COMPRESSED => "Compressed",
            CategoryId::PROGRAMS => "Programs",
            CategoryId::DOCUMENTS => "Documents",
            CategoryId::IMAGES => "Images",
            _ => "",
        };
        if sub.is_empty() {
            base.clone()
        } else {
            base.join(sub)
        }
    }

    async fn live_snapshot_map(&self) -> AHashMap<String, ProgressSnapshotDto> {
        let snaps = self.engine.active_snapshots().await;
        let mut map = AHashMap::with_capacity(snaps.len());
        for s in snaps {
            map.insert(s.download_id.clone(), s);
        }
        map
    }

    /// Actively evicts expired staged probes (`> 120s` TTL), closing their sockets and zeroizing
    /// in-memory credentials (`TH-31` — invoked both lazily and by the `15s` background reaper!).
    pub async fn evict_expired_staged_probes(&self) -> usize {
        let now_ms = self.clock.now_unix_ms();
        let (evicted_count, notify) = {
            let mut guard = self.staged_probes.lock().await;
            let before_total = guard.len();
            let before_browser = guard.values().filter(|v| v.is_browser_capture()).count();
            guard.retain(|_, v| now_ms < v.expires_at_ms);
            let after_browser = guard.values().filter(|v| v.is_browser_capture()).count();
            let evicted = before_total.saturating_sub(guard.len());
            let snap = (after_browser != before_browser).then(|| summarize_browser_staged(&guard));
            (evicted, snap)
        };
        if let Some((pending_count, latest_staged_id, burst_detected)) = notify {
            self.event_bus.publish(AppEvent::StagedDownloadsChanged {
                pending_count,
                latest_staged_id,
                burst_detected,
            });
        }
        evicted_count
    }
}

fn summarize_browser_staged(
    map: &AHashMap<DownloadId, StagedProbeEntry>,
) -> (usize, Option<String>, bool) {
    let mut pending_count = 0_usize;
    let mut latest: Option<(i64, DownloadId)> = None;
    let mut burst_detected = false;
    for (&id, entry) in map {
        if !entry.is_browser_capture() {
            continue;
        }
        pending_count += 1;
        if entry.burst_quarantined {
            burst_detected = true;
        }
        if latest.is_none_or(|(ts, _)| entry.created_at_ms >= ts) {
            latest = Some((entry.created_at_ms, id));
        }
    }
    if pending_count > 1 {
        burst_detected = true;
    }
    (
        pending_count,
        latest.map(|(_, id)| id.to_string()),
        burst_detected,
    )
}

fn normalize_url_key(raw: &str) -> String {
    let without_frag = raw.split('#').next().unwrap_or(raw).trim();
    without_frag.to_owned()
}

fn basename_from_url(raw: &str) -> Option<String> {
    let without_q = raw.split(['?', '#']).next().unwrap_or(raw);
    let seg = without_q.rsplit('/').next()?;
    let trimmed = seg.trim();
    if trimmed.is_empty() || trimmed.contains(':') {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
