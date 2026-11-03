//! The monitor loop: reads the Mac's state every few seconds, applies the
//! stay-awake policy and its guards, logs what changed, and updates the
//! popover and the menu bar icon.

use crate::{
    claude::{Ask, ClaudeInfo, LimitState, Turn, Watcher},
    events, helper, phone,
    settings::{Alert, Mode, Settings},
    storage,
    system::{self, Disk, Power, Thermal},
    tray,
};
use chrono::{DateTime, Local, TimeDelta};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

const TICK: Duration = Duration::from_secs(10);
const IDLE_TICK: Duration = Duration::from_secs(30);
const BATTERY_LOG_EVERY: Duration = Duration::from_secs(30 * 60);
const AUTO_SCAN_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Seconds to wait for a freshly launched helper to show up in `ps`.
const HELPER_START_TIMEOUT: i64 = 20;
const GB: u64 = 1_000_000_000;
/// A hold the helper applied, then something else undid. It's reported after
/// two ticks (a fresh hold takes up to 5 s to apply), and called broken only
/// if the helper's 30-second check hasn't put it back.
const MISMATCH_NOTE_AFTER: Duration = Duration::from_secs(15);
const MISMATCH_AFTER: Duration = Duration::from_secs(50);
const MISMATCH: &str = "macOS still reports normal sleep, so closing the lid would sleep the Mac. \
Press Let it sleep, then Keep awake tonight again.";
/// Questions older than this when Overnight first sees them (say, from
/// before it started) are shown in the popover but not announced.
const ASK_NOTE_WITHIN: Duration = Duration::from_secs(30 * 60);
/// Claude has to have worked this long for "Claude is done" to be worth a
/// note. Quick replies you're watching, and scheduled checks that run for a
/// minute every hour, end without one.
const DONE_AFTER_WORK: Duration = Duration::from_secs(5 * 60);

pub type Shared = Arc<Mutex<Core>>;

/// Wakes the monitor loop early, e.g. right after a button press.
pub struct Kicker(pub Mutex<Sender<()>>);

impl Kicker {
    pub fn kick(&self) {
        let _ = self.0.lock().unwrap().send(());
    }
}

#[derive(Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    /// Control files the root helper watches.
    pub ctl: PathBuf,
    pub log: PathBuf,
    pub settings: PathBuf,
}

/// Everything the popover shows, sent to it on every tick while it's open.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    /// The app's version, shown with the Help link.
    pub version: &'static str,
    pub armed: bool,
    pub holding: bool,
    pub sleep_disabled: bool,
    pub helper_pid: Option<u32>,
    pub armed_since: Option<DateTime<Local>>,
    pub stops_at: Option<DateTime<Local>>,
    pub status: String,
    pub detail: String,
    pub power: Power,
    pub lid_closed: Option<bool>,
    pub thermal: Thermal,
    pub claude: ClaudeInfo,
    pub disk: Disk,
    pub disk_low: bool,
    pub scan: Option<storage::Scan>,
    pub scanning: bool,
    pub settings: Settings,
    pub last_guard: Option<String>,
    pub error: Option<String>,
}

pub struct Core {
    pub paths: Paths,
    pub settings: Settings,
    pub snapshot: Snapshot,
    pub error: Option<String>,
    want_armed: bool,
    /// True while the password prompt is up and the helper is starting.
    arming: bool,
    helper_seen: bool,
    /// When macOS started disagreeing with a hold the helper should have applied.
    mismatch_since: Option<Instant>,
    mismatch_noted: bool,
    /// Sleep is disabled by something other than Overnight (already reported).
    external_off: bool,
    armed_at: Option<DateTime<Local>>,
    stops_at: Option<DateTime<Local>>,
    holding: bool,
    claude_active: bool,
    /// When Claude last started working.
    working_since: Option<Instant>,
    /// How long the spell of work that just ended lasted, until its "done"
    /// note goes out or is skipped.
    worked: Option<Duration>,
    /// The usage-limit state seen last tick, to log and notify on changes.
    limit_seen: Option<(LimitState, Option<DateTime<Local>>)>,
    /// Questions and approvals seen last tick, so each is announced once.
    asks_seen: Vec<Ask>,
    watcher: Watcher,
    prev_lid: Option<bool>,
    prev_ac: Option<bool>,
    disk_low: bool,
    last_battery_log: Option<Instant>,
    scan: Option<storage::Scan>,
    scanning: bool,
    last_scan_at: Option<Instant>,
    last_guard: Option<String>,
}

