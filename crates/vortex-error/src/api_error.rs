//! Boundary error contract (`ApiError`) and core [`VortexError`] wrapper.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::codes::{ErrorCategory, ErrorCode, Recoverability, SuggestedAction};
use crate::trace_id::TraceId;

/// Unified, serializable error contract crossing IPC, Tauri, and UI boundaries (`ADR-015` / `P1.6-T01`).
///
/// Serializes canonically in `camelCase` so Channel A (Native Messaging) and Channel C (Tauri IPC)
/// share identical field names, while accepting legacy `snake_case` aliases when loading persisted SQLite rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    /// Permanent error code (e.g. `VTX-DISK-001`).
    pub code: ErrorCode,
    /// Domain category matching `code`.
    pub category: ErrorCategory,
    /// Recovery classification guiding retries vs. user prompts.
    pub recoverability: Recoverability,
    /// Localization lookup key (e.g. `"errors.disk.insufficient_space"`).
    #[serde(alias = "user_message_key")]
    pub user_message_key: String,
    /// Safe interpolation parameters for i18n formatting (never secrets).
    #[serde(default, alias = "message_params")]
    pub message_params: BTreeMap<String, String>,
    /// Actionable remediation buttons shown to the user.
    #[serde(default, alias = "suggested_actions")]
    pub suggested_actions: Vec<SuggestedAction>,
    /// Sanitized technical details revealed inside the expandable "Details" section.
    #[serde(default, alias = "technical_detail")]
    pub technical_detail: String,
    /// End-to-end trace identifier correlating logs across all 4 processes.
    #[serde(alias = "trace_id")]
    pub trace_id: TraceId,
}

impl ApiError {
    /// Creates an [`ApiError`] pre-populated with canonical defaults for `code`.
    #[must_use]
    pub fn new(code: ErrorCode, trace_id: TraceId) -> Self {
        Self {
            code,
            category: code.category(),
            recoverability: code.default_recoverability(),
            user_message_key: code.default_message_key().to_owned(),
            message_params: BTreeMap::new(),
            suggested_actions: code.default_actions(),
            technical_detail: String::new(),
            trace_id,
        }
    }

    /// Overrides the recoverability classification (e.g. attaching `retry_after_ms` from `429`).
    #[must_use]
    pub const fn with_recoverability(mut self, recoverability: Recoverability) -> Self {
        self.recoverability = recoverability;
        self
    }

    /// Inserts a localized message interpolation parameter.
    #[must_use]
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.message_params.insert(key.into(), value.into());
        self
    }

    /// Sets the sanitized technical diagnostic detail.
    #[must_use]
    pub fn with_technical_detail(mut self, detail: impl Into<String>) -> Self {
        self.technical_detail = detail.into();
        self
    }

    /// Replaces the suggested user actions.
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<SuggestedAction>) -> Self {
        self.suggested_actions = actions;
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.technical_detail.is_empty() {
            write!(
                f,
                "[{}] {} (trace_id={})",
                self.code, self.user_message_key, self.trace_id
            )
        } else {
            write!(
                f,
                "[{}] {} — {} (trace_id={})",
                self.code, self.user_message_key, self.technical_detail, self.trace_id
            )
        }
    }
}

/// Canonical error type returned by Vortex application and infrastructure layers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{api}")]
pub struct VortexError {
    api: Box<ApiError>,
}

impl VortexError {
    /// Creates a [`VortexError`] from an [`ApiError`].
    #[must_use]
    pub fn new(api: ApiError) -> Self {
        Self { api: Box::new(api) }
    }

    /// Convenience constructor for a coded error with trace ID and sanitized detail.
    #[must_use]
    pub fn coded(code: ErrorCode, trace_id: TraceId, detail: impl Into<String>) -> Self {
        Self::new(ApiError::new(code, trace_id).with_technical_detail(detail))
    }

    /// Accesses the underlying [`ApiError`] contract.
    #[must_use]
    pub fn as_api_error(&self) -> &ApiError {
        &self.api
    }

