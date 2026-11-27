//! Hexagonal Ports (Traits) isolating Core Domain & Application from Level 3 Adapters (`P1-T06`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use vortex_domain::{
    ByteRange, CategoryId, Checksum, DomainEventEnvelope, Download, DownloadId, HashAlgorithm,
    QueueId, RangeSet, ResumeValidators,
};
use vortex_error::TraceId;
use vortex_observability::SecretString;

use crate::AppResult;
use crate::config::AppConfig;
use crate::dto::{
    BrowserIntegrationStatusDto, DiagnosticBundleReportDto, HealthStatusDto, LogEntryDto,
    ProgressSnapshotDto,
};

// ============================================================================
// 1. Network / ByteSource Port (`vortex-net`)
// ============================================================================

/// Kernel- or protocol-level RTT telemetry feeding BBR/Gradient2 concurrency (`§8.5` & `P1-T14` / `P1.7-T04`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportMetrics {
    /// Smoothed or latest RTT in microseconds (`WSAIoctl(SIO_TCP_INFO)` or protocol ping).
    pub rtt_us: Option<u64>,
    /// Windowed minimum RTT in microseconds (`60s` min-filter or `TCP_INFO_v1::MinRtt`).
    pub min_rtt_us: Option<u64>,
    /// `true` when traffic traverses an HTTP/SOCKS proxy (`127.0.0.1` socket RTT ignored).
    pub is_proxied: bool,
    /// `true` if connection came warm from the HTTP Keep-Alive / H2 pool.
    pub reused_connection: bool,
    /// `true` when the active stream negotiated `HTTP/2` over `ALPN: h2` (`P1.7-T04`).
    pub is_http2: bool,
}

/// Reason why `probe_and_stream` declined takeover so the browser continues seamlessly via `suggest()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// Cloudflare / WAF TLS JA3/JA4 fingerprint bound (`403`/`503` challenge).
    WafTlsBound,
    /// Single-use download token already consumed by browser's initial `GET` (`403`/`410`).
    SingleUseToken,
    /// Interactive session/authentication challenge (`401`/`407`).
    AuthChallenge,
    /// Browser already downloaded `>= 80%` of a small fast file.
    BrowserNearlyDone,
}

impl FallbackReason {
    /// Wire identifier returned in `DownloadHandoffAck.fallback_reason`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WafTlsBound => "waf_tls_bound",
            Self::SingleUseToken => "single_use_token",
            Self::AuthChallenge => "auth_challenge",
            Self::BrowserNearlyDone => "browser_nearly_done",
        }
    }
}

/// Outbound network request context carrying redacted-safe [`SecretString`] credentials.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    /// Owning download ID.
    pub download_id: DownloadId,
    /// Target URL (`http://` or `https://`).
    pub url: String,
    /// Optional `Referer`.
    pub referrer: Option<String>,
    /// Optional CHIPS `top_level_site`.
    pub top_level_site: Option<String>,
    /// Preserved browser `User-Agent`.
    pub user_agent: Option<String>,
    /// Additional non-secret HTTP headers.
    pub headers: BTreeMap<String, String>,
    /// Zeroized cookie header (`normal + CHIPS`).
    pub cookies: Option<SecretString>,
    /// Zeroized `Authorization` header.
    pub authorization: Option<SecretString>,
    /// Recorded validators for RFC 9110 `If-Range` / `206` verification.
    pub validators: ResumeValidators,
    /// Distributed correlation trace ID.
    pub trace_id: TraceId,
    /// Whether this request originated from a fresh live browser handoff (`403`/`410` -> `FallbackToBrowser`).
    pub is_browser_handoff: bool,
    /// Explicit user opt-in allowing redirects to private/loopback IPs (strictly `false` by default — `TH-04`).
    pub allow_private_redirect: bool,
}

