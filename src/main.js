// Overnight popover. Talks to the Rust side through Tauri commands and the
// "state" event, which arrives every few seconds while the popover is open.
// The page itself only exists while the popover is open. Opened in a plain browser,
// it loads preview-mock.js instead so the UI can be worked on without Tauri.

let invoke;
let listen;
let hideWindow = () => {};

const $ = (id) => document.getElementById(id);
let state = null;
let settings = null; // the form's copy; replaced by each update unless a save is pending
let saveTimer = null;
let armPending = false;
let tab = "tonight";
let logLoadedAt = 0;
let lastRun = null; // the log's summary of the latest run, for the morning hero

// The page is created fresh each time the popover opens: no transitions until
// the first state is drawn, or every switch would slide into place.
document.documentElement.classList.add("booting");
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");

// Formatting

const GB = 1e9;
const fmtGB = (b) => (b / GB >= 100 ? `${Math.round(b / GB)} GB` : `${(b / GB).toFixed(1)} GB`);
const fmtSize = (b) => (b >= GB ? fmtGB(b) : `${Math.round(b / 1e6)} MB`);
// Matches the times Rust writes, which follow the system's 12/24-hour setting.
const clock = (ts) => new Date(ts).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
const clamp = (n, lo, hi, fallback) => (Number.isFinite(n) ? Math.min(hi, Math.max(lo, Math.round(n))) : fallback);

function ago(secs) {
  if (secs < 60) return "now";
  if (secs < 3600) return `${Math.floor(secs / 60)} min`;
  return `${Math.floor(secs / 3600)} h`;
}

