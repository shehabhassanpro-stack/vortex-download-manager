//! SQLite repositories (`SqliteDownloadRepository`, `SqliteCategoryRepository`,
//! `SqliteQueueRepository`, `SqliteEventLogRepository`) with Poison-Pill row isolation (`P1-T08`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use vortex_application::{
    AppResult, CategoryRecord, CategoryRepository, DownloadRepository, EventLogRepository,
    QueueRecord, QueueRepository,
};
use vortex_domain::{
    ByteRange, CategoryId, Checksum, DomainEvent, DomainEventEnvelope, Download, DownloadId,
    DownloadState, EventId, Priority, QueueId, RangeSet, Resource, ResumeValidators, SegmentMap,
    Timestamps,
};
use vortex_error::TraceId;

use crate::db::{SqliteDatabase, map_sql_err};

/// Default retention window for `download_events` audit rows (`90 days` — `P1-T08`).
pub const EVENT_RETENTION_MS: i64 = 90_i64 * 24 * 60 * 60 * 1_000;

/// SQLite repository for `Download` aggregates with ACID checkpointing and Poison-Pill quarantine.
#[derive(Debug, Clone)]
pub struct SqliteDownloadRepository {
    db: SqliteDatabase,
    quarantined_counter: Arc<AtomicU64>,
}

