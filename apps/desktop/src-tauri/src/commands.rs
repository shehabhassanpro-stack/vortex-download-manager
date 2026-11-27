//! Typed Tauri v2 IPC Commands exported automatically to TypeScript via `tauri-specta` (`P1-T30`).
//!
//! Every command returns `Result<T, ApiError>` so the React UI receives strongly-typed `VTX-*`
//! errors with `trace_id`, localized message keys, and suggested actions (`ADR-015`).

#![allow(
    clippy::result_large_err,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    clippy::redundant_closure_for_method_calls
)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;
use vortex_application::{
    AddDownloadCommand, AddDownloadPreviewDto, AppConfig, ApplicationService,
    BrowserIntegrationStatusDto, DiagnosticBundleReportDto, DownloadDetailsDto, DownloadFilterDto,
    DownloadSummaryDto, DuplicateMatchDto, HealthStatusDto, LogEntryDto, PrepareAddInput,
    StagedDownloadDto,
};
use vortex_domain::{CategoryId, Checksum, DownloadId, Priority, QueueId};
use vortex_error::{ApiError, ErrorCode, TraceId};
use vortex_platform::OFFICIAL_EXTENSION_ID;

/// Shared Tauri state wrapper holding [`ApplicationService`].
pub struct CoreState(pub Arc<ApplicationService>);

/// Typed input DTO for `cmd_add_download` (`tauri-specta`).
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AddDownloadRequestDto {
    /// Optional staged warm stream ID from `cmd_prepare_add_download` (`0ms` start!).
    pub staged_id: Option<String>,
    /// Target URL.
    pub url: String,
    /// Optional explicit filename.
    pub file_name: Option<String>,
    /// Optional explicit save directory.
    pub save_dir: Option<String>,
    /// Optional category slug.
    pub category_id: Option<String>,
    /// Optional checksum (`"sha256:..."` / `"blake3:..."`).
    pub expected_checksum: Option<String>,
    /// Start immediately (`true`) or add to queue paused (`false`).
    pub auto_start: bool,
    /// Overwrite existing file instead of auto-numbering.
    pub overwrite_existing: bool,
}