function dur(secs) {
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  if (h && m) return `${h}h ${m}m`;
  return h ? `${h}h` : `${m}m`;
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

// Only touches text that changed, so screen readers announce real changes
// rather than every update.
function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

// Rendering

function render(s) {
  state = s;
  if (!saveTimer) settings = { ...s.settings };

  renderHero(s);
  const arm = $("arm");
  setText(arm, armPending ? "Waiting for your password…" : s.armed ? "Let it sleep" : "Keep awake tonight");
  arm.className = s.armed ? "btn quiet" : "btn primary";
  arm.disabled = armPending;
  // Until Keep awake tonight has worked once, say why a password prompt is coming.
  $("first-run").hidden = s.armed || armPending || s.settings.armed_before || Boolean(morning(s));
  $("error").hidden = !s.error;
  setText($("error"), s.error || "");
  $("error-help").hidden = !s.error;
  $("help").title = `Overnight ${s.version} documentation`;

  renderSettings();
  renderUpdate(s);
  $("login").checked = s.open_at_login;
  setText($("check-updates-sub"), `Version ${s.version}. Looks for new versions on GitHub and asks before installing.`);
  renderClaude(s.claude);
  renderLimit(s);
  renderGuards(s);
  renderStorage(s);
  renderFooter(s);
  if (tab === "log" && Date.now() - logLoadedAt > 15000) loadLog();
}

// The hero says what the Mac will do now, or after a run, how the night went.
// Its note comes from the same state as the Claude pill and the limit
// callout, so the three never disagree.
function renderHero(s) {
  const run = morning(s);
  $("hero").dataset.state = s.armed ? (s.holding ? "on" : "standby") : run ? "morning" : "off";
  $("hero-stats").hidden = !run;
  $("morning-hide").hidden = !run;
  if (run) {
    const ended = new Date(run.to).getHours();
    setText($("status"), ended >= 4 && ended < 13 ? "Last night" : "Your last run");
    const how = run.ended_by ? ` ${run.ended_by.replace(/\.?$/, ".")}` : "";
    setText($("detail"), `${clock(run.from)} to ${clock(run.to)}.${how}`);
    const span = (new Date(run.to) - new Date(run.from)) / 1000;
    const parts = [`Claude worked ${dur(run.claude_secs)}`, `held awake ${dur(run.held_secs)} of ${dur(span)}`];
    if (run.battery_start != null && run.battery_end != null) {
      parts.push(`battery ${run.battery_start}% → ${run.battery_end}%`);
    }
    setText($("hero-stats"), parts.join(" · "));
  } else {
    setText($("status"), s.status);
    setText($("detail"), s.detail);
  }
  const note = needsYou(s);
  $("hero-note").hidden = !note;
  setText($("hero-note-text"), note || "");
}

// What, if anything, needs the user: only what Overnight actually knows.
function needsYou(s) {
  const limit = s.claude.limit;
  // Asking and stopped sessions both wait on the user, so they count together.
  const n = s.claude.needs_you + (limit?.state === "missed" ? limit.sessions : 0);
  if (n) return n === 1 ? "A session needs you" : `${n} sessions need you`;
  if (!limit) return null;
  if (s.claude.auto_continue === false && limit.resets_at) return "Auto-continue is off";
  return null;
}

// The last run, shown as the hero until hidden, the next run, or 16 hours.
function morning(s) {
  if (s.armed || !lastRun?.to) return null;
  if (Date.now() - new Date(lastRun.to) > 16 * 3600 * 1000) return null;
  try {
    if (localStorage.getItem("morningHidden") === lastRun.from) return null;
  } catch {}
  return lastRun;
}

function setValue(id, value) {
  const input = $(id);
  if (document.activeElement !== input) input.value = value;
}

function renderSettings() {
  const s = settings;
  for (const b of document.querySelectorAll(".seg button")) {
    b.setAttribute("aria-checked", String(b.dataset.mode === s.mode));
  }
  const claude = s.mode === "claude";
  $("mode-hint").textContent = claude
    ? `Stays awake while Claude works. Sleeps once Claude is done, waiting for you, paused on a usage limit, or idle for ${s.idle_minutes} min.`
    : "Stays awake until the stop time, a low battery or heat, whichever comes first.";
  setValue("idle", s.idle_minutes);
  $("stop-enabled").checked = s.stop_enabled;
  stopTimes(s.stop_at);
  setValue("stop-at", s.stop_at);
  $("stop-at").disabled = !s.stop_enabled;
  renderAlerts(s);
  $("phone").checked = s.phone_alerts;
  $("phone-setup").hidden = !s.phone_alerts;
  setValue("ntfy-url", s.ntfy_url);
  $("phone-away").checked = s.phone_only_when_away;
  setValue("floor", s.battery_floor);
  $("thermal-guard").checked = s.thermal_guard;
  setValue("warn-gb", s.storage_warn_gb);
  setValue("cleanup-prompt", s.cleanup_prompt);
  $("check-updates").checked = s.check_updates;
}

// A newer version on GitHub. It installs only from here, and not while the
// Mac is being kept awake: the restart would end the night.
function renderUpdate(s) {
  const u = s.update;
  $("update").hidden = !u;
  if (!u) return;
  const busy = s.armed || armPending;
  setText($("update-title"), `Overnight ${u.version} is available`);
  const detail = u.installing
    ? "Downloading. Overnight restarts when it's done."
    : u.error || (busy ? "You can update once it stops keeping the Mac awake." : `You have ${s.version}.`);
  setText($("update-detail"), detail);
  $("update-detail").className = u.error && !u.installing ? "sub bad" : "sub";
  setText($("update-install"), u.installing ? "Updating…" : "Update");
  $("update-install").disabled = u.installing || busy;
}

// Stop at is a menu of half hours. WebKit's time field draws only its digits,
// with no clock to pick from. A stop time set some other way joins the list.
function stopTimes(current) {
  const menu = $("stop-at");
  const wanted = new Set([current]);
  for (let m = 0; m < 24 * 60; m += 30) wanted.add(`${pad(Math.floor(m / 60))}:${pad(m % 60)}`);
  // From noon, so a night's choices (10 PM … 8 AM) read in one run.
  const fromNoon = (v) => (Number(v.slice(0, 2)) * 60 + Number(v.slice(3)) + 720) % 1440;
  const values = [...wanted].filter(Boolean).sort((a, b) => fromNoon(a) - fromNoon(b));
  if (values.join() === [...menu.options].map((o) => o.value).join()) return;
  menu.replaceChildren(
    ...values.map((v) => {
      const [h, m] = v.split(":").map(Number);
      const label = new Date(2000, 0, 1, h, m).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
      return new Option(label, v);
    }),
  );
}
const pad = (n) => String(n).padStart(2, "0");

// Every notification, by kind, with where it can go. Kinds match `Alert` in
// settings.rs; each row reads on from its group: "Claude needs you".
const ALERTS = [
  ["Claude", [
    ["asks", "Needs you", "A question, or a file change to approve"],
    ["done", "Finishes", "After working for 5 min or more"],
    ["limit_hit", "Pauses on a usage limit", "Until the limit resets"],
    ["limit_missed", "Doesn't continue after a limit", "Open the session and send “continue”"],
  ]],
  ["The Mac", [
    ["stop_time", "Stop time reached"],
    ["guard", "Low battery or too hot"],
    ["helper", "Overnight stops early", "Normal sleep is back before morning"],
    ["sleep_outside", "Another app changes sleep", "Closing the lid may not do what you expect"],
  ]],
  ["Storage", [["disk", "Low on disk space"]]],
];

function buildAlerts() {
  const table = $("alerts");
  for (const [group, kinds] of ALERTS) {
    const body = document.createElement("tbody");
    const head = el("tr", "alerts-group");
    const th = el("th", null, group);
    th.colSpan = 3;
    th.scope = "rowgroup";
    head.append(th);
    body.append(head);
    for (const [kind, label, sub] of kinds) {
      const tr = el("tr");
      const name = el("th", "label", label);
      name.scope = "row";
      if (sub) name.append(el("span", "sub", sub));
      tr.append(name);
      for (const channel of ["mac", "phone"]) {
        const box = document.createElement("input");
        box.type = "checkbox";
        box.dataset.kind = kind;
        box.dataset.channel = channel;
        box.setAttribute("aria-label", `${group}: ${label}, ${channel === "mac" ? "on the Mac" : "on the phone"}`);
        box.addEventListener("change", () => {
          const alerts = { ...settings.alerts, [kind]: { ...settings.alerts[kind], [channel]: box.checked } };
          update({ alerts });
        });
        // The whole cell is the target, not just the 13px box.
        const hit = el("label", "alert-hit");
        hit.append(box);
        const td = el("td", channel === "phone" ? "phone-col" : null);
        td.append(hit);
        tr.append(td);
      }
      body.append(tr);
    }
    table.append(body);
  }
}

function renderAlerts(s) {
  for (const box of $("alerts").querySelectorAll("input")) {
    box.checked = s.alerts?.[box.dataset.kind]?.[box.dataset.channel] ?? true;
  }
  $("alerts").classList.toggle("no-phone", !s.phone_alerts);
  $("alerts-hint").hidden = s.phone_alerts;
}

// One word for what Claude is doing overall. "Idle" means no activity for
// the idle time; a usage limit says paused (waiting for the reset) or stopped.
// Stopped outranks the rest, then a question or an approval: neither session
// can go on until the user steps in.
const CLAUDE_PILL = {
  "needs you": "pill warn",
  working: "pill on",
  paused: "pill warn",
  stopped: "pill bad",
  idle: "pill",
  "not running": "pill",
};

function claudeState(c) {
  if (c.limit?.state === "missed") return "stopped";
  if (c.needs_you) return "needs you";
  if (c.active) return "working";
  if (c.limit) return "paused";
  return c.processes ? "idle" : "not running";
}

function renderClaude(c) {
  const pill = $("claude-pill");
  const now = claudeState(c);
  pill.textContent = now;
  pill.className = CLAUDE_PILL[now];

  const list = $("sessions");
  list.replaceChildren();
  if (!c.sessions.length) list.append(el("li", "empty", "No sessions in the last 12 hours."));
  for (const x of c.sessions) {
    const li = el("li");
    const text = el("div");
    const asking = x.turn === "question" || x.turn === "approval";
    // Where to answer, in words, not only in the tooltip.
    const where = x.place ? `${x.turn === "question" ? "Answer" : "Approve"} it in ${x.place}` : "Open the session to answer";
    // Claude's sessions are the usual ones; the others say whose they are.
    const project = [x.agent !== "Claude" && x.agent, x.project].filter(Boolean).join(" · ");
    text.append(el("div", "s-title", x.title), el("span", "s-project", asking ? `${where} · ${project}` : project));
    const stopped = x.limit === "missed";
    const right = x.limit
      ? el("span", stopped ? "s-limit missed" : "s-limit", stopped ? "stopped" : "paused")
      : asking
        ? el("span", "s-limit", x.turn === "question" ? "asking you" : "needs approval")
        : el("span", "s-ago", ago(x.idle_secs));
    const dot = el("span", x.active ? "dot on" : "dot");
    // Working is shown by colour, so say it in words too.
    if (x.active) {
      dot.setAttribute("role", "img");
      dot.setAttribute("aria-label", "working");
    } else {
      dot.setAttribute("aria-hidden", "true");
    }
    li.append(dot, text, right);
    const why = x.limit
      ? stopped
        ? "\nStopped on the usage limit and didn't continue after the reset."
        : "\nPaused on the usage limit until it resets."
      : {
          question: "\nAsked you something and is waiting for your answer.",
          approval: "\nWaiting for you to allow a file change.",
          running: "\nRunning a tool.",
          background: "\nIts turn ended, but work it started in the background is still going.",
          done: "\nDone.",
        }[x.turn] || "";
    li.title = `${x.title}\n${project}${why}`;
    list.append(li);
  }
  // Cursor's own CPU isn't measured: the editor is busy whether or not its agent works.
  const counts = Object.entries(c.running).filter(([agent]) => agent !== "Cursor");
  const n = counts.reduce((sum, [, k]) => sum + k, 0);
  const who = counts.length ? counts.map(([agent, k]) => `${k} ${agent}`).join(" and ") : "0 Claude";
  const cursor = c.running.Cursor ? "; Cursor is open" : "";
  $("claude-meta").textContent = `${who} ${n === 1 ? "process" : "processes"}, using ${Math.round(c.busy_cpu)}% CPU with their tools${cursor}`;
}

// Usage limits: while sessions wait for a reset, and when they didn't
// continue after it. Leads with what to do now; how to prevent it next time
// sits folded underneath.
let fixShownFor = null;

function renderLimit(s) {
  const limit = s.claude.limit;
  const box = $("limit");
  box.hidden = !limit;
  if (!limit) {
    fixShownFor = null;
    return;
  }
  const at = limit.resets_at ? clock(limit.resets_at) : null;
  const one = limit.sessions === 1;
  const who = one ? "A session" : `${limit.sessions} sessions`;
  const autoOff = s.claude.auto_continue === false;
  const waiting = limit.state === "waiting";
  box.className = waiting ? "callout" : "callout missed";

  let title;
  let text;
  if (waiting) {
    const when = at ? `at ${at}` : "after the reset";
    title = at ? `Paused on the usage limit until ${at}` : "Paused on the usage limit";
    const lead = `${who} hit Claude's usage limit.`;
    const night = s.armed && s.settings.mode === "night";
    const late = night && s.stops_at && limit.resets_at && new Date(limit.resets_at) > new Date(s.stops_at);
    if (autoOff) {
      text = `${lead} Auto-continue is off in Claude Code, so Claude won't continue by itself ${when}.`;
    } else if (late) {
      text = `${lead} Your ${clock(s.stops_at)} stop time comes first, so the Mac sleeps before the limit resets. Claude continues once the Mac is awake again.`;
    } else if (night) {
      text = `${lead} The Mac stays awake all night, so Claude should continue by itself ${when}. If it doesn't, you'll get a notification.`;
    } else {
      text = `${lead} A paused session isn't working, so the Mac can sleep. Claude should continue by itself once the limit has reset and the Mac is awake.`;
    }
  } else if (at) {
    title = `Claude didn't continue after the ${at} reset`;
    text = one
      ? `A session is still stopped. Open it and send "continue" to pick up where Claude left off.`
      : `${who} are still stopped. Open each one and send "continue" to pick up where Claude left off.`;
  } else {
    title = "Claude stopped on a usage limit";
    text = limit.message;
  }
  $("limit-title").textContent = title;
  $("limit-text").textContent = text;

  // Only limits that reset can continue by themselves. Offer the settings
  // when auto-continue is off, or once a reset passed without Claude continuing.
  const fix = $("limit-fix");
  const showFix = Boolean(limit.resets_at) && (autoOff || !waiting);
  fix.hidden = !showFix;
  if (!showFix) {
    fixShownFor = null;
    return;
  }
  const urgent = autoOff && waiting;
  $("limit-fix-title").textContent = urgent ? "Turn on auto-continue" : "Make it continue by itself next time";
  // Open when there's still time to act before the reset. Otherwise leave
  // it folded, and don't undo the user's own toggle on every update.
  const key = `${limit.state}:${autoOff}`;
  if (fixShownFor !== key) fix.open = urgent;
  fixShownFor = key;
  setValue("fix-prompt", s.claude.fix_prompt);
}

// `field` holds the same text, for the fallback when the clipboard API is refused.
async function copyText(text, field) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    field.select();
    document.execCommand("copy");
  }
}

