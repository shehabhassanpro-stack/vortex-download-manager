//! Permanent error codes (`VTX-*`), categories, recoverability classifications, and user actions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// High-level error category matching `Vortex-Architecture-Design.md` §23.1 & `Protocol-Spec.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum ErrorCategory {
    /// Network transport, DNS, TLS, and rate-limiting errors (`VTX-NET-*`).
    Network,
    /// Remote HTTP/FTP resource or validator errors (`VTX-RES-*`).
    Resource,
    /// Authentication or proxy authorization errors (`VTX-AUTH-*`).
    Auth,
    /// Local filesystem, permissions, disk space, and AV locking errors (`VTX-DISK-*`).
    Disk,
    /// Cryptographic checksum or length verification errors (`VTX-INTG-*`).
    Integrity,
    /// SQLite database, migration, or config persistence errors (`VTX-DB-*`).
    Database,
    /// Native Messaging, Named Pipe, or IPC framing/version errors (`VTX-IPC-*`).
    Ipc,
    /// Security policy rejections (Path Traversal, UNC, SSRF, DRM) (`VTX-SEC-*`).
    Security,
    /// Isolated internal invariants or caught actor panics (`VTX-BUG-*`).
    Internal,
}

impl ErrorCategory {
    /// Canonical protocol string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "Network",
            Self::Resource => "Resource",
            Self::Auth => "Auth",
            Self::Disk => "Disk",
            Self::Integrity => "Integrity",
            Self::Database => "Database",
            Self::Ipc => "Ipc",
            Self::Security => "Security",
            Self::Internal => "Internal",
        }
    }

    /// Expected `VTX-<PREFIX>-` prefix for codes in this category.
    #[must_use]
    pub const fn code_prefix(self) -> &'static str {
        match self {
            Self::Network => "VTX-NET-",
            Self::Resource => "VTX-RES-",
            Self::Auth => "VTX-AUTH-",
            Self::Disk => "VTX-DISK-",
            Self::Integrity => "VTX-INTG-",
            Self::Database => "VTX-DB-",
            Self::Ipc => "VTX-IPC-",
            Self::Security => "VTX-SEC-",
            Self::Internal => "VTX-BUG-",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ErrorCategory {
    type Err = UnknownEnumValueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Network" => Ok(Self::Network),
            "Resource" => Ok(Self::Resource),
            "Auth" => Ok(Self::Auth),
            "Disk" => Ok(Self::Disk),
            "Integrity" => Ok(Self::Integrity),
            "Database" => Ok(Self::Database),
            "Ipc" => Ok(Self::Ipc),
            "Security" => Ok(Self::Security),
            "Internal" => Ok(Self::Internal),
            _ => Err(UnknownEnumValueError {
                kind: "ErrorCategory",
                value: s.to_owned(),
            }),
        }
    }
}

/// Recoverability classification guiding automated retries vs. user intervention (`P0-T09`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub enum Recoverability {
    /// Transient failure eligible for backoff/jitter retries (optionally respecting `Retry-After`).
    Transient {
        /// Suggested retry delay in milliseconds when supplied by server or policy.
        #[serde(rename = "retryAfterMs", alias = "retry_after_ms")]
        #[cfg_attr(feature = "specta", specta(type = Option<f64>))]
        retry_after_ms: Option<u64>,
    },
    /// Cannot proceed automatically; UI surfaces an actionable control (e.g. Refresh Link).
    UserActionRequired,
    /// Terminal failure for the current attempt (e.g. 404, policy rejection, hash mismatch).
    Permanent,
    /// Automatically mitigated by fallback or self-healing (e.g. single-stream fallback, DB backup restore).
    SelfHealed,
    /// Unexpected internal fault or caught Actor panic isolated from sibling downloads.
    IsolatedBug,
}

