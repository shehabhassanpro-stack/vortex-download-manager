//! Windows OS integration: Owner SID resolution, Single-Instance Named Mutex, Sleep Inhibitor
//! (`SetThreadExecutionState`), Owner-SID Named Pipe SDDL Server Factory (`PIPE_REJECT_REMOTE_CLIENTS`),
//! and `HKCU` Native Messaging Manifest registration (`P1-T22`, `P1-T24`, `P1-T29`).

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use vortex_application::{
    AppResult, BrowserIntegrationStatusDto, PlatformIntegration, SleepInhibitorGuard,
};
use vortex_error::{ErrorCode, TraceId, VortexError};
use vortex_observability::sanitize_path_for_export;

use crate::motw::apply_motw_and_av;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, FALSE, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHCreateItemFromParsingName, SHOpenFolderAndSelectItems,
    ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows_sys::core::{GUID, HRESULT};

/// Canonical Native Messaging Host identifier (`Protocol-Spec.md` §1 & `P1-T29`).
pub const NATIVE_HOST_NAME: &str = "com.vortex.bridge";

/// Deterministic Chromium/Brave MV3 Extension ID derived from `extension/wxt.config.ts` `manifest.key`.
pub const OFFICIAL_EXTENSION_ID: &str = "mpmpknhplomcjbdkbaopakoihjmhpbop";

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Queries the current Windows process token's Owner SID string (e.g., `"S-1-5-21-..."`).
///
/// # Errors
/// Returns `VTX-SEC-001` if querying the process token fails.
pub fn query_current_user_sid(trace_id: TraceId) -> AppResult<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess()` returns a pseudo-handle valid for the lifetime of the process,
    // and `token` is a valid stack pointer receiving the opened `TOKEN_QUERY` handle.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == FALSE || token.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "OpenProcessToken(TOKEN_QUERY) failed",
        ));
    }

    let mut needed: u32 = 0;
    // SAFETY: First call with null buffer and `0` length queries exact byte size into `needed`.
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut needed);
    }
    if needed == 0 {
        // SAFETY: `token` was opened successfully above and is closed once here.
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "GetTokenInformation(TokenUser) returned 0 size",
        ));
    }

    let word_len = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut aligned_buf: Vec<usize> = vec![0; word_len];
    // SAFETY: `aligned_buf` is pointer-aligned and at least `needed` bytes long.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            aligned_buf.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if ok == FALSE {
        // SAFETY: `token` is valid and closed on error exit.
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "GetTokenInformation(TokenUser) failed",
        ));
    }

    // SAFETY: `GetTokenInformation` populated a valid `TOKEN_USER` at the start of `aligned_buf`.
    let sid_ptr = unsafe { (*aligned_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut wide_sid_ptr: *mut u16 = ptr::null_mut();
    // SAFETY: `sid_ptr` points to a valid `SID` within `aligned_buf`; Windows allocates `wide_sid_ptr` via `LocalAlloc`.
    let conv_ok = unsafe { ConvertSidToStringSidW(sid_ptr, &raw mut wide_sid_ptr) };

    // SAFETY: `token` is no longer needed once SID conversion finished.
    unsafe {
        let _ = CloseHandle(token);
    }

    if conv_ok == FALSE || wide_sid_ptr.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "ConvertSidToStringSidW failed",
        ));
    }

    // SAFETY: `wide_sid_ptr` is a NUL-terminated UTF-16 string allocated by `ConvertSidToStringSidW`;
    // we copy it into a Rust `String` and immediately free `wide_sid_ptr` with `LocalFree`.
    let sid_string = unsafe {
        let mut len = 0_usize;
        while *wide_sid_ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(wide_sid_ptr, len);
        let s = String::from_utf16_lossy(slice);
        let _ = LocalFree(wide_sid_ptr.cast::<core::ffi::c_void>() as HLOCAL);
        s
    };

    Ok(sid_string)
}

/// Canonical per-user Named Pipe path (`\\.\pipe\vortex-core-<USER_SID>` — `Protocol-Spec.md` §1).
#[must_use]
pub fn user_named_pipe_path(user_sid: &str) -> String {
    format!(r"\\.\pipe\vortex-core-{user_sid}")
}

/// Creates a Windows Named Pipe server instance hardened with an **Owner-SID DACL**
/// (`D:P(A;;GA;;;<USER_SID>)`) and **`PIPE_REJECT_REMOTE_CLIENTS`** (`ADR-006` & `P1-T24`).
///
/// # Errors
/// Returns `VTX-IPC-001` or `VTX-SEC-001` if SDDL creation or pipe binding fails.
pub fn create_owner_only_pipe_server(
    pipe_name: &str,
    user_sid: &str,
    first_instance: bool,
    trace_id: TraceId,
) -> AppResult<NamedPipeServer> {
    // Protected DACL granting Generic All (`GA`) ONLY to the exact `user_sid`!
    let sddl = format!("D:P(A;;GA;;;{user_sid})");
    let wide_sddl = to_wide_nul(&sddl);
    let mut sd_ptr: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: `wide_sddl` is a valid NUL-terminated UTF-16 SDDL string; on success Windows
    // allocates `sd_ptr` via `LocalAlloc`, which we always release via `LocalFree` below.
    let sddl_ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut sd_ptr,
            ptr::null_mut(),
        )
    };
    if sddl_ok == FALSE || sd_ptr.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!("Failed compiling SDDL DACL for SID {user_sid}"),
        ));
    }

    let mut sec_attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(24),
        lpSecurityDescriptor: sd_ptr,
        bInheritHandle: FALSE,
    };

    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first_instance)
        .reject_remote_clients(true)
        .in_buffer_size(65_536)
        .out_buffer_size(65_536);

    // SAFETY: `sec_attrs` and `sd_ptr` remain valid across `create_with_security_attributes_raw`,
    // which synchronously invokes `CreateNamedPipeW`, after which we immediately `LocalFree(sd_ptr)`.
    let create_res = unsafe {
        opts.create_with_security_attributes_raw(
            pipe_name,
            (&raw mut sec_attrs).cast::<core::ffi::c_void>(),
        )
    };

    // SAFETY: `sd_ptr` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
    unsafe {
        let _ = LocalFree(sd_ptr as HLOCAL);
    }

    create_res.map_err(|e| {
        VortexError::coded(
            ErrorCode::IpcCoreUnreachable,
            trace_id,
            format!("CreateNamedPipeW failed for {pipe_name}: {e}"),
        )
    })
}

/// Outcome of attempting to acquire the OS Single-Instance Named Mutex (`§24.1` & `P1-T22`).
#[derive(Debug)]
pub enum SingleInstanceOutcome {
    /// This process is the primary instance and holds the OS mutex until [`SingleInstanceGuard`] drops.
    Primary(SingleInstanceGuard),
    /// Another Vortex instance is already running for this Windows user!
    SecondaryAlreadyRunning,
}

/// RAII handle holding `Local\VortexDownloadManager_SingleInstance_<USER_SID>`.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    handle: isize,
}

impl SingleInstanceGuard {
    /// Attempts to acquire `Local\VortexDownloadManager_SingleInstance_<suffix>`.
    ///
    /// # Errors
    /// Returns `VTX-BUG-001` if `CreateMutexW` fails unexpectedly.
    pub fn try_acquire(mutex_suffix: &str, trace_id: TraceId) -> AppResult<SingleInstanceOutcome> {
        let name = format!(r"Local\VortexDownloadManager_SingleInstance_{mutex_suffix}");
        let wide = to_wide_nul(&name);

        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; `CreateMutexW` is thread-safe
        // and sets thread-local `LastError` to `ERROR_ALREADY_EXISTS` if the named mutex exists.
        let (handle, last_err) = unsafe {
            let h = CreateMutexW(ptr::null(), FALSE, wide.as_ptr());
            let err = GetLastError();
            (h, err)
        };

        if handle.is_null() {
            return Err(VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("CreateMutexW failed with Win32 error {last_err}"),
            ));
        }

        if last_err == ERROR_ALREADY_EXISTS {
            // SAFETY: Close our duplicate handle immediately so the primary owner retains sole ownership.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(SingleInstanceOutcome::SecondaryAlreadyRunning);
        }

        Ok(SingleInstanceOutcome::Primary(Self {
            handle: handle as isize,
        }))
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: `self.handle` is a valid Win32 Mutex `HANDLE` created by `CreateMutexW`.
            unsafe {
                let _ = CloseHandle(self.handle as HANDLE);
            }
        }
    }
}

/// RAII sleep-prevention token calling `SetThreadExecutionState` (`§24.3` & `P1-T22`).
#[derive(Debug)]
pub struct WindowsSleepGuard {
    active: bool,
}

impl WindowsSleepGuard {
    fn acquire() -> Self {
        // SAFETY: `SetThreadExecutionState` is a pure Win32 power flag API with no pointer arguments.
        unsafe {
            let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        Self { active: true }
    }
}

impl Drop for WindowsSleepGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: Clears `ES_SYSTEM_REQUIRED` while keeping `ES_CONTINUOUS`.
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

impl SleepInhibitorGuard for WindowsSleepGuard {}

#[derive(Debug, Serialize, Deserialize)]
struct ChromiumNativeHostManifest {
    name: String,
    description: String,
    path: String,
    #[serde(rename = "type")]
    host_type: String,
    allowed_origins: Vec<String>,
}

const BROWSER_TARGETS: [(&str, &str); 3] = [
    (
        "Brave",
        r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    ),
    ("Chrome", r"Software\Google\Chrome\NativeMessagingHosts"),
    ("Edge", r"Software\Microsoft\Edge\NativeMessagingHosts"),
];

fn write_hkcu_default_sz(subkey_path: &str, value: &str, trace_id: TraceId) -> AppResult<()> {
    let wide_subkey = to_wide_nul(subkey_path);
    let wide_val = to_wide_nul(value);
    let mut hkey: HKEY = ptr::null_mut();

    // SAFETY: `wide_subkey` is NUL-terminated UTF-16; `HKEY_CURRENT_USER` requires zero Admin rights!
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide_subkey.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            ptr::null(),
            &raw mut hkey,
            ptr::null_mut(),
        )
    };
    if status != 0 || hkey.is_null() {
        return Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!("RegCreateKeyExW failed for HKCU\\{subkey_path}: Win32 {status}"),
        ));
    }

    let byte_len = u32::try_from(wide_val.len() * 2).unwrap_or(0);
    // SAFETY: `hkey` is open with `KEY_WRITE`, `ptr::null()` sets the `(Default)` value, and `wide_val` is NUL-terminated UTF-16 (`REG_SZ`).
    let set_status = unsafe {
        RegSetValueExW(
            hkey,
            ptr::null(),
            0,
            REG_SZ,
            wide_val.as_ptr().cast::<u8>(),
            byte_len,
        )
    };
    // SAFETY: Always close `hkey`.
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if set_status != 0 {
        return Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!("RegSetValueExW failed for HKCU\\{subkey_path}: Win32 {set_status}"),
        ));
    }
    Ok(())
}

fn read_hkcu_default_sz(subkey_path: &str) -> Option<String> {
    let wide_subkey = to_wide_nul(subkey_path);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: Opens `HKCU\<subkey_path>` read-only.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide_subkey.as_ptr(),
            0,
            KEY_READ,
            &raw mut hkey,
        )
    };
    if status != 0 || hkey.is_null() {
        return None;
    }

    let mut buf: Vec<u16> = vec![0; 1024];
    let mut byte_len: u32 = u32::try_from(buf.len() * 2).unwrap_or(2048);
    let mut kind: u32 = 0;
    // SAFETY: `buf` holds `byte_len` bytes; queries `(Default)` value.
    let q_status = unsafe {
        RegQueryValueExW(
            hkey,
            ptr::null(),
            ptr::null(),
            &raw mut kind,
            buf.as_mut_ptr().cast::<u8>(),
            &raw mut byte_len,
        )
    };
    // SAFETY: Close `hkey`.
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if q_status != 0 || kind != REG_SZ || byte_len < 2 {
        return None;
    }
    let u16_len = (byte_len as usize / 2).min(buf.len());
    let slice = buf.get(..u16_len)?;
    let trimmed_len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    Some(String::from_utf16_lossy(slice.get(..trimmed_len)?))
}

/// Production Windows [`PlatformIntegration`] adapter (`P1-T22`).
#[derive(Debug, Clone)]
pub struct WindowsPlatform {
    manifest_dir: PathBuf,
    registry_prefix_override: Option<String>,
    online_flag: Arc<AtomicBool>,
}

impl WindowsPlatform {
    /// Creates a new [`WindowsPlatform`] storing Native Host manifests inside `manifest_dir`.
    #[must_use]
    pub fn new(manifest_dir: PathBuf) -> Self {
        Self {
            manifest_dir,
            registry_prefix_override: None,
            online_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Redirects `HKCU` registry writes under an isolated test prefix (e.g. `Software\VortexTest\<uuid>`)
    /// so automated tests exercise real Win32 Registry calls without touching live browser keys!
    #[must_use]
    pub fn with_registry_prefix_for_test(mut self, prefix: impl Into<String>) -> Self {
        self.registry_prefix_override = Some(prefix.into());
        self
    }

    /// Shared atomic connectivity flag updated by [`crate::lifecycle::spawn_connectivity_monitor`].
    #[must_use]
    pub fn online_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.online_flag)
    }

    fn subkey_for(&self, base_subkey: &str) -> String {
        match &self.registry_prefix_override {
            Some(prefix) => format!(r"{prefix}\{base_subkey}\{NATIVE_HOST_NAME}"),
            None => format!(r"{base_subkey}\{NATIVE_HOST_NAME}"),
        }
    }

    fn manifest_file_path(&self) -> PathBuf {
        self.manifest_dir.join(format!("{NATIVE_HOST_NAME}.json"))
    }
}

#[async_trait]
impl PlatformIntegration for WindowsPlatform {
    fn current_user_sid(&self, trace_id: TraceId) -> AppResult<String> {
        query_current_user_sid(trace_id)
    }

    fn acquire_sleep_inhibitor(
        &self,
        _trace_id: TraceId,
    ) -> AppResult<Box<dyn SleepInhibitorGuard>> {
        Ok(Box::new(WindowsSleepGuard::acquire()))
    }

    async fn apply_motw_and_av_scan(
        &self,
        file_path: &Path,
        source_url: &str,
        referrer_url: Option<&str>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        apply_motw_and_av(file_path, source_url, referrer_url, trace_id).await
    }

    fn register_native_hosts(
        &self,
        bridge_exe: &Path,
        allowed_extension_ids: &[String],
        trace_id: TraceId,
    ) -> AppResult<Vec<BrowserIntegrationStatusDto>> {
        std::fs::create_dir_all(&self.manifest_dir).map_err(|e| {
            VortexError::coded(
                ErrorCode::DiskPermissionDenied,
                trace_id,
                format!("Cannot create manifest dir: {e}"),
            )
        })?;

        let origins: Vec<String> = allowed_extension_ids
            .iter()
            .filter(|id| !id.trim().is_empty())
            .map(|id| format!("chrome-extension://{}/", id.trim()))
            .collect();

        let manifest = ChromiumNativeHostManifest {
            name: NATIVE_HOST_NAME.to_owned(),
            description: "Vortex Download Manager Native Messaging Bridge".to_owned(),
            path: bridge_exe.display().to_string(),
            host_type: "stdio".to_owned(),
            allowed_origins: origins,
        };

        let manifest_path = self.manifest_file_path();
        let json = serde_json::to_string_pretty(&manifest).map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("Failed serializing Native Host manifest: {e}"),
            )
        })?;
        std::fs::write(&manifest_path, json).map_err(|e| {
            VortexError::coded(
                ErrorCode::DiskPermissionDenied,
                trace_id,
                format!("Failed writing {}: {e}", manifest_path.display()),
            )
        })?;

        let manifest_str = manifest_path.display().to_string();
        let sanitized_manifest = sanitize_path_for_export(&manifest_str, None);

        let mut results = Vec::with_capacity(BROWSER_TARGETS.len());
        for (browser_name, base_key) in BROWSER_TARGETS {
            let full_subkey = self.subkey_for(base_key);
            write_hkcu_default_sz(&full_subkey, &manifest_str, trace_id)?;
            results.push(BrowserIntegrationStatusDto {
                browser: browser_name.to_owned(),
                registered: true,
                manifest_path: sanitized_manifest.clone(),
            });
        }

        Ok(results)
    }

    fn check_native_hosts(&self) -> Vec<BrowserIntegrationStatusDto> {
        let expected_manifest = self.manifest_file_path();
        let expected_str = expected_manifest.display().to_string();
        let sanitized_manifest = sanitize_path_for_export(&expected_str, None);

        let manifest_valid = std::fs::read_to_string(&expected_manifest)
            .ok()
            .and_then(|s| serde_json::from_str::<ChromiumNativeHostManifest>(&s).ok())
            .is_some_and(|m| m.name == NATIVE_HOST_NAME && Path::new(&m.path).exists());

        let mut statuses = Vec::with_capacity(BROWSER_TARGETS.len());
        for (browser_name, base_key) in BROWSER_TARGETS {
            let subkey = self.subkey_for(base_key);
            let reg_matches = read_hkcu_default_sz(&subkey)
                .is_some_and(|v| Path::new(&v) == expected_manifest && manifest_valid);
            statuses.push(BrowserIntegrationStatusDto {
                browser: browser_name.to_owned(),
                registered: reg_matches,
                manifest_path: sanitized_manifest.clone(),
            });
        }
        statuses
    }

    fn is_internet_available(&self) -> bool {
        self.online_flag.load(Ordering::Relaxed)
    }

    fn free_disk_bytes(&self, target_dir: &Path, _trace_id: TraceId) -> Option<u64> {
        query_free_disk_bytes(target_dir)
    }

    fn pick_folder(
        &self,
        initial_dir: Option<&Path>,
        trace_id: TraceId,
    ) -> AppResult<Option<PathBuf>> {
        let initial = initial_dir.map(Path::to_path_buf);
        // Spawn a fresh STA thread so COM `IFileOpenDialog` never conflicts with Tokio MTA workers!
        let handle = std::thread::spawn(move || pick_folder_com_sta(initial.as_deref(), trace_id));
        handle.join().map_err(|_| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                "Folder picker STA thread panicked",
            )
        })?
    }

    fn reveal_in_file_manager(&self, path: &Path, trace_id: TraceId) -> AppResult<()> {
        reveal_in_windows_explorer(path, trace_id)
    }

    fn open_file_safely(&self, path: &Path, trace_id: TraceId) -> AppResult<()> {
        open_file_with_shell_execute(path, trace_id)
    }

    fn open_url_in_browser(&self, url: &str, trace_id: TraceId) -> AppResult<()> {
        open_safe_url_with_shell_execute(url, trace_id)
    }

    fn enforce_owner_only_dir_acl(&self, dir: &Path, trace_id: TraceId) -> AppResult<()> {
        enforce_owner_only_directory_acl(dir, trace_id)
    }
}

/// Queries free bytes available to the caller on the volume hosting `target_dir` (`GetDiskFreeSpaceExW`).
#[must_use]
pub fn query_free_disk_bytes(target_dir: &Path) -> Option<u64> {
    let mut probe = target_dir;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    let wide = to_wide_nul(&probe.display().to_string());
    let mut free_avail: u64 = 0;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path and `free_avail` is a valid stack `u64`.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free_avail,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    (ok != FALSE).then_some(free_avail)
}

#[allow(clippy::cast_sign_loss, clippy::too_many_lines)]
fn pick_folder_com_sta(
    initial_dir: Option<&Path>,
    trace_id: TraceId,
) -> AppResult<Option<PathBuf>> {
    use core::ffi::c_void;

    static CLSID_FILE_OPEN_DIALOG: GUID = GUID {
        data1: 0xDC1C_5A9C,
        data2: 0xE88A,
        data3: 0x4DDE,
        data4: [0xA5, 0xA1, 0x60, 0xF8, 0x2A, 0x20, 0xAE, 0xF7],
    };
    static IID_IFILE_OPEN_DIALOG: GUID = GUID {
        data1: 0xD57C_7288,
        data2: 0xD4AD,
        data3: 0x4768,
        data4: [0xBE, 0x02, 0x9D, 0x96, 0x95, 0x32, 0xD9, 0x60],
    };
    static IID_ISHELL_ITEM: GUID = GUID {
        data1: 0x4382_6D1E,
        data2: 0xE718,
        data3: 0x42EE,
        data4: [0xBC, 0x55, 0xA1, 0xE2, 0x61, 0xC3, 0x7B, 0xFE],
    };

    const FOS_PICKFOLDERS: u32 = 0x0000_0020;
    const FOS_FORCEFILESYSTEM: u32 = 0x0000_0040;
    const FOS_PATHMUSTEXIST: u32 = 0x0000_0800;
    const SIGDN_FILESYSPATH: i32 = -2_147_123_200; // 0x80058000
    const HR_ERROR_CANCELLED: HRESULT = -2_147_023_673; // 0x800704C7

    #[repr(C)]
    struct IFileDialogVtbl {
        query_interface: extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: extern "system" fn(*mut c_void) -> u32,
        release: extern "system" fn(*mut c_void) -> u32,
        show: extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        set_file_types: extern "system" fn(*mut c_void, u32, *const c_void) -> HRESULT,
        set_file_type_index: extern "system" fn(*mut c_void, u32) -> HRESULT,
        get_file_type_index: extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        advise: extern "system" fn(*mut c_void, *mut c_void, *mut u32) -> HRESULT,
        unadvise: extern "system" fn(*mut c_void, u32) -> HRESULT,
        set_options: extern "system" fn(*mut c_void, u32) -> HRESULT,
        get_options: extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        set_default_folder: extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        set_folder: extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        get_folder: extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
        get_current_selection: extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
        set_file_name: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_file_name: extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
        set_title: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_ok_button_label: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_file_name_label: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_result: extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IFileDialogRaw {
        lp_vtbl: *const IFileDialogVtbl,
    }

    #[repr(C)]
    struct IShellItemVtbl {
        query_interface: extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: extern "system" fn(*mut c_void) -> u32,
        release: extern "system" fn(*mut c_void) -> u32,
        bind_to_handler: extern "system" fn(
            *mut c_void,
            *mut c_void,
            *const GUID,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
        get_parent: extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
        get_display_name: extern "system" fn(*mut c_void, i32, *mut *mut u16) -> HRESULT,
    }

    #[repr(C)]
    struct IShellItemRaw {
        lp_vtbl: *const IShellItemVtbl,
    }

    // SAFETY: Dedicated STA thread initializes COM, invokes `IFileOpenDialog` (`FOS_PICKFOLDERS`),
    // frees any COM interfaces and `CoTaskMemFree` buffers, and balances `CoUninitialize`.
    unsafe {
        let init_hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let should_uninit = init_hr >= 0;

        let mut dialog_ptr: *mut c_void = ptr::null_mut();
        let create_hr = CoCreateInstance(
            &raw const CLSID_FILE_OPEN_DIALOG,
            ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &raw const IID_IFILE_OPEN_DIALOG,
            &raw mut dialog_ptr,
        );
        if create_hr < 0 || dialog_ptr.is_null() {
            if should_uninit {
                CoUninitialize();
            }
            return Err(VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("CoCreateInstance(CLSID_FileOpenDialog) failed (HRESULT {create_hr:#X})"),
            ));
        }

        let dlg_vtbl = &*(*dialog_ptr.cast::<IFileDialogRaw>()).lp_vtbl;
        let mut opts: u32 = 0;
        let _ = (dlg_vtbl.get_options)(dialog_ptr, &raw mut opts);
        let _ = (dlg_vtbl.set_options)(
            dialog_ptr,
            opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
        );
        let title_w = to_wide_nul("Vortex — اختر مجلد الحفظ / Select Download Folder");
        let _ = (dlg_vtbl.set_title)(dialog_ptr, title_w.as_ptr());

        if let Some(dir) = initial_dir.filter(|p| p.exists()) {
            let dir_w = to_wide_nul(&dir.display().to_string());
            let mut folder_item: *mut c_void = ptr::null_mut();
            let item_hr = SHCreateItemFromParsingName(
                dir_w.as_ptr(),
                ptr::null_mut(),
                &raw const IID_ISHELL_ITEM,
                &raw mut folder_item,
            );
            if item_hr >= 0 && !folder_item.is_null() {
                let _ = (dlg_vtbl.set_folder)(dialog_ptr, folder_item);
                let item_vtbl = &*(*folder_item.cast::<IShellItemRaw>()).lp_vtbl;
                let _ = (item_vtbl.release)(folder_item);
            }
        }

        let show_hr = (dlg_vtbl.show)(dialog_ptr, ptr::null_mut());
        let outcome = if show_hr == HR_ERROR_CANCELLED {
            Ok(None)
        } else if show_hr < 0 {
            Err(VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("IFileOpenDialog::Show failed (HRESULT {show_hr:#X})"),
            ))
        } else {
            let mut result_item: *mut c_void = ptr::null_mut();
            let res_hr = (dlg_vtbl.get_result)(dialog_ptr, &raw mut result_item);
            if res_hr >= 0 && !result_item.is_null() {
                let item_vtbl = &*(*result_item.cast::<IShellItemRaw>()).lp_vtbl;
                let mut wide_path_ptr: *mut u16 = ptr::null_mut();
                let name_hr = (item_vtbl.get_display_name)(
                    result_item,
                    SIGDN_FILESYSPATH,
                    &raw mut wide_path_ptr,
                );
                let selected = if name_hr >= 0 && !wide_path_ptr.is_null() {
                    let mut len = 0_usize;
                    while *wide_path_ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(wide_path_ptr, len);
                    let path_str = String::from_utf16_lossy(slice);
                    CoTaskMemFree(wide_path_ptr.cast::<c_void>());
                    Some(PathBuf::from(path_str))
                } else {
                    None
                };
                let _ = (item_vtbl.release)(result_item);
                Ok(selected)
            } else {
                Ok(None)
            }
        };

        let _ = (dlg_vtbl.release)(dialog_ptr);
        if should_uninit {
            CoUninitialize();
        }
        outcome
    }
}

#[allow(clippy::cast_sign_loss)]
fn reveal_in_windows_explorer(path: &Path, trace_id: TraceId) -> AppResult<()> {
    if !path.exists() {
        return Err(VortexError::coded(
            ErrorCode::ResNotFound,
            trace_id,
            format!("Path does not exist on disk: {}", path.display()),
        ));
    }

    let wide = to_wide_nul(&path.display().to_string());
    // SAFETY: Initializes STA COM on current call, allocates PIDL via `ILCreateFromPathW`,
    // calls `SHOpenFolderAndSelectItems`, always frees PIDL via `ILFree`, and balances `CoUninitialize`.
    let hr = unsafe {
        let init_hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let should_uninit = init_hr >= 0;
        let pidl = ILCreateFromPathW(wide.as_ptr());
        let res_hr = if pidl.is_null() {
            -1
        } else {
            let h = SHOpenFolderAndSelectItems(pidl, 0, ptr::null(), 0);
            ILFree(pidl);
            h
        };
        if should_uninit {
            CoUninitialize();
        }
        res_hr
    };

    if hr >= 0 {
        return Ok(());
    }

    // Safe argument-passing fallback (zero shell interpolation!)
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::DiskPermissionDenied,
                trace_id,
                format!("Failed launching Explorer for {}: {e}", path.display()),
            )
        })?;
    Ok(())
}

fn open_file_with_shell_execute(path: &Path, trace_id: TraceId) -> AppResult<()> {
    if !path.exists() {
        return Err(VortexError::coded(
            ErrorCode::ResNotFound,
            trace_id,
            format!("File does not exist on disk: {}", path.display()),
        ));
    }
    let verb = to_wide_nul("open");
    let file = to_wide_nul(&path.display().to_string());
    // SAFETY: `verb` and `file` are NUL-terminated UTF-16 strings; `ShellExecuteW` opens via
    // Windows Shell honoring `:Zone.Identifier` Mark-of-the-Web SmartScreen checks (`TH-07`).
    let hinst = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (hinst as isize) <= 32 {
        return Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!(
                "ShellExecuteW failed opening {} (code {})",
                path.display(),
                hinst as isize
            ),
        ));
    }
    Ok(())
}

/// Strictly validated shell URL opener supporting ONLY `http://`, `https://`, and the two
/// hardcoded safe Windows Settings URIs (`ms-settings:dateandtime`, `windowsdefender://threat`).
fn open_safe_url_with_shell_execute(url: &str, trace_id: TraceId) -> AppResult<()> {
    let trimmed = url.trim();
    let is_safe_os_settings = matches!(
        trimmed,
        "ms-settings:dateandtime" | "windowsdefender://threat"
    );
    if !is_safe_os_settings {
        let _ = vortex_domain::FilenameSanitizer::validate_download_url(trimmed)?;
    }

    let verb = to_wide_nul("open");
    let target = to_wide_nul(trimmed);
    // SAFETY: `verb` and `target` are NUL-terminated UTF-16 strings validated strictly against the
    // `http://`/`https://` + exact OS settings URI allowlist above.
    let hinst = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (hinst as isize) <= 32 {
        return Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!("ShellExecuteW failed opening URL (code {})", hinst as isize),
        ));
    }
    Ok(())
}