// Shows "Copied" on a button for a moment.
function copied(button) {
  const label = button.textContent;
  button.textContent = "Copied";
  setTimeout(() => (button.textContent = label), 2000);
}

// The last part of an ntfy address, which is what the phone app asks for.
const topicName = (url) => url.replace(/\/+$/, "").split("/").pop();

function renderGuards(s) {
  const p = s.power;
  const battery = $("battery");
  if (p.percent == null) {
    battery.textContent = "No battery";
    battery.className = "value";
  } else {
    const how = p.on_ac ? (p.charging ? "charging" : "plugged in") : p.remaining ? `${p.remaining} left` : "on battery";
    battery.textContent = `${p.percent}%, ${how}`;
    battery.className = !p.on_ac && p.percent <= s.settings.battery_floor + 10 ? "value warn" : "value";
  }
  const thermal = $("thermal");
  thermal.textContent = { nominal: "Normal", fair: "Warm", serious: "Hot", critical: "Critical" }[s.thermal];
  thermal.className = { nominal: "value", fair: "value warn" }[s.thermal] || "value bad";
  $("lid").textContent = s.lid_closed == null ? "No lid" : s.lid_closed ? "Closed" : "Open";
}

function renderStorage(s) {
  const d = s.disk;
  const used = d.total_bytes ? 1 - d.free_bytes / d.total_bytes : 0;
  $("disk-free").textContent = `${fmtGB(d.free_bytes)} free`;
  $("disk-total").textContent = `of ${fmtGB(d.total_bytes)}`;
  $("disk-bar").style.width = `${Math.round(used * 100)}%`;
  $("disk-bar").parentElement.classList.toggle("low", s.disk_low);
  $("disk-hint").textContent = s.disk_low
    ? `Below your ${s.settings.storage_warn_gb} GB warning. Scan to see what's piling up, then ask Claude to clean up.`
    : `${Math.round(used * 100)}% used.`;
  $("storage-dot").hidden = !s.disk_low;

  const scan = $("scan");
  scan.disabled = s.scanning;
  scan.textContent = s.scanning ? "Scanning…" : s.scan ? "Rescan" : "Scan";

  const list = $("places");
  list.replaceChildren();
  if (!s.scan) {
    list.append(
      el("li", "empty", s.scanning ? "Measuring folders. This can take a minute." : "Scan to measure caches, simulators, worktrees and Claude data."),
    );
    $("scan-meta").textContent = "";
    return;
  }
  const max = Math.max(1, ...s.scan.items.map((i) => i.bytes));
  for (const item of s.scan.items) {
    const li = el("li");
    const top = el("div", "p-top");
    top.append(el("span", null, item.label), el("span", "p-size", fmtSize(item.bytes)));
    const bar = el("div", "p-bar");
    const fill = el("div");
    fill.style.width = `${Math.max(2, (item.bytes / max) * 100)}%`;
    bar.append(fill);
    li.append(top, el("div", "p-path", item.path), bar);
    list.append(li);
  }
  const total = s.scan.items.reduce((sum, i) => sum + i.bytes, 0);
  $("scan-meta").textContent = `${fmtSize(total)} in these folders, scanned at ${clock(s.scan.at)}.`;
}

