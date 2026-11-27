//! Lightweight Native Messaging Host (`vortex-bridge.exe` — `< 1 MB`, `P1-T25` / `ADR-005` / `ADR-006`).
//!
//! Responsibilities:
//! 1. Uses Microsoft Research's [`mimalloc`] global allocator.
//! 2. Writes structured redacted JSONL events to independent rotating `%LOCALAPPDATA%\Vortex\logs\bridge.jsonl`
//!    (never writing diagnostic text to `stdout`, which is reserved exclusively for Chromium Native Messaging!).
//! 3. Resolves `<USER_SID>` from the current Windows process token and connects to `\\.\pipe\vortex-core-<USER_SID>`
//!    using **`FILE_FLAG_OVERLAPPED`** with independent reader/writer Win32 events so `ReadFile` and `WriteFile`
//!    execute full-duplex simultaneously without Win32 `FO_SYNCHRONOUS_IO` lock contention!
//! 4. Automatically spawns `vortex.exe --background` (`CREATE_NO_WINDOW | DETACHED_PROCESS`, zero shell!)
//!    if `Vortex Core` is not yet running.
//! 5. Translates Channel A (`stdin`/`stdout` 4B LE UTF-8 JSON) ↔ Channel B (`Named Pipe` 4B LE Protobuf v3)
//!    bidirectionally on two OS threads while propagating `trace_id`!

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimalloc::MiMalloc;
use serde_json::json;
use vortex_error::{ApiError, ErrorCode, TraceId, VortexError};
use vortex_ipc::{
    json_bytes_to_proto_envelope, proto, proto_envelope_to_json_bytes, proto_error_from_api,
    read_native_json_frame, recv_envelope_from, send_envelope_to, write_native_json_frame,
};
use vortex_observability::{RotatingFileConfig, SizeRotatingFile, redact_sensitive_text};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, FALSE, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, TRUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, DETACHED_PROCESS, GetCurrentProcess, OpenProcessToken,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Shared RAII Win32 Named Pipe `HANDLE` opened with `FILE_FLAG_OVERLAPPED` for true full-duplex I/O.
struct SharedPipeHandle(isize);

impl Drop for SharedPipeHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE as isize {
            // SAFETY: Closes owned Named Pipe handle once all reader/writer references drop.
            unsafe {
                let _ = CloseHandle(self.0 as HANDLE);
            }
        }
    }
}

struct OverlappedPipeReader {
    pipe: Arc<SharedPipeHandle>,
    event: isize,
}

impl Drop for OverlappedPipeReader {
    fn drop(&mut self) {
        if self.event != 0 {
            // SAFETY: Closes manual-reset Win32 event handle owned by this reader.
            unsafe {
                let _ = CloseHandle(self.event as HANDLE);
            }
        }
    }
}

impl Read for OverlappedPipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let max_len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: Zeroed `OVERLAPPED` with dedicated manual-reset `hEvent` for concurrent full-duplex read.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event as HANDLE;

        let mut transferred = 0_u32;
        // SAFETY: `self.pipe` is a valid `FILE_FLAG_OVERLAPPED` pipe handle and `buf` is valid for `max_len` bytes.
        let ok = unsafe {
            ReadFile(
                self.pipe.0 as HANDLE,
                buf.as_mut_ptr().cast(),
                max_len,
                &raw mut transferred,
                &raw mut ov,
            )
        };
        if ok == FALSE {
            // SAFETY: Reads thread-local Win32 `LastError`.
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                // SAFETY: Waits on `ov.hEvent` until this specific read completes.
                let wait_ok = unsafe {
                    GetOverlappedResult(
                        self.pipe.0 as HANDLE,
                        &raw const ov,
                        &raw mut transferred,
                        TRUE,
                    )
                };
                if wait_ok == FALSE {
                    // SAFETY: Reads thread-local Win32 `LastError`.
                    let wait_err = unsafe { GetLastError() };
                    if wait_err == ERROR_BROKEN_PIPE || wait_err == ERROR_HANDLE_EOF {
                        return Ok(0);
                    }
                    return Err(io::Error::from_raw_os_error(
                        i32::try_from(wait_err).unwrap_or(1),
                    ));
                }
            } else if err == ERROR_BROKEN_PIPE || err == ERROR_HANDLE_EOF {
                return Ok(0);
            } else {
                return Err(io::Error::from_raw_os_error(
                    i32::try_from(err).unwrap_or(1),
                ));
            }
        }
        Ok(transferred as usize)
    }
}

struct OverlappedPipeWriter {
    pipe: Arc<SharedPipeHandle>,
    event: isize,
}