impl ResourceRequest {
    /// Creates a baseline [`ResourceRequest`] for `download_id` and `url`.
    #[must_use]
    pub fn new(download_id: DownloadId, url: impl Into<String>, trace_id: TraceId) -> Self {
        Self {
            download_id,
            url: url.into(),
            referrer: None,
            top_level_site: None,
            user_agent: None,
            headers: BTreeMap::new(),
            cookies: None,
            authorization: None,
            validators: ResumeValidators::default(),
            trace_id,
            is_browser_handoff: false,
            allow_private_redirect: false,
        }
    }
}

/// Metadata discovered during `probe_and_stream`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceInfo {
    /// Final pinned URL after safe redirects (`<= 10`).
    pub final_url: String,
    /// Total resource byte length (`None` for chunked / unbounded).
    pub total_size: Option<u64>,
    /// `true` if server honored `Range: bytes=0-` with `206 Partial Content`.
    pub supports_range: bool,
    /// Cleaned `Content-Type`.
    pub mime_type: Option<String>,
    /// Sanitized filename parsed from `Content-Disposition` (RFC 6266 / 5987 / 2047).
    pub suggested_filename: Option<String>,
    /// Pre-sanitization filename from `Content-Disposition` so `TH-30` can detect `RTLO`/bidi spoofing.
    pub raw_suggested_filename: Option<String>,
    /// Extracted `ETag`, `Last-Modified` (+ `Date`), and `Content-Length`.
    pub validators: ResumeValidators,
    /// Optional server-advertised digest (`Repr-Digest` / `Content-MD5`).
    pub server_checksum: Option<Checksum>,
    /// Initial socket RTT metrics.
    pub transport_metrics: TransportMetrics,
}

/// Streaming byte reader over an open HTTP response or 16 MiB probe ring buffer.
#[async_trait]
pub trait ByteStream: Send {
    /// Reads the next Zero-Copy [`Bytes`] chunk (`Ok(None)` at clean EOF).
    async fn next_chunk(&mut self) -> AppResult<Option<Bytes>>;

    /// Latest kernel/protocol RTT snapshot on the underlying socket.
    fn transport_metrics(&self) -> TransportMetrics;

    /// Optionally bounds an unbounded probe stream (`Range: bytes=0-`) to stop pumping past
    /// `stop_offset` once Worker #1's tail is stolen by a peer (`P1.7-T03` zero-waste probe).
    fn set_stop_offset(&self, _stop_offset: u64) {}
}

/// Outcome of `ByteSource::probe_and_stream` (`§8.2` & `P1-T15`).
pub enum ProbeOutcome {
    /// Probe succeeded; `initial_stream` continues streaming from byte `0` (buffered up to `16 MiB`)
    /// so Worker #1 starts with **`0ms` wasted latency**!
    Transferable {
        /// Discovered resource metadata.
        info: Box<ResourceInfo>,
        /// Live open stream starting at offset `0`.
        initial_stream: Box<dyn ByteStream>,
    },
    /// WAF TLS lock (`403`/`503`) or single-use token (`410`) detected on browser handoff;
    /// Extension must call `suggest()` so browser finishes on its original open TCP socket!
    FallbackToBrowser {
        /// Structured fallback classification.
        reason: FallbackReason,
        /// HTTP status code received.
        status_code: u16,
        /// Sanitized diagnostic note.
        detail: String,
    },
}

impl std::fmt::Debug for ProbeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transferable { info, .. } => f
                .debug_struct("Transferable")
                .field("info", info)
                .finish_non_exhaustive(),
            Self::FallbackToBrowser {
                reason,
                status_code,
                detail,
            } => f
                .debug_struct("FallbackToBrowser")
                .field("reason", reason)
                .field("status_code", status_code)
                .field("detail", detail)
                .finish(),
        }
    }
}

