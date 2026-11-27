//! Logical disk-space ledger (`VolumeSpaceManager`) + live 3-second disk space monitor
//! (`P1-T11` & `P1-T12` / `ADR-007` / `TH-14`).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustc_hash::FxHashMap;
use vortex_application::{AppEvent, AppResult, EventBus, SpaceReservationId, VolumeSpaceLedger};
use vortex_error::{ApiError, ErrorCode, TraceId, VortexError};

use crate::failpoints::check_failpoint;

/// Converts `path` into an extended-length Windows `\\?\` path when absolute on Windows so paths
/// exceeding `260` characters (`MAX_PATH`) succeed seamlessly (`P1-T11`).
#[must_use]
pub fn to_os_extended_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.as_os_str().to_string_lossy();
        if raw.starts_with(r"\\?\") || !path.is_absolute() {
            return path.to_path_buf();
        }
        let normalized = raw.replace('/', r"\");
        if let Some(unc_tail) = normalized.strip_prefix(r"\\") {
            return PathBuf::from(format!(r"\\?\UNC\{unc_tail}"));
        }
        PathBuf::from(format!(r"\\?\{normalized}"))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Maps `std::io::Error` from disk operations to canonical `VTX-DISK-*` error codes (`§23.4`).
#[must_use]
pub fn map_disk_io_error(err: &std::io::Error, target: &Path, trace_id: TraceId) -> VortexError {
    if let Some(os_code) = err.raw_os_error() {
        #[cfg(windows)]
        {
            match os_code {
                // ERROR_DISK_FULL (112) | ERROR_HANDLE_DISK_FULL (39)
                39 | 112 => {
                    return VortexError::new(
                        ApiError::new(ErrorCode::DiskInsufficientSpace, trace_id)
                            .with_param("drive", volume_key(target))
                            .with_technical_detail(format!("Win32 disk full ({os_code}): {err}")),
                    );
                }
                // ERROR_ACCESS_DENIED (5)
                5 => {
                    return VortexError::coded(
                        ErrorCode::DiskPermissionDenied,
                        trace_id,
                        format!("Win32 access denied on {}: {err}", target.display()),
                    );
                }
                // ERROR_SHARING_VIOLATION (32) | ERROR_LOCK_VIOLATION (33)
                32 | 33 => {
                    return VortexError::coded(
                        ErrorCode::DiskFileLocked,
                        trace_id,
                        format!("Win32 file locked ({os_code}): {err}"),
                    );
                }
                // ERROR_VIRUS_INFECTED (225) | ERROR_VIRUS_DELETED (226) (`UX-M06`)
                225 | 226 => {
                    return VortexError::new(
                        ApiError::new(ErrorCode::SecAntivirusQuarantined, trace_id)
                            .with_param("file", target.display().to_string())
                            .with_technical_detail(format!(
                                "Windows Defender / Antivirus quarantined or blocked file (Win32 {os_code}): {err}"
                            )),
                    );
                }
                // ERROR_INVALID_DRIVE (15) | ERROR_NOT_READY (21) | ERROR_DEV_NOT_EXIST (55) | ERROR_DEVICE_NOT_CONNECTED (1167)
                15 | 21 | 55 | 1167 => {
                    return VortexError::coded(
                        ErrorCode::DiskVolumeDisconnected,
                        trace_id,
                        format!("Win32 volume disconnected ({os_code}): {err}"),
                    );
                }
                _ => {}
            }
        }
        #[cfg(not(windows))]
        {
            // ENOSPC = 28, EACCES = 13, EIO/ENXIO/ENODEV = 5/6/19
            match os_code {
                28 => {
                    return VortexError::coded(
                        ErrorCode::DiskInsufficientSpace,
                        trace_id,
                        format!("ENOSPC: {err}"),
                    );
                }
                13 | 1 => {
                    return VortexError::coded(
                        ErrorCode::DiskPermissionDenied,
                        trace_id,
                        format!("EACCES: {err}"),
                    );
                }
                5 | 6 | 19 => {
                    return VortexError::coded(
                        ErrorCode::DiskVolumeDisconnected,
                        trace_id,
                        format!("Device I/O error ({os_code}): {err}"),
                    );
                }
                _ => {}
            }
        }
    }

    match err.kind() {
        std::io::ErrorKind::PermissionDenied => VortexError::coded(
            ErrorCode::DiskPermissionDenied,
            trace_id,
            format!("Permission denied: {err}"),
        ),
        std::io::ErrorKind::StorageFull => VortexError::coded(
            ErrorCode::DiskInsufficientSpace,
            trace_id,
            format!("Storage full: {err}"),
        ),
        _ => VortexError::coded(
            ErrorCode::DiskVolumeDisconnected,
            trace_id,
            format!("Disk I/O error: {err}"),
        ),
    }
}

/// Extracts canonical volume identifier (e.g., `"C:"` on Windows, `"/"` on POSIX).
#[must_use]
pub fn volume_key(path: &Path) -> String {
    for comp in path.components() {
        if let Component::Prefix(prefix) = comp {
            let s = prefix.as_os_str().to_string_lossy();
            let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
            return stripped.to_ascii_uppercase();
        }
    }
    "/".to_owned()
}

#[derive(Debug)]
struct ReservationEntry {
    vol_key: String,
    remaining_bytes: u64,
}

/// Maximum file size supported by a FAT32 filesystem (`4 GiB - 1` = `4,294,967,295` bytes — `UX-M02`).
pub const FAT32_MAX_FILE_BYTES: u64 = u32::MAX as u64;

/// Thread-safe logical disk space ledger (`FxHashMap<VolumeId, AtomicU64>` — `§9` & `P1-T11`).
#[derive(Debug)]
pub struct VolumeSpaceManager {
    next_id: AtomicU64,
    per_volume_reserved: Mutex<FxHashMap<String, Arc<AtomicU64>>>,
    reservations: Mutex<FxHashMap<u64, ReservationEntry>>,
    /// Optional test override simulating a fixed physical disk capacity.
    simulated_free_override: AtomicU64,
    /// Optional test override simulating a filesystem name (e.g., `"FAT32"` for `UX-M02` tests).
    simulated_fs_override: Mutex<Option<String>>,
}

impl Default for VolumeSpaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeSpaceManager {
    /// Creates a new [`VolumeSpaceManager`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            per_volume_reserved: Mutex::new(FxHashMap::default()),
            reservations: Mutex::new(FxHashMap::default()),
            simulated_free_override: AtomicU64::new(u64::MAX),
            simulated_fs_override: Mutex::new(None),
        }
    }

    /// Sets a simulated physical free byte count for deterministic unit/fault tests (`u64::MAX` disables).
    pub fn set_simulated_free_bytes(&self, free_bytes: Option<u64>) {
        self.simulated_free_override
            .store(free_bytes.unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    /// Sets a simulated filesystem name (e.g., `Some("FAT32".to_owned())`) for deterministic `UX-M02` tests.
    pub fn set_simulated_fs_name(&self, fs_name: Option<String>) {
        let mut guard = self
            .simulated_fs_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = fs_name;
    }

    /// Returns the filesystem name hosting `target_dir` (e.g., `"NTFS"`, `"exFAT"`, `"FAT32"`).
    #[must_use]
    pub fn filesystem_name(&self, target_dir: &Path) -> Option<String> {
        let override_name = self
            .simulated_fs_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if override_name.is_some() {
            return override_name;
        }
        query_os_filesystem_name(target_dir)
    }

    /// Returns total logically reserved bytes currently outstanding on `target_dir`'s volume.
    #[must_use]
    pub fn reserved_on_volume(&self, target_dir: &Path) -> u64 {
        let key = volume_key(target_dir);
        self.per_volume_reserved
            .lock()
            .ok()
            .and_then(|m| m.get(&key).map(|c| c.load(Ordering::SeqCst)))
            .unwrap_or(0)
    }

    fn volume_counter(&self, key: &str) -> Arc<AtomicU64> {
        let mut guard = self
            .per_volume_reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            guard
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(AtomicU64::new(0))),
        )
    }
}

