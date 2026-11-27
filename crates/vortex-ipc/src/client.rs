//! Lightweight synchronous Named Pipe client (`client` feature — zero Tokio/SQLite/Engine deps so
//! `vortex-bridge.exe` compiles to `< 1 MB`, `P1-T24` & `P1-T25`).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use prost::Message;
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::framing::{MAX_PIPE_FRAME_BYTES, encode_frame};
use crate::proto;

/// Encodes and writes `envelope` (`4B LE + Protobuf`, `<= 4 MiB`) into any [`Write`] stream.
///
/// # Errors
/// Returns `VTX-IPC-003` or `VTX-IPC-001` on framing or write error.
pub fn send_envelope_to<W: Write>(
    writer: &mut W,
    envelope: &proto::Envelope,
) -> Result<(), VortexError> {
    let framed = encode_frame(envelope, MAX_PIPE_FRAME_BYTES)?;
    let tid = envelope
        .trace_id
        .parse::<TraceId>()
        .unwrap_or_else(|_| TraceId::new());
    writer.write_all(&framed).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            tid,
            format!("Named Pipe write failed: {e}"),
        )
    })
}

/// Reads and decodes the next `proto::Envelope` from any [`Read`] stream (`Ok(None)` on clean EOF).
///
/// # Errors
/// Returns `VTX-IPC-003` on oversized/malformed frame or `VTX-IPC-001` on broken pipe.
pub fn recv_envelope_from<R: Read>(reader: &mut R) -> Result<Option<proto::Envelope>, VortexError> {
    let mut hdr = [0_u8; 4];
    match reader.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(VortexError::coded(
                ErrorCode::IpcCoreUnreachable,
                TraceId::new(),
                format!("Named Pipe header read failed: {e}"),
            ));
        }
    }

    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 || len > MAX_PIPE_FRAME_BYTES {
        return Err(VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("Named Pipe frame length {len} outside [1, {MAX_PIPE_FRAME_BYTES}]"),
        ));
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("Named Pipe payload truncated: {e}"),
        )
    })?;

    let decoded = proto::Envelope::decode(payload.as_slice()).map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcFrameInvalid,
            TraceId::new(),
            format!("Protobuf decode error on Named Pipe: {e}"),
        )
    })?;
    Ok(Some(decoded))
}

/// Synchronous framed Protobuf client connected to `\\.\pipe\vortex-core-<USER_SID>`.
#[derive(Debug)]
pub struct SyncPipeClient {
    file: File,
}

impl SyncPipeClient {
    /// Connects to `pipe_path` with bounded retry/backoff up to `timeout` (handling `ERROR_PIPE_BUSY = 231`
    /// and waiting for `vortex.exe --background` cold start `< 300ms`).
    ///
    /// # Errors
    /// Returns `VTX-IPC-001` (`IpcCoreUnreachable`) if the pipe cannot be opened within `timeout`.
    pub fn connect_with_timeout(
        pipe_path: &str,
        timeout: Duration,
        trace_id: TraceId,
    ) -> Result<Self, VortexError> {
        let deadline = Instant::now() + timeout;

        loop {
            match OpenOptions::new().read(true).write(true).open(pipe_path) {
                Ok(file) => return Ok(Self { file }),
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(VortexError::coded(
                            ErrorCode::IpcCoreUnreachable,
                            trace_id,
                            format!("Could not connect to Named Pipe {pipe_path}: {err}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }

    /// Clones the underlying OS Named Pipe handle.
    ///
    /// # Errors
    /// Returns `VTX-IPC-001` if `try_clone` fails.
    pub fn try_clone(&self, trace_id: TraceId) -> Result<Self, VortexError> {
        let cloned = self.file.try_clone().map_err(|e| {
            VortexError::coded(
                ErrorCode::IpcCoreUnreachable,
                trace_id,
                format!("Named Pipe handle clone failed: {e}"),
            )
        })?;
        Ok(Self { file: cloned })
    }

    /// Encodes and writes `envelope` (`4B LE + Protobuf`, `<= 4 MiB`) to the Named Pipe.
    ///
    /// # Errors
    /// Returns `VTX-IPC-003` or `VTX-IPC-001` on framing or write error.
    pub fn send_envelope(&mut self, envelope: &proto::Envelope) -> Result<(), VortexError> {
        send_envelope_to(&mut self.file, envelope)
    }

    /// Reads and decodes the next `proto::Envelope` from the Named Pipe (`Ok(None)` on clean EOF).
    ///
    /// # Errors
    /// Returns `VTX-IPC-003` on oversized/malformed frame or `VTX-IPC-001` on broken pipe.
    pub fn recv_envelope(&mut self) -> Result<Option<proto::Envelope>, VortexError> {
        recv_envelope_from(&mut self.file)
    }
}