/// Network protocol adapter port (`HTTP/1.1-2-3`, and later `FTP`/`SFTP`).
#[async_trait]
pub trait ByteSource: Send + Sync {
    /// Sends a single `GET Range: bytes=0-` (`Accept-Encoding: identity`), classifies WAF/token
    /// fallbacks, and keeps the live stream open for Worker #1 (`0ms` start).
    async fn probe_and_stream(&self, req: &ResourceRequest) -> AppResult<ProbeOutcome>;

    /// Opens a bounded or ranged stream `[range.start, range.end)` applying RFC 9110 `If-Range`
    /// and programmatic `206` validator verification.
    async fn open_range(
        &self,
        req: &ResourceRequest,
        range: ByteRange,
    ) -> AppResult<(Box<dyn ByteStream>, TransportMetrics)>;
}

// ============================================================================
// 2. Disk I/O & Volume Space Port (`vortex-io`)
// ============================================================================

/// Crash-recovery `.vortex.meta` sidecar schema (`§12.2` & `P1-T12` — never contains secrets!).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarMetadata {
    /// Schema version (`1`).
    pub version: u32,
    /// Download ID.
    pub download_id: DownloadId,
    /// Redacted or public source URL (stripped of sensitive query params if needed).
    pub url: String,
    /// Pinned final URL.
    pub final_url: Option<String>,
    /// Final sanitized basename.
    pub file_name: String,
    /// Target directory.
    pub save_dir: PathBuf,
    /// Total file size in bytes.
    pub total_size: Option<u64>,
    /// Whether server supports `Range`.
    pub supports_range: bool,
    /// Resume validators (`ETag`, `Last-Modified`, `Content-Length`).
    pub validators: ResumeValidators,
    /// Durably flushed `RangeSet` (strictly after `FlushFileBuffers`!).
    pub completed_ranges: RangeSet,
    /// Expected checksum.
    pub expected_hash: Option<Checksum>,
    /// Last durable checkpoint Unix timestamp (ms).
    pub updated_at_ms: i64,
}

/// Handle to an active logical disk-space reservation inside [`VolumeSpaceLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpaceReservationId(pub u64);

/// Logical disk space ledger preventing concurrent sparse downloads from exceeding real free space (`§12.1.2`).
pub trait VolumeSpaceLedger: Send + Sync {
    /// Reserves `bytes` logically on the volume containing `target_dir`, failing with `VTX-DISK-001`
    /// if `(physical_free - total_reserved - bytes) < min_free_reserve`.
    ///
    /// # Errors
    /// Returns `VTX-DISK-001` (`DiskInsufficientSpace`) or `VTX-DISK-004` (`DiskVolumeDisconnected`).
    fn reserve(
        &self,
        target_dir: &Path,
        bytes: u64,
        min_free_reserve: u64,
        trace_id: TraceId,
    ) -> AppResult<SpaceReservationId>;

    /// Decrements remaining logical reservation as physical sparse extents are written on disk.
    fn record_written(&self, id: SpaceReservationId, written_bytes: u64);

    /// Releases any remaining logical reservation when a download pauses, finishes, or aborts.
    fn release(&self, id: SpaceReservationId);

    /// Queries physical free bytes available to the current user (`GetDiskFreeSpaceExW` on Windows).
    ///
    /// # Errors
    /// Returns `VTX-DISK-004` or `VTX-DISK-002` if the volume cannot be queried.
    fn available_bytes(&self, target_dir: &Path, trace_id: TraceId) -> AppResult<u64>;

    /// Verifies that the destination volume's filesystem supports a file of `total_size` bytes
    /// (e.g. blocks `> 4 GiB - 1` files on `FAT32` upfront with `VTX-DISK-005` — `UX-M02`).
    ///
    /// # Errors
    /// Returns `VTX-DISK-005` (`DiskFilesystemLimit`) if `total_size` exceeds the filesystem limit.
    fn check_file_size_supported(
        &self,
        _target_dir: &Path,
        _total_size: u64,
        _trace_id: TraceId,
    ) -> AppResult<()> {
        Ok(())
    }
}