// The Mac's actual setting, as macOS reports it. Process details stay in the
// tooltip for anyone checking on the helper.
function renderFooter(s) {
  const footer = $("helper");
  const lid = s.sleep_disabled ? "Lid lock on" : "Lid lock off";
  setText(footer, s.armed ? (s.helper_pid ? `${lid} · helper running` : "Starting the helper…") : lid);
  footer.title = s.helper_pid ? `Root helper, process ${s.helper_pid}` : "";
}

// Log tab

async function loadLog() {
  logLoadedAt = Date.now();
  const view = await invoke("get_log");
  lastRun = view.summary;
  renderSummary(view.summary);
  renderEvents(view.events);
}

function renderSummary(sum) {
  const box = $("summary");
  box.replaceChildren();
  if (!sum) {
    box.append(
      el("h3", null, "No nights yet"),
      el("p", "hint", "Press Keep awake tonight, and in the morning you'll see what happened here."),
    );
    return;
  }
  const live = !sum.to;
  // Held time is shorter than the run when Overnight let the Mac sleep while
  // Claude was idle, so show the run's length beside it.
  const span = ((live ? new Date() : new Date(sum.to)) - new Date(sum.from)) / 1000;
  box.append(
    el("h3", null, live ? "Tonight so far" : "Last run"),
    el("div", "when", `${clock(sum.from)} to ${live ? "now" : clock(sum.to)} (${dur(span)})`),
  );
  const stat = (big, small) => {
    const d = el("div", "stat");
    d.append(el("b", null, big), el("span", null, small));
    return d;
  };
  const battery =
    sum.battery_start != null && sum.battery_end != null ? `${sum.battery_start}→${sum.battery_end}%` : "–";
  const stats = el("div", "stats");
  stats.append(stat(dur(sum.held_secs), "held awake"), stat(dur(sum.claude_secs), "Claude busy"), stat(battery, "battery"));
  box.append(stats);
  if (sum.ended_by) box.append(el("p", "ended", `Ended: ${sum.ended_by.replace(/\.?$/, ".")}`));
}

