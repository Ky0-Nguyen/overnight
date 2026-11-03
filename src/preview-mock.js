// Stand-in for the Tauri backend when index.html is opened in a browser.
// Never loaded inside the app. Query options: ?state=off|on|standby&theme=dark
// &limit=waiting|missed&auto=off&ask=question|approval&phone=on&firstrun (never armed before)
// For screenshots: &shot (just the popover, on a transparent page), &tab=storage|log,
// &scrollto=<element id>, &open=<details id>, &padend (room after the last section,
// so even it can scroll to the top).

const params = new URLSearchParams(location.search);
const mode = params.get("state") || "on";
if (params.get("theme")) document.documentElement.dataset.theme = params.get("theme");
document.body.style.cssText = params.has("shot")
  ? "margin:0;background:transparent"
  : "display:grid;place-items:center;min-height:100vh;background:#8e8e93";
document.querySelector(".panel").style.cssText = "width:380px;height:620px";
if (params.get("tab")) localStorage.setItem("tab", params.get("tab"));
if (params.has("padend")) document.querySelector("main").style.paddingBottom = "240px";
// After the first render: open a fold and scroll something into view.
setTimeout(() => {
  if (params.get("open")) document.getElementById(params.get("open")).open = true;
  document.getElementById(params.get("scrollto") || "")?.scrollIntoView({ block: "start" });
}, 400);

const now = Date.now();
const iso = (minsAgo) => new Date(now - minsAgo * 60000).toISOString();
const listeners = [];

const limitMode = params.get("limit"); // waiting | missed
let snap = {
  version: "0.1.0",
  armed: mode !== "off",
  holding: mode === "on",
  sleep_disabled: mode === "on",
  helper_pid: mode === "off" ? null : 48213,
  armed_since: iso(95),
  stops_at: null,
  status: { on: "Awake while Claude works", standby: "The Mac can sleep", off: "Normal sleep" }[mode],
  detail: {
    on: "The lid can close. Sleeps once Claude is done or idle for 15 min, or at 8:00 AM.",
    standby: "Claude is idle, so closing the lid sleeps the Mac. If Claude starts working while the lid is open, Overnight keeps it awake again.",
    off: "Closing the lid puts the Mac to sleep.",
  }[mode],
  power: { on_ac: mode !== "standby", percent: mode === "standby" ? 34 : 88, charging: true, remaining: mode === "standby" ? "2:41" : null },
  lid_closed: false,
  thermal: "nominal",
  claude: {
    processes: 4,
    busy_cpu: mode === "on" ? 63.4 : 1.2,
    active: mode === "on",
    idle_secs: 12,
    sessions: [
      { title: "Refactor auth middleware", project: "billing-api", idle_secs: mode === "on" ? 12 : 1300, active: mode === "on" },
      { title: "Fix flaky e2e checkout test", project: "storefront-web", idle_secs: 140, active: mode === "on" },
      { title: "Weekly metrics report", project: "analytics", idle_secs: 2400, active: false },
      { title: "Review open pull requests and summarise what each one changes", project: "platform-tools", idle_secs: 9000, active: false },
    ],
  },
  disk: { free_bytes: 24.6e9, total_bytes: 494.4e9 },
  disk_low: true,
  scan: {
    at: iso(3),
    secs: 21.4,
    items: [
      { label: "iOS simulators", path: "~/Library/Developer/CoreSimulator", bytes: 38.2e9 },
      { label: "Xcode DerivedData", path: "~/Library/Developer/Xcode/DerivedData", bytes: 17.1e9 },
      { label: "uv cache", path: "~/.cache/uv", bytes: 12.3e9 },
      { label: "Gradle caches", path: "~/.gradle/caches", bytes: 9.8e9 },
      { label: "Claude worktrees", path: "~/Documents/Git/__worktrees", bytes: 6.4e9 },
      { label: "Claude Code transcripts", path: "~/.claude/projects", bytes: 1.2e9 },
      { label: "npm cache", path: "~/.npm", bytes: 0.64e9 },
    ],
  },
  scanning: false,
  settings: {
    mode: "claude",
    stop_enabled: true,
    stop_at: "08:00",
    idle_minutes: 15,
    battery_floor: 10,
    thermal_guard: true,
    alerts: { limit_hit: { mac: true, phone: false }, stop_time: { mac: true, phone: false }, disk: { mac: true, phone: false } },
    storage_warn_gb: 30,
    cleanup_prompt: "Free up disk space on this Mac. Use the disk-cleanup skill if you have it.",
    phone_alerts: params.get("phone") === "on",
    ntfy_url: params.get("phone") === "on" ? "https://ntfy.sh/overnight-k3m9q2xv7tpa" : "",
    phone_only_when_away: true,
    armed_before: !params.has("firstrun"),
    check_updates: true,
  },
  last_guard: null,
  error: null,
  // &update=on: a newer version is waiting.
  update: params.get("update") === "on" ? { version: "0.1.7", installing: false, error: null } : null,
  open_at_login: true,
};