impl Core {
    pub fn new(paths: Paths) -> Self {
        let mut settings = Settings::load(&paths.settings);
        // Settings from before the first-run note existed: the log knows
        // whether Keep awake tonight has ever worked.
        if !settings.armed_before
            && std::fs::read_to_string(&paths.log).is_ok_and(|l| l.contains("\"kind\":\"arm\""))
        {
            settings.armed_before = true;
            let _ = settings.save(&paths.settings);
        }
        Self {
            watcher: Watcher::new(&paths.home),
            snapshot: Snapshot {
                settings: settings.clone(),
                status: "Starting".into(),
                ..Default::default()
            },
            settings,
            paths,
            error: None,
            want_armed: false,
            arming: false,
            helper_seen: false,
            mismatch_since: None,
            mismatch_noted: false,
            external_off: false,
            armed_at: None,
            stops_at: None,
            holding: false,
            claude_active: false,
            working_since: None,
            worked: None,
            limit_seen: None,
            asks_seen: Vec::new(),
            prev_lid: None,
            prev_ac: None,
            disk_low: false,
            last_battery_log: None,
            scan: None,
            scanning: false,
            last_scan_at: None,
            last_guard: None,
        }
    }

    pub fn armed(&self) -> bool {
        self.want_armed
    }
}

pub fn spawn(app: AppHandle, shared: Shared, rx: Receiver<()>) {
    thread::spawn(move || loop {
        tick(&app, &shared);
        // Not armed and the popover closed: only notifications depend on the
        // tick, and they can be a little late. Button presses still kick it.
        let busy = {
            let core = shared.lock().unwrap();
            core.want_armed || core.arming
        };
        let wait = if busy || tray::popover_open(&app) { TICK } else { IDLE_TICK };
        match rx.recv_timeout(wait) {
            Err(RecvTimeoutError::Disconnected) => break,
            _ => while rx.try_recv().is_ok() {},
        }
    });
}

/// Starts keeping the Mac awake. When the helper isn't running yet this shows
/// the admin password prompt and blocks until it's answered, so call it off
/// the main thread.
pub fn arm(shared: &Shared) -> Result<(), String> {
    // Stops the monitor from clearing the hold file while the prompt is up.
    shared.lock().unwrap().arming = true;
    let result = start_helper(shared);
    let mut core = shared.lock().unwrap();
    core.arming = false;
    if let Err(e) = &result {
        core.error = Some(e.clone());
        return result;
    }
    let now = Local::now();
    core.want_armed = true;
    core.helper_seen = false;
    core.armed_at = Some(now);
    core.stops_at = next_stop(&core.settings, now);
    core.holding = true;
    core.last_guard = None;
    core.error = None;
    core.last_battery_log = Some(Instant::now());
    // The first-run note about the password prompt has done its job.
    if !core.settings.armed_before {
        core.settings.armed_before = true;
        let _ = core.settings.save(&core.paths.settings);
        core.snapshot.settings = core.settings.clone();
    }

    let log = core.paths.log.clone();
    let until = core
        .stops_at
        .map_or(String::new(), |t| format!(" until {}", system::clock(t)));
    let msg = match core.settings.mode {
        Mode::Claude => format!("Keeping the Mac awake while Claude works{until}"),
        Mode::Night => format!("Keeping the Mac awake all night{until}"),
    };
    events::append(&log, "arm", msg, core.snapshot.power.percent);
    events::append(&log, "hold_on", "Holding: the lid can close", None);
    if core.claude_active {
        events::append(&log, "claude_active", "Claude is working", None);
    }
    Ok(())
}

fn start_helper(shared: &Shared) -> Result<(), String> {
    let ctl = shared.lock().unwrap().paths.ctl.clone();
    let releasing = ctl.join("release").exists();
    if helper::status(&ctl).running && !releasing {
        return helper::set_hold(&ctl, true).map_err(|e| e.to_string());
    }
    if !helper::wait_until_stopped(&ctl) {
        return Err("The last helper is still shutting down. Try again in a few seconds.".into());
    }
    helper::prepare(&ctl).map_err(|e| e.to_string())?;
    helper::launch(&ctl, std::process::id())
}