const KIND_DOT = {
  arm: "accent",
  hold_on: "accent",
  cleanup: "accent",
  claude_active: "green",
  claude_asks: "orange",
  guard: "orange",
  storage_low: "orange",
  phone_failed: "orange",
  helper_lost: "red",
  update_available: "accent",
  updated: "green",
};

function renderEvents(events) {
  const list = $("events");
  list.replaceChildren();
  if (!events.length) {
    list.append(el("li", "empty", "Nothing logged in the last day and a half."));
    return;
  }
  // Oldest first, so each night reads as it happened; the view starts at the latest.
  let day = null;
  for (const e of [...events].reverse()) {
    const d = new Date(e.ts).toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
    if (d !== day) {
      day = d;
      list.append(el("li", "t-day", d));
    }
    const li = el("li");
    const dot = el("span", `t-dot ${KIND_DOT[e.kind] || ""}`);
    dot.setAttribute("aria-hidden", "true");
    li.append(el("span", "t-time", clock(e.ts)), dot, el("span", null, e.msg));
    list.append(li);
  }
  if (tab === "log") {
    const main = document.querySelector("main");
    main.scrollTop = main.scrollHeight;
  }
}

// Actions

function update(patch) {
  settings = { ...settings, ...patch };
  renderSettings();
  clearTimeout(saveTimer);
  saveTimer = setTimeout(save, 350);
}