impl SqliteDownloadRepository {
    /// Creates a new [`SqliteDownloadRepository`].
    #[must_use]
    pub fn new(db: SqliteDatabase) -> Self {
        Self {
            db,
            quarantined_counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub(crate) fn upsert_download_in_conn(
    db: &SqliteDatabase,
    download: &Download,
    trace_id: TraceId,
) -> AppResult<()> {
    let headers_json =
        serde_json::to_string(&download.resource.headers).unwrap_or_else(|_| "{}".to_owned());
    let state_json = serde_json::to_string(&download.state).map_err(|e| {
        vortex_error::VortexError::coded(
            vortex_error::ErrorCode::DbCorruptOrIncompatible,
            trace_id,
            format!("Failed serializing DownloadState: {e}"),
        )
    })?;
    let validators_json =
        serde_json::to_string(&download.resource.validators).unwrap_or_else(|_| "{}".to_owned());
    let expected_hash_str = download.expected_hash.as_ref().map(ToString::to_string);
    let total_size_i64 = download.total_size.and_then(|v| i64::try_from(v).ok());
    let downloaded_i64 = i64::try_from(download.downloaded_bytes()).unwrap_or(i64::MAX);

    db.with_conn_sync(trace_id, |conn| {
        let tx = conn.transaction().map_err(|e| map_sql_err(&e, trace_id))?;

        tx.execute(
            "INSERT INTO downloads (
                id, url, final_url, referrer, top_level_site, user_agent, mime_type,
                headers_json, has_secrets, file_name, save_dir, total_size, downloaded,
                supports_range, state_slug, state_json, validators_json, priority,
                queue_id, category_id, expected_hash, created_at_ms, started_at_ms,
                updated_at_ms, completed_at_ms, quarantined
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23,
                ?24, ?25, 0
            )
            ON CONFLICT(id) DO UPDATE SET
                url = excluded.url,
                final_url = excluded.final_url,
                referrer = excluded.referrer,
                top_level_site = excluded.top_level_site,
                user_agent = excluded.user_agent,
                mime_type = excluded.mime_type,
                headers_json = excluded.headers_json,
                has_secrets = excluded.has_secrets,
                file_name = excluded.file_name,
                save_dir = excluded.save_dir,
                total_size = excluded.total_size,
                downloaded = excluded.downloaded,
                supports_range = excluded.supports_range,
                state_slug = excluded.state_slug,
                state_json = excluded.state_json,
                validators_json = excluded.validators_json,
                priority = excluded.priority,
                queue_id = excluded.queue_id,
                category_id = excluded.category_id,
                expected_hash = excluded.expected_hash,
                started_at_ms = excluded.started_at_ms,
                updated_at_ms = excluded.updated_at_ms,
                completed_at_ms = excluded.completed_at_ms",
            params![
                download.id.to_string(),
                download.resource.url,
                download.resource.final_url,
                download.resource.referrer,
                download.resource.top_level_site,
                download.resource.user_agent,
                download.resource.mime_type,
                headers_json,
                i32::from(download.resource.has_secret_credentials),
                download.file_name,
                download.save_dir.display().to_string(),
                total_size_i64,
                downloaded_i64,
                i32::from(download.supports_range),
                download.state.as_slug(),
                state_json,
                validators_json,
                download.priority.as_i32(),
                download.queue_id.as_str(),
                download.category_id.as_str(),
                expected_hash_str,
                download.times.created_at_ms,
                download.times.started_at_ms,
                download.times.updated_at_ms,
                download.times.completed_at_ms,
            ],
        )
        .map_err(|e| map_sql_err(&e, trace_id))?;

        replace_segments_in_tx(&tx, download.id, &download.segments.completed, trace_id)?;
        tx.commit().map_err(|e| map_sql_err(&e, trace_id))?;
        Ok(())
    })
}

fn replace_segments_in_tx(
    tx: &rusqlite::Transaction<'_>,
    id: DownloadId,
    completed: &RangeSet,
    trace_id: TraceId,
) -> AppResult<()> {
    let id_str = id.to_string();
    tx.execute(
        "DELETE FROM segments WHERE download_id = ?1",
        params![id_str],
    )
    .map_err(|e| map_sql_err(&e, trace_id))?;
    let mut stmt = tx
        .prepare_cached(
            "INSERT INTO segments (download_id, start_byte, end_byte) VALUES (?1, ?2, ?3)",
        )
        .map_err(|e| map_sql_err(&e, trace_id))?;
    for range in completed.iter() {
        let s = i64::try_from(range.start()).unwrap_or(i64::MAX);
        let e = i64::try_from(range.end()).unwrap_or(i64::MAX);
        stmt.execute(params![id_str, s, e])
            .map_err(|err| map_sql_err(&err, trace_id))?;
    }
    Ok(())
}

#[derive(Debug)]
struct RawDownloadRow {
    id: String,
    url: String,
    final_url: Option<String>,
    referrer: Option<String>,
    top_level_site: Option<String>,
    user_agent: Option<String>,
    mime_type: Option<String>,
    headers_json: String,
    has_secrets: i32,
    file_name: String,
    save_dir: String,
    total_size: Option<i64>,
    supports_range: i32,
    state_json: String,
    validators_json: String,
    priority: i32,
    queue_id: String,
    category_id: String,
    expected_hash: Option<String>,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    updated_at_ms: i64,
    completed_at_ms: Option<i64>,
}

fn decode_row_with_segments(conn: &Connection, raw: RawDownloadRow) -> Result<Download, String> {
    let id = DownloadId::parse(&raw.id).map_err(|e| e.to_string())?;
    let state: DownloadState =
        serde_json::from_str(&raw.state_json).map_err(|e| format!("bad state_json: {e}"))?;
    let validators: ResumeValidators = serde_json::from_str(&raw.validators_json)
        .map_err(|e| format!("bad validators_json: {e}"))?;
    let headers: BTreeMap<String, String> =
        serde_json::from_str(&raw.headers_json).map_err(|e| format!("bad headers_json: {e}"))?;
    let queue_id = QueueId::new(raw.queue_id).map_err(|e| e.to_string())?;
    let category_id = CategoryId::new(raw.category_id).map_err(|e| e.to_string())?;
    let expected_hash = match raw.expected_hash {
        Some(spec) => Some(Checksum::parse_prefixed(&spec).map_err(|e| e.to_string())?),
        None => None,
    };
    let total_size = match raw.total_size {
        Some(v) if v >= 0 => Some(u64::try_from(v).map_err(|e| e.to_string())?),
        Some(neg) => return Err(format!("negative total_size: {neg}")),
        None => None,
    };

    let mut stmt = conn
        .prepare_cached(
            "SELECT start_byte, end_byte FROM segments WHERE download_id = ?1 ORDER BY start_byte ASC",
        )
        .map_err(|e| e.to_string())?;
    let seg_rows = stmt
        .query_map(params![raw.id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| e.to_string())?;

    let mut completed = RangeSet::new();
    for seg_res in seg_rows {
        let (s, e) = seg_res.map_err(|err| err.to_string())?;
        let start = u64::try_from(s).map_err(|err| err.to_string())?;
        let end = u64::try_from(e).map_err(|err| err.to_string())?;
        let br = ByteRange::new(start, end).map_err(|err| err.to_string())?;
        completed.insert(br);
    }

    let mut segments = SegmentMap::new(total_size);
    segments.completed = completed;

    Ok(Download {
        id,
        resource: Resource {
            url: raw.url,
            final_url: raw.final_url,
            referrer: raw.referrer,
            top_level_site: raw.top_level_site,
            user_agent: raw.user_agent,
            mime_type: raw.mime_type,
            headers,
            has_secret_credentials: raw.has_secrets != 0,
            validators,
        },
        save_dir: PathBuf::from(raw.save_dir),
        file_name: raw.file_name,
        total_size,
        state,
        segments,
        priority: Priority::from_i32(raw.priority),
        category_id,
        queue_id,
        supports_range: raw.supports_range != 0,
        expected_hash,
        times: Timestamps {
            created_at_ms: raw.created_at_ms,
            started_at_ms: raw.started_at_ms,
            updated_at_ms: raw.updated_at_ms,
            completed_at_ms: raw.completed_at_ms,
        },
    })
}

fn read_raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDownloadRow> {
    Ok(RawDownloadRow {
        id: row.get(0)?,
        url: row.get(1)?,
        final_url: row.get(2)?,
        referrer: row.get(3)?,
        top_level_site: row.get(4)?,
        user_agent: row.get(5)?,
        mime_type: row.get(6)?,
        headers_json: row.get(7)?,
        has_secrets: row.get(8)?,
        file_name: row.get(9)?,
        save_dir: row.get(10)?,
        total_size: row.get(11)?,
        supports_range: row.get(12)?,
        state_json: row.get(13)?,
        validators_json: row.get(14)?,
        priority: row.get(15)?,
        queue_id: row.get(16)?,
        category_id: row.get(17)?,
        expected_hash: row.get(18)?,
        created_at_ms: row.get(19)?,
        started_at_ms: row.get(20)?,
        updated_at_ms: row.get(21)?,
        completed_at_ms: row.get(22)?,
    })
}

const SELECT_DOWNLOAD_COLS: &str = "
    SELECT id, url, final_url, referrer, top_level_site, user_agent, mime_type,
           headers_json, has_secrets, file_name, save_dir, total_size, supports_range,
           state_json, validators_json, priority, queue_id, category_id, expected_hash,
           created_at_ms, started_at_ms, updated_at_ms, completed_at_ms
    FROM downloads
";

#[async_trait]
impl DownloadRepository for SqliteDownloadRepository {
    async fn save(&self, download: &Download, trace_id: TraceId) -> AppResult<()> {
        let db = self.db.clone();
        let dl = download.clone();
        tokio::task::spawn_blocking(move || upsert_download_in_conn(&db, &dl, trace_id))
            .await
            .map_err(|e| {
                vortex_error::VortexError::coded(
                    vortex_error::ErrorCode::BugInternalIsolated,
                    trace_id,
                    format!("save task panicked: {e}"),
                )
            })?
    }

    async fn checkpoint(
        &self,
        id: DownloadId,
        completed: &RangeSet,
        state: &DownloadState,
        updated_at_ms: i64,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let completed_clone = completed.clone();
        let state_slug = state.as_slug().to_owned();
        let state_json = serde_json::to_string(state).map_err(|e| {
            vortex_error::VortexError::coded(
                vortex_error::ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed serializing state: {e}"),
            )
        })?;
        let downloaded_i64 = i64::try_from(completed.total_bytes()).unwrap_or(i64::MAX);

        self.db
            .with_conn(trace_id, move |conn| {
                let tx = conn.transaction().map_err(|e| map_sql_err(&e, trace_id))?;
                tx.execute(
                    "UPDATE downloads
                     SET downloaded = ?1, state_slug = ?2, state_json = ?3, updated_at_ms = ?4
                     WHERE id = ?5",
                    params![
                        downloaded_i64,
                        state_slug,
                        state_json,
                        updated_at_ms,
                        id.to_string()
                    ],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
                replace_segments_in_tx(&tx, id, &completed_clone, trace_id)?;
                tx.commit().map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }

    async fn find(&self, id: DownloadId, trace_id: TraceId) -> AppResult<Option<Download>> {
        let q_counter = Arc::clone(&self.quarantined_counter);
        self.db
            .with_conn(trace_id, move |conn| {
                let sql = format!("{SELECT_DOWNLOAD_COLS} WHERE id = ?1 AND quarantined = 0");
                let raw_opt = conn
                    .query_row(&sql, params![id.to_string()], read_raw_row)
                    .optional()
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let Some(raw) = raw_opt else {
                    return Ok(None);
                };
                let row_id = raw.id.clone();
                match decode_row_with_segments(conn, raw) {
                    Ok(dl) => Ok(Some(dl)),
                    Err(reason) => {
                        tracing::error!(
                            error_code = %vortex_error::ErrorCode::DbCorruptOrIncompatible,
                            %trace_id,
                            download_id = %row_id,
                            reason = %reason,
                            "Poison-Pill download record quarantined on find()"
                        );
                        let _ = conn.execute(
                            "UPDATE downloads SET quarantined = 1 WHERE id = ?1",
                            params![row_id],
                        );
                        q_counter.fetch_add(1, Ordering::Relaxed);
                        Ok(None)
                    }
                }
            })
            .await
    }

    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<Download>> {
        let q_counter = Arc::clone(&self.quarantined_counter);
        self.db
            .with_conn(trace_id, move |conn| {
                let sql =
                    format!("{SELECT_DOWNLOAD_COLS} WHERE quarantined = 0 ORDER BY created_at_ms DESC");
                let raw_rows: Vec<RawDownloadRow> = {
                    let mut stmt = conn.prepare(&sql).map_err(|e| map_sql_err(&e, trace_id))?;
                    let mapped = stmt
                        .query_map([], read_raw_row)
                        .map_err(|e| map_sql_err(&e, trace_id))?;
                    mapped.filter_map(Result::ok).collect()
                };

                let mut downloads = Vec::with_capacity(raw_rows.len());
                let mut poisoned_ids = Vec::new();
                for raw in raw_rows {
                    let row_id = raw.id.clone();
                    match decode_row_with_segments(conn, raw) {
                        Ok(dl) => downloads.push(dl),
                        Err(reason) => {
                            tracing::error!(
                                error_code = %vortex_error::ErrorCode::DbCorruptOrIncompatible,
                                %trace_id,
                                download_id = %row_id,
                                reason = %reason,
                                "Poison-Pill download record isolated and quarantined during list_all()"
                            );
                            poisoned_ids.push(row_id);
                        }
                    }
                }

                for bad_id in poisoned_ids {
                    let _ = conn.execute(
                        "UPDATE downloads SET quarantined = 1 WHERE id = ?1",
                        params![bad_id],
                    );
                    q_counter.fetch_add(1, Ordering::Relaxed);
                }

                Ok(downloads)
            })
            .await
    }

    async fn delete(&self, id: DownloadId, trace_id: TraceId) -> AppResult<bool> {
        self.db
            .with_conn(trace_id, move |conn| {
                let affected = conn
                    .execute(
                        "DELETE FROM downloads WHERE id = ?1",
                        params![id.to_string()],
                    )
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(affected > 0)
            })
            .await
    }

    fn quarantined_count(&self) -> u64 {
        self.quarantined_counter.load(Ordering::Relaxed)
    }
}

/// SQLite repository for file categories.
#[derive(Debug, Clone)]
pub struct SqliteCategoryRepository {
    db: SqliteDatabase,
}

impl SqliteCategoryRepository {
    /// Creates a new [`SqliteCategoryRepository`].
    #[must_use]
    pub const fn new(db: SqliteDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CategoryRepository for SqliteCategoryRepository {
    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<CategoryRecord>> {
        self.db
            .with_conn(trace_id, move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, name, extensions_json, folder FROM categories ORDER BY id ASC",
                    )
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let mut out = Vec::new();
                for item in rows.flatten() {
                    if let Ok(id) = CategoryId::new(item.0) {
                        let extensions: Vec<String> =
                            serde_json::from_str(&item.2).unwrap_or_default();
                        out.push(CategoryRecord {
                            id,
                            name: item.1,
                            extensions,
                            folder: PathBuf::from(item.3),
                        });
                    }
                }
                Ok(out)
            })
            .await
    }

    async fn upsert(&self, record: &CategoryRecord, trace_id: TraceId) -> AppResult<()> {
        let rec = record.clone();
        self.db
            .with_conn(trace_id, move |conn| {
                let exts_json =
                    serde_json::to_string(&rec.extensions).unwrap_or_else(|_| "[]".to_owned());
                conn.execute(
                    "INSERT INTO categories (id, name, extensions_json, folder)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                        name = excluded.name,
                        extensions_json = excluded.extensions_json,
                        folder = excluded.folder",
                    params![
                        rec.id.as_str(),
                        rec.name,
                        exts_json,
                        rec.folder.display().to_string()
                    ],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }

    async fn find(&self, id: &CategoryId, trace_id: TraceId) -> AppResult<Option<CategoryRecord>> {
        let target_id = id.clone();
        self.db
            .with_conn(trace_id, move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, name, extensions_json, folder FROM categories WHERE id = ?1",
                        params![target_id.as_str()],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(row.and_then(|(id_s, name, exts_s, folder_s)| {
                    CategoryId::new(id_s).ok().map(|cid| CategoryRecord {
                        id: cid,
                        name,
                        extensions: serde_json::from_str(&exts_s).unwrap_or_default(),
                        folder: PathBuf::from(folder_s),
                    })
                }))
            })
            .await
    }
}

/// SQLite repository for scheduling queues.
#[derive(Debug, Clone)]
pub struct SqliteQueueRepository {
    db: SqliteDatabase,
}

impl SqliteQueueRepository {
    /// Creates a new [`SqliteQueueRepository`].
    #[must_use]
    pub const fn new(db: SqliteDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl QueueRepository for SqliteQueueRepository {
    async fn list_all(&self, trace_id: TraceId) -> AppResult<Vec<QueueRecord>> {
        self.db
            .with_conn(trace_id, move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, name, max_parallel, schedule_json, post_action FROM queues ORDER BY id ASC",
                    )
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i32>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    })
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let mut out = Vec::new();
                for item in rows.flatten() {
                    if let Ok(qid) = QueueId::new(item.0) {
                        out.push(QueueRecord {
                            id: qid,
                            name: item.1,
                            max_parallel: u8::try_from(item.2.clamp(1, 32)).unwrap_or(3),
                            schedule_json: item.3,
                            post_action: item.4,
                        });
                    }
                }
                Ok(out)
            })
            .await
    }

    async fn upsert(&self, record: &QueueRecord, trace_id: TraceId) -> AppResult<()> {
        let rec = record.clone();
        self.db
            .with_conn(trace_id, move |conn| {
                conn.execute(
                    "INSERT INTO queues (id, name, max_parallel, schedule_json, post_action)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET
                        name = excluded.name,
                        max_parallel = excluded.max_parallel,
                        schedule_json = excluded.schedule_json,
                        post_action = excluded.post_action",
                    params![
                        rec.id.as_str(),
                        rec.name,
                        i32::from(rec.max_parallel),
                        rec.schedule_json,
                        rec.post_action
                    ],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }

    async fn find(&self, id: &QueueId, trace_id: TraceId) -> AppResult<Option<QueueRecord>> {
        let target_id = id.clone();
        self.db
            .with_conn(trace_id, move |conn| {
                let row = conn
                    .query_row(
                        "SELECT id, name, max_parallel, schedule_json, post_action FROM queues WHERE id = ?1",
                        params![target_id.as_str()],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i32>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(row.and_then(|(id_s, name, max_p, sched, post)| {
                    QueueId::new(id_s).ok().map(|qid| QueueRecord {
                        id: qid,
                        name,
                        max_parallel: u8::try_from(max_p.clamp(1, 32)).unwrap_or(3),
                        schedule_json: sched,
                        post_action: post,
                    })
                }))
            })
            .await
    }
}

/// SQLite audit trail repository (`download_events` table, 90-day auto-retention — `P1-T08`).
#[derive(Debug, Clone)]
pub struct SqliteEventLogRepository {
    db: SqliteDatabase,
}

impl SqliteEventLogRepository {
    /// Creates a new [`SqliteEventLogRepository`].
    #[must_use]
    pub const fn new(db: SqliteDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EventLogRepository for SqliteEventLogRepository {
    async fn append(&self, events: &[DomainEventEnvelope], trace_id: TraceId) -> AppResult<()> {
        if events.is_empty() {
            return Ok(());
        }
        let batch = events.to_vec();
        self.db
            .with_conn(trace_id, move |conn| {
                let tx = conn.transaction().map_err(|e| map_sql_err(&e, trace_id))?;
                {
                    let mut stmt = tx
                        .prepare_cached(
                            "INSERT OR IGNORE INTO download_events
                             (event_id, download_id, trace_id, kind, payload_json, at_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        )
                        .map_err(|e| map_sql_err(&e, trace_id))?;
                    for env in &batch {
                        let payload =
                            serde_json::to_string(&env.event).unwrap_or_else(|_| "{}".to_owned());
                        let kind = event_kind_slug(&env.event);
                        stmt.execute(params![
                            env.event_id.to_string(),
                            env.download_id.to_string(),
                            env.trace_id.to_string(),
                            kind,
                            payload,
                            env.timestamp_ms
                        ])
                        .map_err(|e| map_sql_err(&e, trace_id))?;
                    }
                }
                // Automatic 90-day retention sweep anchored on latest batch timestamp (`P1-T08`)
                if let Some(latest_ts) = batch.last().map(|e| e.timestamp_ms) {
                    let cutoff = latest_ts.saturating_sub(EVENT_RETENTION_MS);
                    let _ = tx.execute(
                        "DELETE FROM download_events WHERE at_ms < ?1",
                        params![cutoff],
                    );
                }
                tx.commit().map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }

    async fn list_for_download(
        &self,
        id: DownloadId,
        limit: usize,
        trace_id: TraceId,
    ) -> AppResult<Vec<DomainEventEnvelope>> {
        let cap = i64::try_from(limit.min(10_000)).unwrap_or(500);
        self.db
            .with_conn(trace_id, move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT event_id, download_id, trace_id, payload_json, at_ms
                         FROM download_events
                         WHERE download_id = ?1
                         ORDER BY seq ASC
                         LIMIT ?2",
                    )
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let rows = stmt
                    .query_map(params![id.to_string(), cap], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    })
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                let mut out = Vec::new();
                for item in rows.flatten() {
                    if let (Ok(eid_u), Ok(did), Ok(tid), Ok(event)) = (
                        item.0.parse(),
                        DownloadId::parse(&item.1),
                        item.2.parse::<TraceId>(),
                        serde_json::from_str::<DomainEvent>(&item.3),
                    ) {
                        out.push(DomainEventEnvelope {
                            event_id: EventId::from_uuid(eid_u),
                            download_id: did,
                            trace_id: tid,
                            timestamp_ms: item.4,
                            event,
                        });
                    }
                }
                Ok(out)
            })
            .await
    }

    async fn prune_before(&self, cutoff_unix_ms: i64, trace_id: TraceId) -> AppResult<u64> {
        self.db
            .with_conn(trace_id, move |conn| {
                let deleted = conn
                    .execute(
                        "DELETE FROM download_events WHERE at_ms < ?1",
                        params![cutoff_unix_ms],
                    )
                    .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(u64::try_from(deleted).unwrap_or(0))
            })
            .await
    }
}

const fn event_kind_slug(evt: &DomainEvent) -> &'static str {
    match evt {
        DomainEvent::DownloadAdded { .. } => "download_added",
        DomainEvent::ProbeStarted => "probe_started",
        DomainEvent::ProbeCompleted { .. } => "probe_completed",
        DomainEvent::DownloadQueued { .. } => "download_queued",
        DomainEvent::SegmentStarted { .. } => "segment_started",
        DomainEvent::SegmentSplit { .. } => "segment_split",
        DomainEvent::ProgressTick { .. } => "progress_tick",
        DomainEvent::DownloadPaused { .. } => "download_paused",
        DomainEvent::DownloadResumed => "download_resumed",
        DomainEvent::RetryScheduled { .. } => "retry_scheduled",
        DomainEvent::LinkExpired { .. } => "link_expired",
        DomainEvent::LinkRefreshed { .. } => "link_refreshed",
        DomainEvent::PostProcessingStarted { .. } => "post_processing_started",
        DomainEvent::VerificationStarted => "verification_started",
        DomainEvent::VerificationPassed { .. } => "verification_passed",
        DomainEvent::VerificationFailed { .. } => "verification_failed",
        DomainEvent::DownloadCompleted { .. } => "download_completed",
        DomainEvent::DownloadFailed { .. } => "download_failed",
    }
}