fn parse_download_id(raw: &str, trace_id: TraceId) -> Result<DownloadId, ApiError> {
    DownloadId::parse(raw).map_err(|_| {
        ApiError::new(ErrorCode::ResNotFound, trace_id)
            .with_technical_detail(format!("Invalid DownloadId UUID: {raw}"))
    })
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_prepare_add_download(
    state: State<'_, CoreState>,
    url: String,
) -> Result<AddDownloadPreviewDto, ApiError> {
    let tid = TraceId::new();
    state
        .0
        .prepare_add_download(
            PrepareAddInput {
                url,
                ..PrepareAddInput::default()
            },
            tid,
        )
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_add_download(
    state: State<'_, CoreState>,
    req: AddDownloadRequestDto,
) -> Result<DownloadSummaryDto, ApiError> {
    let tid = TraceId::new();
    let staged_id = match &req.staged_id {
        Some(s) if !s.trim().is_empty() => Some(parse_download_id(s, tid)?),
        _ => None,
    };
    let expected_hash = match &req.expected_checksum {
        Some(raw) if !raw.trim().is_empty() => {
            Some(Checksum::parse_prefixed(raw.trim()).map_err(|e| e.into_api_error())?)
        }
        _ => None,
    };
    let category_id = req
        .category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| CategoryId::new(s).ok());

    state
        .0
        .add_download(
            AddDownloadCommand {
                staged_id,
                url: req.url,
                referrer: None,
                user_agent: None,
                headers: BTreeMap::new(),
                cookies: None,
                authorization: None,
                file_name: req.file_name.filter(|s| !s.trim().is_empty()),
                save_dir: req
                    .save_dir
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from),
                category_id,
                queue_id: Some(QueueId::main_queue()),
                priority: Priority::Normal,
                expected_hash,
                auto_start: req.auto_start,
                overwrite_existing: req.overwrite_existing,
            },
            tid,
        )
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_list_downloads(
    state: State<'_, CoreState>,
    filter: Option<DownloadFilterDto>,
) -> Result<Vec<DownloadSummaryDto>, ApiError> {
    state
        .0
        .list_downloads(filter, TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_search_downloads(
    state: State<'_, CoreState>,
    query: String,
) -> Result<Vec<DownloadSummaryDto>, ApiError> {
    state
        .0
        .search_downloads(&query, TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_get_download_details(
    state: State<'_, CoreState>,
    id: String,
) -> Result<DownloadDetailsDto, ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .get_download_details(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_pause_download(state: State<'_, CoreState>, id: String) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .pause_download(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_pause_all(state: State<'_, CoreState>) -> Result<(), ApiError> {
    state
        .0
        .pause_all(TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_resume_download(state: State<'_, CoreState>, id: String) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .resume_download(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_cancel_download(
    state: State<'_, CoreState>,
    id: String,
    delete_partial_files: bool,
) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .cancel_download(did, delete_partial_files, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_remove_download(
    state: State<'_, CoreState>,
    id: String,
    delete_completed_file: bool,
) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .remove_download(did, delete_completed_file, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_undo_remove_download(
    state: State<'_, CoreState>,
    id: String,
) -> Result<DownloadSummaryDto, ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .undo_remove_download(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_check_duplicate(
    state: State<'_, CoreState>,
    url: String,
    file_name: Option<String>,
) -> Result<Option<DuplicateMatchDto>, ApiError> {
    state
        .0
        .check_duplicate(&url, file_name.as_deref(), TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_get_settings(state: State<'_, CoreState>) -> Result<AppConfig, ApiError> {
    Ok((*state.0.current_config()).clone())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_update_settings(
    state: State<'_, CoreState>,
    new_config: AppConfig,
) -> Result<AppConfig, ApiError> {
    let updated = state
        .0
        .update_settings(new_config, TraceId::new())
        .await
        .map_err(|e| e.into_api_error())?;
    Ok((*updated).clone())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_get_health_status(
    state: State<'_, CoreState>,
) -> Result<HealthStatusDto, ApiError> {
    state
        .0
        .get_health_status(TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_preview_diagnostic_bundle(
    state: State<'_, CoreState>,
) -> Result<DiagnosticBundleReportDto, ApiError> {
    state
        .0
        .get_diagnostic_summary(TraceId::new())
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_export_diagnostic_bundle(
    state: State<'_, CoreState>,
    dest_zip_path: Option<String>,
) -> Result<DiagnosticBundleReportDto, ApiError> {
    let tid = TraceId::new();
    let target = dest_zip_path.filter(|s| !s.trim().is_empty()).map_or_else(
        || {
            state
                .0
                .current_config()
                .default_download_dir
                .join(format!("vortex-diagnostics-{tid}.zip"))
        },
        PathBuf::from,
    );
    state
        .0
        .export_diagnostic_bundle(&target, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_get_recent_logs(
    state: State<'_, CoreState>,
    limit: u32,
) -> Result<Vec<LogEntryDto>, ApiError> {
    Ok(state.0.recent_logs(limit as usize))
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_set_debug_mode(
    state: State<'_, CoreState>,
    enabled: bool,
) -> Result<bool, ApiError> {
    state
        .0
        .set_debug_mode(enabled, TraceId::new())
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_repair_browser_bridge(
    state: State<'_, CoreState>,
) -> Result<Vec<BrowserIntegrationStatusDto>, ApiError> {
    let tid = TraceId::new();
    let self_exe = std::env::current_exe().map_err(|e| {
        ApiError::new(ErrorCode::DiskPermissionDenied, tid)
            .with_technical_detail(format!("Cannot resolve current_exe: {e}"))
    })?;
    let bridge_exe = self_exe.parent().map_or_else(
        || PathBuf::from("vortex-bridge.exe"),
        |p| p.join("vortex-bridge.exe"),
    );
    let ext_ids = vec![OFFICIAL_EXTENSION_ID.to_owned()];
    state
        .0
        .repair_browser_integration(&bridge_exe, &ext_ids, tid)
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_log_frontend_error(
    component: String,
    message: String,
) -> Result<String, ApiError> {
    let tid = TraceId::new();
    tracing::error!(
        %tid,
        frontend_component = %component,
        detail = %vortex_observability::redact_sensitive_text(&message),
        "Caught React Error Boundary fault in Desktop UI"
    );
    Ok(tid.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_list_staged_downloads(
    state: State<'_, CoreState>,
) -> Result<Vec<StagedDownloadDto>, ApiError> {
    Ok(state.0.list_staged_downloads().await)
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_discard_staged_download(
    state: State<'_, CoreState>,
    staged_id: String,
) -> Result<bool, ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&staged_id, tid)?;
    state
        .0
        .discard_staged_download(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_discard_all_staged_downloads(
    state: State<'_, CoreState>,
) -> Result<u32, ApiError> {
    let count = state
        .0
        .discard_all_staged_downloads(TraceId::new())
        .await
        .map_err(|e| e.into_api_error())?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_block_capture_host(
    state: State<'_, CoreState>,
    staged_id: String,
    host: String,
) -> Result<AppConfig, ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&staged_id, tid)?;
    let updated = state
        .0
        .block_capture_host_and_discard(did, host, tid)
        .await
        .map_err(|e| e.into_api_error())?;
    Ok((*updated).clone())
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::cast_precision_loss)]
pub async fn cmd_free_disk_bytes(
    state: State<'_, CoreState>,
    dir: String,
) -> Result<Option<f64>, ApiError> {
    Ok(state
        .0
        .free_disk_bytes_for_dir(&PathBuf::from(dir), TraceId::new())
        .map(|b| b as f64))
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_pick_folder(
    state: State<'_, CoreState>,
    initial_dir: Option<String>,
) -> Result<Option<String>, ApiError> {
    let svc = Arc::clone(&state.0);
    let init = initial_dir
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);
    let tid = TraceId::new();
    let picked = tokio::task::spawn_blocking(move || svc.pick_folder(init.as_deref(), tid))
        .await
        .map_err(|e| {
            ApiError::new(ErrorCode::BugInternalIsolated, tid)
                .with_technical_detail(format!("Folder picker task failed: {e}"))
        })?
        .map_err(|e| e.into_api_error())?;
    Ok(picked.map(|p| p.display().to_string()))
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_reveal_in_folder(state: State<'_, CoreState>, id: String) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .reveal_in_folder(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_open_downloaded_file(
    state: State<'_, CoreState>,
    id: String,
) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .open_downloaded_file(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_open_main_window(app: tauri::AppHandle) -> Result<(), ApiError> {
    crate::open_or_focus_main_window(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_open_download_referrer(
    state: State<'_, CoreState>,
    id: String,
) -> Result<(), ApiError> {
    let tid = TraceId::new();
    let did = parse_download_id(&id, tid)?;
    state
        .0
        .open_download_referrer(did, tid)
        .await
        .map_err(|e| e.into_api_error())
}

#[tauri::command]
#[specta::specta]
pub async fn cmd_open_external_url(
    state: State<'_, CoreState>,
    url: String,
) -> Result<(), ApiError> {
    let tid = TraceId::new();
    state
        .0
        .open_external_url(&url, tid)
        .map_err(|e| e.into_api_error())
}

/// Builds the [`tauri_specta::Builder`] collecting all typed commands for both runtime registration
/// and `apps/desktop/ui/src/bindings/generated.ts` export.
#[must_use]
pub fn build_specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new().commands(tauri_specta::collect_commands![
        cmd_prepare_add_download,
        cmd_add_download,
        cmd_list_downloads,
        cmd_search_downloads,
        cmd_get_download_details,
        cmd_pause_download,
        cmd_pause_all,
        cmd_resume_download,
        cmd_cancel_download,
        cmd_remove_download,
        cmd_undo_remove_download,
        cmd_check_duplicate,
        cmd_get_settings,
        cmd_update_settings,
        cmd_get_health_status,
        cmd_preview_diagnostic_bundle,
        cmd_export_diagnostic_bundle,
        cmd_get_recent_logs,
        cmd_set_debug_mode,
        cmd_repair_browser_bridge,
        cmd_log_frontend_error,
        cmd_list_staged_downloads,
        cmd_discard_staged_download,
        cmd_discard_all_staged_downloads,
        cmd_block_capture_host,
        cmd_free_disk_bytes,
        cmd_pick_folder,
        cmd_reveal_in_folder,
        cmd_open_downloaded_file,
        cmd_open_main_window,
        cmd_open_download_referrer,
        cmd_open_external_url,
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use specta_typescript::Typescript;

    use super::*;

    #[test]
    fn export_specta_typescript_bindings_to_ui() {
        let out_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui/src/bindings/generated.ts");
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let exporter = Typescript::default()
            .header("/* Auto-generated by tauri-specta (P1.6-T08). Do not edit manually. */\n");
        build_specta_builder()
            .export(exporter, &out_path)
            .expect("tauri-specta TypeScript bindings export must succeed with zero errors");
        let generated = std::fs::read_to_string(&out_path).unwrap();
        assert!(generated.contains("export type ApiError"));
        assert!(generated.contains("suggestedActions"));
        assert!(generated.contains("cmd_open_download_referrer"));
        assert!(generated.contains("cmd_open_external_url"));
    }
}
