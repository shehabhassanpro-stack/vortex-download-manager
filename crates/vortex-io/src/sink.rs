//! Sparse positional `FileSink` (`FSCTL_SET_SPARSE`), true concurrent `FILE_FLAG_OVERLAPPED`
//! positional I/O (`P1.7-T02` / `ADR-021`), non-fatal `SetSparse = FALSE`,
//! `ERROR_SHARING_VIOLATION` jittered rename retry, and atomic `.vortex.meta` sidecar updates
//! (`P1-T11` & `P1-T12` / `ADR-007` & `ADR-008`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use vortex_application::{
    AppResult, FileSink, FileSinkFactory, SidecarMetadata, VolumeSpaceLedger,
};
use vortex_domain::HashAlgorithm;
use vortex_error::{ErrorCode, TraceId, VortexError};

use crate::failpoints::check_failpoint;
use crate::slab::AlignedSlabPool;
use crate::space::{VolumeSpaceManager, map_disk_io_error, to_os_extended_path};

struct SinkFileState {
    file: Option<File>,
    is_sparse: bool,
}

#[cfg(windows)]
#[allow(unsafe_code)]
struct OverlappedEventPool {
    events: std::sync::Mutex<Vec<isize>>,
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl OverlappedEventPool {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::with_capacity(16)),
        }
    }

    fn checkout(&self, trace_id: TraceId) -> AppResult<isize> {
        use windows_sys::Win32::Foundation::{FALSE, TRUE};
        use windows_sys::Win32::System::Threading::CreateEventW;

        if let Ok(mut guard) = self.events.lock()
            && let Some(ev) = guard.pop()
        {
            return Ok(ev);
        }
        // SAFETY: Creating a local unnamed manual-reset Win32 Event (`bManualReset = TRUE`, `bInitialState = FALSE`).
        let handle = unsafe { CreateEventW(std::ptr::null(), TRUE, FALSE, std::ptr::null()) };
        if handle.is_null() {
            return Err(VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                "CreateEventW failed for OVERLAPPED disk I/O",
            ));
        }
        Ok(handle as isize)
    }

    fn checkin(&self, ev: isize) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

        if let Ok(mut guard) = self.events.lock()
            && guard.len() < 32
        {
            guard.push(ev);
            return;
        }
        // SAFETY: `ev` is an owned Win32 Event handle being discarded because pool is full.
        unsafe {
            let _ = CloseHandle(ev as HANDLE);
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Drop for OverlappedEventPool {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

        if let Ok(mut guard) = self.events.lock() {
            for ev in guard.drain(..) {
                // SAFETY: Every pooled `ev` is an owned Win32 Event handle closed exactly once on Drop.
                unsafe {
                    let _ = CloseHandle(ev as HANDLE);
                }
            }
        }
    }
}

/// Positional sparse file sink writing directly to `<file>.vortex` with zero assembling phase,
/// zero synchronous NTFS Zero-Fill (`ADR-007`), and true concurrent `FILE_FLAG_OVERLAPPED`
/// positional I/O across parallel workers (`P1.7-T02` / `ADR-021`).
pub struct SparseFileSink {
    temp_path: PathBuf,
    meta_path: PathBuf,
    _pool: Arc<AlignedSlabPool>,
    inner: Arc<RwLock<SinkFileState>>,
    write_syscalls: Arc<AtomicU64>,
    written_bytes: Arc<AtomicU64>,
    #[cfg(windows)]
    event_pool: Arc<OverlappedEventPool>,
}