impl Recoverability {
    /// Canonical protocol discriminant string.
    #[must_use]
    pub const fn kind_str(self) -> &'static str {
        match self {
            Self::Transient { .. } => "Transient",
            Self::UserActionRequired => "UserActionRequired",
            Self::Permanent => "Permanent",
            Self::SelfHealed => "SelfHealed",
            Self::IsolatedBug => "IsolatedBug",
        }
    }

    /// Returns true if automated retry is permitted.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient { .. })
    }

    /// Returns the retry-after hint in milliseconds when present.
    #[must_use]
    pub const fn retry_after_ms(self) -> Option<u64> {
        match self {
            Self::Transient { retry_after_ms } => retry_after_ms,
            Self::UserActionRequired | Self::Permanent | Self::SelfHealed | Self::IsolatedBug => {
                None
            }
        }
    }

    /// Constructs a [`Recoverability`] from its protocol kind string and optional `retry_after_ms`.
    ///
    /// # Errors
    /// Returns [`UnknownEnumValueError`] if `kind` is unrecognized.
    pub fn from_wire(
        kind: &str,
        retry_after_ms: Option<u64>,
    ) -> Result<Self, UnknownEnumValueError> {
        match kind {
            "Transient" => Ok(Self::Transient { retry_after_ms }),
            "UserActionRequired" => Ok(Self::UserActionRequired),
            "Permanent" => Ok(Self::Permanent),
            "SelfHealed" => Ok(Self::SelfHealed),
            "IsolatedBug" => Ok(Self::IsolatedBug),
            _ => Err(UnknownEnumValueError {
                kind: "Recoverability",
                value: kind.to_owned(),
            }),
        }
    }
}

/// Concrete remediation actions rendered as buttons in the 3-tier Error UX (`Design-System.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum SuggestedAction {
    /// Retry the failed operation.
    Retry,
    /// Launch browser Link Refresh flow for expired signed URLs.
    RefreshLink,
    /// Choose a different destination folder.
    ChangeFolder,
    /// Prompt the user to free space on the target drive.
    FreeDiskSpace,
    /// Open credentials / proxy authentication prompt.
    EnterCredentials,
    /// Reconnect external drive (USB/NAS) and resume.
    ReconnectDrive,
    /// Restart the download from byte 0.
    RestartFromBeginning,
    /// Update Vortex Desktop / Extension to match protocol versions.
    UpdateVortex,
    /// Open diagnostics drawer to export a sanitized diagnostic bundle.
    ExportDiagnostics,
    /// Copy `code + trace_id` to clipboard.
    CopyDiagnostics,
    /// Check network connectivity or DNS settings.
    CheckConnection,
    /// Inspect or verify the download URL.
    InspectUrl,
    /// Re-register browser Native Messaging host manifests.
    RepairBrowserBridge,
    /// Open the original source/referrer webpage in the browser (e.g. CAPTCHA/quota or interactive page).
    OpenReferrerInBrowser,
    /// Guide the user to synchronize their Windows system clock (`VTX-AUTH-002`).
    SyncSystemClock,
    /// Guide the user to inspect Windows Security / Defender quarantine history (`VTX-SEC-003`).
    OpenWindowsSecurity,
}

impl SuggestedAction {
    /// Canonical protocol identifier string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::RefreshLink => "RefreshLink",
            Self::ChangeFolder => "ChangeFolder",
            Self::FreeDiskSpace => "FreeDiskSpace",
            Self::EnterCredentials => "EnterCredentials",
            Self::ReconnectDrive => "ReconnectDrive",
            Self::RestartFromBeginning => "RestartFromBeginning",
            Self::UpdateVortex => "UpdateVortex",
            Self::ExportDiagnostics => "ExportDiagnostics",
            Self::CopyDiagnostics => "CopyDiagnostics",
            Self::CheckConnection => "CheckConnection",
            Self::InspectUrl => "InspectUrl",
            Self::RepairBrowserBridge => "RepairBrowserBridge",
            Self::OpenReferrerInBrowser => "OpenReferrerInBrowser",
            Self::SyncSystemClock => "SyncSystemClock",
            Self::OpenWindowsSecurity => "OpenWindowsSecurity",
        }
    }
}

