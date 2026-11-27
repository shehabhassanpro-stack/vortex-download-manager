//! Binary length-delimited Protobuf framing, capability negotiation, and `ApiError` conversion.

use std::collections::{BTreeMap, BTreeSet};

use prost::Message;
use vortex_error::{ApiError, ErrorCode, Recoverability, SuggestedAction, TraceId, VortexError};

use crate::proto;

/// Current IPC wire protocol version (`Protocol-Spec.md` §2.2).
pub const PROTOCOL_VERSION: u32 = 1;

/// Oldest IPC wire protocol version supported (`N / N-1 / N-2` policy).
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// Maximum Named Pipe / UDS frame payload size (`4 MiB`).
pub const MAX_PIPE_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Maximum Native Messaging Host → Extension payload size (`1 MiB` browser hard cap).
pub const MAX_NATIVE_HOST_TO_EXT_BYTES: usize = 1024 * 1024;

/// Encodes a [`proto::Envelope`] with a 4-byte little-endian length prefix.
///
/// # Errors
/// Returns `VTX-IPC-003` if the encoded length is `0` or exceeds `max_frame_bytes`.
pub fn encode_frame(
    envelope: &proto::Envelope,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, VortexError> {
    let trace_id = envelope
        .trace_id
        .parse::<TraceId>()
        .unwrap_or_else(|_| TraceId::new());
    let payload_len = envelope.encoded_len();

    if payload_len == 0 || payload_len > max_frame_bytes {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!(
                "outbound IPC frame size {payload_len} out of allowed bounds [1, {max_frame_bytes}]"
            ),
        ));
    }

    let len_u32 = u32::try_from(payload_len).map_err(|_| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            "outbound IPC frame length exceeds u32::MAX",
        )
    })?;

    let mut out = Vec::with_capacity(4 + payload_len);
    out.extend_from_slice(&len_u32.to_le_bytes());
    envelope.encode(&mut out).map_err(|err| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!("protobuf encode error: {err}"),
        )
    })?;
    Ok(out)
}

/// Decodes a length-prefixed [`proto::Envelope`] while enforcing strict `DoS` frame bounds.
///
/// # Errors
/// Returns `VTX-IPC-003` if the frame header is truncated, length is `0` or exceeds
/// `max_frame_bytes`, payload length mismatches, or protobuf decoding fails.
pub fn decode_frame(framed: &[u8], max_frame_bytes: usize) -> Result<proto::Envelope, VortexError> {
    let trace_id = TraceId::new();
    let header_slice = framed.get(..4).ok_or_else(|| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            "IPC frame shorter than 4-byte length prefix",
        )
    })?;
    let header_arr: [u8; 4] = header_slice.try_into().map_err(|_| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            "invalid 4-byte IPC length header",
        )
    })?;
    let declared_len = u32::from_le_bytes(header_arr) as usize;

    if declared_len == 0 || declared_len > max_frame_bytes {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!(
                "inbound IPC frame size {declared_len} out of allowed bounds [1, {max_frame_bytes}]"
            ),
        ));
    }

    let payload = framed.get(4..).unwrap_or_default();
    if payload.len() != declared_len {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!(
                "IPC frame length mismatch: header declared {declared_len} bytes, got {}",
                payload.len()
            ),
        ));
    }

    proto::Envelope::decode(payload).map_err(|err| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!("protobuf decode error: {err}"),
        )
    })
}

/// Negotiates the highest mutually supported IPC protocol version (`Protocol-Spec.md` §3).
///
/// # Errors
/// Returns `VTX-IPC-002` (`IpcVersionMismatch`) if the version ranges do not overlap.
pub fn negotiate_version(
    client_min: u32,
    client_max: u32,
    core_min: u32,
    core_max: u32,
    trace_id: TraceId,
) -> Result<u32, VortexError> {
    let highest_common = client_max.min(core_max);
    let lowest_common = client_min.max(core_min);
    if client_min > client_max || core_min > core_max || highest_common < lowest_common {
        return Err(VortexError::coded(
            ErrorCode::IpcVersionMismatch,
            trace_id,
            format!(
                "incompatible IPC protocol ranges: client=[{client_min}..={client_max}], core=[{core_min}..={core_max}]"
            ),
        ));
    }
    Ok(highest_common)
}

/// Computes the deterministic sorted intersection of client and core capability sets.
#[must_use]
pub fn negotiate_capabilities(client_caps: &[String], core_caps: &[&str]) -> Vec<String> {
    let core_set: BTreeSet<&str> = core_caps.iter().copied().collect();
    let mut matched = BTreeSet::new();
    for cap in client_caps {
        if core_set.contains(cap.as_str()) {
            matched.insert(cap.clone());
        }
    }
    matched.into_iter().collect()
}

