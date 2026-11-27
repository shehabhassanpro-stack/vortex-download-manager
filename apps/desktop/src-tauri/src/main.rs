//! Vortex Desktop Composition Root & Tauri 2 Shell (`vortex.exe` — `P1-T30`).
//!
//! Enforces:
//! - Microsoft Research [`mimalloc`] global allocator (`ADR-001` / `§16.1`)
//! - Single-Instance Named Mutex forwarding secondary CLI URLs via Named Pipe (`§24.1`)
//! - **`WebView2` Destroy-on-Close** to System Tray (`Idle RAM < 45 MB` typical, `< 60 MB` ceiling — `ADR-005`)
//!   with optional Warm Instant Mode (`warm_instant_webview`)
//! - `<= 10Hz` Delta UI Projection (`vortex://progress-batch`) + Native Windows Toast Notifications!

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;
mod commands;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimalloc::MiMalloc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::WebviewWindowBuilder;
use tauri::window::{Effect, EffectsBuilder};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use vortex_application::AppEvent;
use vortex_domain::DomainEvent;
use vortex_error::TraceId;
use vortex_ipc::{PROTOCOL_VERSION, SyncPipeClient, proto};
use vortex_platform::{
    SingleInstanceGuard, SingleInstanceOutcome, query_current_user_sid, user_named_pipe_path,
};

use crate::bootstrap::{boot_vortex_core, resolve_data_dir, wired_layers};
use crate::commands::{CoreState, build_specta_builder};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn forward_to_existing_instance_and_exit(sid: &str, cli_url: Option<String>) {
    let tid = TraceId::new();
    let pipe = user_named_pipe_path(sid);
    if let Ok(mut client) =
        SyncPipeClient::connect_with_timeout(&pipe, Duration::from_millis(500), tid)
    {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0);
        let env = proto::Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: TraceId::new().to_string(),
            reply_to: String::new(),
            trace_id: tid.to_string(),
            timestamp_ms: now_ms,
            body: Some(proto::envelope::Body::BringToFront(proto::BringToFront {
                open_url: cli_url,
            })),
        };
        let _ = client.send_envelope(&env);
        let _ = client.recv_envelope();
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StagedChangedEventPayload {
    pending_count: usize,
    latest_staged_id: Option<String>,
    burst_detected: bool,
}

/// Strict `WebView2` navigation allowlist (`TH-33`): prevents any remote origin navigation or
/// phishing page takeover inside Tauri windows while allowing local `tauri.localhost` assets.
fn is_allowed_webview_navigation(url: &tauri::Url) -> bool {
    let scheme = url.scheme();
    if scheme == "about" && url.as_str() == "about:blank" {
        return true;
    }
    if scheme == "tauri" {
        return true;
    }
    if matches!(scheme, "http" | "https") {
        let host = url.host_str().unwrap_or_default();
        if matches!(host, "tauri.localhost" | "ipc.localhost") {
            return true;
        }
        #[cfg(debug_assertions)]
        if matches!(host, "localhost" | "127.0.0.1") && url.port() == Some(5173) {
            return true;
        }
    }
    tracing::warn!(
        scheme,
        host = url.host_str().unwrap_or("<none>"),
        "Blocked unauthorized WebView2 navigation (TH-33)"
    );
    false
}

pub(crate) fn open_or_focus_main_window(app_handle: &AppHandle) {
    if let Some(existing) = app_handle.get_webview_window("main") {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return;
    }

    let effects = EffectsBuilder::new().effect(Effect::Mica).build();
    let mut builder =
        WebviewWindowBuilder::new(app_handle, "main", WebviewUrl::App("index.html".into()))
            .title("Vortex Download Manager")
            .inner_size(1200.0, 760.0)
            .min_inner_size(880.0, 560.0)
            .center()
            .effects(effects)
            .on_navigation(is_allowed_webview_navigation);
    #[cfg(windows)]
    {
        builder = builder.additional_browser_args(
            "--disable-features=AutofillServerCommunication,PasswordManagerOnboarding",
        );
    }
    let built = builder.build();

    if let Ok(win) = built {
        let _ = win.set_focus();
    }
}