impl fmt::Display for SuggestedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SuggestedAction {
    type Err = UnknownEnumValueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Retry" => Ok(Self::Retry),
            "RefreshLink" => Ok(Self::RefreshLink),
            "ChangeFolder" => Ok(Self::ChangeFolder),
            "FreeDiskSpace" => Ok(Self::FreeDiskSpace),
            "EnterCredentials" => Ok(Self::EnterCredentials),
            "ReconnectDrive" => Ok(Self::ReconnectDrive),
            "RestartFromBeginning" => Ok(Self::RestartFromBeginning),
            "UpdateVortex" => Ok(Self::UpdateVortex),
            "ExportDiagnostics" => Ok(Self::ExportDiagnostics),
            "CopyDiagnostics" => Ok(Self::CopyDiagnostics),
            "CheckConnection" => Ok(Self::CheckConnection),
            "InspectUrl" => Ok(Self::InspectUrl),
            "RepairBrowserBridge" => Ok(Self::RepairBrowserBridge),
            "OpenReferrerInBrowser" => Ok(Self::OpenReferrerInBrowser),
            "SyncSystemClock" => Ok(Self::SyncSystemClock),
            "OpenWindowsSecurity" => Ok(Self::OpenWindowsSecurity),
            _ => Err(UnknownEnumValueError {
                kind: "SuggestedAction",
                value: s.to_owned(),
            }),
        }
    }
}

/// Stable, permanent error code registry (`Vortex-Architecture-Design.md` §30.3).
///
/// Uses explicit `#[serde(rename = "VTX-...")]` on every variant so both `serde` and
/// `specta::Type` (`tauri-specta`) emit the exact identical `"VTX-<CAT>-<NNN>"` wire union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum ErrorCode {
    /// `VTX-NET-001`: DNS resolution failed.
    #[serde(rename = "VTX-NET-001")]
    NetDnsFailure,
    /// `VTX-NET-002`: Connection dropped or timed out.
    #[serde(rename = "VTX-NET-002")]
    NetConnectionLost,
    /// `VTX-NET-003`: TLS certificate verification failed (never silently bypassed).
    #[serde(rename = "VTX-NET-003")]
    NetTlsCertificate,
    /// `VTX-NET-004`: Server rate-limited requests (`429 Too Many Requests`).
    #[serde(rename = "VTX-NET-004")]
    NetRateLimited,
    /// `VTX-NET-005`: Malformed HTTP response or invalid `Content-Length`/`Content-Range`.
    #[serde(rename = "VTX-NET-005")]
    NetMalformedResponse,
    /// `VTX-RES-001`: Remote resource not found (`404 Not Found`).
    #[serde(rename = "VTX-RES-001")]
    ResNotFound,
    /// `VTX-RES-002`: Download link expired (`403` / `410`).
    #[serde(rename = "VTX-RES-002")]
    ResLinkExpired,
    /// `VTX-RES-003`: Remote file changed during resume (validator mismatch).
    #[serde(rename = "VTX-RES-003")]
    ResResourceChanged,
    /// `VTX-RES-004`: Server does not support byte ranges (`200` instead of `206`).
    #[serde(rename = "VTX-RES-004")]
    ResRangeUnsupported,
    /// `VTX-RES-005`: Server enforces per-IP concurrency cap; engine stepped down seamlessly (`SelfHealed`).
    #[serde(rename = "VTX-RES-005")]
    ResConcurrencyCapped,
    /// `VTX-RES-006`: Host requires human CAPTCHA verification or free-tier quota was exceeded (`ADR-011`).
    #[serde(rename = "VTX-RES-006")]
    ResQuotaOrCaptcha,
    /// `VTX-AUTH-001`: Authentication (`401`) or proxy authentication (`407`) required.
    #[serde(rename = "VTX-AUTH-001")]
    AuthRequired,
    /// `VTX-AUTH-002`: Local system clock skew invalidated signed URL (`RequestTimeTooSkewed` / `Date` delta).
    #[serde(rename = "VTX-AUTH-002")]
    AuthClockSkew,
    /// `VTX-DISK-001`: Insufficient disk space (`ENOSPC` or < 256 MB safety reserve).
    #[serde(rename = "VTX-DISK-001")]
    DiskInsufficientSpace,
    /// `VTX-DISK-002`: Write permission denied on target folder (`EACCES`).
    #[serde(rename = "VTX-DISK-002")]
    DiskPermissionDenied,
    /// `VTX-DISK-003`: File temporarily locked by AV or another process (`ERROR_SHARING_VIOLATION`).
    #[serde(rename = "VTX-DISK-003")]
    DiskFileLocked,
    /// `VTX-DISK-004`: External storage volume disconnected during transfer.
    #[serde(rename = "VTX-DISK-004")]
    DiskVolumeDisconnected,
    /// `VTX-DISK-005`: File exceeds destination filesystem max size (e.g. `> 4 GiB - 1` on `FAT32`).
    #[serde(rename = "VTX-DISK-005")]
    DiskFilesystemLimit,
    /// `VTX-DISK-006`: Partial `.vortex` file was removed externally before resuming (`UX-M03`).
    #[serde(rename = "VTX-DISK-006")]
    DiskPartialFileMissing,
    /// `VTX-INTG-001`: File cryptographic hash or final size verification mismatch.
    #[serde(rename = "VTX-INTG-001")]
    IntgHashMismatch,
    /// `VTX-DB-001`: SQLite corruption, migration failure, or newer schema incompatibility.
    #[serde(rename = "VTX-DB-001")]
    DbCorruptOrIncompatible,
    /// `VTX-IPC-001`: Browser extension / CLI could not reach Vortex Core.
    #[serde(rename = "VTX-IPC-001")]
    IpcCoreUnreachable,
    /// `VTX-IPC-002`: Incompatible IPC protocol version between client and Core.
    #[serde(rename = "VTX-IPC-002")]
    IpcVersionMismatch,
    /// `VTX-IPC-003`: Invalid IPC frame length (`0` or exceeds `MAX_FRAME_BYTES`).
    #[serde(rename = "VTX-IPC-003")]
    IpcFrameInvalid,
    /// `VTX-IPC-004`: IPC client exceeded message rate limit (`> 100 msg/s`).
    #[serde(rename = "VTX-IPC-004")]
    IpcRateLimited,
    /// `VTX-SEC-001`: Security policy violation (Path Traversal, Symlink TOCTOU, UNC, SSRF, header/URL limits).
    #[serde(rename = "VTX-SEC-001")]
    SecPolicyViolation,
    /// `VTX-SEC-002`: Unsupported DRM protection detected (`ADR-011`).
    #[serde(rename = "VTX-SEC-002")]
    SecDrmUnsupported,
    /// `VTX-SEC-003`: File quarantined or removed by OS Antivirus / Windows Defender (`UX-M06`).
    #[serde(rename = "VTX-SEC-003")]
    SecAntivirusQuarantined,
    /// `VTX-BUG-001`: Unexpected internal error or isolated Actor panic.
    #[serde(rename = "VTX-BUG-001")]
    BugInternalIsolated,
}