/// Positional sparse file writer port (`FSCTL_SET_SPARSE` + coalescing + durable flush — `P1-T11`..`T12`).
#[async_trait]
pub trait FileSink: Send + Sync {
    /// Prepares `.vortex` file: enables `FSCTL_SET_SPARSE` on NTFS/ReFS before setting EOF (or
    /// sequential fallback on exFAT/FAT32). Returns `true` if sparse mode is active.
    async fn prepare(&self, total_size: Option<u64>, trace_id: TraceId) -> AppResult<bool>;

    /// Writes `data` at exact byte `offset` (coalescing aligned blocks internally).
    async fn write_at(&self, offset: u64, data: Bytes, trace_id: TraceId) -> AppResult<()>;

    /// Executes Two-Tier Durable Checkpoint Step 1: flushes internal buffers + calls OS
    /// `FlushFileBuffers` (`sync_data`), and atomically writes `sidecar` to `.vortex.meta`.
    async fn flush_durable(&self, sidecar: &SidecarMetadata, trace_id: TraceId) -> AppResult<()>;

    /// Reads up to `len` bytes from `offset` (used for 64 KiB Link Refresh prefix sanity check).
    async fn read_at(&self, offset: u64, len: usize, trace_id: TraceId) -> AppResult<Bytes>;

    /// Computes full-file digest on blocking pool (`BLAKE3` / `SHA-256` / `SHA-1` / `MD5`).
    async fn verify_hash(&self, algorithm: HashAlgorithm, trace_id: TraceId) -> AppResult<String>;

    /// Flushes remaining coalesced data, attempts non-fatal `SetSparse = FALSE` (`WARN` only on
    /// NTFS refusal), closes handle, retries `ERROR_SHARING_VIOLATION` with jitter, atomically
    /// renames `.vortex` -> `final_path`, deletes `.vortex.meta`, and returns final file size.
    async fn finalize(&self, final_path: &Path, trace_id: TraceId) -> AppResult<u64>;

    /// Total OS positional write syscalls executed by this sink (verifies `Per-Worker Coalescing` — `P1.7-T02`).
    fn write_syscall_count(&self) -> u64 {
        0
    }

    /// Total payload bytes written by this sink across `write_at` calls (`P1.7-T02`).
    fn total_bytes_written(&self) -> u64 {
        0
    }
}

/// Factory creating [`FileSink`] instances and exposing the shared [`VolumeSpaceLedger`].
#[async_trait]
pub trait FileSinkFactory: Send + Sync {
    /// Opens or creates a `.vortex` temporary file sink paired with `meta_path`.
    async fn open(
        &self,
        temp_path: &Path,
        meta_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<Arc<dyn FileSink>>;

    /// Shared logical volume space ledger.
    fn space_ledger(&self) -> Arc<dyn VolumeSpaceLedger>;
}

// ============================================================================
// 3. Persistence, Config & Secret Store Ports (`vortex-storage`)
// ============================================================================

/// Stored category configuration row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CategoryRecord {
    /// Category ID.
    pub id: CategoryId,
    /// Display name.
    pub name: String,
    /// Associated lowercase extensions.
    pub extensions: Vec<String>,
    /// Target directory path.
    pub folder: PathBuf,
}

/// Stored scheduling queue configuration row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct QueueRecord {
    /// Queue ID.
    pub id: QueueId,
    /// Display name.
    pub name: String,
    /// Max parallel active downloads.
    pub max_parallel: u8,
    /// Optional cron/window JSON schedule (Phase 2).
    pub schedule_json: Option<String>,
    /// Optional post-completion action (`"none"`, `"shutdown"`, `"sleep"`, `"exit"`).
    pub post_action: Option<String>,
}

