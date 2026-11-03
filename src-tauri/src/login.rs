//! Open at login, through a LaunchAgent in ~/Library/LaunchAgents. macOS
//! holds the setting, so it isn't in settings.json.

use std::{fs, path::Path};
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

pub fn enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

pub fn set(app: &AppHandle, on: bool) -> Result<(), String> {
    let launch = app.autolaunch();
    if on { launch.enable() } else { launch.disable() }.map_err(|e| e.to_string())
}

/// Turns Open at login on the first time Overnight runs from an Applications
/// folder, and only then, so switching it off sticks. A copy run from
/// Downloads is skipped: the LaunchAgent would point at a path that won't last.
pub fn default_on(app: &AppHandle, home: &Path, data: &Path) {
    let marker = data.join(".login-default");
    if marker.exists() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if in_applications(&exe, home) && set(app, true).is_ok() {
        let _ = fs::write(marker, b"");
    }
}

fn in_applications(exe: &Path, home: &Path) -> bool {
    exe.starts_with("/Applications") || exe.starts_with(home.join("Applications"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_from_an_applications_folder() {
        let home = Path::new("/Users/me");
        assert!(in_applications(Path::new("/Applications/Overnight.app/Contents/MacOS/overnight"), home));
        assert!(in_applications(Path::new("/Users/me/Applications/Overnight.app/Contents/MacOS/overnight"), home));
        assert!(!in_applications(Path::new("/Users/me/Downloads/Overnight.app/Contents/MacOS/overnight"), home));
        assert!(!in_applications(
            Path::new("/private/var/folders/x/AppTranslocation/1/d/Overnight.app/Contents/MacOS/overnight"),
            home
        ));
    }
}