impl ErrorCode {
    /// Complete slice of every registered error code (used for invariants & registry tests).
    pub const ALL: &'static [Self] = &[
        Self::NetDnsFailure,
        Self::NetConnectionLost,
        Self::NetTlsCertificate,
        Self::NetRateLimited,
        Self::NetMalformedResponse,
        Self::ResNotFound,
        Self::ResLinkExpired,
        Self::ResResourceChanged,
        Self::ResRangeUnsupported,
        Self::ResConcurrencyCapped,
        Self::ResQuotaOrCaptcha,
        Self::AuthRequired,
        Self::AuthClockSkew,
        Self::DiskInsufficientSpace,
        Self::DiskPermissionDenied,
        Self::DiskFileLocked,
        Self::DiskVolumeDisconnected,
        Self::DiskFilesystemLimit,
        Self::DiskPartialFileMissing,
        Self::IntgHashMismatch,
        Self::DbCorruptOrIncompatible,
        Self::IpcCoreUnreachable,
        Self::IpcVersionMismatch,
        Self::IpcFrameInvalid,
        Self::IpcRateLimited,
        Self::SecPolicyViolation,
        Self::SecDrmUnsupported,
        Self::SecAntivirusQuarantined,
        Self::BugInternalIsolated,
    ];

    /// Returns the permanent wire code string (e.g. `"VTX-DISK-001"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetDnsFailure => "VTX-NET-001",
            Self::NetConnectionLost => "VTX-NET-002",
            Self::NetTlsCertificate => "VTX-NET-003",
            Self::NetRateLimited => "VTX-NET-004",
            Self::NetMalformedResponse => "VTX-NET-005",
            Self::ResNotFound => "VTX-RES-001",
            Self::ResLinkExpired => "VTX-RES-002",
            Self::ResResourceChanged => "VTX-RES-003",
            Self::ResRangeUnsupported => "VTX-RES-004",
            Self::ResConcurrencyCapped => "VTX-RES-005",
            Self::ResQuotaOrCaptcha => "VTX-RES-006",
            Self::AuthRequired => "VTX-AUTH-001",
            Self::AuthClockSkew => "VTX-AUTH-002",
            Self::DiskInsufficientSpace => "VTX-DISK-001",
            Self::DiskPermissionDenied => "VTX-DISK-002",
            Self::DiskFileLocked => "VTX-DISK-003",
            Self::DiskVolumeDisconnected => "VTX-DISK-004",
            Self::DiskFilesystemLimit => "VTX-DISK-005",
            Self::DiskPartialFileMissing => "VTX-DISK-006",
            Self::IntgHashMismatch => "VTX-INTG-001",
            Self::DbCorruptOrIncompatible => "VTX-DB-001",
            Self::IpcCoreUnreachable => "VTX-IPC-001",
            Self::IpcVersionMismatch => "VTX-IPC-002",
            Self::IpcFrameInvalid => "VTX-IPC-003",
            Self::IpcRateLimited => "VTX-IPC-004",
            Self::SecPolicyViolation => "VTX-SEC-001",
            Self::SecDrmUnsupported => "VTX-SEC-002",
            Self::SecAntivirusQuarantined => "VTX-SEC-003",
            Self::BugInternalIsolated => "VTX-BUG-001",
        }
    }

    /// Returns the owning [`ErrorCategory`].
    #[must_use]
    pub const fn category(self) -> ErrorCategory {
        match self {
            Self::NetDnsFailure
            | Self::NetConnectionLost
            | Self::NetTlsCertificate
            | Self::NetRateLimited
            | Self::NetMalformedResponse => ErrorCategory::Network,
            Self::ResNotFound
            | Self::ResLinkExpired
            | Self::ResResourceChanged
            | Self::ResRangeUnsupported
            | Self::ResConcurrencyCapped
            | Self::ResQuotaOrCaptcha => ErrorCategory::Resource,
            Self::AuthRequired | Self::AuthClockSkew => ErrorCategory::Auth,
            Self::DiskInsufficientSpace
            | Self::DiskPermissionDenied
            | Self::DiskFileLocked
            | Self::DiskVolumeDisconnected
            | Self::DiskFilesystemLimit
            | Self::DiskPartialFileMissing => ErrorCategory::Disk,
            Self::IntgHashMismatch => ErrorCategory::Integrity,
            Self::DbCorruptOrIncompatible => ErrorCategory::Database,
            Self::IpcCoreUnreachable
            | Self::IpcVersionMismatch
            | Self::IpcFrameInvalid
            | Self::IpcRateLimited => ErrorCategory::Ipc,
            Self::SecPolicyViolation | Self::SecDrmUnsupported | Self::SecAntivirusQuarantined => {
                ErrorCategory::Security
            }
            Self::BugInternalIsolated => ErrorCategory::Internal,
        }
    }

    /// Returns the default [`Recoverability`] defined by the error registry.
    #[must_use]
    pub const fn default_recoverability(self) -> Recoverability {
        match self {
            Self::NetDnsFailure
            | Self::NetConnectionLost
            | Self::NetRateLimited
            | Self::NetMalformedResponse
            | Self::DiskFileLocked
            | Self::IpcCoreUnreachable
            | Self::IpcRateLimited => Recoverability::Transient {
                retry_after_ms: None,
            },
            Self::ResLinkExpired
            | Self::ResResourceChanged
            | Self::ResQuotaOrCaptcha
            | Self::AuthRequired
            | Self::AuthClockSkew
            | Self::DiskInsufficientSpace
            | Self::DiskPermissionDenied
            | Self::DiskVolumeDisconnected
            | Self::DiskFilesystemLimit
            | Self::DiskPartialFileMissing
            | Self::IpcVersionMismatch => Recoverability::UserActionRequired,
            Self::NetTlsCertificate
            | Self::ResNotFound
            | Self::IntgHashMismatch
            | Self::IpcFrameInvalid
            | Self::SecPolicyViolation
            | Self::SecDrmUnsupported
            | Self::SecAntivirusQuarantined => Recoverability::Permanent,
            Self::ResRangeUnsupported
            | Self::ResConcurrencyCapped
            | Self::DbCorruptOrIncompatible => Recoverability::SelfHealed,
            Self::BugInternalIsolated => Recoverability::IsolatedBug,
        }
    }

    /// Returns the canonical i18n lookup key for user-facing messages.
    #[must_use]
    pub const fn default_message_key(self) -> &'static str {
        match self {
            Self::NetDnsFailure => "errors.net.dns_failure",
            Self::NetConnectionLost => "errors.net.connection_lost",
            Self::NetTlsCertificate => "errors.net.tls_certificate",
            Self::NetRateLimited => "errors.net.rate_limited",
            Self::NetMalformedResponse => "errors.net.malformed_response",
            Self::ResNotFound => "errors.res.not_found",
            Self::ResLinkExpired => "errors.res.link_expired",
            Self::ResResourceChanged => "errors.res.resource_changed",
            Self::ResRangeUnsupported => "errors.res.range_unsupported",
            Self::ResConcurrencyCapped => "errors.res.concurrency_capped",
            Self::ResQuotaOrCaptcha => "errors.res.quota_or_captcha",
            Self::AuthRequired => "errors.auth.required",
            Self::AuthClockSkew => "errors.auth.clock_skew",
            Self::DiskInsufficientSpace => "errors.disk.insufficient_space",
            Self::DiskPermissionDenied => "errors.disk.permission_denied",
            Self::DiskFileLocked => "errors.disk.file_locked",
            Self::DiskVolumeDisconnected => "errors.disk.volume_disconnected",
            Self::DiskFilesystemLimit => "errors.disk.filesystem_limit",
            Self::DiskPartialFileMissing => "errors.disk.partial_file_missing",
            Self::IntgHashMismatch => "errors.intg.hash_mismatch",
            Self::DbCorruptOrIncompatible => "errors.db.corrupt_or_incompatible",
            Self::IpcCoreUnreachable => "errors.ipc.core_unreachable",
            Self::IpcVersionMismatch => "errors.ipc.version_mismatch",
            Self::IpcFrameInvalid => "errors.ipc.frame_invalid",
            Self::IpcRateLimited => "errors.ipc.rate_limited",
            Self::SecPolicyViolation => "errors.sec.policy_violation",
            Self::SecDrmUnsupported => "errors.sec.drm_unsupported",
            Self::SecAntivirusQuarantined => "errors.sec.antivirus_quarantined",
            Self::BugInternalIsolated => "errors.bug.internal_isolated",
        }
    }

    /// Returns default suggested user actions for this error code.
    #[must_use]
    pub fn default_actions(self) -> Vec<SuggestedAction> {
        match self {
            Self::NetDnsFailure => vec![SuggestedAction::Retry, SuggestedAction::CheckConnection],
            Self::NetConnectionLost
            | Self::NetRateLimited
            | Self::NetMalformedResponse
            | Self::DiskFileLocked
            | Self::IpcRateLimited => vec![SuggestedAction::Retry],
            Self::NetTlsCertificate | Self::SecPolicyViolation | Self::SecDrmUnsupported => {
                vec![SuggestedAction::CopyDiagnostics]
            }
            Self::SecAntivirusQuarantined => vec![
                SuggestedAction::OpenWindowsSecurity,
                SuggestedAction::CopyDiagnostics,
            ],
            Self::ResNotFound => vec![SuggestedAction::InspectUrl],
            Self::ResLinkExpired => vec![
                SuggestedAction::RefreshLink,
                SuggestedAction::OpenReferrerInBrowser,
            ],
            Self::ResQuotaOrCaptcha => vec![
                SuggestedAction::OpenReferrerInBrowser,
                SuggestedAction::Retry,
            ],
            Self::ResResourceChanged | Self::IntgHashMismatch => {
                vec![SuggestedAction::RestartFromBeginning]
            }
            Self::ResRangeUnsupported | Self::ResConcurrencyCapped => vec![],
            Self::AuthRequired => vec![SuggestedAction::EnterCredentials],
            Self::AuthClockSkew => vec![SuggestedAction::SyncSystemClock, SuggestedAction::Retry],
            Self::DiskInsufficientSpace => vec![
                SuggestedAction::ChangeFolder,
                SuggestedAction::FreeDiskSpace,
                SuggestedAction::Retry,
            ],
            Self::DiskPermissionDenied | Self::DiskFilesystemLimit => {
                vec![SuggestedAction::ChangeFolder]
            }
            Self::DiskPartialFileMissing => vec![
                SuggestedAction::RestartFromBeginning,
                SuggestedAction::ChangeFolder,
            ],
            Self::DiskVolumeDisconnected => {
                vec![SuggestedAction::ReconnectDrive, SuggestedAction::Retry]
            }
            Self::DbCorruptOrIncompatible => {
                vec![
                    SuggestedAction::UpdateVortex,
                    SuggestedAction::ExportDiagnostics,
                ]
            }
            Self::IpcCoreUnreachable => {
                vec![SuggestedAction::RepairBrowserBridge, SuggestedAction::Retry]
            }
            Self::IpcVersionMismatch => vec![SuggestedAction::UpdateVortex],
            Self::IpcFrameInvalid | Self::BugInternalIsolated => {
                vec![
                    SuggestedAction::ExportDiagnostics,
                    SuggestedAction::CopyDiagnostics,
                ]
            }
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ErrorCode {
    type Err = UnknownEnumValueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "VTX-NET-001" => Ok(Self::NetDnsFailure),
            "VTX-NET-002" => Ok(Self::NetConnectionLost),
            "VTX-NET-003" => Ok(Self::NetTlsCertificate),
            "VTX-NET-004" => Ok(Self::NetRateLimited),
            "VTX-NET-005" => Ok(Self::NetMalformedResponse),
            "VTX-RES-001" => Ok(Self::ResNotFound),
            "VTX-RES-002" => Ok(Self::ResLinkExpired),
            "VTX-RES-003" => Ok(Self::ResResourceChanged),
            "VTX-RES-004" => Ok(Self::ResRangeUnsupported),
            "VTX-RES-005" => Ok(Self::ResConcurrencyCapped),
            "VTX-RES-006" => Ok(Self::ResQuotaOrCaptcha),
            "VTX-AUTH-001" => Ok(Self::AuthRequired),
            "VTX-AUTH-002" => Ok(Self::AuthClockSkew),
            "VTX-DISK-001" => Ok(Self::DiskInsufficientSpace),
            "VTX-DISK-002" => Ok(Self::DiskPermissionDenied),
            "VTX-DISK-003" => Ok(Self::DiskFileLocked),
            "VTX-DISK-004" => Ok(Self::DiskVolumeDisconnected),
            "VTX-DISK-005" => Ok(Self::DiskFilesystemLimit),
            "VTX-DISK-006" => Ok(Self::DiskPartialFileMissing),
            "VTX-INTG-001" => Ok(Self::IntgHashMismatch),
            "VTX-DB-001" => Ok(Self::DbCorruptOrIncompatible),
            "VTX-IPC-001" => Ok(Self::IpcCoreUnreachable),
            "VTX-IPC-002" => Ok(Self::IpcVersionMismatch),
            "VTX-IPC-003" => Ok(Self::IpcFrameInvalid),
            "VTX-IPC-004" => Ok(Self::IpcRateLimited),
            "VTX-SEC-001" => Ok(Self::SecPolicyViolation),
            "VTX-SEC-002" => Ok(Self::SecDrmUnsupported),
            "VTX-SEC-003" => Ok(Self::SecAntivirusQuarantined),
            "VTX-BUG-001" => Ok(Self::BugInternalIsolated),
            _ => Err(UnknownEnumValueError {
                kind: "ErrorCode",
                value: s.to_owned(),
            }),
        }
    }
}