impl std::fmt::Debug for SparseFileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SparseFileSink")
            .field("temp_path", &self.temp_path)
            .field("meta_path", &self.meta_path)
            .field(
                "write_syscalls",
                &self.write_syscalls.load(Ordering::Relaxed),
            )
            .field("written_bytes", &self.written_bytes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SparseFileSink {
    /// Opens or creates a `.vortex` temporary file sink backed by `pool` (`FILE_FLAG_OVERLAPPED` on Windows).
    ///
    /// # Errors
    /// Returns a typed `VTX-DISK-*` or `VTX-SEC-001` error if parent directories or `.vortex` cannot be opened.
    pub fn open_sync(
        temp_path: &Path,
        meta_path: &Path,
        pool: Arc<AlignedSlabPool>,
        trace_id: TraceId,
    ) -> AppResult<Self> {
        if let Some(parent) = temp_path.parent() {
            reject_dir_symlink_or_junction(parent, trace_id)?;
            let ext_parent = to_os_extended_path(parent);
            fs::create_dir_all(&ext_parent).map_err(|e| map_disk_io_error(&e, parent, trace_id))?;
            reject_dir_symlink_or_junction(parent, trace_id)?;
        }
        reject_file_reparse_point(meta_path, trace_id)?;
        let file = open_regular_file_no_reparse(temp_path, true, false, true, trace_id)?;

        Ok(Self {
            temp_path: temp_path.to_path_buf(),
            meta_path: meta_path.to_path_buf(),
            _pool: pool,
            inner: Arc::new(RwLock::new(SinkFileState {
                file: Some(file),
                is_sparse: false,
            })),
            write_syscalls: Arc::new(AtomicU64::new(0)),
            written_bytes: Arc::new(AtomicU64::new(0)),
            #[cfg(windows)]
            event_pool: Arc::new(OverlappedEventPool::new()),
        })
    }
}

#[async_trait]
impl FileSink for SparseFileSink {
    async fn prepare(&self, total_size: Option<u64>, trace_id: TraceId) -> AppResult<bool> {
        check_failpoint("io.prepare", trace_id)?;
        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        #[cfg(windows)]
        let event_pool = Arc::clone(&self.event_pool);

        tokio::task::spawn_blocking(move || {
            let mut guard = inner.write().map_err(|_| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    "FileSink rwlock poisoned",
                )
            })?;
            let sparse_supported = {
                let Some(file) = guard.file.as_ref() else {
                    return Err(VortexError::coded(
                        ErrorCode::BugInternalIsolated,
                        trace_id,
                        "FileSink handle already closed",
                    ));
                };
                #[cfg(windows)]
                let ok = enable_sparse_if_supported(file, &event_pool);
                #[cfg(not(windows))]
                let ok = enable_sparse_if_supported(file);
                if ok
                    && let Some(sz) = total_size
                    && sz > 0
                {
                    let current_len = file.metadata().map_or(0, |m| m.len());
                    if current_len < sz {
                        file.set_len(sz)
                            .map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;
                    }
                }
                ok
            };
            guard.is_sparse = sparse_supported;
            Ok(sparse_supported)
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("prepare blocking task panicked: {e}"),
            )
        })?
    }

    async fn write_at(&self, offset: u64, data: Bytes, trace_id: TraceId) -> AppResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        check_failpoint("io.write_at", trace_id)?;

        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        let syscalls = Arc::clone(&self.write_syscalls);
        let written_counter = Arc::clone(&self.written_bytes);
        #[cfg(windows)]
        let event_pool = Arc::clone(&self.event_pool);

        tokio::task::spawn_blocking(move || {
            // SHARED read lock: multiple workers execute parallel non-overlapping `WriteFile`
            // operations simultaneously into NTFS/NVMe Multi-Queue with ZERO mutual exclusion (`ADR-021`)!
            let guard = inner.read().map_err(|_| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    "FileSink rwlock poisoned",
                )
            })?;
            let Some(file) = guard.file.as_ref() else {
                return Err(VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    "FileSink handle already closed",
                ));
            };

            let len = data.len() as u64;
            #[cfg(windows)]
            let calls =
                write_all_overlapped(file, &data, offset, &event_pool, &temp_path, trace_id)?;
            #[cfg(not(windows))]
            let calls = {
                write_all_positional(file, &data, offset, &temp_path, trace_id)?;
                1_u64
            };
            syscalls.fetch_add(calls, Ordering::Relaxed);
            written_counter.fetch_add(len, Ordering::Relaxed);
            Ok(())
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("write_at blocking task panicked: {e}"),
            )
        })?
    }

    async fn flush_durable(&self, sidecar: &SidecarMetadata, trace_id: TraceId) -> AppResult<()> {
        check_failpoint("io.before_flush", trace_id)?;
        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        let meta_path = self.meta_path.clone();
        let sidecar_json = serde_json::to_vec_pretty(sidecar).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed serializing .vortex.meta: {e}"),
            )
        })?;

        tokio::task::spawn_blocking(move || {
            {
                // EXCLUSIVE write lock acts as a Flush Barrier: waits for every in-flight parallel
                // overlapped `WriteFile` to finish before calling `FlushFileBuffers` (`ADR-008`)!
                let guard = inner.write().map_err(|_| {
                    VortexError::coded(
                        ErrorCode::BugInternalIsolated,
                        trace_id,
                        "FileSink rwlock poisoned",
                    )
                })?;
                let Some(file) = guard.file.as_ref() else {
                    return Err(VortexError::coded(
                        ErrorCode::BugInternalIsolated,
                        trace_id,
                        "FileSink handle already closed",
                    ));
                };
                // GOLDEN RULE (`ADR-008`): `FlushFileBuffers` (`sync_data`) BEFORE SQLite / `.meta`!
                file.sync_data()
                    .map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;
            }

            check_failpoint("io.after_flush_before_sidecar", trace_id)?;
            write_sidecar_atomic(&meta_path, &sidecar_json, trace_id)?;
            Ok(())
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("flush_durable blocking task panicked: {e}"),
            )
        })?
    }

    async fn read_at(&self, offset: u64, len: usize, trace_id: TraceId) -> AppResult<Bytes> {
        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        tokio::task::spawn_blocking(move || {
            // Drain any concurrent in-flight writes before reading prefix bytes
            let _guard = inner.write().map_err(|_| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    "FileSink rwlock poisoned",
                )
            })?;
            let ext_temp = to_os_extended_path(&temp_path);
            let mut reader =
                File::open(&ext_temp).map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;
            let mut buf = vec![0_u8; len];
            let n = reader
                .read(&mut buf)
                .map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;
            buf.truncate(n);
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("read_at task panicked: {e}"),
            )
        })?
    }

    async fn verify_hash(&self, algorithm: HashAlgorithm, trace_id: TraceId) -> AppResult<String> {
        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        tokio::task::spawn_blocking(move || {
            {
                let _guard = inner.write().map_err(|_| {
                    VortexError::coded(
                        ErrorCode::BugInternalIsolated,
                        trace_id,
                        "FileSink rwlock poisoned",
                    )
                })?;
            }
            compute_file_digest(&temp_path, algorithm, trace_id)
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("verify_hash task panicked: {e}"),
            )
        })?
    }

    async fn finalize(&self, final_path: &Path, trace_id: TraceId) -> AppResult<u64> {
        let inner = Arc::clone(&self.inner);
        let temp_path = self.temp_path.clone();
        let meta_path = self.meta_path.clone();
        let dest_path = final_path.to_path_buf();
        #[cfg(windows)]
        let event_pool = Arc::clone(&self.event_pool);

        tokio::task::spawn_blocking(move || {
            {
                let mut guard = inner.write().map_err(|_| {
                    VortexError::coded(
                        ErrorCode::BugInternalIsolated,
                        trace_id,
                        "FileSink rwlock poisoned",
                    )
                })?;
                if let Some(file) = guard.file.take() {
                    file.sync_data()
                        .map_err(|e| map_disk_io_error(&e, &temp_path, trace_id))?;

                    // Attempt non-fatal `SetSparse = FALSE` (`ADR-007` & `P1-T11`!)
                    if guard.is_sparse {
                        let simulated_refusal =
                            check_failpoint("io.set_sparse_false_refused", trace_id).is_err();
                        #[cfg(windows)]
                        let cleared = !simulated_refusal
                            && clear_sparse_attribute_non_fatal(&file, &temp_path, &event_pool);
                        #[cfg(not(windows))]
                        let cleared = !simulated_refusal
                            && clear_sparse_attribute_non_fatal(&file, &temp_path);
                        if !cleared {
                            tracing::warn!(
                                %trace_id,
                                path = %temp_path.display(),
                                "NTFS declined clearing sparse flag (SetSparse=FALSE); proceeding safely with atomic rename"
                            );
                        }
                    }
                    // Explicitly drop `file` BEFORE `fs::rename` on Windows!
                    drop(file);
                }
            }

            rename_with_av_lock_retry(&temp_path, &dest_path, trace_id)?;
            let _ = fs::remove_file(to_os_extended_path(&meta_path));

            let ext_dest = to_os_extended_path(&dest_path);
            let final_len = fs::metadata(&ext_dest)
                .map_err(|e| map_disk_io_error(&e, &dest_path, trace_id))?
                .len();
            Ok(final_len)
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("finalize task panicked: {e}"),
            )
        })?
    }

    fn write_syscall_count(&self) -> u64 {
        self.write_syscalls.load(Ordering::Relaxed)
    }

    fn total_bytes_written(&self) -> u64 {
        self.written_bytes.load(Ordering::Relaxed)
    }
}