/// Lets the Mac sleep normally again.
pub fn disarm(shared: &Shared, reason: &str) {
    let mut core = shared.lock().unwrap();
    let battery = core.snapshot.power.percent;
    core.error = None;
    end_run(&mut core, "disarm", reason, battery);
}

fn end_run(core: &mut Core, kind: &str, reason: &str, battery: Option<u8>) {
    helper::release(&core.paths.ctl);
    if core.want_armed {
        events::append(&core.paths.log, kind, reason, battery);
    }
    core.want_armed = false;
    core.helper_seen = false;
    core.holding = false;
    core.armed_at = None;
    core.stops_at = None;
}

pub fn apply_settings(shared: &Shared, settings: Settings) -> Result<(), String> {
    let mut core = shared.lock().unwrap();
    let s = settings.sanitized();
    s.save(&core.paths.settings).map_err(|e| e.to_string())?;
    if core.want_armed {
        core.stops_at = next_stop(&s, Local::now());
    }
    core.snapshot.settings = s.clone();
    core.settings = s;
    Ok(())
}

pub fn tick(app: &AppHandle, shared: &Shared) {
    // Readings first, outside the lock.
    let ctl = shared.lock().unwrap().paths.ctl.clone();
    let power = system::power();
    let lid = system::lid_closed();
    let thermal = system::thermal();
    let disk = system::disk();
    let sleep_disabled = system::sleep_disabled();
    let helper = helper::status(&ctl);
    let now = Local::now();

    let mut notes: Vec<Note> = Vec::new();
    let mut scan = false;
    let snapshot = {
        let mut guard = shared.lock().unwrap();
        let core = &mut *guard;
        let s = core.settings.clone();
        let idle_after = Duration::from_secs(u64::from(s.idle_minutes) * 60);
        let claude = core.watcher.poll(idle_after);
        let log = core.paths.log.clone();
        let pct = power.percent;

        if let (Some(prev), Some(cur)) = (core.prev_lid, lid) {
            if prev != cur {
                let (kind, msg) = if cur { ("lid_closed", "Lid closed") } else { ("lid_opened", "Lid opened") };
                events::append(&log, kind, msg, pct);
            }
        }
        core.prev_lid = lid;
        if core.prev_ac.is_some_and(|ac| ac != power.on_ac) {
            let msg = if power.on_ac { "Plugged in" } else { "Running on battery" };
            events::append(&log, "power", msg, pct);
        }
        core.prev_ac = Some(power.on_ac);

        // The helper process is the source of truth for "armed".
        if helper.running {
            core.helper_seen = true;
        }
        if core.want_armed && !helper.running {
            let late = core
                .armed_at
                .is_some_and(|t| (now - t).num_seconds() > HELPER_START_TIMEOUT);
            if core.helper_seen || late {
                let msg = if core.helper_seen {
                    "The helper stopped unexpectedly. Normal sleep is back."
                } else {
                    "The helper didn't start. Normal sleep is back."
                };
                end_run(core, "helper_lost", msg, pct);
                core.error = Some(msg.into());
                notes.push((Alert::Helper, "Overnight stopped keeping the Mac awake".into(), msg.into()));
            }
        }
        if !core.want_armed && !core.arming && !helper.running {
            helper::cleanup(&core.paths.ctl);
        }

        if claude.active != core.claude_active {
            if claude.active {
                events::append(&log, "claude_active", "Claude is working", None);
            } else {
                events::append(&log, "claude_idle", "Claude is done or waiting for you", None);
            }
        }
        let went_idle = core.claude_active && !claude.active;
        if claude.active && !core.claude_active {
            core.working_since = Some(Instant::now());
            core.worked = None;
        }
        if went_idle {
            core.worked = core.working_since.map(|t| t.elapsed());
        }
        core.claude_active = claude.active;

        let limit_now = claude.limit.as_ref().map(|l| (l.state, l.resets_at));
        if limit_now != core.limit_seen {
            if let Some((kind, msg, note)) = limit_change(core.limit_seen, &claude, now) {
                events::append(&log, kind, msg, None);
                notes.extend(note);
            }
            core.limit_seen = limit_now;
        }
        // A usage limit or a question has its own notes, so "Claude is idle"
        // would only repeat them.
        let limit_pending = claude.limit.is_some() || claude.needs_you > 0;

        let fresh: Vec<&Ask> = claude
            .asks
            .iter()
            .filter(|a| !core.asks_seen.contains(a))
            .filter(|a| a.since.elapsed().unwrap_or_default() < ASK_NOTE_WITHIN)
            .collect();
        if let Some(note) = ask_note(&fresh) {
            events::append(&log, "claude_asks", note.1.clone(), None);
            notes.push(note);
        }
        core.asks_seen = claude.asks.clone();

        let mut released_for_idle = false;
        if core.want_armed {
            if let Some((alert, reason)) = guard_trip(&s, &power, thermal, core.stops_at, now) {
                end_run(core, "guard", &reason, pct);
                notes.push((alert, "Overnight let the Mac sleep again".into(), reason.clone()));
                core.last_guard = Some(reason);
            } else {
                let grace = core
                    .armed_at
                    .is_some_and(|t| (now - t).to_std().unwrap_or_default() < idle_after);
                let hold = should_hold(s.mode, claude.active, grace);
                // Checked every tick, so the helper follows even if the file goes missing.
                if hold != core.paths.ctl.join("hold").exists() {
                    if let Err(e) = helper::set_hold(&core.paths.ctl, hold) {
                        core.error = Some(e.to_string());
                    }
                }
                if hold != core.holding {
                    if hold {
                        events::append(&log, "hold_on", "Holding: the lid can close", pct);
                    } else {
                        events::append(&log, "hold_off", "Released: the Mac can sleep", pct);
                        released_for_idle = true;
                        if !limit_pending && worked_long(core.worked.take()) {
                            notes.push((
                                Alert::Done,
                                "Claude is done".into(),
                                "Nothing is left running, so the Mac can sleep now.".into(),
                            ));
                        }
                    }
                    core.holding = hold;
                }
                if core.last_battery_log.is_none_or(|t| t.elapsed() >= BATTERY_LOG_EVERY) {
                    if let Some(p) = pct {
                        events::append(&log, "battery", format!("Battery {p}%"), Some(p));
                    }
                    core.last_battery_log = Some(Instant::now());
                }
            }
        }
        // While a Claude-mode run is waiting out its grace period, the
        // "released" note above covers this.
        let waiting = core.want_armed && s.mode == Mode::Claude;
        if went_idle && !released_for_idle && !waiting && !limit_pending && worked_long(core.worked.take()) {
            notes.push((
                Alert::Done,
                "Claude is done".into(),
                "Nothing is left running.".into(),
            ));
        }

        let low = disk.total_bytes > 0 && disk.free_bytes < u64::from(s.storage_warn_gb) * GB;
        if low && !core.disk_low {
            let free = fmt_gb(disk.free_bytes);
            events::append(&log, "storage_low", format!("Only {free} free"), None);
            notes.push((
                Alert::Disk,
                "Disk space is getting low".into(),
                format!("{free} free. Open Overnight to ask Claude to clean up."),
            ));
        }
        core.disk_low = low;
        if low && !core.scanning && core.last_scan_at.is_none_or(|t| t.elapsed() >= AUTO_SCAN_EVERY) {
            scan = true;
        }

        // The helper applies a hold within about 5 s. If macOS still reports
        // normal sleep well after that, say so instead of claiming to be awake.
        // The registry reading is cheap; pmset, the authority, is asked only
        // when the two seem to disagree.
        let expect_disabled = core.want_armed && core.holding && helper.running;
        let sleep_disabled = sleep_disabled || (expect_disabled && system::sleep_disabled_pmset());
        if expect_disabled && !sleep_disabled {
            // Something turned sleep back on mid-hold. The helper puts the
            // hold back within 30 s; say so, and only call it broken if that
            // doesn't happen.
            let since = core.mismatch_since.get_or_insert_with(Instant::now).elapsed();
            if since >= MISMATCH_NOTE_AFTER && !core.mismatch_noted {
                core.mismatch_noted = true;
                events::append(&log, "sleep_external", "Sleep was turned back on outside Overnight", pct);
                notes.push((
                    Alert::SleepOutside,
                    "Sleep was turned back on outside Overnight".into(),
                    "Overnight is putting the hold back within 30 seconds. Until then, closing the lid sleeps the Mac."
                        .into(),
                ));
            }
            if since > MISMATCH_AFTER {
                core.error = Some(MISMATCH.into());
            }
        } else {
            core.mismatch_since = None;
            core.mismatch_noted = false;
            if core.error.as_deref() == Some(MISMATCH) {
                core.error = None;
            }
        }

        // Sleep turned off by something other than Overnight, outside a run:
        // the guards can't step in, so say so once.
        let external_off = !core.want_armed && !core.arming && sleep_disabled && !helper.running;
        if external_off && !core.external_off {
            events::append(&log, "sleep_external", "Sleep was turned off outside Overnight", pct);
            notes.push((
                Alert::SleepOutside,
                "Sleep was turned off outside Overnight".into(),
                "Closing the lid won't sleep the Mac, and Overnight's battery and heat guards aren't watching. \
To undo it, run sudo pmset -a disablesleep 0 in Terminal."
                    .into(),
            ));
        }
        core.external_off = external_off;

        let (status, detail) = status_line(core, &claude, sleep_disabled, helper.running);
        core.snapshot = Snapshot {
            armed: core.want_armed,
            holding: core.want_armed && core.holding,
            sleep_disabled,
            helper_pid: helper.pid,
            armed_since: core.armed_at,
            stops_at: core.stops_at,
            status,
            detail,
            power,
            lid_closed: lid,
            thermal,
            claude,
            disk,
            disk_low: low,
            scan: core.scan.clone(),
            scanning: core.scanning,
            settings: s,
            version: env!("CARGO_PKG_VERSION"),
            last_guard: core.last_guard.clone(),
            error: core.error.clone(),
        };
        core.snapshot.clone()
    };

    if !notes.is_empty() {
        forward_to_phone(shared, &notes, lid);
    }
    for (alert, title, body) in notes {
        if snapshot.settings.channels(alert).mac {
            let _ = app.notification().builder().title(title).body(body).show();
        }
    }
    if scan {
        start_scan(app);
    }
    publish(app, &snapshot);
}