/// Sensitive per-download credentials stored encrypted via Windows DPAPI / OS Keyring (`P1-T10` & `TH-28`).
#[derive(Debug, Clone, Default)]
pub struct SecretCredentials {
    /// Encrypted cookie jar string (`normal + CHIPS`).
    pub cookies: Option<SecretString>,
    /// Encrypted `Authorization` header.
    pub authorization: Option<SecretString>,
    /// Normalized `scheme://host:port` origin pin (`TH-28` Origin-Pinned Secret Vault).
    pub origin_key: Option<String>,
}

impl SecretCredentials {
    /// Extracts a normalized lowercase `scheme://host:port` origin key from an HTTP(S) URL (`TH-28`).
    #[must_use]
    pub fn origin_key_from_url(raw_url: &str) -> Option<String> {
        vortex_domain::FilenameSanitizer::origin_key_from_url(raw_url)
    }

    /// Returns `true` if `target_url` matches the pinned origin (or if legacy credentials had no pin).
    #[must_use]
    pub fn origin_matches(&self, target_url: &str) -> bool {
        let Some(pinned) = self.origin_key.as_deref() else {
            return true;
        };
        Self::origin_key_from_url(target_url).is_some_and(|actual| actual == pinned)
    }
}

/// Repository port for `Download` aggregates and durable `RangeSet` checkpoints (`P1-T08`).
#[async_trait]
pub trait DownloadRepository: Send + Sync {
    /// Inserts or updates the full [`Download`] aggregate and its durable completed intervals.
    async fn save(&self, download: &Download, trace_id: TraceId) -> AppResult<()>;

    /// Commits a durable checkpoint (`completed` intervals + state + updated timestamp) inside a single ACID transaction.
    async fn checkpoint(
        &self,
        id: DownloadId,
        completed: &RangeSet,
        state: &vortex_domain::DownloadState,
        updated_at_ms: i64,
        trace_id: TraceId,
    ) -> AppResult<()>;

    /// Loads a single [`Download`] by ID.
    async fn find(&self, id: DownloadId, trace_id: TraceId) -> AppResult<Option<Download>>;

    /// Lists all non-quarantined downloads ordered by `created_at DESC` (isolating Poison-Pill rows!).
    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<Download>>;

    /// Deletes a download record and its segments.
    async fn delete(&self, id: DownloadId, trace_id: TraceId) -> AppResult<bool>;

    /// Total number of corrupted Poison-Pill rows isolated during reads.
    fn quarantined_count(&self) -> u64;
}

/// Repository port for download categories.
#[async_trait]
pub trait CategoryRepository: Send + Sync {
    /// Lists all configured categories.
    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<CategoryRecord>>;
    /// Inserts or updates a category.
    async fn upsert(&self, record: &CategoryRecord, trace_id: TraceId) -> AppResult<()>;
    /// Finds a category by ID.
    async fn find(&self, id: &CategoryId, trace_id: TraceId) -> AppResult<Option<CategoryRecord>>;
}

/// Repository port for scheduling queues.
#[async_trait]
pub trait QueueRepository: Send + Sync {
    /// Lists all configured queues.
    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<QueueRecord>>;
    /// Inserts or updates a queue.
    async fn upsert(&self, record: &QueueRecord, trace_id: TraceId) -> AppResult<()>;
    /// Finds a queue by ID.
    async fn find(&self, id: &QueueId, trace_id: TraceId) -> AppResult<Option<QueueRecord>>;
}

/// Audit event trail repository (`download_events` table, 90-day retention — `P1-T08`).
#[async_trait]
pub trait EventLogRepository: Send + Sync {
    /// Appends one or more domain event envelopes.
    async fn append(&self, events: &[DomainEventEnvelope], trace_id: TraceId) -> AppResult<()>;
    /// Lists events for a download in chronological order.
    async fn list_for_download(
        &self,
        id: DownloadId,
        limit: usize,
        trace_id: TraceId,
    ) -> AppResult<Vec<DomainEventEnvelope>>;
    /// Deletes audit events older than `cutoff_unix_ms`.
    async fn prune_before(&self, cutoff_unix_ms: i64, trace_id: TraceId) -> AppResult<u64>;
}

