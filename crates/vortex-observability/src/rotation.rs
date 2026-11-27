//! Non-blocking size-rotating JSONL log sink with streaming gzip compression, 14-day retention,
//! sink-level Defense-in-Depth redaction, and `O(1)` metadata priority routing (`P0-T11` / §22.2).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use flate2::Compression;
use flate2::write::GzEncoder;
#[cfg(feature = "full")]
use tracing::{Level, Metadata};

#[cfg(feature = "full")]
use crate::redaction::{might_contain_unredacted_secret, redact_sensitive_text};

/// Default per-file rotation limit: `10 MiB` (`10 MiB × 5 = 50 MiB` cap).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Default maximum rotated archives kept alongside the active file.
pub const DEFAULT_MAX_FILES: usize = 5;

/// Default retention window for archived log files (`14 days`).
pub const DEFAULT_RETENTION_DAYS: u64 = 14;

/// Bounded queue depth for non-blocking log delivery.
const DEFAULT_QUEUE_CAPACITY: usize = 4_096;

/// Reserve head-room slots exclusively for `ERROR`/`WARN` events under flood pressure.
const PRIORITY_RESERVE_THRESHOLD: usize = 512;

/// Configuration for [`SizeRotatingFile`] and [`NonBlockingLogSink`].
#[derive(Debug, Clone)]
pub struct RotatingFileConfig {
    /// Directory where log files are written.
    pub log_dir: PathBuf,
    /// Base filename stem (e.g. `"core"` → `core.jsonl`).
    pub file_stem: String,
    /// Maximum size of the active `.jsonl` file before rotating.
    pub max_file_bytes: u64,
    /// Maximum number of log files (active + compressed archives).
    pub max_files: usize,
    /// Maximum age of archived `.jsonl.gz` files in days.
    pub retention_days: u64,
}

impl RotatingFileConfig {
    /// Builds a configuration with Vortex production defaults (`10 MiB × 5`, `14 days`).
    #[must_use]
    pub fn production_defaults(log_dir: PathBuf, file_stem: impl Into<String>) -> Self {
        Self {
            log_dir,
            file_stem: file_stem.into(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }

    /// Path to the active JSONL file.
    #[must_use]
    pub fn active_path(&self) -> PathBuf {
        self.log_dir.join(format!("{}.jsonl", self.file_stem))
    }

    /// Path to rotated compressed archive slot `index` (`1..max_files`).
    #[must_use]
    pub fn archive_path(&self, index: usize) -> PathBuf {
        self.log_dir
            .join(format!("{}.{index}.jsonl.gz", self.file_stem))
    }
}

/// Synchronous size-rotating log file writer that streams gzip rotation without RAM spikes.
pub struct SizeRotatingFile {
    config: RotatingFileConfig,
    file: Option<File>,
    current_bytes: u64,
}

impl SizeRotatingFile {
    /// Opens or creates the active JSONL file and prunes expired archives.
    ///
    /// # Errors
    /// Returns an I/O error if the log directory or file cannot be opened.
    pub fn open(config: RotatingFileConfig) -> io::Result<Self> {
        fs::create_dir_all(&config.log_dir)?;
        prune_expired_archives(&config);

        let active = config.active_path();
        let file = OpenOptions::new().create(true).append(true).open(&active)?;
        let current_bytes = file.metadata()?.len();

        Ok(Self {
            config,
            file: Some(file),
            current_bytes,
        })
    }

    /// Writes `bytes` to the active file, rotating and streaming-gzipping when `max_file_bytes` is reached.
    ///
    /// # Errors
    /// Returns an I/O error if writing or rotating fails.
    pub fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.current_bytes > 0
            && self.current_bytes.saturating_add(bytes.len() as u64) > self.config.max_file_bytes
        {
            self.rotate()?;
        }

        if let Some(ref mut f) = self.file {
            f.write_all(bytes)?;
            self.current_bytes = self.current_bytes.saturating_add(bytes.len() as u64);
        }
        Ok(())
    }

    /// Flushes OS buffers for the active log file.
    ///
    /// # Errors
    /// Returns an I/O error if flushing fails.
    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(ref mut f) = self.file {
            f.flush()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Flush and explicitly close active File handle BEFORE reading/truncating (Windows handle hygiene).
        if let Some(mut old_handle) = self.file.take() {
            old_handle.flush()?;
        }

        let active = self.config.active_path();
        let max_archives = self.config.max_files.saturating_sub(1);

        if max_archives > 0 {
            let oldest = self.config.archive_path(max_archives);
            if oldest.exists() {
                let _ = fs::remove_file(&oldest);
            }

            for slot in (1..max_archives).rev() {
                let src = self.config.archive_path(slot);
                let dst = self.config.archive_path(slot + 1);
                if src.exists() {
                    let _ = fs::rename(&src, &dst);
                }
            }

            // Stream-compress active file into slot 1 (.1.jsonl.gz) with an 8 KiB buffer (zero 10 MiB heap spike)
            let src_file = File::open(&active)?;
            let mut reader = BufReader::with_capacity(8 * 1024, src_file);
            let first_archive = self.config.archive_path(1);
            let archive_file = File::create(&first_archive)?;
            let mut encoder = GzEncoder::new(archive_file, Compression::default());
            io::copy(&mut reader, &mut encoder)?;
            let mut finished = encoder.finish()?;
            finished.flush()?;
        }

        let reopened = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&active)?;
        self.file = Some(reopened);
        self.current_bytes = 0;

        prune_expired_archives(&self.config);
        Ok(())
    }
}