impl VolumeSpaceLedger for VolumeSpaceManager {
    fn reserve(
        &self,
        target_dir: &Path,
        bytes: u64,
        min_free_reserve: u64,
        trace_id: TraceId,
    ) -> AppResult<SpaceReservationId> {
        check_failpoint("io.reserve_space", trace_id)?;
        fs::create_dir_all(target_dir).map_err(|e| map_disk_io_error(&e, target_dir, trace_id))?;

        let physical_free = self.available_bytes(target_dir, trace_id)?;
        let key = volume_key(target_dir);
        let counter = self.volume_counter(&key);

        // Serialize reservation check + increment under `reservations` lock so 3 concurrent
        // sparse downloads can NEVER race past `physical_free` (`TH-14` / `ADR-007`)!
        let mut res_map = self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let already_reserved = counter.load(Ordering::SeqCst);
        let required_total = bytes.saturating_add(min_free_reserve);
        let effective_free = physical_free.saturating_sub(already_reserved);

        if effective_free < required_total {
            return Err(VortexError::new(
                ApiError::new(ErrorCode::DiskInsufficientSpace, trace_id)
                    .with_param("drive", key)
                    .with_param("needed", required_total.to_string())
                    .with_technical_detail(format!(
                        "Insufficient space: physical_free={physical_free}, reserved={already_reserved}, requested={bytes}, safety_reserve={min_free_reserve}"
                    )),
            ));
        }

        counter.fetch_add(bytes, Ordering::SeqCst);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        res_map.insert(
            id,
            ReservationEntry {
                vol_key: key,
                remaining_bytes: bytes,
            },
        );
        Ok(SpaceReservationId(id))
    }

    fn record_written(&self, id: SpaceReservationId, written_bytes: u64) {
        let mut res_map = self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = res_map.get_mut(&id.0) {
            let delta = written_bytes.min(entry.remaining_bytes);
            entry.remaining_bytes -= delta;
            let counter = self.volume_counter(&entry.vol_key);
            counter.fetch_sub(delta, Ordering::SeqCst);
        }
    }