/// Encrypted credential store port (`Windows DPAPI` / OS Keyring — zero plaintext on disk, `P1-T10`).
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Encrypts and persists credentials for `download_id`.
    async fn store(
        &self,
        download_id: DownloadId,
        creds: &SecretCredentials,
        trace_id: TraceId,
    ) -> AppResult<()>;

    /// Decrypts credentials for `download_id` into zeroized [`SecretString`]s.
    async fn load(
        &self,
        download_id: DownloadId,
        trace_id: TraceId,
    ) -> AppResult<Option<SecretCredentials>>;

    /// Securely deletes stored ciphertext when download completes or is removed.
    async fn delete(&self, download_id: DownloadId, trace_id: TraceId) -> AppResult<()>;
}

/// Database health & maintenance port (`PRAGMA quick_check`, `wal_checkpoint(TRUNCATE)`, backups).
#[async_trait]
pub trait DatabaseHealthPort: Send + Sync {
    /// Runs `PRAGMA quick_check` and returns `true` if `"ok"`.
    async fn quick_check(&self, trace_id: TraceId) -> AppResult<bool>;
    /// Runs `PRAGMA wal_checkpoint(TRUNCATE)` during Graceful Shutdown (`§24.3`).
    async fn wal_checkpoint_truncate(&self, trace_id: TraceId) -> AppResult<()>;
    /// Creates an online SQLite backup (`keeping last 3`).
    async fn create_backup(&self, trace_id: TraceId) -> AppResult<PathBuf>;
}

/// Atomic configuration port (`config.toml` + live `watch::Receiver<Arc<AppConfig>>` — `P1-T09`).
#[async_trait]
pub trait ConfigPort: Send + Sync {
    /// Current immutable configuration snapshot.
    fn current(&self) -> Arc<AppConfig>;
    /// Subscribes to live Hot-Reload updates.
    fn subscribe(&self) -> watch::Receiver<Arc<AppConfig>>;
    /// Validates and atomically persists `new_config` (`tmp -> fsync -> .bak -> rename`) and broadcasts it.
    async fn update(&self, new_config: AppConfig, trace_id: TraceId) -> AppResult<Arc<AppConfig>>;
}

// ============================================================================
// 4. Platform Integration, EventBus & Engine Control Ports
// ============================================================================

/// RAII token keeping `ES_CONTINUOUS | ES_SYSTEM_REQUIRED` active until dropped (`§24.3`).
pub trait SleepInhibitorGuard: Send + Sync {}

/// OS-specific integration port (`vortex-platform` — `P1-T13` & `P1-T22`).
#[async_trait]
pub trait PlatformIntegration: Send + Sync {
    /// Resolves the current OS user security identifier (`Owner SID` on Windows) for Named Pipe & Mutex naming.
    ///
    /// # Errors
    /// Returns an error if the OS process token cannot be queried.
    fn current_user_sid(&self, trace_id: TraceId) -> AppResult<String>;

    /// Prevents system idle sleep while active downloads are transferring.
    ///
    /// # Errors
    /// Returns an error if the OS power state call fails.
    fn acquire_sleep_inhibitor(&self, trace_id: TraceId)
    -> AppResult<Box<dyn SleepInhibitorGuard>>;

    /// Writes Windows Mark-of-the-Web (`:Zone.Identifier`) and triggers `IAttachmentExecute` AV scan (`TH-07`).
    async fn apply_motw_and_av_scan(
        &self,
        file_path: &Path,
        source_url: &str,
        referrer_url: Option<&str>,
        trace_id: TraceId,
    ) -> AppResult<()>;