fn prune_expired_archives(config: &RotatingFileConfig) {
    let max_age = Duration::from_secs(config.retention_days.saturating_mul(86_400));
    let now = SystemTime::now();

    let Ok(entries) = fs::read_dir(&config.log_dir) else {
        return;
    };

    let prefix = format!("{}.", config.file_stem);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) || Path::new(name).extension().is_none_or(|e| e != "gz") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && let Ok(age) = now.duration_since(modified)
            && age > max_age
        {
            let _ = fs::remove_file(&path);
        }
    }
}

enum LogCommand {
    Record(Vec<u8>),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// Drains every currently queued record across both priority and normal channels before flushing.
///
/// Returns `true` if a `Shutdown` command was encountered during draining.
fn drain_all_pending(
    high_rx: &Receiver<LogCommand>,
    normal_rx: &Receiver<LogCommand>,
    rotating: &mut SizeRotatingFile,
) -> bool {
    let mut pending_acks = Vec::new();
    let mut should_shutdown = false;

    loop {
        let mut progressed = false;
        while let Ok(cmd) = high_rx.try_recv() {
            progressed = true;
            match cmd {
                LogCommand::Record(buf) => {
                    let _ = rotating.write_record(&buf);
                }
                LogCommand::Flush(ack) => pending_acks.push(ack),
                LogCommand::Shutdown => should_shutdown = true,
            }
        }
        while let Ok(cmd) = normal_rx.try_recv() {
            progressed = true;
            if let LogCommand::Record(buf) = cmd {
                let _ = rotating.write_record(&buf);
            }
        }
        if !progressed {
            break;
        }
    }

    let _ = rotating.flush();
    for ack in pending_acks {
        let _ = ack.send(());
    }
    should_shutdown
}

/// Non-blocking bounded log sink ensuring logging NEVER stalls Tokio or hot-path download loops.
///
/// Under extreme log floods, low-severity events (`INFO`/`DEBUG`/`TRACE`) are dropped first while
/// preserving headroom for `ERROR`/`WARN`, and every dropped event increments `dropped_log_events`.
#[derive(Clone)]
pub struct NonBlockingLogSink {
    high_tx: SyncSender<LogCommand>,
    normal_tx: SyncSender<LogCommand>,
    dropped_counter: Arc<AtomicU64>,
}

/// RAII guard that flushes and joins the background log writer thread on drop.
pub struct LogWorkerGuard {
    high_tx: SyncSender<LogCommand>,
    worker: Option<JoinHandle<()>>,
}

impl LogWorkerGuard {
    /// Synchronously flushes all queued high- and normal-priority log entries to disk.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.high_tx.send(LogCommand::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for LogWorkerGuard {
    fn drop(&mut self) {
        self.flush();
        let _ = self.high_tx.send(LogCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl NonBlockingLogSink {
    /// Spawns a dedicated background I/O thread backed by [`SizeRotatingFile`].
    ///
    /// # Errors
    /// Returns an I/O error if the log file cannot be opened or the thread cannot be spawned.
    pub fn spawn(
        config: RotatingFileConfig,
        dropped_counter: Arc<AtomicU64>,
    ) -> io::Result<(Self, LogWorkerGuard)> {
        let mut rotating = SizeRotatingFile::open(config)?;
        let (high_tx, high_rx) = mpsc::sync_channel::<LogCommand>(PRIORITY_RESERVE_THRESHOLD);
        let (normal_tx, normal_rx) = mpsc::sync_channel::<LogCommand>(DEFAULT_QUEUE_CAPACITY);

        let worker = thread::Builder::new()
            .name("vortex-log-writer".to_owned())
            .spawn(move || {
                loop {
                    if drain_all_pending(&high_rx, &normal_rx, &mut rotating) {
                        return;
                    }

                    match high_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(LogCommand::Record(buf)) => {
                            let _ = rotating.write_record(&buf);
                        }
                        Ok(LogCommand::Flush(ack)) => {
                            let shutdown = drain_all_pending(&high_rx, &normal_rx, &mut rotating);
                            let _ = ack.send(());
                            if shutdown {
                                return;
                            }
                        }
                        Ok(LogCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = drain_all_pending(&high_rx, &normal_rx, &mut rotating);
                            return;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            })?;

        let sink = Self {
            high_tx: high_tx.clone(),
            normal_tx,
            dropped_counter,
        };
        let guard = LogWorkerGuard {
            high_tx,
            worker: Some(worker),
        };
        Ok((sink, guard))
    }

    /// Enqueues a single pre-formatted JSONL line without ever blocking the caller.
    pub fn send_line(&self, line: Vec<u8>, high_priority: bool) {
        let sender = if high_priority {
            &self.high_tx
        } else {
            &self.normal_tx
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            sender.try_send(LogCommand::Record(line))
        {
            self.dropped_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Total number of log events dropped due to bounded queue saturation.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped_counter.load(Ordering::Relaxed)
    }
}

/// `tracing_subscriber::fmt::MakeWriter` adapter routing lines into [`NonBlockingLogSink`]
/// with exact `O(1)` metadata priority routing and sink-level Defense-in-Depth redaction.
#[cfg(feature = "full")]
pub struct SinkLineWriter {
    sink: NonBlockingLogSink,
    high_priority: bool,
    buf: Vec<u8>,
}

#[cfg(feature = "full")]
impl io::Write for SinkLineWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "full")]
impl Drop for SinkLineWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let mut payload = std::mem::take(&mut self.buf);
        // Sink-level Defense-in-Depth: zero allocation when clean, full scrub if any secret pattern appears!
        if might_contain_unredacted_secret(&payload)
            && let Ok(raw_str) = std::str::from_utf8(&payload)
        {
            let has_trailing_newline = raw_str.ends_with('\n');
            let mut scrubbed = redact_sensitive_text(raw_str);
            if has_trailing_newline && !scrubbed.ends_with('\n') {
                scrubbed.push('\n');
            }
            payload = scrubbed.into_bytes();
        }
        self.sink.send_line(payload, self.high_priority);
    }
}

#[cfg(feature = "full")]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for NonBlockingLogSink {
    type Writer = SinkLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SinkLineWriter {
            sink: self.clone(),
            high_priority: false,
            buf: Vec::with_capacity(256),
        }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        SinkLineWriter {
            sink: self.clone(),
            high_priority: *meta.level() <= Level::WARN,
            buf: Vec::with_capacity(256),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn size_rotating_file_rotates_and_streams_gzip_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RotatingFileConfig {
            log_dir: tmp.path().to_path_buf(),
            file_stem: "core".to_owned(),
            max_file_bytes: 120,
            max_files: 3,
            retention_days: 14,
        };
        let mut writer = SizeRotatingFile::open(cfg.clone()).unwrap();

        for i in 0..8 {
            let line = format!("{{\"seq\":{i},\"msg\":\"payload-line-for-rotation-test\"}}\n");
            writer.write_record(line.as_bytes()).unwrap();
        }
        writer.flush().unwrap();

        assert!(cfg.active_path().is_file());
        assert!(cfg.archive_path(1).is_file());
        assert!(cfg.archive_path(2).is_file());
        assert!(!cfg.archive_path(3).exists());

        let gz_file = File::open(cfg.archive_path(1)).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(gz_file);
        let mut content = String::new();
        io::Read::read_to_string(&mut decoder, &mut content).unwrap();
        assert!(content.contains("payload-line-for-rotation-test"));
    }
}