/// Sends this tick's notifications to the phone as well, each kind the user
/// wants there, when phone alerts are on. In
/// the background, so a slow server never holds up the tick; a failure goes
/// in the log, where the morning summary shows it.
fn forward_to_phone(shared: &Shared, notes: &[Note], lid: Option<bool>) {
    let (s, log) = {
        let core = shared.lock().unwrap();
        (core.settings.clone(), core.paths.log.clone())
    };
    if !s.phone_alerts || s.ntfy_url.is_empty() || (s.phone_only_when_away && !phone::away(lid)) {
        return;
    }
    let notes: Vec<Note> = notes.iter().filter(|n| s.channels(n.0).phone).cloned().collect();
    if notes.is_empty() {
        return;
    }
    thread::spawn(move || {
        for (_, title, body) in notes {
            if let Err(e) = phone::send(&s.ntfy_url, &title, &body) {
                events::append(&log, "phone_failed", format!("Phone alert not sent: {e}"), None);
            }
        }
    });
}

/// Updates the menu bar icon, and the popover while it's open. A hidden
/// popover gets nothing: redrawing it every tick cost CPU and up to 100 MB of
/// graphics memory. `tray::show_window` catches it up when it opens.
pub fn publish(app: &AppHandle, s: &Snapshot) {
    if tray::popover_open(app) {
        let _ = app.emit("state", s);
    }
    tray::update(app, s);
}