/// Applies an Owner-Only Protected NTFS DACL (`D:P(A;OICI;FA;;;<USER_SID>)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`)
/// to `%LOCALAPPDATA%\Vortex` (`TH-31`) so other non-admin users on shared PCs cannot read SQLite/logs/secrets!
///
/// # Errors
/// Returns `VTX-SEC-001` or `VTX-DISK-002` if compiling SDDL or applying `SetNamedSecurityInfoW` fails.
pub fn enforce_owner_only_directory_acl(dir: &Path, trace_id: TraceId) -> AppResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!("Failed creating protected directory {}: {e}", dir.display()),
        )
    })?;

    let user_sid = query_current_user_sid(trace_id)?;
    let sddl = format!("D:P(A;OICI;FA;;;{user_sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let wide_sddl = to_wide_nul(&sddl);
    let mut sd_ptr: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: `wide_sddl` is a valid NUL-terminated UTF-16 SDDL string; on success Windows
    // allocates `sd_ptr` via `LocalAlloc`, which we always release with `LocalFree`.
    let sddl_ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut sd_ptr,
            ptr::null_mut(),
        )
    };
    if sddl_ok == FALSE || sd_ptr.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!("Failed compiling directory SDDL for SID {user_sid}"),
        ));
    }

    let mut dacl_present: i32 = 0;
    let mut dacl_defaulted: i32 = 0;
    let mut dacl_ptr: *mut ACL = ptr::null_mut();

    // SAFETY: `sd_ptr` is a valid security descriptor allocated above; `dacl_ptr` borrows into `sd_ptr`.
    let dacl_ok = unsafe {
        GetSecurityDescriptorDacl(
            sd_ptr,
            &raw mut dacl_present,
            &raw mut dacl_ptr,
            &raw mut dacl_defaulted,
        )
    };
    if dacl_ok == FALSE || dacl_present == FALSE || dacl_ptr.is_null() {
        // SAFETY: Always release `sd_ptr`.
        unsafe {
            let _ = LocalFree(sd_ptr as HLOCAL);
        }
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "Failed extracting DACL from compiled SDDL security descriptor",
        ));
    }

    let mut wide_path = to_wide_nul(&dir.display().to_string());
    // SAFETY: `wide_path` is a writable NUL-terminated UTF-16 path and `dacl_ptr` remains valid
    // inside `sd_ptr` for the entire synchronous `SetNamedSecurityInfoW` call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl_ptr,
            ptr::null_mut(),
        )
    };

    // SAFETY: Release `sd_ptr` after `SetNamedSecurityInfoW` completes.
    unsafe {
        let _ = LocalFree(sd_ptr as HLOCAL);
    }

    if status != 0 {
        return Err(VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!(
                "SetNamedSecurityInfoW failed on {} (Win32 {status})",
                dir.display()
            ),
        ));
    }
    Ok(())
}
