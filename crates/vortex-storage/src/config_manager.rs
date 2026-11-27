//! Atomic `ConfigManager` (`config.toml` -> `tmp -> fsync -> .bak -> rename`) with automatic
//! `.bak` recovery, unknown-field `WARN` tolerance, JSON export/import, and live `watch` Hot-Reload (`P1-T09`).

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;
use vortex_application::{AppConfig, AppResult, ConfigPort};
use vortex_error::{ErrorCode, TraceId, VortexError};

const KNOWN_TOP_LEVEL_CONFIG_KEYS: &[&str] = &[
    "schema_version",
    "max_connections_per_download",
    "max_concurrent_downloads",
    "checkpoint_interval_secs",
    "idle_timeout_secs",
    "max_retries",
    "min_free_disk_bytes",
    "default_download_dir",
    "auto_start_handoff",
    "warm_instant_webview",
    "prevent_sleep_while_active",
    "clipboard_monitor_enabled",
    "theme",
    "language",
    "global_speed_limit_bps",
    "capture_rules",
    "feature_flags",
];

/// Atomic configuration manager implementing [`ConfigPort`].
#[derive(Debug, Clone)]
pub struct ConfigManager {
    config_path: PathBuf,
    backup_path: PathBuf,
    sender: Arc<watch::Sender<Arc<AppConfig>>>,
}

impl ConfigManager {
    /// Loads `config.toml` (recovering automatically from `config.toml.bak` or clean defaults if
    /// missing/corrupted, per `ADR-017` & `§24.2`), persists defaults if absent, and initializes Hot-Reload.
    ///
    /// # Errors
    /// Returns `VTX-DB-001` only if the parent config directory cannot be created.
    pub fn load_or_init(config_path: &Path, trace_id: TraceId) -> AppResult<(Self, bool)> {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    format!("Failed creating config directory: {e}"),
                )
            })?;
        }
        let backup_path = config_path.with_extension("toml.bak");
        let mut recovered_from_backup = false;

        let initial_cfg = if config_path.exists() {
            match Self::parse_and_validate_file(config_path, trace_id) {
                Ok(cfg) => cfg,
                Err(primary_err) => {
                    tracing::warn!(
                        error_code = %ErrorCode::DbCorruptOrIncompatible,
                        %trace_id,
                        detail = %primary_err,
                        "Primary config.toml is corrupt; attempting recovery from config.toml.bak"
                    );
                    recovered_from_backup = true;
                    if backup_path.exists()
                        && let Ok(bak_cfg) = Self::parse_and_validate_file(&backup_path, trace_id)
                    {
                        bak_cfg
                    } else {
                        AppConfig::default()
                    }
                }
            }
        } else {
            AppConfig::default()
        };

        let shared = Arc::new(initial_cfg);
        let (tx, _) = watch::channel(Arc::clone(&shared));
        let mgr = Self {
            config_path: config_path.to_path_buf(),
            backup_path,
            sender: Arc::new(tx),
        };

        if !config_path.exists() || recovered_from_backup {
            mgr.write_atomic_sync(&shared, trace_id)?;
        }

        Ok((mgr, recovered_from_backup))
    }

    /// Exports sanitized JSON configuration string (contains zero secrets by design — `§24.4`).
    ///
    /// # Errors
    /// Returns `VTX-DB-001` on JSON serialization error.
    pub fn export_clean_json(&self, trace_id: TraceId) -> AppResult<String> {
        let snap = self.current();
        serde_json::to_string_pretty(&*snap).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed exporting config JSON: {e}"),
            )
        })
    }

    /// Validates, atomically saves, and hot-reloads an imported JSON configuration.
    ///
    /// # Errors
    /// Returns `VTX-SEC-001` if JSON is malformed or violates bounds.
    pub async fn import_clean_json(
        &self,
        json_text: &str,
        trace_id: TraceId,
    ) -> AppResult<Arc<AppConfig>> {
        let parsed: AppConfig = serde_json::from_str(json_text).map_err(|e| {
            VortexError::coded(
                ErrorCode::SecPolicyViolation,
                trace_id,
                format!("Invalid config JSON: {e}"),
            )
        })?;
        self.update(parsed, trace_id).await
    }

    fn parse_and_validate_file(path: &Path, trace_id: TraceId) -> AppResult<AppConfig> {
        let raw = fs::read_to_string(path).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed reading {}: {e}", path.display()),
            )
        })?;

        // Warn on unknown keys without failing startup (`§30.4`)
        let value: toml::Value = raw.parse().map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Malformed TOML in {}: {e}", path.display()),
            )
        })?;
        if let Some(table) = value.as_table() {
            let known: BTreeSet<&str> = KNOWN_TOP_LEVEL_CONFIG_KEYS.iter().copied().collect();
            for key in table.keys() {
                if !known.contains(key.as_str()) {
                    tracing::warn!(
                        %trace_id,
                        unknown_key = %key,
                        "Ignoring unknown key in config.toml for forward/backward compatibility"
                    );
                }
            }
        }

        let cfg: AppConfig = toml::from_str(&raw).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Schema mismatch in {}: {e}", path.display()),
            )
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn write_atomic_sync(&self, cfg: &AppConfig, trace_id: TraceId) -> AppResult<()> {
        let serialized = toml::to_string_pretty(cfg).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed serializing AppConfig to TOML: {e}"),
            )
        })?;

        let tmp_path = self.config_path.with_extension("toml.tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| {
                    VortexError::coded(
                        ErrorCode::DiskPermissionDenied,
                        trace_id,
                        format!("Failed opening config.toml.tmp: {e}"),
                    )
                })?;
            file.write_all(serialized.as_bytes()).map_err(|e| {
                VortexError::coded(
                    ErrorCode::DiskInsufficientSpace,
                    trace_id,
                    format!("Failed writing config.toml.tmp: {e}"),
                )
            })?;
            file.sync_all().map_err(|e| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    format!("Failed fsync on config.toml.tmp: {e}"),
                )
            })?;
        }

        if self.config_path.exists() {
            let _ = fs::copy(&self.config_path, &self.backup_path);
        }

        fs::rename(&tmp_path, &self.config_path).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Failed atomic rename config.toml.tmp -> config.toml: {e}"),
            )
        })?;

        Ok(())
    }
}

#[async_trait]
impl ConfigPort for ConfigManager {
    fn current(&self) -> Arc<AppConfig> {
        Arc::clone(&self.sender.borrow())
    }

    fn subscribe(&self) -> watch::Receiver<Arc<AppConfig>> {
        self.sender.subscribe()
    }

    async fn update(&self, new_config: AppConfig, trace_id: TraceId) -> AppResult<Arc<AppConfig>> {
        new_config.validate()?;
        let shared = Arc::new(new_config);
        let mgr = self.clone();
        let to_write = Arc::clone(&shared);
        tokio::task::spawn_blocking(move || mgr.write_atomic_sync(&to_write, trace_id))
            .await
            .map_err(|e| {
                VortexError::coded(
                    ErrorCode::BugInternalIsolated,
                    trace_id,
                    format!("Config atomic write panicked: {e}"),
                )
            })??;
        let _ = self.sender.send_replace(Arc::clone(&shared));
        tracing::info!(%trace_id, "AppConfig updated atomically and hot-reloaded");
        Ok(shared)
    }
}