/// Measures the usual space hogs in the background.
pub fn start_scan(app: &AppHandle) {
    let shared = app.state::<Shared>().inner().clone();
    let (home, snapshot) = {
        let mut core = shared.lock().unwrap();
        if core.scanning {
            return;
        }
        core.scanning = true;
        core.snapshot.scanning = true;
        (core.paths.home.clone(), core.snapshot.clone())
    };
    publish(app, &snapshot);
    let app = app.clone();
    thread::spawn(move || {
        let result = storage::scan(&home);
        let snapshot = {
            let mut core = shared.lock().unwrap();
            core.scanning = false;
            core.last_scan_at = Some(Instant::now());
            core.scan = Some(result.clone());
            core.snapshot.scanning = false;
            core.snapshot.scan = Some(result);
            core.snapshot.clone()
        };
        publish(&app, &snapshot);
    });
}

/// Claude mode doesn't wait out usage limits: Claude continues by itself
/// after a reset, and wakes the Mac's activity back up when it does.
/// Whether a spell of work that ended was long enough to announce.
fn worked_long(worked: Option<Duration>) -> bool {
    worked.is_some_and(|w| w >= DONE_AFTER_WORK)
}

fn should_hold(mode: Mode, claude_active: bool, grace: bool) -> bool {
    match mode {
        Mode::Night => true,
        Mode::Claude => claude_active || grace,
    }
}