/// Converts a domain [`ApiError`] into its Protobuf wire representation.
#[must_use]
pub fn proto_error_from_api(err: &ApiError) -> proto::ApiError {
    proto::ApiError {
        code: err.code.as_str().to_owned(),
        category: err.category.as_str().to_owned(),
        recoverability: err.recoverability.kind_str().to_owned(),
        retry_after_ms: err.recoverability.retry_after_ms(),
        user_message_key: err.user_message_key.clone(),
        message_params: err.message_params.clone(),
        suggested_actions: err
            .suggested_actions
            .iter()
            .map(|a| a.as_str().to_owned())
            .collect(),
        technical_detail: err.technical_detail.clone(),
        trace_id: err.trace_id.to_string(),
    }
}

/// Converts a Protobuf [`proto::ApiError`] back into a typed [`ApiError`].
///
/// # Errors
/// Returns `VTX-IPC-003` if required enum discriminants or `trace_id` are invalid.
pub fn proto_error_to_api(wire: &proto::ApiError) -> Result<ApiError, VortexError> {
    let trace_id = wire
        .trace_id
        .parse::<TraceId>()
        .unwrap_or_else(|_| TraceId::new());
    let code: ErrorCode = wire.code.parse().map_err(|err| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            trace_id,
            format!("invalid ApiError.code: {err}"),
        )
    })?;
    let recoverability = Recoverability::from_wire(&wire.recoverability, wire.retry_after_ms)
        .map_err(|err| {
            VortexError::coded(
                ErrorCode::IpcFrameInvalid,
                trace_id,
                format!("invalid ApiError.recoverability: {err}"),
            )
        })?;
    let mut actions = Vec::with_capacity(wire.suggested_actions.len());
    for raw_action in &wire.suggested_actions {
        if let Ok(parsed) = raw_action.parse::<SuggestedAction>() {
            actions.push(parsed);
        }
    }
    let message_params: BTreeMap<String, String> = wire.message_params.clone();

    Ok(ApiError {
        code,
        category: code.category(),
        recoverability,
        user_message_key: wire.user_message_key.clone(),
        message_params,
        suggested_actions: actions,
        technical_detail: wire.technical_detail.clone(),
        trace_id,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn framing_rejects_zero_truncated_mismatched_corrupt_and_oversized_frames() {
        assert_eq!(
            decode_frame(&[1_u8, 2], MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );

        let zero = [0_u8; 4];
        assert_eq!(
            decode_frame(&zero, MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );

        let too_large = (u32::try_from(MAX_PIPE_FRAME_BYTES).unwrap() + 1).to_le_bytes();
        assert_eq!(
            decode_frame(&too_large, MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );

        let mut mismatch = Vec::from(10_u32.to_le_bytes());
        mismatch.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(
            decode_frame(&mismatch, MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );

        let mut corrupt = Vec::from(2_u32.to_le_bytes());
        corrupt.extend_from_slice(&[0xFF, 0xFF]);
        assert_eq!(
            decode_frame(&corrupt, MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );

        let empty = proto::Envelope::default();
        assert_eq!(
            encode_frame(&empty, MAX_PIPE_FRAME_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::IpcFrameInvalid
        );
    }

    #[test]
    fn handshake_version_and_capability_negotiation() {
        let tid = TraceId::new();
        assert_eq!(negotiate_version(1, 3, 1, 2, tid).unwrap(), 2);
        assert_eq!(
            negotiate_version(3, 4, 1, 2, tid).unwrap_err().code(),
            ErrorCode::IpcVersionMismatch
        );

        let caps = negotiate_capabilities(
            &[
                "handoff".to_owned(),
                "future-v9".to_owned(),
                "media".to_owned(),
            ],
            &["handoff", "media", "diag-dump"],
        );
        assert_eq!(caps, vec!["handoff", "media"]);
    }

    #[test]
    fn api_error_round_trips_through_proto() {
        let tid = TraceId::new();
        let original = ApiError::new(ErrorCode::NetRateLimited, tid)
            .with_recoverability(Recoverability::Transient {
                retry_after_ms: Some(5_000),
            })
            .with_param("host", "cdn.example.com")
            .with_technical_detail("429 Too Many Requests");

        let wire = proto_error_from_api(&original);
        let decoded = proto_error_to_api(&wire).unwrap();
        assert_eq!(decoded, original);

        let mut bad_code = wire.clone();
        bad_code.code = "INVALID".to_owned();
        assert_eq!(
            proto_error_to_api(&bad_code).unwrap_err().code(),
            ErrorCode::IpcFrameInvalid
        );

        let mut bad_rec = wire;
        bad_rec.recoverability = "UNKNOWN".to_owned();
        assert_eq!(
            proto_error_to_api(&bad_rec).unwrap_err().code(),
            ErrorCode::IpcFrameInvalid
        );
    }
}