/// Default [`FileSinkFactory`] sharing a [`VolumeSpaceManager`] and [`AlignedSlabPool`].
#[derive(Debug, Clone)]
pub struct SparseFileSinkFactory {
    space_manager: Arc<VolumeSpaceManager>,
    slab_pool: Arc<AlignedSlabPool>,
}

impl SparseFileSinkFactory {
    /// Creates a new [`SparseFileSinkFactory`].
    #[must_use]
    pub fn new(space_manager: Arc<VolumeSpaceManager>, slab_pool: Arc<AlignedSlabPool>) -> Self {
        Self {
            space_manager,
            slab_pool,
        }
    }

    /// Accesses the concrete [`VolumeSpaceManager`].
    #[must_use]
    pub fn volume_space_manager(&self) -> &Arc<VolumeSpaceManager> {
        &self.space_manager
    }
}

impl Default for SparseFileSinkFactory {
    fn default() -> Self {
        Self::new(
            Arc::new(VolumeSpaceManager::new()),
            Arc::new(AlignedSlabPool::default()),
        )
    }
}

#[async_trait]
impl FileSinkFactory for SparseFileSinkFactory {
    async fn open(
        &self,
        temp_path: &Path,
        meta_path: &Path,
        trace_id: TraceId,
    ) -> AppResult<Arc<dyn FileSink>> {
        let t_path = temp_path.to_path_buf();
        let m_path = meta_path.to_path_buf();
        let pool = Arc::clone(&self.slab_pool);
        let sink = tokio::task::spawn_blocking(move || {
            SparseFileSink::open_sync(&t_path, &m_path, pool, trace_id)
        })
        .await
        .map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("open sink panicked: {e}"),
            )
        })??;
        Ok(Arc::new(sink))
    }

    fn space_ledger(&self) -> Arc<dyn VolumeSpaceLedger> {
        Arc::clone(&self.space_manager) as Arc<dyn VolumeSpaceLedger>
    }
}