    /// Registers `com.vortex.bridge` Native Messaging manifests in `HKCU` for Brave, Chrome, and Edge (`no Admin`).
    ///
    /// # Errors
    /// Returns an error if writing manifest files or `HKCU` registry keys fails.
    fn register_native_hosts(
        &self,
        bridge_exe: &Path,
        allowed_extension_ids: &[String],
        trace_id: TraceId,
    ) -> AppResult<Vec<BrowserIntegrationStatusDto>>;

    /// Audits Native Messaging manifest status across Brave, Chrome, and Edge.
    fn check_native_hosts(&self) -> Vec<BrowserIntegrationStatusDto>;

    /// Checks whether general internet connectivity is currently available.
    fn is_internet_available(&self) -> bool;

    /// Queries free bytes available on the volume hosting `target_dir` (`GetDiskFreeSpaceExW` on Windows).
    fn free_disk_bytes(&self, _target_dir: &Path, _trace_id: TraceId) -> Option<u64> {
        None
    }

    /// Opens the native OS folder picker dialog (`IFileOpenDialog` / `FOS_PICKFOLDERS` on Windows — `P1.5-T05`).
    ///
    /// # Errors
    /// Returns [`crate::AppResult`] error if the OS shell dialog fails unexpectedly.
    fn pick_folder(
        &self,
        _initial_dir: Option<&Path>,
        _trace_id: TraceId,
    ) -> AppResult<Option<PathBuf>> {
        Ok(None)
    }

    /// Reveals `path` inside the native OS file manager (`SHOpenFolderAndSelectItems` on Windows — `P1.5-T05`).
    ///
    /// # Errors
    /// Returns [`crate::AppResult`] error if the file does not exist or Explorer cannot be launched.
    fn reveal_in_file_manager(&self, _path: &Path, _trace_id: TraceId) -> AppResult<()> {
        Ok(())
    }

    /// Opens a completed file using the OS shell (`ShellExecuteW` respecting `MotW` `SmartScreen` — `TH-07` / `TH-23`).
    ///
    /// # Errors
    /// Returns [`crate::AppResult`] error if the file does not exist or launching fails.
    fn open_file_safely(&self, _path: &Path, _trace_id: TraceId) -> AppResult<()> {
        Ok(())
    }

    /// Opens a validated `http://` or `https://` URL in the user's default browser (`VTX-RES-006` / `UX-M01`).
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if `url` is not a valid `http`/`https` URL or launching fails.
    fn open_url_in_browser(&self, _url: &str, _trace_id: TraceId) -> AppResult<()> {
        Ok(())
    }

    /// Hardens `dir` with a restrictive current-user-only NTFS DACL (`Owner SID` + `SYSTEM` — `TH-31`).
    ///
    /// # Errors
    /// Returns [`crate::AppResult`] if applying the directory ACL fails unexpectedly.
    fn enforce_owner_only_dir_acl(&self, _dir: &Path, _trace_id: TraceId) -> AppResult<()> {
        Ok(())
    }
}

/// Broadcast application events consumed by UI Projection, Persistence, Tray, and IPC push (`§10.1`).
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Domain aggregate transition event.
    Domain(DomainEventEnvelope),
    /// Throttled (`<= 10Hz`) batch of active download progress snapshots.
    ProgressBatch(Vec<ProgressSnapshotDto>),
    /// Live configuration hot-reload notification.
    ConfigChanged(Arc<AppConfig>),
    /// Live disk monitor warning (`< 256 MiB` free).
    LowDiskSpace {
        /// Target folder path.
        target_dir: PathBuf,
        /// Remaining free bytes.
        free_bytes: u64,
    },
    /// Internet connectivity state transition.
    ConnectivityChanged {
        /// `true` when back online.
        online: bool,
    },
    /// Single-instance secondary launch requested focusing UI (and optionally adding `open_url`).
    BringToFront {
        /// Optional URL passed on command line.
        open_url: Option<String>,
    },
    /// Pending staged confirmation queue changed (UI pulls `cmd_list_staged_downloads` — zero cold-boot race!).
    StagedDownloadsChanged {
        /// Current count of pending staged items awaiting user confirmation.
        pending_count: usize,
        /// Most recently staged `DownloadId`, if any.
        latest_staged_id: Option<String>,
        /// `true` when multiple rapid handoffs arrived within `4s` (`Ad-Burst Storm` — `TH-20`).
        burst_detected: bool,
    },
}