    /// Consumes `self` and yields the owned [`ApiError`].
    #[must_use]
    pub fn into_api_error(self) -> ApiError {
        *self.api
    }

    /// Returns the stable [`ErrorCode`].
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.api.code
    }

    /// Returns the [`TraceId`].
    #[must_use]
    pub const fn trace_id(&self) -> TraceId {
        self.api.trace_id
    }
}

impl From<ApiError> for VortexError {
    fn from(api: ApiError) -> Self {
        Self::new(api)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn api_error_builder_and_json_round_trip() {
        let trace = TraceId::new();
        let err = ApiError::new(ErrorCode::DiskInsufficientSpace, trace)
            .with_param("drive", "D:")
            .with_param("needed", "4.2 GB")
            .with_technical_detail("GetDiskFreeSpaceExW returned 128 MiB free");

        assert_eq!(err.code, ErrorCode::DiskInsufficientSpace);
        assert_eq!(err.category, ErrorCategory::Disk);
        assert_eq!(err.recoverability, Recoverability::UserActionRequired);
        assert_eq!(err.trace_id, trace);
        assert!(
            err.suggested_actions
                .contains(&SuggestedAction::ChangeFolder)
        );

        let json = serde_json::to_string(&err).unwrap();
        assert!(
            json.contains("\"suggestedActions\""),
            "must emit camelCase: {json}"
        );
        assert!(json.contains("\"technicalDetail\""));
        assert!(json.contains("\"traceId\""));
        let decoded: ApiError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, err);

        // Backward compatibility: legacy snake_case SQLite rows still deserialize cleanly!
        let legacy_json = format!(
            r#"{{"code":"VTX-RES-002","category":"Resource","recoverability":{{"kind":"UserActionRequired"}},"user_message_key":"errors.res.link_expired","message_params":{{}},"suggested_actions":["RefreshLink"],"technical_detail":"old row","trace_id":"{trace}"}}"#
        );
        let decoded_legacy: ApiError = serde_json::from_str(&legacy_json).unwrap();
        assert_eq!(decoded_legacy.code, ErrorCode::ResLinkExpired);
        assert_eq!(
            decoded_legacy.suggested_actions,
            vec![SuggestedAction::RefreshLink]
        );

        let wrapped = VortexError::from(err.clone());
        assert_eq!(wrapped.code(), ErrorCode::DiskInsufficientSpace);
        assert_eq!(wrapped.trace_id(), trace);
        assert_eq!(wrapped.as_api_error(), &err);
        assert!(wrapped.to_string().contains("VTX-DISK-001"));
        assert_eq!(wrapped.into_api_error(), err);

        let no_detail = ApiError::new(ErrorCode::ResNotFound, trace)
            .with_actions(vec![SuggestedAction::InspectUrl]);
        assert!(no_detail.to_string().contains("VTX-RES-001"));
        let coded = VortexError::coded(ErrorCode::IpcFrameInvalid, trace, "frame too big");
        assert_eq!(coded.code(), ErrorCode::IpcFrameInvalid);

        #[cfg(feature = "specta")]
        {
            let mut types = specta::Types::default();
            let _api_dt = <ApiError as specta::Type>::definition(&mut types);
            let _code_dt = <ErrorCode as specta::Type>::definition(&mut types);
            let _cat_dt = <ErrorCategory as specta::Type>::definition(&mut types);
            let _rec_dt = <Recoverability as specta::Type>::definition(&mut types);
            let _act_dt = <SuggestedAction as specta::Type>::definition(&mut types);
            let _tid_dt = <TraceId as specta::Type>::definition(&mut types);
        }
    }

    #[test]
    fn transient_error_carries_retry_after() {
        let trace = TraceId::new();
        let err = ApiError::new(ErrorCode::NetRateLimited, trace).with_recoverability(
            Recoverability::Transient {
                retry_after_ms: Some(30_000),
            },
        );
        assert!(err.recoverability.is_retryable());
        assert_eq!(err.recoverability.retry_after_ms(), Some(30_000));
    }
}