fn write_sidecar_atomic(meta_path: &Path, payload: &[u8], trace_id: TraceId) -> AppResult<()> {
    let tmp_meta = meta_path.with_extension("meta.tmp");
    reject_file_reparse_point(&tmp_meta, trace_id)?;
    reject_file_reparse_point(meta_path, trace_id)?;
    let ext_tmp = to_os_extended_path(&tmp_meta);
    let ext_meta = to_os_extended_path(meta_path);
    {
        let mut f = open_regular_file_no_reparse(&tmp_meta, false, true, false, trace_id)?;
        f.write_all(payload)
            .map_err(|e| map_disk_io_error(&e, meta_path, trace_id))?;
        f.sync_data()
            .map_err(|e| map_disk_io_error(&e, meta_path, trace_id))?;
    }
    fs::rename(&ext_tmp, &ext_meta).map_err(|e| map_disk_io_error(&e, meta_path, trace_id))?;
    Ok(())
}

/// Bounded 5-attempt jittered retry for `ERROR_SHARING_VIOLATION` (`Win32 32` — AV / Search Indexer lock, `§23.4`).
fn rename_with_av_lock_retry(src: &Path, dst: &Path, trace_id: TraceId) -> AppResult<()> {
    reject_file_reparse_point(src, trace_id)?;
    reject_file_reparse_point(dst, trace_id)?;
    if let Some(dst_parent) = dst.parent() {
        reject_dir_symlink_or_junction(dst_parent, trace_id)?;
    }
    let ext_src = to_os_extended_path(src);
    let ext_dst = to_os_extended_path(dst);
    let delays_ms = [45_u64, 95, 180, 310, 480];

    for (attempt, &delay_ms) in delays_ms.iter().enumerate() {
        if check_failpoint("io.rename_sharing_violation", trace_id).is_err() {
            tracing::warn!(
                error_code = %ErrorCode::DiskFileLocked,
                %trace_id,
                attempt = attempt + 1,
                "Simulated ERROR_SHARING_VIOLATION during atomic rename; retrying"
            );
            std::thread::sleep(Duration::from_millis(delay_ms));
            continue;
        }

        match fs::rename(&ext_src, &ext_dst) {
            Ok(()) => return Ok(()),
            Err(err) if is_sharing_violation(&err) => {
                tracing::warn!(
                    error_code = %ErrorCode::DiskFileLocked,
                    %trace_id,
                    attempt = attempt + 1,
                    "File temporarily locked by AV/Explorer during rename; retrying"
                );
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            Err(other) => return Err(map_disk_io_error(&other, dst, trace_id)),
        }
    }

    // Final attempt after 5 retries
    if check_failpoint("io.rename_sharing_violation", trace_id).is_err() {
        return Err(VortexError::coded(
            ErrorCode::DiskFileLocked,
            trace_id,
            "Rename remained locked by ERROR_SHARING_VIOLATION after 5 retries",
        ));
    }
    fs::rename(&ext_src, &ext_dst).map_err(|e| map_disk_io_error(&e, dst, trace_id))
}

fn is_sharing_violation(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(32 | 33))
}

