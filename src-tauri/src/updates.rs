//! New versions from GitHub Releases. Overnight looks for one now and then and
//! says when it finds one, but installs only when asked, and never while it's
//! keeping the Mac awake: the restart would end the night, since the helper
//! puts normal sleep back as soon as this process is gone.

use crate::{
    events,
    monitor::{Kicker, Shared},
    tray,
};
use serde::Serialize;
use std::{
    fs,
    sync::Mutex,
    thread,
    time::{Duration, SystemTime},
};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::{Update, UpdaterExt};

/// Long enough after launch that a login doesn't wait on the network.
const FIRST_CHECK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// The thread wakes this often to see whether a check is due. Sleep pauses
/// its timer, so it can't simply wait the full six hours.
const WAKE: Duration = Duration::from_secs(30 * 60);
const BUSY: &str = "Overnight can update once it stops keeping the Mac awake.";

#[derive(Default)]
pub struct Updates(Mutex<State>);

#[derive(Default)]
struct State {
    found: Option<Update>,
    installing: bool,
    error: Option<String>,
}

/// What the popover shows about a new version.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateView {
    pub version: String,
    pub installing: bool,
    pub error: Option<String>,
}

pub fn view(app: &AppHandle) -> Option<UpdateView> {
    let st = app.try_state::<Updates>()?;
    let st = st.0.lock().unwrap();
    let found = st.found.as_ref()?;
    Some(UpdateView { version: found.version.clone(), installing: st.installing, error: st.error.clone() })
}

/// The version found, for the "What's new" link.
pub fn found_version(app: &AppHandle) -> Option<String> {
    view(app).map(|v| v.version)
}

pub fn spawn(app: AppHandle) {
    thread::spawn(move || {
        thread::sleep(FIRST_CHECK);
        let mut last: Option<SystemTime> = None;
        loop {
            let on = app.state::<Shared>().lock().unwrap().settings.check_updates;
            let now = SystemTime::now();
            if on && due(last, now) {
                // A failed check (offline, say) waits for the next one too.
                last = Some(now);
                let _ = check(&app);
            }
            thread::sleep(WAKE);
        }
    });
}

/// The tray's Check for Updates: checks now, even with checking turned off,
/// and always answers.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || match check(&app) {
        Ok(Some(_)) => tray::show_window(&app),
        Ok(None) => notify(&app, "Overnight is up to date", &format!("You have version {}.", app.package_info().version)),
        Err(e) => notify(&app, "Couldn't check for updates", &e),
    });
}

/// Asks GitHub for the latest release. A version newer than this one is kept
/// for the popover, and announced once.
fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let found = tauri::async_runtime::block_on(async {
        app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())
    })?;
    let Some(update) = found else {
        app.state::<Updates>().inner().0.lock().unwrap().found = None;
        return Ok(None);
    };
    let version = update.version.clone();
    {
        let mut st = app.state::<Updates>().inner().0.lock().unwrap();
        if !st.installing {
            st.found = Some(update);
            st.error = None;
        }
    }
    let log = app.state::<Shared>().lock().unwrap().paths.log.clone();
    let seen = log.with_file_name("update-announced");
    if fs::read_to_string(&seen).ok().as_deref().map(str::trim) != Some(version.as_str()) {
        let _ = fs::write(&seen, &version);
        events::append(&log, "update_available", format!("Overnight {version} is available"), None);
        notify(app, &format!("Overnight {version} is available"), "Open Overnight to update.");
    }
    app.state::<Kicker>().kick();
    Ok(Some(version))
}

/// Downloads and installs the version found, then restarts into it.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let busy = || app.state::<Shared>().lock().unwrap().busy();
    if busy() {
        return Err(BUSY.into());
    }
    let update = {
        let mut st = app.state::<Updates>().inner().0.lock().unwrap();
        let Some(update) = st.found.clone() else {
            return Err("There's no update to install.".into());
        };
        st.installing = true;
        st.error = None;
        update
    };
    app.state::<Kicker>().kick();

    let result = async {
        let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
        // Keep awake tonight may have been pressed during the download.
        if busy() {
            return Err(BUSY.to_string());
        }
        update.install(bytes).map_err(|e| e.to_string())
    }
    .await;

    if let Err(e) = &result {
        let mut st = app.state::<Updates>().inner().0.lock().unwrap();
        st.installing = false;
        st.error = Some(e.clone());
        drop(st);
        app.state::<Kicker>().kick();
        return result;
    }
    let log = app.state::<Shared>().lock().unwrap().paths.log.clone();
    events::append(&log, "updated", format!("Updated to {}", update.version), None);
    app.restart();
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Whether the next background check is due. A clock set backwards counts as
/// due, rather than waiting until it catches up.
fn due(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t).map_or(true, |d| d >= EVERY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_every_six_hours() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(due(None, t));
        assert!(!due(Some(t), t + Duration::from_secs(5 * 3600)));
        assert!(due(Some(t), t + EVERY));
        assert!(due(Some(t), t - Duration::from_secs(60)));
    }
}
