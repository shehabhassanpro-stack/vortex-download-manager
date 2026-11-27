//! Contract-first IPC protocol crate (`crates/vortex-ipc` — Level 3 Transport, `P0-T13`, `P0-T14`, `P1-T24`).
//!
//! Split via Cargo features (`client` vs `server`) without pulling `vortex-engine` or `SQLite`,
//! ensuring `vortex-bridge.exe` remains `< 1 MB` (`ADR-005` / `ADR-010`).

#![forbid(unsafe_code)]

pub mod client;
mod framing;
pub mod json_bridge;
mod rate_limit;
#[cfg(feature = "server")]
pub mod server;
mod validation;

/// Generated `vortex.ipc.v1` Protobuf wire types from `proto/vortex_ipc.proto`.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used
)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/vortex.ipc.v1.rs"));
}

pub use client::{SyncPipeClient, recv_envelope_from, send_envelope_to};
pub use framing::{
    MAX_NATIVE_HOST_TO_EXT_BYTES, MAX_PIPE_FRAME_BYTES, MIN_SUPPORTED_PROTOCOL_VERSION,
    PROTOCOL_VERSION, decode_frame, encode_frame, negotiate_capabilities, negotiate_version,
    proto_error_from_api, proto_error_to_api,
};
pub use json_bridge::{
    json_bytes_to_proto_envelope, proto_envelope_to_json_bytes, read_native_json_frame,
    write_native_json_frame,
};
pub use rate_limit::{DEFAULT_IPC_BURST_CAPACITY, DEFAULT_IPC_RATE_LIMIT_PER_SEC, IpcRateLimiter};
#[cfg(feature = "server")]
pub use server::{
    CORE_CAPABILITIES, MAX_CONCURRENT_PIPE_CLIENTS, MAX_CONSECUTIVE_RATE_LIMIT_VIOLATIONS,
    PipeServerInstanceFactory, proto_capture_rules_from_config, serve_ipc_stream, spawn_ipc_server,
};
pub use validation::{
    MAX_COOKIES_BYTES, MAX_ENVELOPE_ID_BYTES, MAX_HEADER_COUNT, MAX_HEADER_NAME_BYTES,
    MAX_HEADER_VALUE_BYTES, MAX_SUGGESTED_NAME_BYTES, MAX_URL_BYTES, validate_download_handoff,
    validate_envelope_header,
};
