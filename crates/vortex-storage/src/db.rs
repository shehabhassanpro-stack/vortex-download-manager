//! SQLite WAL database manager, numbered migrations, Online Backup, `PRAGMA quick_check`
//! startup self-healing, and `.vortex.meta` sidecar re-import (`P1-T07` / `ADR-008` / `ADR-017`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, backup::Backup};
use vortex_application::{AppResult, DatabaseHealthPort, SidecarMetadata};
use vortex_domain::{
    CategoryId, Clock, Download, DownloadState, Priority, QueueId, Resource, SegmentMap, Timestamps,
};
use vortex_error::{ErrorCode, TraceId, VortexError};

/// Latest SQLite schema `user_version` supported by this build of Vortex.
pub const CURRENT_DB_SCHEMA_VERSION: u32 = 1;

/// Maximum number of rolling SQLite backup files retained alongside `vortex.db`.
pub const MAX_DB_BACKUPS: usize = 3;

const MIGRATION_V1_SQL: &str = r"
CREATE TABLE IF NOT EXISTS queues (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    max_parallel    INTEGER NOT NULL DEFAULT 3,
    schedule_json   TEXT,
    post_action     TEXT
);

CREATE TABLE IF NOT EXISTS categories (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    extensions_json TEXT NOT NULL,
    folder          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS downloads (
    id              TEXT PRIMARY KEY NOT NULL,
    url             TEXT NOT NULL,
    final_url       TEXT,
    referrer        TEXT,
    top_level_site  TEXT,
    user_agent      TEXT,
    mime_type       TEXT,
    headers_json    TEXT NOT NULL DEFAULT '{}',
    has_secrets     INTEGER NOT NULL DEFAULT 0,
    file_name       TEXT NOT NULL,
    save_dir        TEXT NOT NULL,
    total_size      INTEGER,
    downloaded      INTEGER NOT NULL DEFAULT 0,
    supports_range  INTEGER NOT NULL DEFAULT 0,
    state_slug      TEXT NOT NULL,
    state_json      TEXT NOT NULL,
    validators_json TEXT NOT NULL,
    priority        INTEGER NOT NULL DEFAULT 2,
    queue_id        TEXT NOT NULL REFERENCES queues(id),
    category_id     TEXT NOT NULL REFERENCES categories(id),
    expected_hash   TEXT,
    created_at_ms   INTEGER NOT NULL,
    started_at_ms   INTEGER,
    updated_at_ms   INTEGER NOT NULL,
    completed_at_ms INTEGER,
    quarantined     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_downloads_created ON downloads(created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state_slug);

CREATE TABLE IF NOT EXISTS segments (
    download_id     TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    start_byte      INTEGER NOT NULL,
    end_byte        INTEGER NOT NULL,
    PRIMARY KEY (download_id, start_byte)
);

CREATE TABLE IF NOT EXISTS download_events (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id        TEXT NOT NULL UNIQUE,
    download_id     TEXT NOT NULL,
    trace_id        TEXT NOT NULL,
    kind            TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    at_ms           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_download ON download_events(download_id, seq ASC);
CREATE INDEX IF NOT EXISTS idx_events_at ON download_events(at_ms);

CREATE TABLE IF NOT EXISTS encrypted_secrets (
    download_id     TEXT PRIMARY KEY NOT NULL,
    ciphertext_b64  TEXT NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);
";

/// Outcome report returned when opening and verifying `vortex.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbOpenReport {
    /// `true` if `vortex.db` failed `PRAGMA quick_check` and was auto-recovered!
    pub recovered_from_corruption: bool,
    /// Path to the quarantined `.corrupt.<ts>` file if corruption occurred.
    pub quarantined_db_path: Option<PathBuf>,
    /// Number of `.vortex.meta` sidecar downloads re-imported during recovery.
    pub sidecars_imported: usize,
    /// Schema version active after migrations.
    pub schema_version: u32,
}

/// Thread-safe SQLite WAL connection manager (`Single Writer` serialized cleanly on blocking pool).
#[derive(Debug, Clone)]
pub struct SqliteDatabase {
    db_path: PathBuf,
    backup_dir: PathBuf,
    conn: Arc<Mutex<Connection>>,
    clock: Arc<dyn Clock>,
}

impl SqliteDatabase {
    /// Opens `db_path`, executes `PRAGMA quick_check` (auto-healing from backup + `.vortex.meta`
    /// sidecars in `scan_dirs` if corrupted), backs up before migrations, and enforces schema compatibility.
    ///
    /// # Errors
    /// Returns `VTX-DB-001` if the database belongs to a newer version of Vortex (`user_version > CURRENT`)
    /// or if the filesystem cannot create `db_path`.
    pub fn open_with_self_healing(
        db_path: &Path,
        scan_dirs: &[PathBuf],
        default_download_root: &Path,
        clock: Arc<dyn Clock>,
        trace_id: TraceId,
    ) -> AppResult<(Self, DbOpenReport)> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    format!("Failed creating DB parent directory: {e}"),
                )
            })?;
        }
        let backup_dir = db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("backups");
        fs::create_dir_all(&backup_dir).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed creating DB backup directory: {e}"),
            )
        })?;

        let mut recovered = false;
        let mut quarantined_path = None;

        // Step 1: Check existing DB file integrity (`PRAGMA quick_check`)
        if db_path.exists() && !Self::verify_file_integrity(db_path) {
            recovered = true;
            let ts = clock.now_unix_ms();
            let corrupt_dest = db_path.with_extension(format!("db.corrupt.{ts}"));
            tracing::error!(
                error_code = %ErrorCode::DbCorruptOrIncompatible,
                %trace_id,
                "SQLite PRAGMA quick_check failed; quarantining corrupt DB and restoring backup"
            );
            let _ = fs::rename(db_path, &corrupt_dest);
            // Also clean stale WAL/SHM siblings of the corrupt DB
            let _ = fs::remove_file(PathBuf::from(format!("{}-wal", db_path.display())));
            let _ = fs::remove_file(PathBuf::from(format!("{}-shm", db_path.display())));
            quarantined_path = Some(corrupt_dest);

            if let Some(latest_backup) = Self::find_latest_healthy_backup(&backup_dir) {
                let _ = fs::copy(&latest_backup, db_path);
            }
        }

        let conn = Self::open_configured_connection(db_path, trace_id)?;
        let current_v = Self::read_user_version(&conn, trace_id)?;

        // Reject cleanly if opened by an older Vortex binary against a newer schema (`§30.4` / `P1-T07`)
        if current_v > CURRENT_DB_SCHEMA_VERSION {
            return Err(VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!(
                    "Database schema version ({current_v}) is newer than supported version ({CURRENT_DB_SCHEMA_VERSION}); please update Vortex"
                ),
            ));
        }

        let db = Self {
            db_path: db_path.to_path_buf(),
            backup_dir,
            conn: Arc::new(Mutex::new(conn)),
            clock,
        };

        if current_v < CURRENT_DB_SCHEMA_VERSION {
            // Take automatic backup if upgrading an existing non-empty database (`current_v > 0`)
            if current_v > 0 {
                db.create_backup_sync(trace_id)?;
            }
            db.run_migrations_sync(current_v, default_download_root, trace_id)?;
        } else {
            db.seed_defaults_sync(default_download_root, trace_id)?;
        }

        let mut sidecars_imported = 0;
        if recovered {
            sidecars_imported = db.import_sidecars_from_dirs(scan_dirs, trace_id)?;
        }

        let report = DbOpenReport {
            recovered_from_corruption: recovered,
            quarantined_db_path: quarantined_path,
            sidecars_imported,
            schema_version: CURRENT_DB_SCHEMA_VERSION,
        };

        Ok((db, report))
    }

    /// Executes a synchronous closure holding the single SQLite connection on `tokio::task::spawn_blocking`.
    ///
    /// # Errors
    /// Returns `VTX-DB-001` on mutex poison, join error, or SQLite failure.
    pub async fn with_conn<F, T>(&self, trace_id: TraceId, op: F) -> AppResult<T>
    where
        F: FnOnce(&mut Connection) -> AppResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn_arc = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn_arc.lock().map_err(|_| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    "SQLite connection mutex poisoned",
                )
            })?;
            op(&mut guard)
        })
        .await
        .map_err(|join_err| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("SQLite blocking task panicked: {join_err}"),
            )
        })?
    }

    /// Synchronous helper for internal startup/test operations.
    pub(crate) fn with_conn_sync<F, T>(&self, trace_id: TraceId, op: F) -> AppResult<T>
    where
        F: FnOnce(&mut Connection) -> AppResult<T>,
    {
        let mut guard = self.conn.lock().map_err(|_| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                "SQLite connection mutex poisoned",
            )
        })?;
        op(&mut guard)
    }

    /// Imports `.vortex.meta` sidecar files discovered in `scan_dirs` so downloads survive DB loss (`P1-T07`).
    ///
    /// # Errors
    /// Returns `VTX-DB-001` only if writing recovered rows into SQLite fails.
    pub fn import_sidecars_from_dirs(
        &self,
        scan_dirs: &[PathBuf],
        trace_id: TraceId,
    ) -> AppResult<usize> {
        let mut meta_files = Vec::new();
        for dir in scan_dirs {
            Self::collect_sidecar_files(dir, 0, &mut meta_files);
        }

        let mut imported = 0_usize;
        for meta_path in meta_files {
            let Ok(raw_json) = fs::read_to_string(&meta_path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<SidecarMetadata>(&raw_json) else {
                tracing::warn!(path = %meta_path.display(), "Skipping malformed .vortex.meta sidecar");
                continue;
            };

            let mut resource = Resource::new(meta.url.clone());
            resource.final_url.clone_from(&meta.final_url);
            resource.validators.clone_from(&meta.validators);
            let category = vortex_domain::CategorizationPolicy::classify(&meta.file_name, None).id;
            let mut segments = SegmentMap::new(meta.total_size);
            segments.completed.clone_from(&meta.completed_ranges);
            let downloaded = segments.completed.total_bytes();

            let restored = Download {
                id: meta.download_id,
                resource,
                save_dir: meta.save_dir,
                file_name: meta.file_name,
                total_size: meta.total_size,
                state: DownloadState::Paused {
                    paused_at_ms: meta.updated_at_ms,
                    downloaded_bytes: downloaded,
                },
                segments,
                priority: Priority::Normal,
                category_id: category,
                queue_id: QueueId::main_queue(),
                supports_range: meta.supports_range,
                expected_hash: meta.expected_hash,
                times: Timestamps {
                    created_at_ms: meta.updated_at_ms,
                    started_at_ms: Some(meta.updated_at_ms),
                    updated_at_ms: meta.updated_at_ms,
                    completed_at_ms: None,
                },
            };

            crate::repos::upsert_download_in_conn(self, &restored, trace_id)?;
            imported += 1;
        }
        Ok(imported)
    }

    fn collect_sidecar_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 2 || !dir.exists() {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::collect_sidecar_files(&path, depth + 1, out);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(".vortex.meta"))
            {
                out.push(path);
            }
        }
    }

    fn open_configured_connection(db_path: &Path, trace_id: TraceId) -> AppResult<Connection> {
        let conn = Connection::open(db_path).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed opening SQLite database: {e}"),
            )
        })?;
        conn.busy_timeout(Duration::from_millis(5_000))
            .map_err(|e| map_sql_err(&e, trace_id))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|e| map_sql_err(&e, trace_id))?;
        Ok(conn)
    }

    fn verify_file_integrity(db_path: &Path) -> bool {
        let Ok(conn) = Connection::open(db_path) else {
            return false;
        };
        let check: Result<String, _> = conn.query_row("PRAGMA quick_check;", [], |row| row.get(0));
        matches!(check.as_deref(), Ok("ok"))
    }

    fn find_latest_healthy_backup(backup_dir: &Path) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = fs::read_dir(backup_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bak"))
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .rev()
            .find(|candidate| Self::verify_file_integrity(candidate))
    }

    fn read_user_version(conn: &Connection, trace_id: TraceId) -> AppResult<u32> {
        conn.query_row("PRAGMA user_version;", [], |row| row.get::<_, u32>(0))
            .map_err(|e| map_sql_err(&e, trace_id))
    }

    fn run_migrations_sync(
        &self,
        from_version: u32,
        default_download_root: &Path,
        trace_id: TraceId,
    ) -> AppResult<()> {
        self.with_conn_sync(trace_id, |conn| {
            let tx = conn.transaction().map_err(|e| map_sql_err(&e, trace_id))?;
            if from_version < 1 {
                tx.execute_batch(MIGRATION_V1_SQL)
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                tx.pragma_update(None, "user_version", CURRENT_DB_SCHEMA_VERSION)
                    .map_err(|e| map_sql_err(&e, trace_id))?;
            }
            tx.commit().map_err(|e| map_sql_err(&e, trace_id))?;
            Ok(())
        })?;
        self.seed_defaults_sync(default_download_root, trace_id)
    }

    fn seed_defaults_sync(&self, default_download_root: &Path, trace_id: TraceId) -> AppResult<()> {
        self.with_conn_sync(trace_id, |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO queues (id, name, max_parallel, schedule_json, post_action)
                 VALUES (?1, ?2, ?3, NULL, 'none')",
                rusqlite::params![QueueId::MAIN_ID, "Main Queue", 3_i32],
            )
            .map_err(|e| map_sql_err(&e, trace_id))?;

            let builtin_cats = [
                (
                    CategoryId::VIDEO,
                    "Video",
                    &["mp4", "mkv", "webm", "avi", "mov"][..],
                    "Video",
                ),
                (
                    CategoryId::AUDIO,
                    "Music",
                    &["mp3", "m4a", "flac", "wav", "opus"][..],
                    "Music",
                ),
                (
                    CategoryId::COMPRESSED,
                    "Compressed",
                    &["zip", "rar", "7z", "iso", "tar", "gz"][..],
                    "Compressed",
                ),
                (
                    CategoryId::PROGRAMS,
                    "Programs",
                    &["exe", "msi", "apk", "dmg"][..],
                    "Programs",
                ),
                (
                    CategoryId::DOCUMENTS,
                    "Documents",
                    &["pdf", "docx", "xlsx", "pptx", "txt", "epub"][..],
                    "Documents",
                ),
                (
                    CategoryId::IMAGES,
                    "Images",
                    &["png", "jpg", "jpeg", "webp", "svg"][..],
                    "Images",
                ),
                (CategoryId::OTHER, "General", &[][..], "General"),
            ];

            for (id, name, exts, subfolder) in builtin_cats {
                let exts_json = serde_json::to_string(exts).unwrap_or_else(|_| "[]".to_owned());
                let folder = default_download_root.join(subfolder).display().to_string();
                conn.execute(
                    "INSERT OR IGNORE INTO categories (id, name, extensions_json, folder)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id, name, exts_json, folder],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
            }
            Ok(())
        })
    }

    /// Creates a consistent SQLite Online Backup synchronously and prunes older backups beyond `MAX_DB_BACKUPS`.
    ///
    /// # Errors
    /// Returns `VTX-DB-001` if SQLite online backup fails.
    pub fn create_backup_sync(&self, trace_id: TraceId) -> AppResult<PathBuf> {
        let ts = self.clock.now_unix_ms();
        let dest_path = self.backup_dir.join(format!("vortex-{ts}.db.bak"));
        self.with_conn_sync(trace_id, |src_conn| {
            let mut dst_conn =
                Connection::open(&dest_path).map_err(|e| map_sql_err(&e, trace_id))?;
            let backup =
                Backup::new(src_conn, &mut dst_conn).map_err(|e| map_sql_err(&e, trace_id))?;
            backup
                .run_to_completion(128, Duration::from_millis(5), None)
                .map_err(|e| map_sql_err(&e, trace_id))?;
            Ok(())
        })?;

        // Retain only the newest `MAX_DB_BACKUPS`
        if let Ok(entries) = fs::read_dir(&self.backup_dir) {
            let mut baks: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "bak"))
                .collect();
            baks.sort();
            if baks.len() > MAX_DB_BACKUPS {
                let excess = baks.len() - MAX_DB_BACKUPS;
                for old in baks.into_iter().take(excess) {
                    let _ = fs::remove_file(old);
                }
            }
        }
        Ok(dest_path)
    }

    /// Exposes the underlying `vortex.db` file path.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[async_trait]
impl DatabaseHealthPort for SqliteDatabase {
    async fn quick_check(&self, trace_id: TraceId) -> AppResult<bool> {
        self.with_conn(trace_id, move |conn| {
            let res: String = conn
                .query_row("PRAGMA quick_check;", [], |row| row.get(0))
                .map_err(|e| map_sql_err(&e, trace_id))?;
            Ok(res == "ok")
        })
        .await
    }

    async fn wal_checkpoint_truncate(&self, trace_id: TraceId) -> AppResult<()> {
        self.with_conn(trace_id, move |conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(|e| map_sql_err(&e, trace_id))
        })
        .await
    }

    async fn create_backup(&self, trace_id: TraceId) -> AppResult<PathBuf> {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.create_backup_sync(trace_id))
            .await
            .map_err(|e| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    format!("Backup task panicked: {e}"),
                )
            })?
    }
}

pub(crate) fn map_sql_err(err: &rusqlite::Error, trace_id: TraceId) -> VortexError {
    VortexError::coded(
        ErrorCode::DbCorruptOrIncompatible,
        trace_id,
        format!("SQLite error: {err}"),
    )
}