/// Opens a compact centered Mica Smart Confirmation popup (`580x500`) when Vortex is idling in the
/// System Tray, or focuses the already-open `"main"` window so the user isn't interrupted by duplicate
/// overlapping windows (`P1.5-T04`).
fn open_or_focus_confirm_window(app_handle: &AppHandle) {
    if let Some(main_win) = app_handle.get_webview_window("main") {
        let _ = main_win.show();
        let _ = main_win.unminimize();
        let _ = main_win.set_focus();
        return;
    }

    if let Some(existing) = app_handle.get_webview_window("confirm") {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return;
    }

    let effects = EffectsBuilder::new().effect(Effect::Mica).build();
    let mut builder = WebviewWindowBuilder::new(
        app_handle,
        "confirm",
        WebviewUrl::App("index.html?mode=confirm".into()),
    )
    .title("Vortex — تأكيد التحميل")
    .inner_size(580.0, 520.0)
    .min_inner_size(480.0, 420.0)
    .center()
    .effects(effects)
    .on_navigation(is_allowed_webview_navigation);
    #[cfg(windows)]
    {
        builder = builder.additional_browser_args(
            "--disable-features=AutofillServerCommunication,PasswordManagerOnboarding",
        );
    }
    let built = builder.build();

    if let Ok(win) = built {
        let _ = win.set_focus();
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let _ = wired_layers();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let background_only = args.iter().any(|a| a == "--background");
    let headless_core = args.iter().any(|a| a == "--headless-core");
    let cli_open_url = args
        .iter()
        .find(|a| a.starts_with("http://") || a.starts_with("https://"))
        .cloned();

    let boot_trace = TraceId::new();
    let sid = std::env::var("VORTEX_PIPE_SID_OVERRIDE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| query_current_user_sid(boot_trace).ok())
        .unwrap_or_else(|| "S-1-5-21-LOCAL".to_owned());

    let single_instance = match SingleInstanceGuard::try_acquire(&sid, boot_trace) {
        Ok(SingleInstanceOutcome::Primary(guard)) => guard,
        Ok(SingleInstanceOutcome::SecondaryAlreadyRunning) => {
            forward_to_existing_instance_and_exit(&sid, cli_open_url);
            return ExitCode::SUCCESS;
        }
        Err(_) => return ExitCode::from(1),
    };

    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::from(1);
    };

    let data_dir = resolve_data_dir();
    let Ok(core) = rt.block_on(boot_vortex_core(&data_dir)) else {
        return ExitCode::from(1);
    };

    // Headless Core mode used by deterministic E2E & Idle RAM / Cold Start budget benchmarks (`P1-T35` / `P1-T36`)!
    if headless_core {
        rt.block_on(async {
            let _keep_mutex = single_instance;
            let _keep_obs = &core.observability;
            let _ = tokio::signal::ctrl_c().await;
            let _ = core.shutdown.execute(TraceId::new()).await;
        });
        return ExitCode::SUCCESS;
    }

    tauri::async_runtime::set(rt.handle().clone());
    let specta_builder = build_specta_builder();
    let user_quit_requested = Arc::new(AtomicBool::new(false));
    let quit_flag_run = Arc::clone(&user_quit_requested);

    let app_service = Arc::clone(&core.app);
    let event_bus = Arc::clone(&core.event_bus);
    let shutdown_coord = Arc::clone(&core.shutdown);
    let _keep_obs = core.observability;
    let _keep_mutex = single_instance;

    let builder_res = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(CoreState(Arc::clone(&app_service)))
        .invoke_handler(specta_builder.invoke_handler())
        .setup(move |tauri_app| {
            let handle = tauri_app.handle().clone();

            // Build System Tray (`§15.6` & `P1-T30`)
            let open_item =
                MenuItem::with_id(&handle, "open", "فتح Vortex / Open", true, None::<&str>)?;
            let pause_item = MenuItem::with_id(
                &handle,
                "pause_all",
                "إيقاف الكل مؤقتاً / Pause All",
                true,
                None::<&str>,
            )?;
            let quit_item =
                MenuItem::with_id(&handle, "quit", "خروج آمن / Exit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(&handle, &[&open_item, &pause_item, &quit_item])?;

            let tray_app_svc = Arc::clone(&app_service);
            let tray_shutdown = Arc::clone(&shutdown_coord);
            let tray_quit_flag = Arc::clone(&user_quit_requested);

            let mut tray_builder = TrayIconBuilder::new()
                .menu(&tray_menu)
                .tooltip("Vortex Download Manager")
                .on_menu_event(move |app_h, ev| match ev.id.as_ref() {
                    "open" => open_or_focus_main_window(app_h),
                    "pause_all" => {
                        let svc = Arc::clone(&tray_app_svc);
                        tauri::async_runtime::spawn(async move {
                            let _ = svc.pause_all(TraceId::new()).await;
                        });
                    }
                    "quit" => {
                        tray_quit_flag.store(true, Ordering::SeqCst);
                        let sc = Arc::clone(&tray_shutdown);
                        let h = app_h.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = sc.execute(TraceId::new()).await;
                            h.exit(0);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        open_or_focus_main_window(tray.app_handle());
                    }
                });

            if let Some(default_icon) = handle.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(default_icon);
            }
            let tray_icon = tray_builder.build(&handle)?;

            // Forward `<= 10Hz` Progress Deltas, live Tray tooltip (`UX-M12`), Domain Completions (Toast!), and `BringToFront` to WebView2 when open
            let mut rx = vortex_application::EventBus::subscribe(event_bus.as_ref());
            let fwd_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut last_tray_update = std::time::Instant::now()
                    .checked_sub(Duration::from_secs(2))
                    .unwrap_or_else(std::time::Instant::now);
                while let Ok(ev) = rx.recv().await {
                    match ev {
                        AppEvent::ProgressBatch(batch) => {
                            if last_tray_update.elapsed() >= Duration::from_secs(1) {
                                last_tray_update = std::time::Instant::now();
                                let active: usize = batch
                                    .iter()
                                    .filter(|s| s.state_slug == "downloading")
                                    .count();
                                let total_bps: f64 = batch
                                    .iter()
                                    .filter(|s| s.state_slug == "downloading")
                                    .map(|s| s.speed_fast_bps)
                                    .sum();
                                let tip = if active > 0 {
                                    let mb_s = total_bps / (1024.0 * 1024.0);
                                    format!("Vortex — {active} active · {mb_s:.2} MB/s")
                                } else {
                                    "Vortex Download Manager".to_owned()
                                };
                                let _ = tray_icon.set_tooltip(Some(tip));
                            }
                            if fwd_handle.get_webview_window("main").is_some() {
                                let _ = fwd_handle.emit("vortex://progress-batch", &batch);
                            }
                        }
                        AppEvent::Domain(env) => {
                            if let DomainEvent::DownloadCompleted { .. } = &env.event {
                                let _ = fwd_handle
                                    .notification()
                                    .builder()
                                    .title("Vortex — اكتمل التحميل")
                                    .body(format!("اكتمل التحميل بنجاح ({})", env.download_id))
                                    .show();
                            }
                            if fwd_handle.get_webview_window("main").is_some() {
                                let _ = fwd_handle
                                    .emit("vortex://domain-event", &env.download_id.to_string());
                            }
                        }
                        AppEvent::StagedDownloadsChanged {
                            pending_count,
                            latest_staged_id,
                            burst_detected,
                        } => {
                            if pending_count > 0 {
                                open_or_focus_confirm_window(&fwd_handle);
                            }
                            let _ = fwd_handle.emit(
                                "vortex://staged-changed",
                                StagedChangedEventPayload {
                                    pending_count,
                                    latest_staged_id,
                                    burst_detected,
                                },
                            );
                        }
                        AppEvent::BringToFront { open_url } => {
                            let is_staged_url = open_url
                                .as_deref()
                                .is_some_and(|u| u.starts_with("vortex://staged/"));
                            if is_staged_url {
                                open_or_focus_confirm_window(&fwd_handle);
                            } else {
                                open_or_focus_main_window(&fwd_handle);
                                if let Some(u) = open_url {
                                    let _ = fwd_handle.emit("vortex://open-url", u);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            });

            // Only create WebView2 window immediately when NOT launched with `--background`!
            if !background_only {
                open_or_focus_main_window(&handle);
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                let state = window.state::<CoreState>();
                if state.0.current_config().warm_instant_webview {
                    // Optional Warm Instant Mode (`< 80ms` dialog open on high-RAM machines)
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Default (`warm_instant_webview == false`): allow Tauri to DESTROY the WebView2
                // window completely so Idle RAM stays `< 45 MB` (`< 60 MB` ceiling)!
            }
        })
        .build(tauri::generate_context!());

    let Ok(tauri_app) = builder_res else {
        return ExitCode::from(1);
    };

    tauri_app.run(move |_app_handle, run_ev| {
        if let RunEvent::ExitRequested { api, .. } = run_ev
            && !quit_flag_run.load(Ordering::SeqCst)
        {
            // Keep Rust Core + Tray alive when the last WebView2 window is destroyed!
            api.prevent_exit();
        }
    });

    ExitCode::SUCCESS
}
