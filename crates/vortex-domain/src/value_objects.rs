//! Core domain value objects (`P1-T01`): `DownloadId`, `EventId`, `QueueId`, `CategoryId`,
//! `WorkerId`, invariant-checked `ByteRange`, `Checksum`, `ResumeValidators` (RFC 9110 §13.1.5),
//! `Priority`, and `Timestamps`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::DomainResult;

/// Time-sortable `UUIDv7` primary identifier for a `Download` aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DownloadId(Uuid);

impl DownloadId {
    /// Generates a fresh time-ordered `UUIDv7`.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing [`Uuid`].
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Parses a canonical hyphenated UUID string.
    ///
    /// # Errors
    /// Returns [`VortexError`] (`VTX-SEC-001`) if `raw` is not a valid UUID.
    pub fn parse(raw: &str) -> DomainResult<Self> {
        Uuid::parse_str(raw).map(Self).map_err(|err| {
            VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!("Invalid DownloadId UUID: {err}"),
            )
        })
    }

    /// Returns the underlying [`Uuid`].
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for DownloadId {
    type Err = VortexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Time-sortable `UUIDv7` identifier for an immutable `DomainEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(Uuid);

impl EventId {
    /// Generates a fresh time-ordered `UUIDv7`.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing [`Uuid`].
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier of a download scheduling queue (e.g., `"main"`, `"night"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(transparent)]
pub struct QueueId(String);

impl QueueId {
    /// Built-in default queue identifier (`"main"`).
    pub const MAIN_ID: &'static str = "main";

    /// Returns the default main queue identifier.
    #[must_use]
    pub fn main_queue() -> Self {
        Self(Self::MAIN_ID.to_owned())
    }