async function save() {
  clearTimeout(saveTimer);
  try {
    await invoke("save_settings", { settings });
  } finally {
    saveTimer = null;
  }
}

// The page is closed a few seconds after the popover hides, so commit a
// field that's still being edited and save straight away.
window.addEventListener("blur", () => {
  document.activeElement?.blur();
  if (saveTimer) save();
});

function showTab(name) {
  tab = name;
  try {
    localStorage.setItem("tab", name);
  } catch {}
  for (const b of document.querySelectorAll(".tabs button")) {
    const on = b.dataset.tab === name;
    b.setAttribute("aria-selected", String(on));
    // One tab stop for the tab list; arrow keys move between tabs.
    b.tabIndex = on ? 0 : -1;
  }
  for (const t of ["tonight", "storage", "log"]) $(`tab-${t}`).hidden = t !== name;
  document.querySelector("main").scrollTop = 0;
  if (name === "log") loadLog();
}

function bind() {
  $("arm").addEventListener("click", async () => {
    if (state?.armed) {
      await invoke("disarm");
      return;
    }
    armPending = true;
    if (state) render(state);
    try {
      await invoke("arm");
    } catch {
      // The reason comes back as state.error.
    } finally {
      armPending = false;
      if (state) render(state);
    }
  });

  const tabs = [...document.querySelectorAll(".tabs button")];
  for (const b of tabs) {
    b.addEventListener("click", () => showTab(b.dataset.tab));
    b.addEventListener("keydown", (e) => {
      const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
      if (!step) return;
      const next = tabs[(tabs.indexOf(b) + step + tabs.length) % tabs.length];
      showTab(next.dataset.tab);
      next.focus();
    });
  }

  // Help links name a section of the documentation; Rust opens the docs there.
  document.addEventListener("click", (e) => {
    const link = e.target.closest("[data-docs]");
    if (link) invoke("open_docs", { section: link.dataset.docs || null });
  });

  // Points at the usage-limit callout, or at the sessions waiting for an answer.
  $("hero-note").addEventListener("click", () => {
    showTab("tonight");
    const target = state?.claude.needs_you ? $("sessions") : $("limit");
    target.scrollIntoView({ block: "start", behavior: reducedMotion.matches ? "auto" : "smooth" });
    target.focus({ preventScroll: true });
  });
  $("morning-hide").addEventListener("click", () => {
    try {
      localStorage.setItem("morningHidden", lastRun?.from || "");
    } catch {}
    if (state) render(state);
  });
  for (const b of document.querySelectorAll(".seg button")) {
    b.addEventListener("click", () => update({ mode: b.dataset.mode }));
  }

  const num = (id, key, lo, hi) =>
    $(id).addEventListener("change", (e) => update({ [key]: clamp(Number(e.target.value), lo, hi, settings[key]) }));
  num("idle", "idle_minutes", 1, 240);
  num("floor", "battery_floor", 5, 90);
  num("warn-gb", "storage_warn_gb", 1, 4000);
  $("stop-enabled").addEventListener("change", (e) => update({ stop_enabled: e.target.checked }));
  $("stop-at").addEventListener("change", (e) => e.target.value && update({ stop_at: e.target.value }));
  $("thermal-guard").addEventListener("change", (e) => update({ thermal_guard: e.target.checked }));
  $("cleanup-prompt").addEventListener("change", (e) => update({ cleanup_prompt: e.target.value }));

  $("copy-fix").addEventListener("click", async () => {
    await copyText(state?.claude.fix_prompt || $("fix-prompt").value, $("fix-prompt"));
    copied($("copy-fix"));
  });

  $("phone").addEventListener("change", async (e) => {
    // First time on: start with a random topic on the public server.
    const url = e.target.checked && !settings.ntfy_url ? await invoke("new_ntfy_topic") : settings.ntfy_url;
    update({ phone_alerts: e.target.checked, ntfy_url: url });
  });
  $("ntfy-url").addEventListener("change", (e) => update({ ntfy_url: e.target.value.trim() }));
  $("phone-away").addEventListener("change", (e) => update({ phone_only_when_away: e.target.checked }));
  $("copy-topic").addEventListener("click", async () => {
    await copyText(topicName($("ntfy-url").value.trim()), $("ntfy-url"));
    copied($("copy-topic"));
  });
  $("phone-test").addEventListener("click", async () => {
    const result = $("phone-result");
    result.className = "value";
    result.textContent = "Sending…";
    try {
      await invoke("test_phone_alert", { url: $("ntfy-url").value.trim() });
      result.className = "value ok";
      result.textContent = "Sent. Check your phone.";
    } catch (err) {
      result.className = "value bad";
      result.textContent = String(err);
    }
  });
  $("scan").addEventListener("click", () => invoke("scan_storage"));
  $("cleanup").addEventListener("click", async () => {
    const hint = $("cleanup-hint");
    try {
      await invoke("clean_up_storage");
      hint.textContent = "Opened Claude Code in Terminal.";
    } catch (e) {
      hint.textContent = String(e);
    }
  });
  $("update-install").addEventListener("click", async () => {
    try {
      await invoke("install_update");
    } catch {
      // The reason comes back as state.update.error.
    }
  });
  $("update-notes").addEventListener("click", () => invoke("open_release"));
  $("check-updates").addEventListener("change", (e) => update({ check_updates: e.target.checked }));
  // macOS holds this one, not the settings file.
  $("login").addEventListener("change", async (e) => {
    try {
      await invoke("set_open_at_login", { enabled: e.target.checked });
    } catch {
      e.target.checked = !e.target.checked;
    }
  });
  $("reveal-log").addEventListener("click", () => invoke("reveal_log"));
  $("quit").addEventListener("click", () => invoke("quit"));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideWindow();
  });
}

async function boot() {
  if (window.__TAURI__) {
    invoke = window.__TAURI__.core.invoke;
    listen = window.__TAURI__.event.listen;
    hideWindow = () => invoke("hide_popover");
  } else {
    ({ invoke, listen } = await import("./preview-mock.js"));
  }
  buildAlerts();
  bind();
  try {
    tab = localStorage.getItem("tab") || tab;
  } catch {}
  showTab(tab);
  await listen("state", (e) => render(e.payload));
  // The last run's summary, for the morning hero.
  try {
    lastRun = (await invoke("get_log"))?.summary ?? null;
  } catch {}
  render(await invoke("get_state"));
  requestAnimationFrame(() => requestAnimationFrame(() => document.documentElement.classList.remove("booting")));
  // The page is new each time the popover opens, and get_state may be up to
  // half a minute old: ask for a fresh reading.
  invoke("popover_ready");
}

boot();
