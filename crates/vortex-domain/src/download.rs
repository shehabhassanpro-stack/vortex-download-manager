//! `Download` aggregate root and explicit Type-State FSM (`P1-T03` / `Vortex-Architecture-Design.md` §7.2–7.3).
//!
//! Models all 11 canonical lifecycle states as a strongly typed Rust `enum` with per-state fields,
//! guaranteeing at compile time that state-specific metrics (e.g., live speed vs verified checksum)
//! cannot be accessed in invalid states, and emitting immutable [`DomainEventEnvelope`]s on every transition.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vortex_error::{ApiError, ErrorCode, TraceId, VortexError};

use crate::DomainResult;
use crate::events::{DomainEvent, DomainEventEnvelope, PostProcessStage};
use crate::range_set::{ActiveSegment, SegmentMap};
use crate::value_objects::{
    ByteRange, CategoryId, Checksum, DownloadId, HashAlgorithm, Priority, QueueId,
    ResumeValidators, Timestamps, WorkerId,
};

/// Remote resource descriptor (secrets such as cookies are stored encrypted in `SecretStore`,
/// never in plaintext inside `Resource` — `TH-08`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    /// Original URL requested by user or browser handoff.
    pub url: String,
    /// Final pinned URL resolved after HTTP redirects.
    pub final_url: Option<String>,
    /// Optional HTTP `Referer` URL.
    pub referrer: Option<String>,
    /// Optional CHIPS `top_level_site` origin (`https://example.com`).
    pub top_level_site: Option<String>,
    /// Browser `User-Agent` preserved from handoff.
    pub user_agent: Option<String>,
    /// Detected MIME type.
    pub mime_type: Option<String>,
    /// Non-secret custom headers (secret headers/cookies are stored in `SecretStore` keyed by `DownloadId`).
    pub headers: BTreeMap<String, String>,
    /// Indicates whether encrypted cookies/credentials exist in `SecretStore` for this download.
    pub has_secret_credentials: bool,
    /// Validators (`ETag`, `Last-Modified`, `Content-Length`) used for safe RFC 9110 resume.
    pub validators: ResumeValidators,
}

impl Resource {
    /// Creates a minimal [`Resource`] for `url`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            final_url: None,
            referrer: None,
            top_level_site: None,
            user_agent: None,
            mime_type: None,
            headers: BTreeMap::new(),
            has_secret_credentials: false,
            validators: ResumeValidators::default(),
        }
    }

    /// Active fetch URL (`final_url` if pinned, otherwise `url`).
    #[must_use]
    pub fn effective_url(&self) -> &str {
        self.final_url.as_deref().unwrap_or(&self.url)
    }
}

/// Explicit Type-State Finite State Machine for `Download` (`Vortex-Architecture-Design.md` §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DownloadState {
    /// Newly added; awaiting user confirmation or immediate start.
    Pending,
    /// Performing `GET Range: bytes=0-` probe or resume re-validation.
    Probing {
        /// Probe start timestamp (ms).
        started_at_ms: i64,
    },
    /// Probe succeeded; waiting for an available concurrency slot in its `Queue`.
    Queued {
        /// Queue entry timestamp (ms).
        queued_at_ms: i64,
    },
    /// Actively transferring bytes via `1..=N` connection workers.
    Downloading {
        /// Number of currently running connection workers.
        active_workers: u8,
        /// Fast-horizon EWMA speed (`tau = 1.5s`, bytes/sec).
        speed_fast_bps: f64,
        /// Slow-horizon EWMA speed (`tau = 8.0s`, bytes/sec).
        speed_slow_bps: f64,
        /// Stabilized ETA in seconds.
        eta_secs: Option<u64>,
        /// Whether server confirmed `206 Partial Content` range support.
        supports_range: bool,
    },
    /// Cleanly paused after a durable checkpoint (`FlushFileBuffers` + `SQLite COMMIT`).
    Paused {
        /// Pause timestamp (ms).
        paused_at_ms: i64,
        /// Durable bytes persisted on disk.
        downloaded_bytes: u64,
    },
    /// Waiting out Decorrelated Full-Jitter backoff after transient network failure.
    Retrying {
        /// 1-based retry attempt count.
        attempt: u32,
        /// Scheduled wakeup Unix timestamp (ms).
        next_retry_at_ms: i64,
        /// Transient `ErrorCode` that caused retry.
        last_error: ErrorCode,
    },
    /// Requires user intervention (expired link `VTX-RES-002`, mutated validator `VTX-RES-003`, disk full `VTX-DISK-001`).
    NeedsAttention {
        /// Stable `ErrorCode` explaining required user action.
        reason_code: ErrorCode,
        /// Sanitized human/diagnostic context.
        detail: String,
    },
    /// All bytes received; running post-processing (sparse clear + atomic rename + MotW/AV or mux).
    PostProcessing {
        /// Current post-processing stage.
        stage: PostProcessStage,
    },
    /// Computing cryptographic digest (`BLAKE3` / `SHA-256` / `SHA-1` / `MD5`) on blocking pool.
    Verifying {
        /// Hash algorithm in progress.
        algorithm: HashAlgorithm,
    },
    /// Terminal completed state; `.vortex` temp file atomically renamed to final target.
    Completed {
        /// Completion Unix timestamp (ms).
        completed_at_ms: i64,
        /// Final verified file size on disk.
        final_size_bytes: u64,
        /// Verified checksum if integrity check ran.
        verified_checksum: Option<Checksum>,
    },
    /// Failed state carrying full structured [`ApiError`] (retriable manually via `RetryManually`).
    Failed {
        /// Failure Unix timestamp (ms).
        failed_at_ms: i64,
        /// Structured error contract.
        error: ApiError,
    },
}