    /// Creates a validated non-empty [`QueueId`].
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if trimmed identifier is empty or exceeds 64 chars.
    pub fn new(raw: impl Into<String>) -> DomainResult<Self> {
        let s = raw.into();
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.len() > 64 {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                "QueueId must be 1..=64 non-whitespace characters",
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Borrows the raw string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for QueueId {
    fn default() -> Self {
        Self::main_queue()
    }
}

impl fmt::Display for QueueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier of a download file category (e.g., `"video"`, `"compressed"`, `"programs"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(transparent)]
pub struct CategoryId(String);

impl CategoryId {
    /// Canonical category IDs matching `Vortex-Architecture-Design.md` §15.3.
    pub const VIDEO: &'static str = "video";
    /// Audio files category ID.
    pub const AUDIO: &'static str = "audio";
    /// Archives and disk images category ID.
    pub const COMPRESSED: &'static str = "compressed";
    /// Executables and installers category ID.
    pub const PROGRAMS: &'static str = "programs";
    /// Office and text documents category ID.
    pub const DOCUMENTS: &'static str = "documents";
    /// Raster and vector images category ID.
    pub const IMAGES: &'static str = "images";
    /// Fallback category ID.
    pub const OTHER: &'static str = "other";

    /// Creates a validated [`CategoryId`].
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if empty or longer than 64 characters.
    pub fn new(raw: impl Into<String>) -> DomainResult<Self> {
        let s = raw.into();
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.len() > 64 {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                "CategoryId must be 1..=64 non-whitespace characters",
            ));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// Creates one of the built-in category IDs infallibly.
    #[must_use]
    pub fn builtin(id: &'static str) -> Self {
        Self(id.to_owned())
    }

    /// Borrows the category slug.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for CategoryId {
    fn default() -> Self {
        Self::builtin(Self::OTHER)
    }
}

impl fmt::Display for CategoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 1-based identifier of a concurrent connection worker (`1..=32` typically).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(transparent)]
pub struct WorkerId(u16);

impl WorkerId {
    /// Creates a [`WorkerId`] (`0` is normalized to `1`).
    #[must_use]
    pub const fn new(index: u16) -> Self {
        if index == 0 { Self(1) } else { Self(index) }
    }

    /// Underlying 1-based worker number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Encodes this worker into the `2..=33` visual bin state byte (`Design-System.md` §4.2).
    #[must_use]
    pub const fn as_bin_marker(self) -> u8 {
        let zero_based = self.0.saturating_sub(1) % 32;
        #[allow(clippy::cast_possible_truncation)]
        let marker = 2_u8 + (zero_based as u8);
        marker
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "W{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
struct RawByteRange {
    start: u64,
    end: u64,
}

/// Half-open byte interval `[start, end)` guaranteed by type invariant to satisfy `start < end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(try_from = "RawByteRange", into = "RawByteRange")]
pub struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    /// Constructs a validated half-open byte interval `[start, end)`.
    ///
    /// # Errors
    /// Returns `VTX-NET-005` if `start >= end`.
    pub fn new(start: u64, end: u64) -> DomainResult<Self> {
        if start >= end {
            return Err(VortexError::coded(
                ErrorCode::NetMalformedResponse,
                TraceId::new(),
                format!("ByteRange invariant violated: start ({start}) must be < end ({end})"),
            ));
        }
        Ok(Self { start, end })
    }

    /// Inclusive start byte offset.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Exclusive end byte offset (`end > start`).
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Inclusive last byte offset (`end - 1`), guaranteed overflow-free by `start < end`.
    #[must_use]
    pub const fn last_inclusive(self) -> u64 {
        self.end - 1
    }

    /// Strictly positive length in bytes (`>= 1`).
    #[must_use]
    #[allow(clippy::len_without_is_empty)]
    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    /// Formats as an RFC 9110 HTTP `Range` header value (`bytes=start-last_inclusive`).
    #[must_use]
    pub fn to_http_range_header(self) -> String {
        format!("bytes={}-{}", self.start, self.last_inclusive())
    }

    /// Returns `true` if `offset` lies inside `[start, end)`.
    #[must_use]
    pub const fn contains_offset(self, offset: u64) -> bool {
        offset >= self.start && offset < self.end
    }

    /// Returns `true` if `self` and `other` share at least one byte.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Returns `true` if `self` and `other` overlap or are immediately adjacent (`end == other.start`).
    #[must_use]
    pub const fn touches_or_overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// Splits `[start, end)` at `mid` into `([start, mid), [mid, end))` if `start < mid < end`.
    #[must_use]
    pub const fn split_at(self, mid: u64) -> Option<(Self, Self)> {
        if self.start < mid && mid < self.end {
            Some((
                Self {
                    start: self.start,
                    end: mid,
                },
                Self {
                    start: mid,
                    end: self.end,
                },
            ))
        } else {
            None
        }
    }

    /// Computes the exact midpoint if `len() >= 2`.
    #[must_use]
    pub const fn midpoint(self) -> Option<u64> {
        if self.end - self.start >= 2 {
            Some(self.start + (self.end - self.start) / 2)
        } else {
            None
        }
    }
}

impl TryFrom<RawByteRange> for ByteRange {
    type Error = VortexError;

    fn try_from(raw: RawByteRange) -> Result<Self, Self::Error> {
        Self::new(raw.start, raw.end)
    }
}

impl From<ByteRange> for RawByteRange {
    fn from(range: ByteRange) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// Supported file integrity hash algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    /// BLAKE3 (256-bit / 64 hex chars, SIMD Merkle tree).
    Blake3,
    /// SHA-256 (256-bit / 64 hex chars, SHA-NI hardware accelerated).
    Sha256,
    /// Legacy SHA-1 (160-bit / 40 hex chars — compatibility only).
    Sha1,
    /// Legacy MD5 (128-bit / 32 hex chars — compatibility only).
    Md5,
}

impl HashAlgorithm {
    /// Expected lowercase hexadecimal digest character length.
    #[must_use]
    pub const fn hex_len(self) -> usize {
        match self {
            Self::Blake3 | Self::Sha256 => 64,
            Self::Sha1 => 40,
            Self::Md5 => 32,
        }
    }

    /// Canonical prefix name (`"blake3"`, `"sha256"`, `"sha1"`, `"md5"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blake3 => "blake3",
            Self::Sha256 => "sha256",
            Self::Sha1 => "sha1",
            Self::Md5 => "md5",
        }
    }
}

/// Validated expected file digest (`algorithm` + normalized lowercase hex string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Checksum {
    /// Hash algorithm.
    pub algorithm: HashAlgorithm,
    /// Normalized lowercase hexadecimal digest.
    pub hex_digest: String,
}

