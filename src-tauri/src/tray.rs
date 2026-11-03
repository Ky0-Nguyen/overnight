//! The menu bar icon, its right-click menu, and the popover under it.

use crate::{
    links,
    monitor::{self, Kicker, Shared, Snapshot},
    updates,
};
use std::{
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WebviewWindow, WebviewWindowBuilder, Wry,
};
use tauri_plugin_positioner::{Position, WindowExt};

const TRAY_ID: &str = "main";
/// How long a hidden popover keeps its page, so a quick reopen reuses it.
const CLOSE_AFTER: Duration = Duration::from_secs(3);
const OFF: &[u8] = include_bytes!("../icons/tray/off.png");
const STANDBY: &[u8] = include_bytes!("../icons/tray/standby.png");
const ON: &[u8] = include_bytes!("../icons/tray/on.png");

static ICON: AtomicU8 = AtomicU8::new(u8::MAX);
static TRAY_PLACED: AtomicBool = AtomicBool::new(false);
static HIDDEN_AT_MS: AtomicU64 = AtomicU64::new(0);
/// Counts openings, so a close scheduled before the latest one is dropped.
static OPENED: AtomicU64 = AtomicU64::new(0);

pub struct TrayMenu {
    toggle: MenuItem<Wry>,
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Overnight", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Keep awake tonight", true, None::<&str>)?;
    let help = MenuItem::with_id(app, "help", "Help", true, None::<&str>)?;
    let report = MenuItem::with_id(app, "report", "Report a Problem…", true, None::<&str>)?;
    let updates = MenuItem::with_id(app, "updates", "Check for Updates…", true, None::<&str>)?;
    let github = MenuItem::with_id(app, "github", "Overnight on GitHub", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Overnight", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&open, &toggle, &separator, &help, &report, &updates, &github, &separator2, &quit],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(OFF)?)
        .icon_as_template(true)
        .tooltip("Overnight")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "toggle" => toggle_armed(app),
            "help" => links::docs(None),
            "report" => links::report_problem(&app.package_info().version.to_string()),
            "updates" => updates::check_now(app),
            "github" => links::repo(),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
            TRAY_PLACED.store(true, Ordering::Relaxed);
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(TrayMenu { toggle });
    Ok(())
}

/// Swaps the icon and menu text when the state changes, and keeps the tooltip current.
pub fn update(app: &AppHandle, s: &Snapshot) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let state: u8 = match (s.armed, s.holding) {
        (true, true) => 2,
        (true, false) => 1,
        _ => 0,
    };
    if ICON.swap(state, Ordering::Relaxed) != state {
        if let Ok(img) = Image::from_bytes([OFF, STANDBY, ON][usize::from(state)]) {
            let _ = tray.set_icon(Some(img));
            let _ = tray.set_icon_as_template(true);
        }
        if let Some(menu) = app.try_state::<TrayMenu>() {
            let _ = menu
                .toggle
                .set_text(if s.armed { "Let it sleep" } else { "Keep awake tonight" });
        }
    }
    let _ = tray.set_tooltip(Some(format!("Overnight: {}", s.status)));
}

/// Opens the popover under the menu bar icon. Its page is closed a few seconds
/// after it hides, so this usually creates it again.
pub fn show_window(app: &AppHandle) {
    OPENED.fetch_add(1, Ordering::Relaxed);
    if let Some(w) = app.get_webview_window("main") {
        reveal(&w);
        // The popover isn't updated while hidden: show the last reading
        // straight away, then take a fresh one.
        let snapshot = app.state::<Shared>().lock().unwrap().snapshot.clone();
        let _ = app.emit("state", snapshot);
        app.state::<Kicker>().kick();
        return;
    }
    let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main") else {
        return;
    };
    match WebviewWindowBuilder::from_config(app, config).and_then(|b| b.build()) {
        // Shown at once, while macOS still treats this as the user's click,
        // so the popover takes focus and hides again when it loses it. The
        // window is transparent until the page draws, a moment later.
        Ok(w) => reveal(&w),
        Err(e) => eprintln!("Couldn't open the popover: {e}"),
    }
}

/// Places the popover under the menu bar icon and shows it.
pub fn reveal(w: &WebviewWindow) {
    let at = if TRAY_PLACED.load(Ordering::Relaxed) {
        Position::TrayCenter
    } else {
        Position::TopRight
    };
    let _ = w.move_window(at);
    let _ = w.show();
    let _ = w.set_focus();
}

pub fn popover_open(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .is_some_and(|w| w.is_visible().unwrap_or(false))
}

/// Hides the popover (it lost focus, or Escape), then closes its page unless
/// it opens again first. The page and its WebKit processes take about 50 MB
/// while they exist; reopening costs a fraction of a second.
pub fn hide_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let _ = w.hide();
    HIDDEN_AT_MS.store(now_ms(), Ordering::Relaxed);
    let opened = OPENED.load(Ordering::Relaxed);
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(CLOSE_AFTER);
        if OPENED.load(Ordering::Relaxed) != opened {
            return;
        }
        if let Some(w) = app.get_webview_window("main").filter(|w| !w.is_visible().unwrap_or(true)) {
            let _ = w.destroy();
        }
    });
}

fn toggle_window(app: &AppHandle) {
    if popover_open(app) {
        hide_window(app);
    } else if now_ms().saturating_sub(HIDDEN_AT_MS.load(Ordering::Relaxed)) > 300 {
        // Clicking the icon while the popover is open blurs it first; don't
        // reopen what that same click just closed.
        show_window(app);
    }
}

fn toggle_armed(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        let shared = app.state::<Shared>().inner().clone();
        let armed = shared.lock().unwrap().armed();
        if armed {
            monitor::disarm(&shared, "You stopped it from the menu bar.");
        } else {
            let _ = monitor::arm(&shared);
        }
        app.state::<Kicker>().kick();
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