impl DownloadState {
    /// Canonical lowercase status slug used in SQLite indices and UI filters.
    #[must_use]
    pub const fn as_slug(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Probing { .. } => "probing",
            Self::Queued { .. } => "queued",
            Self::Downloading { .. } => "downloading",
            Self::Paused { .. } => "paused",
            Self::Retrying { .. } => "retrying",
            Self::NeedsAttention { .. } => "needs_attention",
            Self::PostProcessing { .. } => "post_processing",
            Self::Verifying { .. } => "verifying",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
        }
    }

    /// Returns `true` if the download occupies an active transfer slot.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Probing { .. }
                | Self::Downloading { .. }
                | Self::Retrying { .. }
                | Self::PostProcessing { .. }
                | Self::Verifying { .. }
        )
    }
}

/// Commands accepted by [`Download::apply`] to drive validated state transitions.
#[derive(Debug, Clone)]
pub enum DownloadCommand {
    /// Start initial probe (`Pending -> Probing`), or manual retry (`Failed -> Probing`).
    StartProbe,
    /// Complete probe and move to `Queued` (if `queue_full`) or `Downloading`.
    CompleteProbe {
        /// Resolved content length.
        total_size: Option<u64>,
        /// Server range capability.
        supports_range: bool,
        /// Fresh resume validators.
        validators: ResumeValidators,
        /// Pinned final URL after redirects.
        final_url: Option<String>,
        /// Optional updated sanitized filename from `Content-Disposition`.
        resolved_file_name: Option<String>,
        /// Whether the target queue has reached `max_parallel`.
        queue_full: bool,
        /// Redacted final URL for event logging.
        final_url_redacted: Option<String>,
    },
    /// Slot became available (`Queued -> Downloading`).
    PromoteFromQueue {
        /// Whether range requests are supported.
        supports_range: bool,
    },
    /// Register a new worker segment lease.
    StartWorkerSegment {
        /// Worker ID.
        worker_id: WorkerId,
        /// Assigned byte interval.
        assigned_range: ByteRange,
        /// Initial bounded chunk lease end.
        lease_end: u64,
    },
    /// Record work-stealing split between two workers.
    RecordSplit {
        /// Slow worker whose unleased tail was halved.
        from_worker: WorkerId,
        /// Fast worker stealing `[stolen_range.start, stolen_range.end)`.
        new_worker: WorkerId,
        /// Stolen interval.
        stolen_range: ByteRange,
        /// Initial bounded chunk lease end for `new_worker`.
        lease_end: u64,
    },
    /// Record written chunk and EWMA speed snapshot (`Downloading`).
    RecordProgress {
        /// Worker that wrote `chunk`.
        worker_id: WorkerId,
        /// Written byte interval.
        chunk: ByteRange,
        /// Fast-horizon EWMA speed (`tau = 1.5s`).
        speed_fast_bps: f64,
        /// Slow-horizon EWMA speed (`tau = 8.0s`).
        speed_slow_bps: f64,
        /// Stabilized ETA in seconds.
        eta_secs: Option<u64>,
    },
    /// Pause active/queued/retrying download (idempotent if already `Paused`).
    Pause,
    /// Resume from `Paused` (`Paused -> Probing` for RFC 9110 validator check).
    Resume,
    /// Schedule transient retry (`Downloading | Probing -> Retrying`).
    ScheduleRetry {
        /// 1-based attempt number.
        attempt: u32,
        /// Backoff delay in ms.
        retry_after_ms: u64,
        /// Transient error code.
        code: ErrorCode,
    },
    /// Backoff elapsed (`Retrying -> Downloading`).
    BackoffElapsed {
        /// Whether range requests are supported.
        supports_range: bool,
    },
    /// Transition to `NeedsAttention` (link expired `403`/`410`, validator mismatch, or low disk).
    MarkNeedsAttention {
        /// Reason error code.
        reason_code: ErrorCode,
        /// Sanitized explanation.
        detail: String,
    },
    /// Apply refreshed link after 64 KiB byte-prefix verification (`NeedsAttention -> Probing`).
    RefreshLink {
        /// Fresh URL.
        new_url: String,
        /// Redacted URL for audit trail.
        new_url_redacted: String,
    },
    /// Transition `Downloading -> PostProcessing` once all bytes are written.
    BeginPostProcessing {
        /// Stage to enter.
        stage: PostProcessStage,
    },
    /// Transition `PostProcessing -> Verifying` (when `expected_hash` is present) or `PostProcessing -> Completed`.
    AdvanceAfterPostProcessing {
        /// Verified final size on disk (used when no hash check is configured).
        final_size_bytes: u64,
    },
    /// Record cryptographic hash verification outcome (`Verifying -> Completed | Failed`).
    CompleteVerification {
        /// Computed lowercase hex digest.
        actual_hex: String,
        /// Final file size on disk.
        final_size_bytes: u64,
    },
    /// Transition to `Failed` on unrecoverable error.
    Fail {
        /// Structured `ApiError`.
        error: ApiError,
    },
}