/// Returned when parsing an unknown protocol string into an error taxonomy enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value: `{value}`")]
pub struct UnknownEnumValueError {
    /// Name of the target enum type.
    pub kind: &'static str,
    /// Unrecognized wire string.
    pub value: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn all_error_codes_are_unique_match_prefix_and_match_serde_rename() {
        let mut seen = BTreeSet::new();
        for &code in ErrorCode::ALL {
            let wire = code.as_str();
            assert!(
                seen.insert(wire),
                "duplicate error code wire string detected: {wire}"
            );
            assert!(
                wire.starts_with(code.category().code_prefix()),
                "{wire} does not match expected prefix {}",
                code.category().code_prefix()
            );

            // Round-trip string & JSON serialization (verifies #[serde(rename = ...)] == as_str())
            let parsed: ErrorCode = wire.parse().unwrap();
            assert_eq!(parsed, code);

            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let from_json: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(from_json, code);

            assert_ne!(code.default_message_key(), "");
            let _rec = code.default_recoverability();
            let _acts = code.default_actions();
            assert_eq!(code.to_string(), wire);
        }
        assert_eq!(seen.len(), 29);
        assert!("VTX-INVALID-999".parse::<ErrorCode>().is_err());
        assert!(serde_json::from_str::<ErrorCode>("\"VTX-NOPE-000\"").is_err());
    }