impl Drop for OverlappedPipeWriter {
    fn drop(&mut self) {
        if self.event != 0 {
            // SAFETY: Closes manual-reset Win32 event handle owned by this writer.
            unsafe {
                let _ = CloseHandle(self.event as HANDLE);
            }
        }
    }
}

impl Write for OverlappedPipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: Zeroed `OVERLAPPED` with dedicated manual-reset `hEvent` for concurrent full-duplex write.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event as HANDLE;

        let mut written = 0_u32;
        // SAFETY: `self.pipe` is a valid `FILE_FLAG_OVERLAPPED` pipe handle and `buf` is valid for `len` bytes.
        let ok = unsafe {
            WriteFile(
                self.pipe.0 as HANDLE,
                buf.as_ptr().cast(),
                len,
                &raw mut written,
                &raw mut ov,
            )
        };
        if ok == FALSE {
            // SAFETY: Reads thread-local Win32 `LastError`.
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                // SAFETY: Waits on `ov.hEvent` until this specific write completes.
                let wait_ok = unsafe {
                    GetOverlappedResult(
                        self.pipe.0 as HANDLE,
                        &raw const ov,
                        &raw mut written,
                        TRUE,
                    )
                };
                if wait_ok == FALSE {
                    // SAFETY: Reads thread-local Win32 `LastError`.
                    let wait_err = unsafe { GetLastError() };
                    return Err(io::Error::from_raw_os_error(
                        i32::try_from(wait_err).unwrap_or(1),
                    ));
                }
            } else {
                return Err(io::Error::from_raw_os_error(
                    i32::try_from(err).unwrap_or(1),
                ));
            }
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn open_overlapped_pipe_pair(
    pipe_path: &str,
    timeout: Duration,
    trace_id: TraceId,
) -> Result<(OverlappedPipeReader, OverlappedPipeWriter), VortexError> {
    let wide: Vec<u16> = OsStr::new(pipe_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let deadline = Instant::now() + timeout;

    let raw_pipe = loop {
        // SAFETY: Opens `pipe_path` with `FILE_FLAG_OVERLAPPED` so `ReadFile` and `WriteFile` never serialize on `FO_SYNCHRONOUS_IO`.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            break h;
        }
        if Instant::now() >= deadline {
            // SAFETY: Queries thread-local Win32 `LastError`.
            let code = unsafe { GetLastError() };
            return Err(VortexError::coded(
                ErrorCode::IpcCoreUnreachable,
                trace_id,
                format!("Could not connect to Named Pipe {pipe_path}: Win32 {code}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let shared = Arc::new(SharedPipeHandle(raw_pipe as isize));
    // SAFETY: Creates two anonymous manual-reset Win32 Event objects (`rx` and `tx`).
    let (rx_ev, tx_ev) = unsafe {
        let r = CreateEventW(ptr::null(), TRUE, FALSE, ptr::null());
        let w = CreateEventW(ptr::null(), TRUE, FALSE, ptr::null());
        (r, w)
    };
    if rx_ev.is_null() || tx_ev.is_null() {
        if !rx_ev.is_null() {
            // SAFETY: Cleans up `rx_ev` on partial failure.
            unsafe {
                let _ = CloseHandle(rx_ev);
            }
        }
        if !tx_ev.is_null() {
            // SAFETY: Cleans up `tx_ev` on partial failure.
            unsafe {
                let _ = CloseHandle(tx_ev);
            }
        }
        return Err(VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            trace_id,
            "CreateEventW failed for overlapped pipe",
        ));
    }

    Ok((
        OverlappedPipeReader {
            pipe: Arc::clone(&shared),
            event: rx_ev as isize,
        },
        OverlappedPipeWriter {
            pipe: shared,
            event: tx_ev as isize,
        },
    ))
}

struct BridgeLogger {
    inner: Option<Mutex<SizeRotatingFile>>,
}

impl BridgeLogger {
    fn init() -> Self {
        let log_dir = std::env::var_os("VORTEX_LOG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(|base| PathBuf::from(base).join("Vortex").join("logs"))
            });
        let inner = log_dir.and_then(|dir| {
            SizeRotatingFile::open(RotatingFileConfig::production_defaults(dir, "bridge"))
                .ok()
                .map(Mutex::new)
        });
        Self { inner }
    }

    fn log(&self, level: &str, trace_id: &str, msg_id: &str, message: &str) {
        let Some(lock) = &self.inner else {
            return;
        };
        let clean_msg = redact_sensitive_text(message);
        let line = json!({
            "ts_ms": unix_now_ms(),
            "level": level,
            "target": "vortex_bridge",
            "trace_id": trace_id,
            "msg_id": msg_id,
            "message": clean_msg,
        });
        if let Ok(mut file) = lock.lock()
            && let Ok(mut bytes) = serde_json::to_vec(&line)
        {
            bytes.push(b'\n');
            let _ = file.write_record(&bytes);
        }
    }
}

fn current_user_sid() -> Option<String> {
    if let Ok(override_sid) = std::env::var("VORTEX_PIPE_SID_OVERRIDE")
        && !override_sid.trim().is_empty()
    {
        return Some(override_sid);
    }

    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess()` returns a pseudo-handle; `token` receives an owned `TOKEN_QUERY` handle.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == FALSE || token.is_null() {
        return None;
    }

    let mut needed: u32 = 0;
    // SAFETY: Queries required byte size for `TokenUser`.
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut needed);
    }
    if needed == 0 {
        // SAFETY: Closes opened `token` handle.
        unsafe {
            let _ = CloseHandle(token);
        }
        return None;
    }

    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buf: Vec<usize> = vec![0; words];
    // SAFETY: `buf` is pointer-aligned and holds at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if ok == FALSE {
        // SAFETY: Closes opened `token` handle on error.
        unsafe {
            let _ = CloseHandle(token);
        }
        return None;
    }

    // SAFETY: `buf` starts with a valid `TOKEN_USER` populated by `GetTokenInformation`.
    let sid_ptr = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut wide_sid: *mut u16 = ptr::null_mut();
    // SAFETY: Converts valid `sid_ptr` into NUL-terminated UTF-16 string allocated via `LocalAlloc`.
    let conv_ok = unsafe { ConvertSidToStringSidW(sid_ptr, &raw mut wide_sid) };
    // SAFETY: `token` handle is no longer needed.
    unsafe {
        let _ = CloseHandle(token);
    }
    if conv_ok == FALSE || wide_sid.is_null() {
        return None;
    }

    // SAFETY: Copies UTF-16 characters up to NUL and frees `wide_sid` with `LocalFree`.
    let out = unsafe {
        let mut len = 0_usize;
        while *wide_sid.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide_sid, len));
        let _ = LocalFree(wide_sid.cast::<core::ffi::c_void>() as HLOCAL);
        s
    };
    Some(out)
}

fn find_sibling_core_exe() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("VORTEX_CORE_EXE") {
        let p = PathBuf::from(custom);
        if p.exists() {
            return Some(p);
        }
    }
    let self_exe = std::env::current_exe().ok()?;
    let parent = self_exe.parent()?;
    for candidate_name in ["vortex.exe", "vortex-desktop.exe"] {
        let candidate = parent.join(candidate_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn try_autospawn_core_background(core_exe: &Path, logger: &BridgeLogger, trace_id: &str) -> bool {
    // Direct `Command::new` (NEVER `cmd.exe /c` or `powershell` — `TH-19` defense!)
    let res = Command::new(core_exe)
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();

    match res {
        Ok(_child) => {
            logger.log(
                "INFO",
                trace_id,
                "",
                "Auto-spawned Vortex Core in --background mode",
            );
            true
        }
        Err(err) => {
            logger.log(
                "WARN",
                trace_id,
                "",
                &format!("Failed auto-spawning Vortex Core: {err}"),
            );
            false
        }
    }
}

fn connect_or_autospawn_pipe(
    pipe_path: &str,
    logger: &BridgeLogger,
    trace_id: TraceId,
) -> Result<(OverlappedPipeReader, OverlappedPipeWriter), VortexError> {
    if let Ok(pair) = open_overlapped_pipe_pair(pipe_path, Duration::from_millis(80), trace_id) {
        return Ok(pair);
    }

    if let Some(core_exe) = find_sibling_core_exe() {
        let _ = try_autospawn_core_background(&core_exe, logger, &trace_id.to_string());
        return open_overlapped_pipe_pair(pipe_path, Duration::from_millis(1_100), trace_id);
    }

    open_overlapped_pipe_pair(pipe_path, Duration::from_millis(150), trace_id)
}

fn write_ipc_error_to_stdout(
    stdout_lock: &Mutex<io::Stdout>,
    reply_to: &str,
    trace_id: TraceId,
    err: &VortexError,
) {
    let err_env = proto::Envelope {
        protocol_version: vortex_ipc::PROTOCOL_VERSION,
        message_id: TraceId::new().to_string(),
        reply_to: reply_to.to_owned(),
        trace_id: trace_id.to_string(),
        timestamp_ms: unix_now_ms(),
        body: Some(proto::envelope::Body::Error(proto_error_from_api(
            err.as_api_error(),
        ))),
    };
    if let Ok(json_bytes) = proto_envelope_to_json_bytes(&err_env)
        && let Ok(mut out) = stdout_lock.lock()
    {
        let _ = write_native_json_frame(&mut *out, &json_bytes);
    }
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let logger = Arc::new(BridgeLogger::init());
    let boot_tid = TraceId::new();

    let Some(sid) = current_user_sid() else {
        logger.log(
            "ERROR",
            &boot_tid.to_string(),
            "",
            "Failed resolving Windows process Owner SID",
        );
        return ExitCode::from(1);
    };
    let pipe_path = format!(r"\\.\pipe\vortex-core-{sid}");
    let stdout_shared = Arc::new(Mutex::new(io::stdout()));
    let mut stdin = io::stdin().lock();

    // Wait for the first Native Messaging JSON frame before opening Named Pipe so idle browser startup is instant
    let first_raw = match read_native_json_frame(&mut stdin) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            logger.log(
                "WARN",
                &boot_tid.to_string(),
                "",
                &format!("Invalid initial Native Messaging frame: {err}"),
            );
            return ExitCode::from(2);
        }
    };

    let first_env = match json_bytes_to_proto_envelope(&first_raw) {
        Ok(env) => env,
        Err(err) => {
            write_ipc_error_to_stdout(&stdout_shared, "", boot_tid, &err);
            return ExitCode::from(2);
        }
    };

    let first_tid = first_env.trace_id.parse::<TraceId>().unwrap_or(boot_tid);

    let (mut pipe_reader, mut pipe_writer) =
        match connect_or_autospawn_pipe(&pipe_path, &logger, first_tid) {
            Ok(pair) => pair,
            Err(err) => {
                logger.log(
                    "WARN",
                    &first_tid.to_string(),
                    &first_env.message_id,
                    &format!("Core Named Pipe unreachable: {err}"),
                );
                let unreachable =
                    VortexError::new(ApiError::new(ErrorCode::IpcCoreUnreachable, first_tid));
                write_ipc_error_to_stdout(
                    &stdout_shared,
                    &first_env.message_id,
                    first_tid,
                    &unreachable,
                );
                return ExitCode::SUCCESS;
            }
        };

    // Spawn `Pipe -> Stdout` reader thread (full-duplex `FILE_FLAG_OVERLAPPED`: never blocks `pipe_writer`!)
    let reader_stdout = Arc::clone(&stdout_shared);
    let reader_logger = Arc::clone(&logger);
    let reader_join = std::thread::Builder::new()
        .name("vortex-bridge-pipe-rx".to_owned())
        .spawn(move || {
            while let Ok(Some(env)) = recv_envelope_from(&mut pipe_reader) {
                reader_logger.log(
                    "DEBUG",
                    &env.trace_id,
                    &env.message_id,
                    "Forwarding Core -> Extension frame",
                );
                if let Ok(json_bytes) = proto_envelope_to_json_bytes(&env)
                    && let Ok(mut out) = reader_stdout.lock()
                    && write_native_json_frame(&mut *out, &json_bytes).is_err()
                {
                    break;
                }
            }
        });

    logger.log(
        "INFO",
        &first_env.trace_id,
        &first_env.message_id,
        "Forwarding initial Extension -> Core frame",
    );
    if let Err(err) = send_envelope_to(&mut pipe_writer, &first_env) {
        write_ipc_error_to_stdout(&stdout_shared, &first_env.message_id, first_tid, &err);
        return ExitCode::SUCCESS;
    }

    // Pump remaining `Stdin -> Pipe` frames until Chromium closes `stdin` on SW idle/exit
    loop {
        match read_native_json_frame(&mut stdin) {
            Ok(Some(raw)) => match json_bytes_to_proto_envelope(&raw) {
                Ok(env) => {
                    let tid = env.trace_id.parse::<TraceId>().unwrap_or(boot_tid);
                    logger.log(
                        "DEBUG",
                        &env.trace_id,
                        &env.message_id,
                        "Forwarding Extension -> Core frame",
                    );
                    if let Err(err) = send_envelope_to(&mut pipe_writer, &env) {
                        write_ipc_error_to_stdout(&stdout_shared, &env.message_id, tid, &err);
                        break;
                    }
                }
                Err(err) => {
                    write_ipc_error_to_stdout(&stdout_shared, "", boot_tid, &err);
                }
            },
            Ok(None) => break,
            Err(err) => {
                logger.log(
                    "WARN",
                    &boot_tid.to_string(),
                    "",
                    &format!("Closing bridge on invalid stdin frame: {err}"),
                );
                break;
            }
        }
    }

    drop(pipe_writer);
    if let Ok(j) = reader_join {
        let _ = j.join();
    }
    ExitCode::SUCCESS
}