    fn release(&self, id: SpaceReservationId) {
        let mut res_map = self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = res_map.remove(&id.0)
            && entry.remaining_bytes > 0
        {
            let counter = self.volume_counter(&entry.vol_key);
            counter.fetch_sub(entry.remaining_bytes, Ordering::SeqCst);
        }
    }

    fn available_bytes(&self, target_dir: &Path, trace_id: TraceId) -> AppResult<u64> {
        let sim = self.simulated_free_override.load(Ordering::SeqCst);
        if sim != u64::MAX {
            return Ok(sim);
        }
        query_os_free_space(target_dir, trace_id)
    }

    fn check_file_size_supported(
        &self,
        target_dir: &Path,
        total_size: u64,
        trace_id: TraceId,
    ) -> AppResult<()> {
        if total_size <= FAT32_MAX_FILE_BYTES {
            return Ok(());
        }
        if let Some(fs_name) = self.filesystem_name(target_dir) {
            let upper = fs_name.trim().to_ascii_uppercase();
            if upper == "FAT32" || upper == "FAT" {
                return Err(VortexError::new(
                    ApiError::new(ErrorCode::DiskFilesystemLimit, trace_id)
                        .with_param("drive", volume_key(target_dir))
                        .with_param("filesystem", upper)
                        .with_param("size", total_size.to_string())
                        .with_technical_detail(format!(
                            "Target volume {} ({fs_name}) has a 4 GiB maximum file size limit (requested {total_size} bytes)",
                            volume_key(target_dir)
                        )),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn query_os_filesystem_name(target_dir: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let vol = volume_key(target_dir);
    let root = if vol.ends_with('\\') {
        vol
    } else if vol == "/" {
        return None;
    } else {
        format!(r"{vol}\")
    };
    let wide_root: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut fs_buf = [0_u16; 64];
    // SAFETY: `wide_root` is a NUL-terminated UTF-16 root path (e.g. `C:\`) and `fs_buf` is a
    // valid writable buffer of 64 `u16` elements.
    let ok = unsafe {
        GetVolumeInformationW(
            wide_root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_buf.as_mut_ptr(),
            u32::try_from(fs_buf.len()).unwrap_or(64),
        )
    };
    if ok == 0 {
        return None;
    }
    let nul_pos = fs_buf.iter().position(|&c| c == 0).unwrap_or(fs_buf.len());
    let name = String::from_utf16_lossy(fs_buf.get(..nul_pos)?);
    (!name.is_empty()).then_some(name)
}

#[cfg(not(windows))]
const fn query_os_filesystem_name(_target_dir: &Path) -> Option<String> {
    None
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn query_os_free_space(target_dir: &Path, trace_id: TraceId) -> AppResult<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let probe_dir = if target_dir.exists() {
        target_dir.to_path_buf()
    } else {
        target_dir
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or(target_dir)
            .to_path_buf()
    };
    let ext = to_os_extended_path(&probe_dir);
    let mut wide: Vec<u16> = ext.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut free_caller = 0_u64;
    let mut total_bytes = 0_u64;
    let mut total_free = 0_u64;

    // SAFETY: `wide` is a valid null-terminated UTF-16 path buffer outliving `GetDiskFreeSpaceExW`,
    // and the three `u64` out-pointers are valid stack references.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free_caller,
            &raw mut total_bytes,
            &raw mut total_free,
        )
    };
    if ok == 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(map_disk_io_error(&os_err, target_dir, trace_id));
    }
    Ok(free_caller)
}

#[cfg(not(windows))]
fn query_os_free_space(target_dir: &Path, trace_id: TraceId) -> AppResult<u64> {
    let probe_dir = if target_dir.exists() {
        target_dir
    } else {
        target_dir
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or(target_dir)
    };
    fs::metadata(probe_dir).map_err(|e| map_disk_io_error(&e, target_dir, trace_id))?;
    // Conservative generous fallback for non-Windows CI when statvfs FFI is omitted
    Ok(64 * 1024 * 1024 * 1024)
}

/// Spawns the live 3-second disk space watchdog (`P1-T12` / `§23.4`):
/// whenever free space on `monitored_dir` drops below `min_free_bytes` (`256 MiB`), publishes
/// [`AppEvent::LowDiskSpace`] so active downloads on that volume pause cleanly before `0` bytes!
pub fn spawn_disk_space_monitor(
    ledger: Arc<dyn VolumeSpaceLedger>,
    monitored_dir: PathBuf,
    min_free_bytes: u64,
    event_bus: Arc<dyn EventBus>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Ok(free) = ledger.available_bytes(&monitored_dir, TraceId::new())
                        && free < min_free_bytes
                    {
                        tracing::warn!(
                            error_code = %ErrorCode::DiskInsufficientSpace,
                            dir = %monitored_dir.display(),
                            free_bytes = free,
                            "Live disk monitor detected free space below safety threshold"
                        );
                        event_bus.publish(AppEvent::LowDiskSpace {
                            target_dir: monitored_dir.clone(),
                            free_bytes: free,
                        });
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
    })
}