/// Non-blocking publish/subscribe event bus port.
pub trait EventBus: Send + Sync {
    /// Publishes an [`AppEvent`] to all active subscribers.
    fn publish(&self, event: AppEvent);

    /// Subscribes a new broadcast receiver.
    fn subscribe(&self) -> broadcast::Receiver<AppEvent>;
}

/// Port implemented by `vortex-engine`'s `Supervisor` so `vortex-application` commands orchestrate
/// active downloads without Level 2 depending on Level 3 (`Hexagonal DIP`).
#[async_trait]
pub trait EngineControlPort: Send + Sync {
    /// Starts or enqueues a `Download` (optionally reusing an already-opened `ProbeOutcome::Transferable` stream for `0ms` start!).
    async fn start_download(
        &self,
        download: Download,
        preopened_probe: Option<(Box<ResourceInfo>, Box<dyn ByteStream>)>,
        trace_id: TraceId,
    ) -> AppResult<()>;

    /// Durably flushes and pauses an active download.
    async fn pause_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()>;

    /// Durably flushes and pauses all active downloads.
    async fn pause_all(&self, trace_id: TraceId) -> AppResult<()>;

    /// Resumes a paused/failed/refreshed download.
    async fn resume_download(&self, id: DownloadId, trace_id: TraceId) -> AppResult<()>;

    /// Cancels an active download and optionally deletes its temporary `.vortex` / `.vortex.meta` files.
    async fn cancel_download(
        &self,
        id: DownloadId,
        delete_partial_files: bool,
        trace_id: TraceId,
    ) -> AppResult<()>;

    /// Returns latest live progress snapshots for active downloads.
    async fn active_snapshots(&self) -> Vec<ProgressSnapshotDto>;

    /// Executes Graceful Shutdown (`<= 5s` budget: flush all actors + checkpoints).
    async fn shutdown_gracefully(&self, trace_id: TraceId) -> AppResult<()>;
}

/// Self-Health Check, Live Redacted Logs, 30m Debug Mode, and Sanitized `.zip` Diagnostic Bundle port (`P1-T23` & `P1-T34`).
#[async_trait]
pub trait DiagnosticsPort: Send + Sync {
    /// Runs the system-wide Self-Health Check (`Core` + `SQLite` + `Named Pipe` + `Browser Registry` + `Disk`).
    async fn check_health(&self, trace_id: TraceId) -> AppResult<HealthStatusDto>;

    /// Builds an in-memory preview manifest of the sanitized diagnostic bundle without writing to disk yet.
    async fn preview_bundle(&self, trace_id: TraceId) -> AppResult<DiagnosticBundleReportDto>;

    /// Generates and writes the sanitized `.zip` diagnostic bundle to `dest_zip_path`.
    async fn export_bundle(
        &self,
        dest_zip_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<DiagnosticBundleReportDto>;

    /// Returns the most recent `limit` redacted events from the in-memory `FlightRecorder`.
    fn recent_logs(&self, limit: usize) -> Vec<LogEntryDto>;

    /// Enables or disables live `DEBUG` mode (auto-reverting after 30 minutes when enabled).
    ///
    /// # Errors
    /// Returns [`crate::AppResult`] error if the tracing filter cannot be reloaded.
    fn set_debug_mode(&self, enabled: bool, trace_id: TraceId) -> AppResult<bool>;

    /// Returns whether live `DEBUG` mode is currently active.
    fn is_debug_mode_active(&self) -> bool;
}
