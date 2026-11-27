//! Actor-based download engine (`crates/vortex-engine` — Level 3, `P1-T18` → `P1-T20`).
//!
//! Zero `unsafe` code (`#![forbid(unsafe_code)]`), zero shared mutable download state, bounded MPSC
//! backpressure, Bulkhead panic isolation (`JoinError::is_panic`), Hybrid Adaptive Bounded Chunk
//! Leases (`100%` HTTP/1.1 Keep-Alive reuse + Work Stealing), and Two-Tier Crash-Safe Checkpoints.

#![forbid(unsafe_code)]

pub mod actors;
pub mod host_capability;
pub mod supervisor;
pub mod worker;

pub use actors::{
    BroadcastEventBus, PersistenceHandle, PersistenceMsg, PriorityQueueScheduler,
    build_sidecar_snapshot,
};
pub use host_capability::{
    DEFAULT_HOST_CAPABILITY_TTL_MS, HALF_OPEN_PROBE_INTERVAL_MS, HostCapabilityCache,
    MAX_HOST_CAPABILITY_ENTRIES, SmartHostProfileCache,
};
pub use supervisor::EngineSupervisor;
pub use worker::{ActorControlMsg, EngineContext, MidCheckpointHook, run_download_actor};

/// Layer identifier used by Composition Root smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_application::layer_name();
    "vortex-engine"
}