/// A notification: its kind, which decides where it goes, then title and body.
type Note = (Alert, String, String);

/// What to say about sessions that just asked something. Like every note it
/// may go to the phone, so it never names a session or project.
fn ask_note(fresh: &[&Ask]) -> Option<Note> {
    let first = fresh.first()?;
    let n = fresh.len();
    let same = fresh.iter().all(|a| a.turn == first.turn);
    Some(match (first.turn, same, n) {
        (Turn::Approval, true, 1) => (
            Alert::Asks,
            "Claude needs your approval".into(),
            "A session is waiting for you to allow a file change. It can't go on until you do.".into(),
        ),
        (Turn::Approval, true, _) => (
            Alert::Asks,
            "Claude needs your approval".into(),
            format!("{n} sessions are waiting for you to allow a file change."),
        ),
        (_, true, 1) => (
            Alert::Asks,
            "Claude is asking you something".into(),
            "A session is waiting for your answer. It can't go on until you reply.".into(),
        ),
        (_, true, _) => (Alert::Asks, "Claude is asking you something".into(), format!("{n} sessions are waiting for your answer.")),
        _ => (Alert::Asks, "Claude needs you".into(), format!("{n} sessions are waiting for your answer or approval.")),
    })
}

/// What to log and notify when the usage-limit picture changes.
fn limit_change(
    before: Option<(LimitState, Option<DateTime<Local>>)>,
    claude: &ClaudeInfo,
    now: DateTime<Local>,
) -> Option<(&'static str, String, Option<Note>)> {
    let hhmm = |t: Option<DateTime<Local>>| t.map_or("an unknown time".into(), system::clock);
    let Some(limit) = &claude.limit else {
        // Gone: the sessions continued (a missed limit that simply aged out isn't news).
        return match before {
            Some((LimitState::Waiting, _)) => Some(("limit_cleared", "Claude continued after the usage limit reset".into(), None)),
            Some((LimitState::Missed, Some(at))) if now - at < TimeDelta::hours(2) => {
                Some(("limit_cleared", "Claude continued after the usage limit reset".into(), None))
            }
            _ => None,
        };
    };
    let at = hhmm(limit.resets_at);
    match (before.map(|b| b.0), limit.state, limit.resets_at) {
        (Some(LimitState::Waiting), LimitState::Waiting, _) | (Some(LimitState::Missed), LimitState::Missed, _) => None,
        (_, LimitState::Waiting, _) => {
            let body = if claude.auto_continue == Some(false) {
                format!("It resets at {at}. Auto-continue is off in Claude Code, so Claude won't continue by itself.")
            } else {
                format!("It resets at {at}, and Claude should continue by itself then.")
            };
            let note = (Alert::LimitHit, "Claude hit its usage limit".into(), body);
            Some(("limit_hit", format!("Claude hit its usage limit, resets at {at}"), Some(note)))
        }
        (_, LimitState::Missed, Some(_)) => Some((
            "limit_missed",
            format!("Claude didn't continue after the {at} reset"),
            Some((
                Alert::LimitMissed,
                "Claude didn't continue after the reset".into(),
                "Open the session and send \"continue\". Overnight shows how to make it continue by itself next time."
                    .into(),
            )),
        )),
        (_, LimitState::Missed, None) => Some((
            "limit_missed",
            format!("Claude stopped: {}", limit.message),
            Some((Alert::LimitMissed, "Claude stopped".into(), limit.message.clone())),
        )),
    }
}