impl Checksum {
    /// Creates a validated [`Checksum`].
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if `hex_digest` length or hex characters are invalid for `algorithm`.
    pub fn new(algorithm: HashAlgorithm, raw_hex: &str) -> DomainResult<Self> {
        let cleaned = raw_hex.trim().to_ascii_lowercase();
        if cleaned.len() != algorithm.hex_len() || !cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                format!(
                    "Expected {} hex characters for {}, got {}",
                    algorithm.hex_len(),
                    algorithm.as_str(),
                    cleaned.len()
                ),
            ));
        }
        Ok(Self {
            algorithm,
            hex_digest: cleaned,
        })
    }

    /// Parses `"sha256:<hex>"`, `"blake3:<hex>"`, `"sha1:<hex>"`, or `"md5:<hex>"`.
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` on unknown algorithm prefix or malformed hex digest.
    pub fn parse_prefixed(spec: &str) -> DomainResult<Self> {
        let Some((algo_raw, hex_part)) = spec.trim().split_once(':') else {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                TraceId::new(),
                "Checksum must use `<algo>:<hex>` format",
            ));
        };
        let algorithm = match algo_raw.trim().to_ascii_lowercase().as_str() {
            "blake3" => HashAlgorithm::Blake3,
            "sha256" | "sha-256" => HashAlgorithm::Sha256,
            "sha1" | "sha-1" => HashAlgorithm::Sha1,
            "md5" => HashAlgorithm::Md5,
            other => {
                return Err(VortexError::coded(
                    ErrorCode::SecPolicyViolation,
                    TraceId::new(),
                    format!("Unsupported hash algorithm `{other}`"),
                ));
            }
        };
        Self::new(algorithm, hex_part)
    }

    /// Compares `computed_hex` against `self.hex_digest` in fixed-length XOR loop.
    #[must_use]
    pub fn matches_hex(&self, computed_hex: &str) -> bool {
        let candidate = computed_hex.trim().as_bytes();
        let expected = self.hex_digest.as_bytes();
        if candidate.len() != expected.len() {
            return false;
        }
        let mut diff = 0_u8;
        for (&a, &b) in candidate.iter().zip(expected.iter()) {
            diff |= a.to_ascii_lowercase() ^ b;
        }
        diff == 0
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.as_str(), self.hex_digest)
    }
}

/// HTTP `ETag` validator explicitly distinguishing Strong (`"..."`) from Weak (`W/"..."`)
/// per RFC 9110 §8.8.3 & §13.1.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum EtagValidator {
    /// Strong entity-tag safe for `If-Range` (`"xyz"`).
    Strong(String),
    /// Weak entity-tag (`W/"xyz"`) — MUST NEVER be sent in `If-Range`!
    Weak(String),
}

impl EtagValidator {
    /// Parses an HTTP `ETag` header value into [`Strong`](Self::Strong) or [`Weak`](Self::Weak).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(rest) = trimmed
            .strip_prefix("W/")
            .or_else(|| trimmed.strip_prefix("w/"))
        {
            let tag = rest.trim();
            if tag.is_empty() {
                return None;
            }
            let normalized = if tag.starts_with('"') && tag.ends_with('"') && tag.len() >= 2 {
                format!("W/{tag}")
            } else {
                format!("W/\"{tag}\"")
            };
            Some(Self::Weak(normalized))
        } else {
            let normalized =
                if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
                    trimmed.to_owned()
                } else {
                    format!("\"{trimmed}\"")
                };
            Some(Self::Strong(normalized))
        }
    }

    /// Raw wire representation (`"..."` or `W/"..."`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Strong(s) | Self::Weak(s) => s,
        }
    }

    /// Returns `true` only if this `ETag` is Strong (allowed in `If-Range`).
    #[must_use]
    pub const fn is_strong(&self) -> bool {
        matches!(self, Self::Strong(_))
    }
}

/// HTTP `Last-Modified` validator paired with optional origin `Date` for RFC 9110 §8.8.2.2
/// strong-validator verification (`Date - Last-Modified >= 1s`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct LastModifiedValidator {
    /// Exact HTTP-date string from `Last-Modified` header.
    pub raw_http_date: String,
    /// Parsed `Last-Modified` Unix timestamp in seconds, if parseable.
    pub last_modified_epoch_secs: Option<i64>,
    /// Parsed origin `Date` header Unix timestamp in seconds from the same response.
    pub response_date_epoch_secs: Option<i64>,
}

impl LastModifiedValidator {
    /// Returns `true` iff `response_date - last_modified >= 1` second per RFC 9110 §8.8.2.2.
    #[must_use]
    pub const fn is_strong_per_rfc9110(&self) -> bool {
        match (self.response_date_epoch_secs, self.last_modified_epoch_secs) {
            (Some(date_s), Some(lm_s)) => date_s.saturating_sub(lm_s) >= 1,
            _ => false,
        }
    }
}

/// Action instructed by RFC 9110 §13.1.5 when building a resume `Range` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfRangeDirective {
    /// Send `If-Range: <value>` using a verified Strong `ETag` or Strong `Last-Modified`.
    SendHeader(String),
    /// Only weak/absent validators exist: omit `If-Range`, send `Range`, and validate `206`
    /// response headers programmatically before writing any byte (`VTX-Architecture` §8.6.3).
    ManualValidate206,
}

/// Resource validators persisted across checkpoints to prevent mixed-version file corruption (`TH-10`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ResumeValidators {
    /// Parsed Strong or Weak `ETag`.
    pub etag: Option<EtagValidator>,
    /// Parsed `Last-Modified` + origin `Date` strength metadata.
    pub last_modified: Option<LastModifiedValidator>,
    /// Expected total resource length in bytes.
    pub content_length: Option<u64>,
}

impl ResumeValidators {
    /// Implements the strict 3-branch `If-Range` selector from `Vortex-Architecture-Design.md` §8.6.3:
    /// 1. Strong `ETag` -> `If-Range: "<etag>"`
    /// 2. Weak/missing `ETag` + Strong `Last-Modified` (`Date - LM >= 1s`) -> `If-Range: <HTTP-date>`
    /// 3. Weak validators only -> omit `If-Range` and perform `ManualValidate206`.
    #[must_use]
    pub fn select_if_range(&self) -> IfRangeDirective {
        if let Some(EtagValidator::Strong(strong_etag)) = &self.etag {
            return IfRangeDirective::SendHeader(strong_etag.clone());
        }
        if let Some(lm) = &self.last_modified
            && lm.is_strong_per_rfc9110()
            && !lm.raw_http_date.trim().is_empty()
        {
            return IfRangeDirective::SendHeader(lm.raw_http_date.clone());
        }
        IfRangeDirective::ManualValidate206
    }

    /// Verifies that `fresh` response headers on resume match previously recorded validators.
    ///
    /// # Errors
    /// Returns `VTX-RES-003` (`ResResourceChanged`) if size, `ETag`, or `Last-Modified` mutated.
    pub fn validate_on_resume(&self, fresh: &Self) -> DomainResult<()> {
        if let (Some(expected_len), Some(actual_len)) = (self.content_length, fresh.content_length)
            && expected_len != actual_len
        {
            return Err(Self::mismatch_error(format!(
                "Content-Length changed from {expected_len} to {actual_len}"
            )));
        }
        if let (Some(expected_etag), Some(actual_etag)) = (&self.etag, &fresh.etag)
            && expected_etag != actual_etag
        {
            return Err(Self::mismatch_error(format!(
                "ETag changed from {} to {}",
                expected_etag.as_str(),
                actual_etag.as_str()
            )));
        }
        if let (Some(expected_lm), Some(actual_lm)) = (&self.last_modified, &fresh.last_modified)
            && expected_lm.raw_http_date != actual_lm.raw_http_date
        {
            return Err(Self::mismatch_error(format!(
                "Last-Modified changed from `{}` to `{}`",
                expected_lm.raw_http_date, actual_lm.raw_http_date
            )));
        }
        Ok(())
    }

    fn mismatch_error(detail: String) -> VortexError {
        VortexError::coded(ErrorCode::ResResourceChanged, TraceId::new(), detail)
    }
}

/// Scheduling priority inside a `Queue` (`Critical > High > Normal > Low`).
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Background / lowest priority.
    Low = 1,
    /// Default user download priority.
    #[default]
    Normal = 2,
    /// Elevated priority scheduled ahead of `Normal`.
    High = 3,
    /// Immediate top-of-queue priority.
    Critical = 4,
}

impl Priority {
    /// Numeric priority weight (`1..=4`) for SQLite storage.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Restores [`Priority`] from its numeric weight (`<=1` -> `Low`, `2` -> `Normal`, `3` -> `High`, `>=4` -> `Critical`).
    #[must_use]
    pub const fn from_i32(val: i32) -> Self {
        match val {
            ..=1 => Self::Low,
            2 => Self::Normal,
            3 => Self::High,
            4.. => Self::Critical,
        }
    }
}

/// Lifecycle Unix millisecond timestamps attached to a `Download`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Timestamps {
    /// Creation Unix timestamp (ms).
    pub created_at_ms: i64,
    /// First active byte transfer timestamp (ms).
    pub started_at_ms: Option<i64>,
    /// Last state mutation timestamp (ms).
    pub updated_at_ms: i64,
    /// Completion Unix timestamp (ms).
    pub completed_at_ms: Option<i64>,
}

impl Timestamps {
    /// Initializes timestamps at `now_ms`.
    #[must_use]
    pub const fn new(now_ms: i64) -> Self {
        Self {
            created_at_ms: now_ms,
            started_at_ms: None,
            updated_at_ms: now_ms,
            completed_at_ms: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_enforces_start_lt_end_even_via_serde() {
        assert!(ByteRange::new(10, 10).is_err());
        assert!(ByteRange::new(20, 10).is_err());

        let r = ByteRange::new(100, 200).unwrap();
        assert_eq!(r.len(), 100);
        assert_eq!(r.last_inclusive(), 199);
        assert_eq!(r.to_http_range_header(), "bytes=100-199");
        assert!(r.contains_offset(100));
        assert!(!r.contains_offset(200));
        assert_eq!(r.midpoint(), Some(150));

        let corrupted_json = r#"{"start":500,"end":500}"#;
        assert!(serde_json::from_str::<ByteRange>(corrupted_json).is_err());
    }

    #[test]
    fn rfc9110_if_range_never_emits_weak_etag() {
        let weak_only = ResumeValidators {
            etag: EtagValidator::parse(r#"W/"nginx-weak-1""#),
            last_modified: Some(LastModifiedValidator {
                raw_http_date: "Wed, 25 Nov 2026 22:00:00 GMT".to_owned(),
                last_modified_epoch_secs: Some(1_000),
                response_date_epoch_secs: Some(1_000), // delta = 0s < 1s -> NOT strong!
            }),
            content_length: Some(4096),
        };
        assert_eq!(
            weak_only.select_if_range(),
            IfRangeDirective::ManualValidate206
        );

        let weak_etag_with_strong_lm = ResumeValidators {
            etag: EtagValidator::parse(r#"w/"cf-tag""#),
            last_modified: Some(LastModifiedValidator {
                raw_http_date: "Wed, 25 Nov 2026 22:00:00 GMT".to_owned(),
                last_modified_epoch_secs: Some(1_000),
                response_date_epoch_secs: Some(1_005), // delta = 5s >= 1s -> strong!
            }),
            content_length: Some(4096),
        };
        assert_eq!(
            weak_etag_with_strong_lm.select_if_range(),
            IfRangeDirective::SendHeader("Wed, 25 Nov 2026 22:00:00 GMT".to_owned())
        );

        let strong_etag = ResumeValidators {
            etag: EtagValidator::parse(r#""strong-v1""#),
            last_modified: None,
            content_length: Some(4096),
        };
        assert_eq!(
            strong_etag.select_if_range(),
            IfRangeDirective::SendHeader(r#""strong-v1""#.to_owned())
        );

        let mutated = ResumeValidators {
            etag: EtagValidator::parse(r#""strong-v2""#),
            last_modified: None,
            content_length: Some(4096),
        };
        let err = strong_etag.validate_on_resume(&mutated).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ResResourceChanged);
    }

    #[test]
    fn checksum_parses_and_compares_case_insensitively() {
        let hex64 = "a".repeat(64);
        let cs = Checksum::parse_prefixed(&format!("SHA256:{hex64}")).unwrap();
        assert_eq!(cs.algorithm, HashAlgorithm::Sha256);
        assert!(cs.matches_hex(&"A".repeat(64)));
        assert!(!cs.matches_hex(&"b".repeat(64)));
        assert!(Checksum::parse_prefixed("sha256:nothex").is_err());
    }

    #[test]
    fn ids_priorities_and_worker_markers_behave_as_specified() {
        let d1 = DownloadId::new();
        let parsed: DownloadId = d1.to_string().parse().unwrap();
        assert_eq!(d1, parsed);
        assert!(DownloadId::parse("bad").is_err());

        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
        assert_eq!(Priority::from_i32(Priority::High.as_i32()), Priority::High);

        assert_eq!(WorkerId::new(1).as_bin_marker(), 2);
        assert_eq!(WorkerId::new(32).as_bin_marker(), 33);
        assert_eq!(WorkerId::new(33).as_bin_marker(), 2);
    }
}