    #[test]
    fn recoverability_wire_round_trip() {
        let cases = [
            Recoverability::Transient {
                retry_after_ms: Some(1500),
            },
            Recoverability::Transient {
                retry_after_ms: None,
            },
            Recoverability::UserActionRequired,
            Recoverability::Permanent,
            Recoverability::SelfHealed,
            Recoverability::IsolatedBug,
        ];

        for rec in cases {
            let decoded = Recoverability::from_wire(rec.kind_str(), rec.retry_after_ms()).unwrap();
            assert_eq!(decoded, rec);
        }
        assert!(Recoverability::from_wire("Bogus", None).is_err());
    }

    #[test]
    fn category_and_action_round_trip() {
        let categories = [
            ErrorCategory::Network,
            ErrorCategory::Resource,
            ErrorCategory::Auth,
            ErrorCategory::Disk,
            ErrorCategory::Integrity,
            ErrorCategory::Database,
            ErrorCategory::Ipc,
            ErrorCategory::Security,
            ErrorCategory::Internal,
        ];
        for cat in categories {
            assert_eq!(cat.as_str().parse::<ErrorCategory>().unwrap(), cat);
            assert_eq!(cat.to_string(), cat.as_str());
        }
        assert!("UnknownCat".parse::<ErrorCategory>().is_err());

        let actions = [
            SuggestedAction::Retry,
            SuggestedAction::RefreshLink,
            SuggestedAction::ChangeFolder,
            SuggestedAction::FreeDiskSpace,
            SuggestedAction::EnterCredentials,
            SuggestedAction::ReconnectDrive,
            SuggestedAction::RestartFromBeginning,
            SuggestedAction::UpdateVortex,
            SuggestedAction::ExportDiagnostics,
            SuggestedAction::CopyDiagnostics,
            SuggestedAction::CheckConnection,
            SuggestedAction::InspectUrl,
            SuggestedAction::RepairBrowserBridge,
            SuggestedAction::OpenReferrerInBrowser,
            SuggestedAction::SyncSystemClock,
            SuggestedAction::OpenWindowsSecurity,
        ];
        for action in actions {
            assert_eq!(action.as_str().parse::<SuggestedAction>().unwrap(), action);
            assert_eq!(action.to_string(), action.as_str());
        }
        assert!("UnknownAction".parse::<SuggestedAction>().is_err());
    }
}
