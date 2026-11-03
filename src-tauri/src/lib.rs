mod claude;
mod codex;
mod cursor;
mod events;
mod helper;
mod links;
mod login;
mod monitor;
mod phone;
mod settings;
mod storage;
mod system;
mod tray;
mod updates;

use chrono::{Local, TimeDelta};
use monitor::{Kicker, Paths, Shared, Snapshot};
use serde::Serialize;
use settings::Settings;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{mpsc, Arc, Mutex},
};
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};

#[derive(Serialize)]
struct LogView {
    /// Newest first.
    events: Vec<events::Event>,
    summary: Option<events::Summary>,
}

#[tauri::command]
fn get_state(shared: State<Shared>) -> Snapshot {
    shared.lock().unwrap().snapshot.clone()
}

/// May show the admin password prompt, so it runs off the main thread. The
/// prompt steals focus and hides the popover; it reopens with the result.
#[tauri::command]
async fn arm(app: AppHandle) -> Result<(), String> {
    let shared = app.state::<Shared>().inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || monitor::arm(&shared))
        .await
        .map_err(|e| e.to_string())?;
    app.state::<Kicker>().kick();
    tray::show_window(&app);
    result
}

#[tauri::command]
fn disarm(shared: State<Shared>, kick: State<Kicker>) {
    monitor::disarm(&shared, "You stopped it.");
    kick.kick();
}

#[tauri::command]
fn save_settings(settings: Settings, shared: State<Shared>, kick: State<Kicker>) -> Result<(), String> {
    monitor::apply_settings(&shared, settings)?;
    kick.kick();
    Ok(())
}

#[tauri::command]
fn scan_storage(app: AppHandle) {
    monitor::start_scan(&app);
}

#[tauri::command]
fn clean_up_storage(shared: State<Shared>) -> Result<(), String> {
    let (paths, prompt) = {
        let core = shared.lock().unwrap();
        (core.paths.clone(), core.settings.cleanup_prompt.clone())
    };
    storage::launch_cleanup(&paths.home, &paths.data, &prompt)?;
    events::append(&paths.log, "cleanup", "Asked Claude to clean up storage", None);
    Ok(())
}

#[tauri::command]
fn get_log(shared: State<Shared>) -> LogView {
    let log = shared.lock().unwrap().paths.log.clone();
    let now = Local::now();
    let events = events::read_since(&log, now - TimeDelta::hours(36));
    let summary = events::summarize(&events, now);
    LogView { events: events.into_iter().rev().take(200).collect(), summary }
}

#[tauri::command]
fn reveal_log(shared: State<Shared>) {
    let log = shared.lock().unwrap().paths.log.clone();
    let _ = Command::new("/usr/bin/open").arg("-R").arg(log).status();
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// The documentation, at one of its sections ("reset", "lid-lock", ...).
#[tauri::command]
fn open_docs(section: Option<String>) {
    links::docs(section.as_deref());
}

/// A new GitHub issue with the app and macOS versions filled in.
#[tauri::command]
fn report_problem(app: AppHandle) {
    links::report_problem(&app.package_info().version.to_string());
}

/// A fresh, hard-to-guess topic on the public ntfy server.
#[tauri::command]
fn new_ntfy_topic() -> String {
    phone::new_topic_url()
}

/// Sends a test alert to the address in the field, saved or not.
#[tauri::command]
async fn test_phone_alert(url: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        phone::send(&url, "Overnight test alert", "Phone alerts work. You'll get one when Claude needs you.")
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The popover page calls this once it has drawn the last reading; take a
/// fresh one now that someone is looking.
#[tauri::command]
fn popover_ready(kick: State<Kicker>) {
    kick.kick();
}

/// The popover's Update button. On success the app restarts into the new
/// version, so this only ever returns an error.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    updates::install(app).await
}

#[tauri::command]
fn open_release(app: AppHandle) {
    if let Some(version) = updates::found_version(&app) {
        links::release(&version);
    }
}

#[tauri::command]
fn set_open_at_login(enabled: bool, app: AppHandle, kick: State<Kicker>) -> Result<(), String> {
    let result = login::set(&app, enabled);
    kick.kick();
    result
}

/// Escape in the popover.
#[tauri::command]
fn hide_popover(app: AppHandle) {
    tray::hide_window(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if std::env::args().any(|a| a == "--probe") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        println!("{}", serde_json::to_string_pretty(&monitor::probe(&home)).unwrap());
        return;
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let home = app.path().home_dir()?;
            let data = app.path().app_data_dir()?;
            fs::create_dir_all(&data)?;
            let welcomed = data.join(".welcomed");
            login::default_on(app.handle(), &home, &data);
            let paths = Paths {
                ctl: data.join("ctl"),
                log: data.join("events.jsonl"),
                settings: data.join("settings.json"),
                home,
                data,
            };
            let shared: Shared = Arc::new(Mutex::new(monitor::Core::new(paths)));
            let (tx, rx) = mpsc::channel();
            app.manage(shared.clone());
            app.manage(Kicker(Mutex::new(tx)));
            app.manage(updates::Updates::default());
            tray::build(app.handle())?;
            monitor::spawn(app.handle().clone(), shared, rx);
            updates::spawn(app.handle().clone());
            // First launch: open the popover so it's clear where the app lives.
            if !welcomed.exists() {
                let _ = fs::write(&welcomed, b"");
                tray::show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(false) = event {
                tray::hide_window(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            arm,
            disarm,
            save_settings,
            scan_storage,
            clean_up_storage,
            get_log,
            reveal_log,
            quit,
            popover_ready,
            hide_popover,
            new_ntfy_topic,
            test_phone_alert,
            open_docs,
            report_problem,
            install_update,
            open_release,
            set_open_at_login
        ])
        .build(tauri::generate_context!())
        .expect("error while building Overnight");

    app.run(|app, event| match event {
        // Closing the popover's page leaves no windows open; that alone
        // mustn't quit the app. Quit calls exit with a code.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        // The helper also restores sleep on its own once this process is gone;
        // this just makes it immediate.
        RunEvent::Exit => {
            if let Some(shared) = app.try_state::<Shared>() {
                monitor::disarm(&shared, "Overnight was quit.");
            }
        }
        _ => {}
    });
}