fn compute_file_digest(
    path: &Path,
    algorithm: HashAlgorithm,
    trace_id: TraceId,
) -> AppResult<String> {
    use sha2::Digest as _;

    let ext = to_os_extended_path(path);
    let mut file = File::open(&ext).map_err(|e| map_disk_io_error(&e, path, trace_id))?;
    let mut buf = vec![0_u8; 1024 * 1024];

    match algorithm {
        HashAlgorithm::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
                if n == 0 {
                    break;
                }
                if let Some(chunk) = buf.get(..n) {
                    hasher.update(chunk);
                }
            }
            Ok(hasher.finalize().to_hex().to_string())
        }
        HashAlgorithm::Sha256 => {
            let mut hasher = sha2::Sha256::new();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
                if n == 0 {
                    break;
                }
                if let Some(chunk) = buf.get(..n) {
                    hasher.update(chunk);
                }
            }
            Ok(hex_lower(&hasher.finalize()))
        }
        HashAlgorithm::Sha1 => {
            let mut hasher = sha1::Sha1::new();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
                if n == 0 {
                    break;
                }
                if let Some(chunk) = buf.get(..n) {
                    hasher.update(chunk);
                }
            }
            Ok(hex_lower(&hasher.finalize()))
        }
        HashAlgorithm::Md5 => {
            let mut hasher = md5::Md5::new();
            loop {
                let n = file
                    .read(&mut buf)
                    .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
                if n == 0 {
                    break;
                }
                if let Some(chunk) = buf.get(..n) {
                    hasher.update(chunk);
                }
            }
            Ok(hex_lower(&hasher.finalize()))
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn write_all_overlapped(
    file: &File,
    mut buf: &[u8],
    mut offset: u64,
    event_pool: &OverlappedEventPool,
    target: &Path,
    trace_id: TraceId,
) -> AppResult<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE, TRUE};
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    let raw_handle = file.as_raw_handle();
    let event = event_pool.checkout(trace_id)?;
    let mut syscalls = 0_u64;

    let write_result = (|| -> AppResult<u64> {
        while !buf.is_empty() {
            let chunk_len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            // SAFETY: `OVERLAPPED` is a plain Win32 POD struct; zero-init clears `Internal`, `InternalHigh`, and `Pointer`.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.Anonymous.Anonymous.Offset = (offset & 0xFFFF_FFFF) as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
            ov.hEvent = event as HANDLE;

            let mut transferred = 0_u32;
            syscalls += 1;
            // SAFETY: `raw_handle` is a live `FILE_FLAG_OVERLAPPED` file handle, `buf` is valid for
            // `chunk_len` bytes, and `ov` remains pinned on stack until `GetOverlappedResult` completes!
            let ok = unsafe {
                WriteFile(
                    raw_handle,
                    buf.as_ptr(),
                    chunk_len,
                    &raw mut transferred,
                    &raw mut ov,
                )
            };
            if ok == 0 {
                // SAFETY: `GetLastError` reads the calling thread's Win32 error slot.
                let err_code = unsafe { GetLastError() };
                if err_code == ERROR_IO_PENDING {
                    // SAFETY: `ov` is live on stack and `bWait = TRUE` blocks until the kernel finishes referencing `buf` & `ov`.
                    let waited = unsafe {
                        GetOverlappedResult(raw_handle, &raw const ov, &raw mut transferred, TRUE)
                    };
                    if waited == 0 {
                        let io_err = std::io::Error::last_os_error();
                        return Err(map_disk_io_error(&io_err, target, trace_id));
                    }
                } else {
                    let io_err =
                        std::io::Error::from_raw_os_error(i32::try_from(err_code).unwrap_or(5));
                    return Err(map_disk_io_error(&io_err, target, trace_id));
                }
            }
            if transferred == 0 {
                return Err(VortexError::coded(
                    ErrorCode::DiskInsufficientSpace,
                    trace_id,
                    "WriteFile(OVERLAPPED) wrote 0 bytes",
                ));
            }
            let written = transferred as usize;
            offset += written as u64;
            buf = buf.get(written..).unwrap_or(&[]);
        }
        Ok(syscalls)
    })();

    event_pool.checkin(event);
    write_result
}