/// The reason to stop keeping the Mac awake, if any guard applies.
fn guard_trip(
    s: &Settings,
    power: &Power,
    thermal: Thermal,
    stops_at: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> Option<(Alert, String)> {
    // Worded to read right both as a notification now and in the morning's
    // summary of how the night ended.
    if let Some(at) = stops_at.filter(|at| now >= *at) {
        return Some((Alert::StopTime, format!("Reached your {} stop time.", system::clock(at))));
    }
    if let Some(pct) = power.percent.filter(|p| !power.on_ac && *p < s.battery_floor) {
        return Some((Alert::Guard, format!("The battery dropped to {pct}%, below your {}% floor.", s.battery_floor)));
    }
    if s.thermal_guard && thermal >= Thermal::Serious {
        return Some((Alert::Guard, if thermal == Thermal::Critical {
            "The Mac was running critically hot.".into()
        } else {
            "The Mac was running hot.".into()
        }));
    }
    None
}

fn next_stop(s: &Settings, now: DateTime<Local>) -> Option<DateTime<Local>> {
    if !s.stop_enabled {
        return None;
    }
    let today = now
        .date_naive()
        .and_time(s.stop_time()?)
        .and_local_timezone(Local)
        .earliest()?;
    Some(if today > now { today } else { today + TimeDelta::days(1) })
}

fn status_line(core: &Core, claude: &ClaudeInfo, sleep_disabled: bool, helper_running: bool) -> (String, String) {
    let idle = core.settings.idle_minutes;
    let (status, detail) = if !core.want_armed {
        if sleep_disabled && !helper_running {
            (
                "Sleep is turned off",
                "Something other than Overnight turned it off. Closing the lid won't sleep the Mac, \
and Overnight's battery and heat guards aren't watching."
                    .to_string(),
            )
        } else {
            ("Normal sleep", "Closing the lid puts the Mac to sleep.".to_string())
        }
    } else if !core.holding {
        let why = if claude.needs_you > 0 { "Claude is waiting for you" } else { "Claude is idle" };
        (
            "The Mac can sleep",
            format!(
                "{why}, so closing the lid sleeps the Mac. If Claude starts working while the lid \
is open, Overnight keeps it awake again."
            ),
        )
    } else {
        let stop = core.stops_at.map(system::clock);
        match core.settings.mode {
            Mode::Night => (
                "Awake all night",
                format!("The lid can close. Stays awake until {}.", stop.as_deref().unwrap_or("you stop it")),
            ),
            // The needs-you note says which; this says why the Mac is still up.
            Mode::Claude if claude.needs_you > 0 => (
                "Claude needs you",
                if claude.active {
                    "Other sessions are still working, so the Mac stays awake until they finish.".to_string()
                } else {
                    format!("The Mac sleeps if nothing else starts in the next {idle} min.")
                },
            ),
            Mode::Claude if claude.active => (
                "Awake while Claude works",
                match stop {
                    Some(t) => format!("The lid can close. Sleeps once Claude is done or idle for {idle} min, or at {t}."),
                    None => format!("The lid can close. Sleeps once Claude is done or idle for {idle} min."),
                },
            ),
            Mode::Claude => (
                "Awake, waiting for Claude",
                format!("Sleeps if nothing happens in the next {idle} min."),
            ),
        }
    };
    (status.to_string(), detail)
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.0} GB", bytes as f64 / GB as f64)
}

