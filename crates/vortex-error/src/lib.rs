//! Unified error taxonomy for Vortex Download Manager (`crates/vortex-error` — Level 0).
//!
//! Zero internal workspace dependencies (`ADR-015`). Every error crossing process, IPC, or UI
//! boundaries carries:
//! - A permanent, version-stable [`ErrorCode`] (`VTX-<CAT>-<NNN>`)
//! - An [`ErrorCategory`] (9 canonical domains)
//! - Explicit [`Recoverability`] guidance
//! - A [`TraceId`] (`UUIDv7`) correlating logs across Extension, Bridge, Core, and UI
//! - Actionable [`SuggestedAction`]s and an i18n message key without leaking secrets.

#![forbid(unsafe_code)]

mod api_error;
mod codes;
mod trace_id;

pub use api_error::{ApiError, VortexError};
pub use codes::{ErrorCategory, ErrorCode, Recoverability, SuggestedAction};
pub use trace_id::TraceId;
