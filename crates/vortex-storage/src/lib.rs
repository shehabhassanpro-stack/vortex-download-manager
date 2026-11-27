//! SQLite WAL persistence, numbered migrations, self-healing, Poison-Pill repositories,
//! atomic `ConfigManager`, and Windows DPAPI `SecretStore` (`crates/vortex-storage` — Level 3,
//! `P1-T07` → `P1-T10`).

pub mod config_manager;
pub mod db;
pub mod repos;
pub mod secrets;

pub use config_manager::ConfigManager;
pub use db::{CURRENT_DB_SCHEMA_VERSION, DbOpenReport, MAX_DB_BACKUPS, SqliteDatabase};
pub use repos::{
    EVENT_RETENTION_MS, SqliteCategoryRepository, SqliteDownloadRepository,
    SqliteEventLogRepository, SqliteQueueRepository,
};
pub use secrets::DpapiSecretStore;

/// Layer identifier used by Composition Root smoke checks.
#[must_use]
pub const fn layer_name() -> &'static str {
    let _ = vortex_application::layer_name();
    "vortex-storage"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use rusqlite::Connection;
    use tempfile::tempdir;
    use vortex_application::{
        AppConfig, CategoryRepository, ConfigPort, DatabaseHealthPort, DownloadRepository,
        EventLogRepository, QueueRepository, SecretCredentials, SecretStore, SidecarMetadata,
    };
    use vortex_domain::{
        ByteRange, CategoryId, Download, DownloadCommand, DownloadId, FakeClock, Priority, QueueId,
        RangeSet, Resource, ResumeValidators, WorkerId,
    };
    use vortex_error::{ErrorCode, TraceId};
    use vortex_observability::{ExposeSecret, SecretString};

    use super::*;

    #[tokio::test]
    async fn sqlite_wal_migrations_poison_pill_isolation_and_sidecar_self_healing() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("vortex.db");
        let downloads_dir = dir.path().join("Downloads");
        fs::create_dir_all(&downloads_dir).unwrap();
        let clock = Arc::new(FakeClock::new(1_764_094_000_000));
        let trace = TraceId::new();

        let (db, report) = SqliteDatabase::open_with_self_healing(
            &db_path,
            std::slice::from_ref(&downloads_dir),
            &downloads_dir,
            clock.clone(),
            trace,
        )
        .unwrap();
        assert!(!report.recovered_from_corruption);
        assert!(db.quick_check(trace).await.unwrap());

        let dl_repo = SqliteDownloadRepository::new(db.clone());
        let cat_repo = SqliteCategoryRepository::new(db.clone());
        let queue_repo = SqliteQueueRepository::new(db.clone());
        let evt_repo = SqliteEventLogRepository::new(db.clone());

        assert_eq!(cat_repo.list_all(trace).await.unwrap().len(), 7);
        assert_eq!(queue_repo.list_all(trace).await.unwrap().len(), 1);

        // Save healthy download + checkpoint + audit events
        let (mut dl, ev_added) = Download::new_pending(
            DownloadId::new(),
            Resource::new("https://cdn.example.com/archive.iso"),
            "https://cdn.example.com/archive.iso".to_owned(),
            downloads_dir.clone(),
            "archive.iso".to_owned(),
            CategoryId::builtin(CategoryId::COMPRESSED),
            QueueId::main_queue(),
            Priority::High,
            None,
            1_000,
            trace,
        );
        dl.apply(DownloadCommand::StartProbe, 1_010, trace).unwrap();
        dl.apply(
            DownloadCommand::CompleteProbe {
                total_size: Some(8_192),
                supports_range: true,
                validators: ResumeValidators::default(),
                final_url: None,
                resolved_file_name: None,
                queue_full: false,
                final_url_redacted: None,
            },
            1_020,
            trace,
        )
        .unwrap();
        dl.apply(
            DownloadCommand::RecordProgress {
                worker_id: WorkerId::new(1),
                chunk: ByteRange::new(0, 4_096).unwrap(),
                speed_fast_bps: 1000.0,
                speed_slow_bps: 1000.0,
                eta_secs: Some(4),
            },
            1_030,
            trace,
        )
        .unwrap();
        dl_repo.save(&dl, trace).await.unwrap();
        evt_repo.append(&[ev_added], trace).await.unwrap();
        assert_eq!(
            evt_repo
                .list_for_download(dl.id, 50, trace)
                .await
                .unwrap()
                .len(),
            1
        );

        // Inject a Poison-Pill row with corrupted JSON directly into SQLite (`P1-T08`)
        db.with_conn(trace, |conn| {
            conn.execute(
                "INSERT INTO downloads (
                    id, url, headers_json, file_name, save_dir, downloaded, supports_range,
                    state_slug, state_json, validators_json, priority, queue_id, category_id,
                    created_at_ms, updated_at_ms, quarantined
                ) VALUES (
                    ?1, 'https://bad', '{not-json', 'bad.bin', 'D:/', 0, 0,
                    'pending', '{corrupt-state!', '{}', 2, 'main', 'other', 999, 999, 0
                )",
                rusqlite::params![DownloadId::new().to_string()],
            )
            .unwrap();
            Ok(())
        })
        .await
        .unwrap();

        // list_all() isolates & quarantines the poison pill while returning the healthy download!
        let listed = dl_repo.list_all(trace).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed.first().unwrap().id, dl.id);
        assert_eq!(dl_repo.quarantined_count(), 1);

        // Create backup, write a `.vortex.meta` sidecar, then corrupt `vortex.db` on disk!
        let _bak = db.create_backup(trace).await.unwrap();
        db.wal_checkpoint_truncate(trace).await.unwrap();
        drop(dl_repo);
        drop(cat_repo);
        drop(queue_repo);
        drop(evt_repo);
        drop(db);

        let sidecar_id = DownloadId::new();
        let mut sidecar_ranges = RangeSet::new();
        sidecar_ranges.insert(ByteRange::new(0, 2048).unwrap());
        let sidecar = SidecarMetadata {
            version: 1,
            download_id: sidecar_id,
            url: "https://cdn.example.com/sidecar.zip".to_owned(),
            final_url: None,
            file_name: "sidecar.zip".to_owned(),
            save_dir: downloads_dir.clone(),
            total_size: Some(4096),
            supports_range: true,
            validators: ResumeValidators::default(),
            completed_ranges: sidecar_ranges,
            expected_hash: None,
            updated_at_ms: 2_000,
        };
        fs::write(
            downloads_dir.join("sidecar.zip.vortex.meta"),
            serde_json::to_string_pretty(&sidecar).unwrap(),
        )
        .unwrap();

        // Corrupt `vortex.db` header bytes!
        fs::write(&db_path, b"CORRUPTED_GARBAGE_NOT_SQLITE_AT_ALL_00000000000").unwrap();

        let (healed_db, heal_report) = SqliteDatabase::open_with_self_healing(
            &db_path,
            std::slice::from_ref(&downloads_dir),
            &downloads_dir,
            clock.clone(),
            trace,
        )
        .unwrap();
        assert!(heal_report.recovered_from_corruption);
        assert!(heal_report.quarantined_db_path.unwrap().exists());
        assert_eq!(heal_report.sidecars_imported, 1);

        let healed_repo = SqliteDownloadRepository::new(healed_db.clone());
        assert!(healed_repo.find(dl.id, trace).await.unwrap().is_some());
        assert!(healed_repo.find(sidecar_id, trace).await.unwrap().is_some());
        drop(healed_repo);
        drop(healed_db);

        // Verify newer `user_version` is cleanly rejected with `VTX-DB-001` (`P1-T07`)
        {
            let raw_conn = Connection::open(&db_path).unwrap();
            raw_conn
                .pragma_update(None, "user_version", 99_u32)
                .unwrap();
        }
        let err =
            SqliteDatabase::open_with_self_healing(&db_path, &[], &downloads_dir, clock, trace)
                .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DbCorruptOrIncompatible);
    }

    #[tokio::test]
    async fn config_manager_atomic_save_bak_recovery_and_hot_reload() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let trace = TraceId::new();

        let (mgr, recovered) = ConfigManager::load_or_init(&cfg_path, trace).unwrap();
        assert!(!recovered);
        let mut rx = mgr.subscribe();

        let updated = AppConfig {
            max_connections_per_download: 16,
            language: "en".to_owned(),
            ..AppConfig::default()
        };
        mgr.update(updated, trace).await.unwrap();
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().max_connections_per_download, 16);

        // Corrupt primary `config.toml`; verify automatic recovery from `config.toml.bak`!
        fs::write(&cfg_path, "invalid === [toml").unwrap();
        let (mgr2, recovered2) = ConfigManager::load_or_init(&cfg_path, trace).unwrap();
        assert!(recovered2);
        // `.bak` held initial valid config (`8` connections)
        assert_eq!(mgr2.current().max_connections_per_download, 8);
    }

    #[tokio::test]
    async fn dpapi_secret_store_encrypts_large_64kib_cookies_with_zero_plaintext_on_disk() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("secrets.db");
        let clock = Arc::new(FakeClock::default());
        let trace = TraceId::new();

        let (db, _) =
            SqliteDatabase::open_with_self_healing(&db_path, &[], dir.path(), clock.clone(), trace)
                .unwrap();
        let store = DpapiSecretStore::new(db, clock);

        let id = DownloadId::new();
        let canary_cookie = format!(
            "cf_clearance=SUPER_SECRET_CANARY_9911; pad={}",
            "x".repeat(8192)
        );
        let creds = SecretCredentials {
            cookies: Some(SecretString::from(canary_cookie.clone())),
            authorization: Some(SecretString::from("Bearer TOP_SECRET_JWT_777".to_owned())),
            origin_key: SecretCredentials::origin_key_from_url(
                "https://cdn.example.com/archive.zip",
            ),
        };
        store.store(id, &creds, trace).await.unwrap();

        // Inspect raw SQLite file + WAL on disk: ZERO plaintext occurrences of canary!
        let db_bytes = fs::read(&db_path).unwrap_or_default();
        let wal_bytes =
            fs::read(PathBuf::from(format!("{}-wal", db_path.display()))).unwrap_or_default();
        let combined = [db_bytes, wal_bytes].concat();
        let lossy = String::from_utf8_lossy(&combined);
        assert!(!lossy.contains("SUPER_SECRET_CANARY_9911"));
        assert!(!lossy.contains("TOP_SECRET_JWT_777"));

        // Decrypt back and verify exact match + TH-28 origin pinning!
        let loaded = store.load(id, trace).await.unwrap().unwrap();
        assert_eq!(
            loaded.cookies.as_ref().unwrap().expose_secret(),
            canary_cookie
        );
        assert_eq!(
            loaded.authorization.as_ref().unwrap().expose_secret(),
            "Bearer TOP_SECRET_JWT_777"
        );
        assert!(loaded.origin_matches("https://cdn.example.com/other/file.bin"));
        assert!(!loaded.origin_matches("https://evil.example.net/steal"));
        assert!(!loaded.origin_matches("http://cdn.example.com/downgrade"));

        store.delete(id, trace).await.unwrap();
        assert!(store.load(id, trace).await.unwrap().is_none());
    }
}
