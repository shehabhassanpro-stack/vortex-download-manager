//! Deterministic fault-injection registry (`Failpoints` — `P1-T12` / `ADR-018`).
//!
//! Uses a single `Relaxed` atomic fast-path counter (`ACTIVE_FAILPOINTS == 0`) so production hot
//! paths pay zero lock contention when no failpoints are armed.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashMap;
use vortex_error::{ErrorCode, TraceId, VortexError};

/// Behavior triggered when execution hits an armed failpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailAction {
    /// Fail every hit with `ErrorCode`.
    AlwaysError(ErrorCode),
    /// Fail the next `remaining` hits with `ErrorCode`, then auto-disarm.
    CountedError {
        /// Error code to return.
        code: ErrorCode,
        /// Remaining failures before disarming.
        remaining: u32,
    },
}

static ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGISTRY: std::sync::LazyLock<Mutex<FxHashMap<&'static str, FailAction>>> =
    std::sync::LazyLock::new(|| Mutex::new(FxHashMap::default()));

/// Arms a named failpoint (`e.g. "io.rename_sharing_violation"`).
pub fn configure_failpoint(name: &'static str, action: FailAction) {
    if let Ok(mut map) = REGISTRY.lock() {
        let was_present = map.insert(name, action).is_some();
        if !was_present {
            ACTIVE_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Disarms a specific failpoint.
pub fn remove_failpoint(name: &'static str) {
    if let Ok(mut map) = REGISTRY.lock()
        && map.remove(name).is_some()
    {
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Clears all armed failpoints.
pub fn clear_all_failpoints() {
    if let Ok(mut map) = REGISTRY.lock() {
        map.clear();
        ACTIVE_COUNT.store(0, Ordering::SeqCst);
    }
}

/// Evaluates failpoint `name` (`O(1)` single atomic load when no failpoints are armed).
///
/// # Errors
/// Returns the configured [`VortexError`] when `name` is armed.
pub fn check_failpoint(name: &'static str, trace_id: TraceId) -> Result<(), VortexError> {
    if ACTIVE_COUNT.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }
    let Ok(mut map) = REGISTRY.lock() else {
        return Ok(());
    };
    let mut should_remove = false;
    let triggered_code = match map.get_mut(name) {
        Some(FailAction::AlwaysError(code)) => Some(*code),
        Some(FailAction::CountedError { code, remaining }) => {
            if *remaining > 0 {
                let c = *code;
                *remaining -= 1;
                if *remaining == 0 {
                    should_remove = true;
                }
                Some(c)
            } else {
                should_remove = true;
                None
            }
        }
        None => None,
    };

    if should_remove && map.remove(name).is_some() {
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
    }

    if let Some(code) = triggered_code {
        return Err(VortexError::coded(
            code,
            trace_id,
            format!("Failpoint `{name}` triggered"),
        ));
    }
    Ok(())
}