/// One set of readings as JSON, for `overnight --probe`.
pub fn probe(home: &Path) -> serde_json::Value {
    let mut watcher = Watcher::new(home);
    let idle = Duration::from_secs(15 * 60);
    watcher.poll(idle);
    thread::sleep(Duration::from_secs(1)); // CPU usage needs two samples
    serde_json::json!({
        "power": system::power(),
        "lid_closed": system::lid_closed(),
        "thermal": system::thermal(),
        "disk": system::disk(),
        "sleep_disabled": system::sleep_disabled(),
        "helper_running": helper::status(&home.join("Library/Application Support/dev.local.overnight/ctl")).running,
        "claude": watcher.poll(idle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 30, h, m, 0).unwrap()
    }

    fn battery(on_ac: bool, pct: u8) -> Power {
        Power { on_ac, percent: Some(pct), ..Default::default() }
    }

    #[test]
    fn stop_time_rolls_to_tomorrow() {
        let s = Settings::default(); // 08:00
        assert_eq!(next_stop(&s, at(23, 0)), Some(at(8, 0) + TimeDelta::days(1)));
        assert_eq!(next_stop(&s, at(6, 30)), Some(at(8, 0)));
        let off = Settings { stop_enabled: false, ..Settings::default() };
        assert_eq!(next_stop(&off, at(23, 0)), None);
    }

    #[test]
    fn guards() {
        let s = Settings::default(); // floor 10%, thermal guard on
        assert!(guard_trip(&s, &battery(false, 50), Thermal::Nominal, None, at(2, 0)).is_none());
        // "Below 10%": 10% itself still holds.
        assert!(guard_trip(&s, &battery(false, 10), Thermal::Nominal, None, at(2, 0)).is_none());
        assert!(guard_trip(&s, &battery(false, 9), Thermal::Nominal, None, at(2, 0)).unwrap().1.contains("9%"));
        // Plugged in: low charge is fine.
        assert!(guard_trip(&s, &battery(true, 5), Thermal::Nominal, None, at(2, 0)).is_none());
        assert!(guard_trip(&s, &battery(true, 90), Thermal::Serious, None, at(2, 0)).unwrap().1.contains("hot"));
        assert!(guard_trip(&s, &battery(true, 90), Thermal::Fair, None, at(2, 0)).is_none());
        let (alert, stop) = guard_trip(&s, &battery(true, 90), Thermal::Nominal, Some(at(8, 0)), at(8, 0)).unwrap();
        assert_eq!(alert, Alert::StopTime);
        assert!(stop.contains(&system::clock(at(8, 0))), "{stop}");
    }

    #[test]
    fn done_needs_real_work() {
        assert!(!worked_long(None));
        assert!(!worked_long(Some(Duration::from_secs(70)))); // an hourly check
        assert!(worked_long(Some(DONE_AFTER_WORK)));
    }

    #[test]
    fn hold_policy() {
        assert!(should_hold(Mode::Night, false, false));
        assert!(should_hold(Mode::Claude, true, false));
        assert!(should_hold(Mode::Claude, false, true));
        // Paused on a usage limit counts as not working: no waiting it out.
        assert!(!should_hold(Mode::Claude, false, false));
    }

    #[test]
    fn ask_notes() {
        let ask = |turn| Ask { session: PathBuf::from("/s.jsonl"), turn, since: std::time::SystemTime::now() };
        let (q, a) = (ask(Turn::Question), ask(Turn::Approval));
        assert!(ask_note(&[]).is_none());
        assert_eq!(ask_note(&[&q]).unwrap().1, "Claude is asking you something");
        assert!(ask_note(&[&a, &a]).unwrap().2.starts_with("2 sessions"));
        assert_eq!(ask_note(&[&q, &a]).unwrap(), (Alert::Asks, "Claude needs you".into(), "2 sessions are waiting for your answer or approval.".into()));
    }

    fn limited(state: LimitState, resets_at: Option<DateTime<Local>>) -> ClaudeInfo {
        ClaudeInfo {
            limit: Some(crate::claude::Limit { state, resets_at, message: "limit".into(), sessions: 1 }),
            ..Default::default()
        }
    }

    #[test]
    fn limit_changes() {
        let reset = Some(at(2, 0));
        let hit = limit_change(None, &limited(LimitState::Waiting, reset), at(1, 0)).unwrap();
        assert_eq!(hit.0, "limit_hit");
        let note = hit.2.unwrap();
        assert_eq!(note.0, Alert::LimitHit);
        assert!(note.2.contains(&system::clock(at(2, 0))));
        // Same state again: nothing new.
        assert!(limit_change(Some((LimitState::Waiting, reset)), &limited(LimitState::Waiting, reset), at(1, 5)).is_none());
        let missed = limit_change(Some((LimitState::Waiting, reset)), &limited(LimitState::Missed, reset), at(2, 15)).unwrap();
        assert_eq!(missed.0, "limit_missed");
        let cleared = limit_change(Some((LimitState::Waiting, reset)), &ClaudeInfo::default(), at(2, 3)).unwrap();
        assert_eq!(cleared.0, "limit_cleared");
        // A missed limit that aged out isn't reported as continued.
        assert!(limit_change(Some((LimitState::Missed, reset)), &ClaudeInfo::default(), at(5, 0)).is_none());
    }
}