/// `Download` Aggregate Root (`Vortex-Architecture-Design.md` §7.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Download {
    /// Unique `UUIDv7` identifier.
    pub id: DownloadId,
    /// Remote HTTP/HTTPS resource metadata.
    pub resource: Resource,
    /// Target save directory (`save_dir`).
    pub save_dir: PathBuf,
    /// Sanitized final file basename (e.g., `"ubuntu-26.04.iso"`; in-flight temp file is `"ubuntu-26.04.iso.vortex"`).
    pub file_name: String,
    /// Total file size in bytes (`None` for chunked / unknown-length streams).
    pub total_size: Option<u64>,
    /// Explicit Type-State FSM state.
    pub state: DownloadState,
    /// Coalesced completed `RangeSet` + active workers.
    pub segments: SegmentMap,
    /// Queue scheduling priority.
    pub priority: Priority,
    /// Automatic or user-selected category ID.
    pub category_id: CategoryId,
    /// Owning queue ID.
    pub queue_id: QueueId,
    /// Whether server confirmed `206 Partial Content` support.
    pub supports_range: bool,
    /// Optional expected cryptographic digest.
    pub expected_hash: Option<Checksum>,
    /// Lifecycle timestamps.
    pub times: Timestamps,
}

impl Download {
    /// Creates a new `Download` in `Pending` state and emits [`DomainEvent::DownloadAdded`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_pending(
        id: DownloadId,
        resource: Resource,
        url_redacted: String,
        save_dir: PathBuf,
        file_name: String,
        category_id: CategoryId,
        queue_id: QueueId,
        priority: Priority,
        expected_hash: Option<Checksum>,
        now_ms: i64,
        trace_id: TraceId,
    ) -> (Self, DomainEventEnvelope) {
        let evt = DomainEventEnvelope::new(
            id,
            trace_id,
            now_ms,
            DomainEvent::DownloadAdded {
                url_redacted,
                file_name: file_name.clone(),
                category_id: category_id.clone(),
                queue_id: queue_id.clone(),
            },
        );
        let download = Self {
            id,
            resource,
            save_dir,
            file_name,
            total_size: None,
            state: DownloadState::Pending,
            segments: SegmentMap::new(None),
            priority,
            category_id,
            queue_id,
            supports_range: false,
            expected_hash,
            times: Timestamps::new(now_ms),
        };
        (download, evt)
    }

    /// Path to the final destination file (`<save_dir>/<file_name>`).
    #[must_use]
    pub fn final_path(&self) -> PathBuf {
        self.save_dir.join(&self.file_name)
    }

    /// Path to the active temporary file (`<save_dir>/<file_name>.vortex`).
    #[must_use]
    pub fn temp_path(&self) -> PathBuf {
        self.save_dir.join(format!("{}.vortex", self.file_name))
    }

    /// Path to the crash-recovery sidecar metadata file (`<save_dir>/<file_name>.vortex.meta`).
    #[must_use]
    pub fn meta_sidecar_path(&self) -> PathBuf {
        self.save_dir
            .join(format!("{}.vortex.meta", self.file_name))
    }

    /// Total durably/actively recorded bytes in `segments.completed`.
    #[must_use]
    pub const fn downloaded_bytes(&self) -> u64 {
        self.segments.completed.total_bytes()
    }

    /// Applies a validated [`DownloadCommand`], mutating state and returning emitted [`DomainEventEnvelope`]s.
    ///
    /// # Errors
    /// Returns [`VortexError`] if the transition is illegal from the current [`DownloadState`].
    #[allow(clippy::too_many_lines)]
    pub fn apply(
        &mut self,
        cmd: DownloadCommand,
        now_ms: i64,
        trace_id: TraceId,
    ) -> DomainResult<Vec<DomainEventEnvelope>> {
        let mut events = Vec::with_capacity(2);

        match cmd {
            DownloadCommand::StartProbe => match &self.state {
                DownloadState::Pending
                | DownloadState::Failed { .. }
                | DownloadState::Queued { .. }
                | DownloadState::Retrying { .. } => {
                    self.state = DownloadState::Probing {
                        started_at_ms: now_ms,
                    };
                    self.times.updated_at_ms = now_ms;
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::ProbeStarted,
                    ));
                }
                DownloadState::Probing { .. } => {} // Idempotent
                other => return Err(Self::illegal_transition("StartProbe", other, trace_id)),
            },

            DownloadCommand::CompleteProbe {
                total_size,
                supports_range,
                validators,
                final_url,
                resolved_file_name,
                queue_full,
                final_url_redacted,
            } => {
                if !matches!(self.state, DownloadState::Probing { .. }) {
                    return Err(Self::illegal_transition(
                        "CompleteProbe",
                        &self.state,
                        trace_id,
                    ));
                }
                // On resume (already has completed bytes), verify remote validators did not mutate
                if self.downloaded_bytes() > 0 {
                    if !supports_range {
                        let code = ErrorCode::ResRangeUnsupported;
                        self.segments.clear_active_workers();
                        self.state = DownloadState::NeedsAttention {
                            reason_code: code,
                            detail: "Origin server no longer supports HTTP Range on resume"
                                .to_owned(),
                        };
                        self.times.updated_at_ms = now_ms;
                        events.push(DomainEventEnvelope::new(
                            self.id,
                            trace_id,
                            now_ms,
                            DomainEvent::LinkExpired {
                                code,
                                detail: "Server returned 200 instead of 206 on resume".to_owned(),
                            },
                        ));
                        return Ok(events);
                    }
                    if let Err(mismatch_err) =
                        self.resource.validators.validate_on_resume(&validators)
                    {
                        let detail = mismatch_err.as_api_error().technical_detail.clone();
                        self.segments.clear_active_workers();
                        self.state = DownloadState::NeedsAttention {
                            reason_code: ErrorCode::ResResourceChanged,
                            detail: detail.clone(),
                        };
                        self.times.updated_at_ms = now_ms;
                        events.push(DomainEventEnvelope::new(
                            self.id,
                            trace_id,
                            now_ms,
                            DomainEvent::LinkExpired {
                                code: ErrorCode::ResResourceChanged,
                                detail,
                            },
                        ));
                        return Ok(events);
                    }
                }

                self.total_size = total_size;
                self.segments.total_size = total_size;
                self.supports_range = supports_range;
                self.resource.validators = validators;
                if let Some(f_url) = final_url {
                    self.resource.final_url = Some(f_url);
                }
                if self.downloaded_bytes() == 0
                    && let Some(clean_name) = resolved_file_name
                {
                    self.file_name = clean_name;
                }
                self.times.updated_at_ms = now_ms;

                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::ProbeCompleted {
                        total_size,
                        supports_range,
                        final_url_redacted,
                    },
                ));

                if queue_full {
                    self.state = DownloadState::Queued {
                        queued_at_ms: now_ms,
                    };
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadQueued {
                            queue_id: self.queue_id.clone(),
                        },
                    ));
                } else {
                    if self.times.started_at_ms.is_none() {
                        self.times.started_at_ms = Some(now_ms);
                    }
                    self.state = DownloadState::Downloading {
                        active_workers: 1,
                        speed_fast_bps: 0.0,
                        speed_slow_bps: 0.0,
                        eta_secs: None,
                        supports_range,
                    };
                }
            }

            DownloadCommand::PromoteFromQueue { supports_range } => {
                if !matches!(self.state, DownloadState::Queued { .. }) {
                    return Err(Self::illegal_transition(
                        "PromoteFromQueue",
                        &self.state,
                        trace_id,
                    ));
                }
                if self.times.started_at_ms.is_none() {
                    self.times.started_at_ms = Some(now_ms);
                }
                self.supports_range = supports_range;
                self.state = DownloadState::Downloading {
                    active_workers: 1,
                    speed_fast_bps: 0.0,
                    speed_slow_bps: 0.0,
                    eta_secs: None,
                    supports_range,
                };
                self.times.updated_at_ms = now_ms;
            }

            DownloadCommand::StartWorkerSegment {
                worker_id,
                assigned_range,
                lease_end,
            } => {
                if !matches!(self.state, DownloadState::Downloading { .. }) {
                    return Err(Self::illegal_transition(
                        "StartWorkerSegment",
                        &self.state,
                        trace_id,
                    ));
                }
                self.segments.active.retain(|s| s.worker_id != worker_id);
                self.segments
                    .active
                    .push(ActiveSegment::new(worker_id, assigned_range, lease_end));
                if let DownloadState::Downloading { active_workers, .. } = &mut self.state {
                    *active_workers = u8::try_from(self.segments.active.len()).unwrap_or(u8::MAX);
                }
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::SegmentStarted {
                        worker_id,
                        range: assigned_range,
                    },
                ));
            }

            DownloadCommand::RecordSplit {
                from_worker,
                new_worker,
                stolen_range,
                lease_end,
            } => {
                if !matches!(self.state, DownloadState::Downloading { .. }) {
                    return Err(Self::illegal_transition(
                        "RecordSplit",
                        &self.state,
                        trace_id,
                    ));
                }
                if let Some(victim) = self
                    .segments
                    .active
                    .iter_mut()
                    .find(|s| s.worker_id == from_worker)
                    && let Ok(trimmed) =
                        ByteRange::new(victim.assigned_range.start(), stolen_range.start())
                {
                    victim.assigned_range = trimmed;
                    victim.active_lease_end = victim.active_lease_end.min(trimmed.end());
                }
                self.segments.active.retain(|s| s.worker_id != new_worker);
                self.segments
                    .active
                    .push(ActiveSegment::new(new_worker, stolen_range, lease_end));
                if let DownloadState::Downloading { active_workers, .. } = &mut self.state {
                    *active_workers = u8::try_from(self.segments.active.len()).unwrap_or(u8::MAX);
                }
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::SegmentSplit {
                        from_worker,
                        new_worker,
                        stolen_range,
                    },
                ));
            }

            DownloadCommand::RecordProgress {
                worker_id,
                chunk,
                speed_fast_bps,
                speed_slow_bps,
                eta_secs,
            } => {
                if !matches!(self.state, DownloadState::Downloading { .. }) {
                    return Err(Self::illegal_transition(
                        "RecordProgress",
                        &self.state,
                        trace_id,
                    ));
                }
                self.segments.record_written(worker_id, chunk)?;
                let active_count = u8::try_from(self.segments.active.len()).unwrap_or(u8::MAX);
                self.state = DownloadState::Downloading {
                    active_workers: active_count,
                    speed_fast_bps,
                    speed_slow_bps,
                    eta_secs,
                    supports_range: self.supports_range,
                };
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::ProgressTick {
                        downloaded_bytes: self.downloaded_bytes(),
                        total_size: self.total_size,
                        speed_fast_bps,
                        speed_slow_bps,
                        eta_secs,
                        active_workers: active_count,
                    },
                ));
            }

            DownloadCommand::Pause => match &self.state {
                DownloadState::Paused { .. } => {} // Idempotent (§4.1)
                DownloadState::Pending
                | DownloadState::Probing { .. }
                | DownloadState::Queued { .. }
                | DownloadState::Downloading { .. }
                | DownloadState::Retrying { .. } => {
                    self.segments.clear_active_workers();
                    let downloaded = self.downloaded_bytes();
                    self.state = DownloadState::Paused {
                        paused_at_ms: now_ms,
                        downloaded_bytes: downloaded,
                    };
                    self.times.updated_at_ms = now_ms;
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadPaused {
                            downloaded_bytes: downloaded,
                        },
                    ));
                }
                other => return Err(Self::illegal_transition("Pause", other, trace_id)),
            },

            DownloadCommand::Resume => match &self.state {
                DownloadState::Paused { .. } => {
                    self.state = DownloadState::Probing {
                        started_at_ms: now_ms,
                    };
                    self.times.updated_at_ms = now_ms;
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadResumed,
                    ));
                }
                DownloadState::Probing { .. } | DownloadState::Downloading { .. } => {} // Idempotent
                other => return Err(Self::illegal_transition("Resume", other, trace_id)),
            },

            DownloadCommand::ScheduleRetry {
                attempt,
                retry_after_ms,
                code,
            } => match &self.state {
                DownloadState::Probing { .. }
                | DownloadState::Downloading { .. }
                | DownloadState::Retrying { .. } => {
                    self.segments.clear_active_workers();
                    let signed_delay = i64::try_from(retry_after_ms).unwrap_or(i64::MAX);
                    self.state = DownloadState::Retrying {
                        attempt,
                        next_retry_at_ms: now_ms.saturating_add(signed_delay),
                        last_error: code,
                    };
                    self.times.updated_at_ms = now_ms;
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::RetryScheduled {
                            attempt,
                            retry_after_ms,
                            code,
                        },
                    ));
                }
                other => return Err(Self::illegal_transition("ScheduleRetry", other, trace_id)),
            },

            DownloadCommand::BackoffElapsed { supports_range } => {
                if !matches!(self.state, DownloadState::Retrying { .. }) {
                    return Err(Self::illegal_transition(
                        "BackoffElapsed",
                        &self.state,
                        trace_id,
                    ));
                }
                self.state = DownloadState::Downloading {
                    active_workers: 1,
                    speed_fast_bps: 0.0,
                    speed_slow_bps: 0.0,
                    eta_secs: None,
                    supports_range,
                };
                self.times.updated_at_ms = now_ms;
            }

            DownloadCommand::MarkNeedsAttention {
                reason_code,
                detail,
            } => match &self.state {
                DownloadState::Probing { .. }
                | DownloadState::Downloading { .. }
                | DownloadState::Retrying { .. }
                | DownloadState::Paused { .. } => {
                    self.segments.clear_active_workers();
                    self.state = DownloadState::NeedsAttention {
                        reason_code,
                        detail: detail.clone(),
                    };
                    self.times.updated_at_ms = now_ms;
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::LinkExpired {
                            code: reason_code,
                            detail,
                        },
                    ));
                }
                other => {
                    return Err(Self::illegal_transition(
                        "MarkNeedsAttention",
                        other,
                        trace_id,
                    ));
                }
            },

            DownloadCommand::RefreshLink {
                new_url,
                new_url_redacted,
            } => {
                if !matches!(
                    self.state,
                    DownloadState::NeedsAttention { .. } | DownloadState::Failed { .. }
                ) {
                    return Err(Self::illegal_transition(
                        "RefreshLink",
                        &self.state,
                        trace_id,
                    ));
                }
                self.resource.url = new_url;
                self.resource.final_url = None;
                self.state = DownloadState::Probing {
                    started_at_ms: now_ms,
                };
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::LinkRefreshed { new_url_redacted },
                ));
            }

            DownloadCommand::BeginPostProcessing { stage } => {
                if !matches!(
                    self.state,
                    DownloadState::Downloading { .. } | DownloadState::PostProcessing { .. }
                ) {
                    return Err(Self::illegal_transition(
                        "BeginPostProcessing",
                        &self.state,
                        trace_id,
                    ));
                }
                if let Some(total) = self.total_size
                    && !self.segments.completed.is_complete(total)
                {
                    return Err(VortexError::coded(
                        ErrorCode::IntgHashMismatch,
                        trace_id,
                        format!(
                            "Cannot finalize download: completed {} of {total} bytes",
                            self.downloaded_bytes()
                        ),
                    ));
                }
                self.segments.clear_active_workers();
                self.state = DownloadState::PostProcessing { stage };
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::PostProcessingStarted { stage },
                ));
            }

            DownloadCommand::AdvanceAfterPostProcessing { final_size_bytes } => {
                if !matches!(self.state, DownloadState::PostProcessing { .. }) {
                    return Err(Self::illegal_transition(
                        "AdvanceAfterPostProcessing",
                        &self.state,
                        trace_id,
                    ));
                }
                self.times.updated_at_ms = now_ms;
                if let Some(expected) = &self.expected_hash {
                    self.state = DownloadState::Verifying {
                        algorithm: expected.algorithm,
                    };
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::VerificationStarted,
                    ));
                } else {
                    self.times.completed_at_ms = Some(now_ms);
                    self.state = DownloadState::Completed {
                        completed_at_ms: now_ms,
                        final_size_bytes,
                        verified_checksum: None,
                    };
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadCompleted { final_size_bytes },
                    ));
                }
            }

            DownloadCommand::CompleteVerification {
                actual_hex,
                final_size_bytes,
            } => {
                if !matches!(self.state, DownloadState::Verifying { .. }) {
                    return Err(Self::illegal_transition(
                        "CompleteVerification",
                        &self.state,
                        trace_id,
                    ));
                }
                let Some(expected) = self.expected_hash.clone() else {
                    return Err(Self::illegal_transition(
                        "CompleteVerification(no expected_hash)",
                        &self.state,
                        trace_id,
                    ));
                };
                self.times.updated_at_ms = now_ms;

                if expected.matches_hex(&actual_hex) {
                    self.times.completed_at_ms = Some(now_ms);
                    self.state = DownloadState::Completed {
                        completed_at_ms: now_ms,
                        final_size_bytes,
                        verified_checksum: Some(expected.clone()),
                    };
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::VerificationPassed { checksum: expected },
                    ));
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadCompleted { final_size_bytes },
                    ));
                } else {
                    let err = ApiError::new(ErrorCode::IntgHashMismatch, trace_id)
                        .with_technical_detail(format!(
                            "Expected {expected}, computed {actual_hex}"
                        ));
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::VerificationFailed {
                            expected,
                            actual_hex,
                        },
                    ));
                    events.push(DomainEventEnvelope::new(
                        self.id,
                        trace_id,
                        now_ms,
                        DomainEvent::DownloadFailed { error: err.clone() },
                    ));
                    self.state = DownloadState::Failed {
                        failed_at_ms: now_ms,
                        error: err,
                    };
                }
            }

            DownloadCommand::Fail { error } => {
                self.segments.clear_active_workers();
                self.times.updated_at_ms = now_ms;
                events.push(DomainEventEnvelope::new(
                    self.id,
                    trace_id,
                    now_ms,
                    DomainEvent::DownloadFailed {
                        error: error.clone(),
                    },
                ));
                self.state = DownloadState::Failed {
                    failed_at_ms: now_ms,
                    error,
                };
            }
        }

        Ok(events)
    }

    fn illegal_transition(
        cmd_name: &'static str,
        state: &DownloadState,
        trace_id: TraceId,
    ) -> VortexError {
        VortexError::coded(
            ErrorCode::BugInternalIsolated,
            trace_id,
            format!(
                "Illegal FSM command `{cmd_name}` while download is in `{}` state",
                state.as_slug()
            ),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
mod tests {
    use super::*;
    use crate::value_objects::EtagValidator;

    fn sample_download(expected_hash: Option<Checksum>) -> Download {
        let (dl, _) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/ubuntu.iso"),
            "https://cdn.example.com/ubuntu.iso".to_owned(),
            PathBuf::from("D:/Downloads"),
            "ubuntu.iso".to_owned(),
            CategoryId::builtin(CategoryId::COMPRESSED),
            QueueId::main_queue(),
            Priority::Normal,
            expected_hash,
            1_000,
            TraceId::new(),
        );
        dl
    }

    #[test]
    fn happy_path_lifecycle_with_pause_resume_and_checksum_verification() {
        let hash_hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let expected = Checksum::new(HashAlgorithm::Sha256, hash_hex).unwrap();
        let mut dl = sample_download(Some(expected));
        let trace = TraceId::new();

        dl.apply(DownloadCommand::StartProbe, 1_010, trace).unwrap();
        assert_eq!(dl.state.as_slug(), "probing");

        let validators = ResumeValidators {
            etag: EtagValidator::parse(r#""v1""#),
            last_modified: None,
            content_length: Some(1_000),
        };
        dl.apply(
            DownloadCommand::CompleteProbe {
                total_size: Some(1_000),
                supports_range: true,
                validators: validators.clone(),
                final_url: None,
                resolved_file_name: None,
                queue_full: false,
                final_url_redacted: None,
            },
            1_020,
            trace,
        )
        .unwrap();
        assert_eq!(dl.state.as_slug(), "downloading");

        dl.apply(
            DownloadCommand::StartWorkerSegment {
                worker_id: WorkerId::new(1),
                assigned_range: ByteRange::new(0, 1_000).unwrap(),
                lease_end: 1_000,
            },
            1_025,
            trace,
        )
        .unwrap();

        dl.apply(
            DownloadCommand::RecordProgress {
                worker_id: WorkerId::new(1),
                chunk: ByteRange::new(0, 500).unwrap(),
                speed_fast_bps: 10_000.0,
                speed_slow_bps: 9_500.0,
                eta_secs: Some(1),
            },
            1_050,
            trace,
        )
        .unwrap();

        // Pause is idempotent
        let ev1 = dl.apply(DownloadCommand::Pause, 1_060, trace).unwrap();
        assert_eq!(ev1.len(), 1);
        let ev2 = dl.apply(DownloadCommand::Pause, 1_061, trace).unwrap();
        assert_eq!(ev2.len(), 0);

        // Resume re-validates via Probing
        dl.apply(DownloadCommand::Resume, 1_100, trace).unwrap();
        assert_eq!(dl.state.as_slug(), "probing");

        dl.apply(
            DownloadCommand::CompleteProbe {
                total_size: Some(1_000),
                supports_range: true,
                validators,
                final_url: None,
                resolved_file_name: None,
                queue_full: false,
                final_url_redacted: None,
            },
            1_110,
            trace,
        )
        .unwrap();

        dl.apply(
            DownloadCommand::RecordProgress {
                worker_id: WorkerId::new(1),
                chunk: ByteRange::new(500, 1_000).unwrap(),
                speed_fast_bps: 12_000.0,
                speed_slow_bps: 10_000.0,
                eta_secs: Some(0),
            },
            1_150,
            trace,
        )
        .unwrap();

        dl.apply(
            DownloadCommand::BeginPostProcessing {
                stage: PostProcessStage::FinalizeFile,
            },
            1_160,
            trace,
        )
        .unwrap();

        dl.apply(
            DownloadCommand::AdvanceAfterPostProcessing {
                final_size_bytes: 1_000,
            },
            1_170,
            trace,
        )
        .unwrap();
        assert_eq!(dl.state.as_slug(), "verifying");

        dl.apply(
            DownloadCommand::CompleteVerification {
                actual_hex: hash_hex.to_owned(),
                final_size_bytes: 1_000,
            },
            1_180,
            trace,
        )
        .unwrap();
        assert_eq!(dl.state.as_slug(), "completed");
        assert!(dl.apply(DownloadCommand::Pause, 1_200, trace).is_err());
    }

    #[test]
    fn resume_detects_mutated_etag_and_moves_to_needs_attention_then_link_refresh_recovers() {
        let mut dl = sample_download(None);
        let trace = TraceId::new();

        dl.apply(DownloadCommand::StartProbe, 100, trace).unwrap();
        dl.apply(
            DownloadCommand::CompleteProbe {
                total_size: Some(2_000),
                supports_range: true,
                validators: ResumeValidators {
                    etag: EtagValidator::parse(r#""orig""#),
                    last_modified: None,
                    content_length: Some(2_000),
                },
                final_url: None,
                resolved_file_name: None,
                queue_full: false,
                final_url_redacted: None,
            },
            110,
            trace,
        )
        .unwrap();
        dl.apply(
            DownloadCommand::RecordProgress {
                worker_id: WorkerId::new(1),
                chunk: ByteRange::new(0, 512).unwrap(),
                speed_fast_bps: 1_000.0,
                speed_slow_bps: 1_000.0,
                eta_secs: Some(2),
            },
            120,
            trace,
        )
        .unwrap();
        dl.apply(DownloadCommand::Pause, 130, trace).unwrap();
        dl.apply(DownloadCommand::Resume, 140, trace).unwrap();

        // Server returns mutated ETag on resume probe!
        dl.apply(
            DownloadCommand::CompleteProbe {
                total_size: Some(2_000),
                supports_range: true,
                validators: ResumeValidators {
                    etag: EtagValidator::parse(r#""mutated""#),
                    last_modified: None,
                    content_length: Some(2_000),
                },
                final_url: None,
                resolved_file_name: None,
                queue_full: false,
                final_url_redacted: None,
            },
            150,
            trace,
        )
        .unwrap();

        assert_eq!(dl.state.as_slug(), "needs_attention");

        dl.apply(
            DownloadCommand::RefreshLink {
                new_url: "https://cdn2.example.com/ubuntu.iso?sig=fresh".to_owned(),
                new_url_redacted: "https://cdn2.example.com/ubuntu.iso?sig=[REDACTED]".to_owned(),
            },
            200,
            trace,
        )
        .unwrap();
        assert_eq!(dl.state.as_slug(), "probing");
    }
}