if (limitMode) {
  snap.claude.active = false;
  snap.claude.sessions[0].limit = limitMode;
  snap.claude.sessions[1].limit = limitMode;
  snap.claude.auto_continue = params.get("auto") === "off" ? false : true;
  snap.claude.fix_prompt =
    "My Claude Code sessions stop when they hit the usage limit and don't continue after it resets, even though my Mac stays awake overnight. Please check and fix this:\n1. In ~/.claude/settings.json, make sure \"autoContinueAtUsageLimit\" isn't false...";
  snap.claude.limit = {
    state: limitMode,
    resets_at: new Date(now + (limitMode === "waiting" ? 95 : -25) * 60000).toISOString(),
    message: "You've hit your session limit · resets 2am (Europe/London)",
    sessions: 2,
  };
}

// &ask=question|approval: the second session is waiting for an answer, the first is done.
const askMode = params.get("ask");
if (askMode) {
  snap.claude.needs_you = 1;
  Object.assign(snap.claude.sessions[1], { turn: askMode, place: "the Claude app", active: false, idle_secs: 260 });
  Object.assign(snap.claude.sessions[0], { turn: "done", active: false, idle_secs: 90 });
  // The backend lists sessions that need the user first.
  snap.claude.sessions.unshift(snap.claude.sessions.splice(1, 1)[0]);
  if (mode === "on") {
    snap.status = "Claude needs you";
    snap.detail = "Other sessions are still working, so the Mac stays awake until they finish.";
  } else if (mode === "standby") {
    snap.detail = "Claude is waiting for you, so closing the lid sleeps the Mac. If Claude starts working while the lid is open, Overnight keeps it awake again.";
  }
}

const events = [
  ["2026-10-29T23:05:00", "arm", "Keeping the Mac awake while Claude works until 8:00 AM", 100],
  ["2026-10-29T23:05:00", "hold_on", "Holding: the lid can close"],
  ["2026-10-29T23:05:10", "claude_active", "Claude is working"],
  ["2026-10-29T23:07:00", "lid_closed", "Lid closed", 100],
  ["2026-10-29T23:35:00", "battery", "Battery 99%", 99],
  ["2026-10-30T02:48:00", "claude_idle", "Claude idle for 15 min"],
  ["2026-10-30T02:48:00", "hold_off", "Released: the Mac can sleep", 81],
  ["2026-10-30T07:52:00", "lid_opened", "Lid opened", 80],
  ["2026-10-30T08:00:10", "guard", "Reached your 8:00 AM stop time.", 80],
  ["2026-10-30T09:14:00", "storage_low", "Only 25 GB free"],
].map(([ts, kind, msg, battery]) => ({ ts: new Date(ts).toISOString(), kind, msg, battery }));

function emit() {
  for (const fn of listeners) fn({ payload: structuredClone(snap) });
}

const handlers = {
  get_state: () => structuredClone(snap),
  get_log: () => ({
    events: [...events].reverse(),
    summary: {
      from: events[0].ts,
      to: events[8].ts,
      held_secs: 13380,
      claude_secs: 13370,
      battery_start: 100,
      battery_end: 80,
      ended_by: "Reached your 8:00 AM stop time.",
    },
  }),
  arm: async () => {
    await new Promise((r) => setTimeout(r, 800));
    Object.assign(snap, { armed: true, holding: true, status: "Awake while Claude works" });
    emit();
  },
  disarm: () => {
    Object.assign(snap, { armed: false, holding: false, status: "Normal sleep", detail: "Closing the lid puts the Mac to sleep." });
    emit();
  },
  save_settings: ({ settings }) => {
    snap.settings = settings;
    emit();
  },
  scan_storage: () => {
    snap.scanning = true;
    emit();
    setTimeout(() => {
      snap.scanning = false;
      emit();
    }, 1500);
  },
  clean_up_storage: () => undefined,
  new_ntfy_topic: () => "https://ntfy.sh/overnight-k3m9q2xv7tpa",
  test_phone_alert: async ({ url }) => {
    await new Promise((r) => setTimeout(r, 600));
    if (!/^https?:\/\/[^/]+\/.+/.test(url)) {
      throw "That isn't an ntfy topic address. It should look like https://ntfy.sh/your-topic.";
    }
  },
  reveal_log: () => undefined,
  quit: () => undefined,
  open_docs: () => undefined,
  report_problem: () => undefined,
  open_release: () => undefined,
  set_open_at_login: ({ enabled }) => {
    snap.open_at_login = enabled;
    emit();
  },
  install_update: async () => {
    snap.update = { ...snap.update, installing: true };
    emit();
    await new Promise((r) => setTimeout(r, 1500));
    snap.update = { ...snap.update, installing: false, error: "Couldn't reach GitHub." };
    emit();
    throw snap.update.error;
  },
};

export async function invoke(cmd, args) {
  return handlers[cmd]?.(args);
}

export async function listen(_event, fn) {
  listeners.push(fn);
  return () => {};
}