#[cfg(not(windows))]
fn write_all_positional(
    file: &File,
    buf: &[u8],
    offset: u64,
    target: &Path,
    trace_id: TraceId,
) -> AppResult<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
        .map_err(|e| map_disk_io_error(&e, target, trace_id))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn device_io_control_overlapped(
    raw_handle: windows_sys::Win32::Foundation::HANDLE,
    ioctl_code: u32,
    in_buf: *const std::ffi::c_void,
    in_len: u32,
    event_pool: &OverlappedEventPool,
) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE, TRUE};
    use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};

    let Ok(event) = event_pool.checkout(TraceId::new()) else {
        return false;
    };
    // SAFETY: `OVERLAPPED` is a plain Win32 POD struct; zero-init is valid before setting `hEvent`.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event as HANDLE;
    let mut bytes_returned = 0_u32;

    // SAFETY: `raw_handle` is a live overlapped Win32 handle, `in_buf` is valid for `in_len` bytes,
    // and `ov` stays alive on stack until `GetOverlappedResult(..., TRUE)` completes.
    let ok = unsafe {
        DeviceIoControl(
            raw_handle,
            ioctl_code,
            in_buf,
            in_len,
            std::ptr::null_mut(),
            0,
            &raw mut bytes_returned,
            &raw mut ov,
        )
    };
    let success = if ok != 0 {
        true
    } else {
        // SAFETY: Reads thread-local Win32 error code immediately after `DeviceIoControl`.
        let err = unsafe { GetLastError() };
        if err == ERROR_IO_PENDING {
            // SAFETY: Waits until overlapped IOCTL completes before `ov` or `in_buf` leave scope.
            unsafe {
                GetOverlappedResult(raw_handle, &raw const ov, &raw mut bytes_returned, TRUE) != 0
            }
        } else {
            false
        }
    };
    event_pool.checkin(event);
    success
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn enable_sparse_if_supported(file: &File, event_pool: &OverlappedEventPool) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    const FILE_SUPPORTS_SPARSE_FILES_FLAG: u32 = 0x0000_0040;
    let raw_handle = file.as_raw_handle();
    let mut fs_flags = 0_u32;

    // SAFETY: `raw_handle` is a live open Win32 file handle owned by `file`, and `fs_flags` is a
    // valid stack `u32`; optional name/serial pointers are allowed to be null per Win32 docs.
    let vol_ok = unsafe {
        GetVolumeInformationByHandleW(
            raw_handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut fs_flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if vol_ok == 0 || (fs_flags & FILE_SUPPORTS_SPARSE_FILES_FLAG) == 0 {
        return false;
    }

    let set_true: u8 = 1;
    device_io_control_overlapped(
        raw_handle,
        FSCTL_SET_SPARSE,
        (&raw const set_true).cast(),
        1,
        event_pool,
    )
}

#[cfg(not(windows))]
const fn enable_sparse_if_supported(_file: &File) -> bool {
    true
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn clear_sparse_attribute_non_fatal(
    file: &File,
    path: &Path,
    event_pool: &OverlappedEventPool,
) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    // Only clear `FILE_ATTRIBUTE_SPARSE_FILE` when 100% of logical bytes are physically allocated;
    // otherwise Windows NTFS would materialize/zero-fill remaining sparse zero holes (`ADR-007`)!
    if let Ok(meta) = file.metadata() {
        let logical_len = meta.len();
        if logical_len > 0 {
            let ext = to_os_extended_path(path);
            let wide: Vec<u16> = ext
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut high = 0_u32;
            // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; `high` is a valid stack `u32`.
            let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
            let allocated = (u64::from(high) << 32) | u64::from(low);
            if allocated < logical_len {
                return false;
            }
        }
    }

    let raw_handle = file.as_raw_handle();
    let set_false: u8 = 0;
    device_io_control_overlapped(
        raw_handle,
        FSCTL_SET_SPARSE,
        (&raw const set_false).cast(),
        1,
        event_pool,
    )
}

#[cfg(not(windows))]
const fn clear_sparse_attribute_non_fatal(_file: &File, _path: &Path) -> bool {
    true
}

/// Anti-TOCTOU NTFS Reparse-Point / Symlink / Junction Guard (`TH-29`):
/// rejects opening any file handle carrying `FILE_ATTRIBUTE_REPARSE_POINT` and confirms via
/// `GetFinalPathNameByHandleW` that the opened handle truly resides inside its expected parent!
/// Optionally enables `FILE_FLAG_OVERLAPPED` for true concurrent multi-worker positional writes (`ADR-021`).
#[cfg(windows)]
#[allow(unsafe_code)]
fn open_regular_file_no_reparse(
    path: &Path,
    read: bool,
    truncate: bool,
    overlapped: bool,
    trace_id: TraceId,
) -> AppResult<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_OVERLAPPED, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    };

    reject_file_reparse_point(path, trace_id)?;
    let ext_path = to_os_extended_path(path);
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
    if overlapped {
        flags |= FILE_FLAG_OVERLAPPED;
    }
    let mut opts = OpenOptions::new();
    opts.read(read)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(flags);
    let file = opts
        .open(&ext_path)
        .map_err(|e| map_disk_io_error(&e, path, trace_id))?;

    let raw_handle = file.as_raw_handle();
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` is a plain C POD struct of integer fields where all-zeroes is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `raw_handle` is a live Win32 file handle owned by `file` and `info` is valid stack storage.
    let info_ok = unsafe { GetFileInformationByHandle(raw_handle, &raw mut info) };
    if info_ok != 0 && (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!(
                "Refused opening NTFS reparse point / symlink target (TH-29): {}",
                path.display()
            ),
        ));
    }

    // Verify canonical handle destination stays inside the expected parent directory (blocks parent Junction TOCTOU!)
    if let Some(parent) = path.parent()
        && let Ok(canon_parent) = fs::canonicalize(to_os_extended_path(parent))
    {
        let mut final_buf = vec![0_u16; 4096];
        // SAFETY: `raw_handle` is live and `final_buf` holds 4096 `u16` elements (`VOLUME_NAME_DOS = 0`).
        let len = unsafe {
            GetFinalPathNameByHandleW(
                raw_handle,
                final_buf.as_mut_ptr(),
                u32::try_from(final_buf.len()).unwrap_or(4096),
                0,
            )
        } as usize;
        if len > 0
            && len < final_buf.len()
            && let Some(slice) = final_buf.get(..len)
        {
            let final_str = String::from_utf16_lossy(slice).to_ascii_lowercase();
            let expected_prefix = canon_parent.to_string_lossy().to_ascii_lowercase();
            if !final_str.starts_with(&expected_prefix) {
                return Err(VortexError::coded(
                    ErrorCode::SecPolicyViolation,
                    trace_id,
                    format!(
                        "Opened file handle escaped expected save directory via reparse point (TH-29): {}",
                        path.display()
                    ),
                ));
            }
        }
    }

    if truncate {
        file.set_len(0)
            .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
    }

    Ok(file)
}

#[cfg(not(windows))]
fn open_regular_file_no_reparse(
    path: &Path,
    read: bool,
    truncate: bool,
    _overlapped: bool,
    trace_id: TraceId,
) -> AppResult<File> {
    reject_file_reparse_point(path, trace_id)?;
    let file = OpenOptions::new()
        .read(read)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)
        .map_err(|e| map_disk_io_error(&e, path, trace_id))?;
    Ok(file)
}

fn reject_file_reparse_point(path: &Path, trace_id: TraceId) -> AppResult<()> {
    let ext = to_os_extended_path(path);
    let Ok(meta) = fs::symlink_metadata(&ext) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!("Refused symlink file target (TH-29): {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT_U32: u32 = 0x0400;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_U32) != 0 {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                format!(
                    "Refused NTFS reparse point file target (TH-29): {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Rejects symlinks and NTFS directory junctions (`IO_REPARSE_TAG_SYMLINK` / `IO_REPARSE_TAG_MOUNT_POINT`)
/// on the download save directory while remaining compatible with `OneDrive` cloud-placeholder tags (`TH-29`).
fn reject_dir_symlink_or_junction(dir: &Path, trace_id: TraceId) -> AppResult<()> {
    let ext = to_os_extended_path(dir);
    let Ok(meta) = fs::symlink_metadata(&ext) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!(
                "Refused symlink download directory (TH-29): {}",
                dir.display()
            ),
        ));
    }
    #[cfg(windows)]
    {
        if is_windows_symlink_or_junction_dir(&ext) {
            return Err(VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                format!(
                    "Refused NTFS directory junction / mount point save directory (TH-29): {}",
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn is_windows_symlink_or_junction_dir(ext_path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, WIN32_FIND_DATAW,
    };

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    // Strip trailing backslashes so `FindFirstFileW` inspects the directory entry itself!
    let raw_str = ext_path.to_string_lossy();
    let trimmed = raw_str.trim_end_matches(['\\', '/']);
    // Volume roots like `\\?\C:` cannot be queried as directory entries by `FindFirstFileW`
    if trimmed.ends_with(':') || trimmed.is_empty() {
        return false;
    }

    let wide: Vec<u16> = std::ffi::OsStr::new(trimmed)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `WIN32_FIND_DATAW` is a plain C POD struct of integer/array fields where all-zeroes is valid.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: `wide` is a NUL-terminated UTF-16 path and `data` is valid stack storage.
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &raw mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: `handle` is valid and closed exactly once.
    unsafe {
        let _ = FindClose(handle);
    }
    if (data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) == 0 {
        return false;
    }
    matches!(
        data.dwReserved0,
        IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK
    )
}
