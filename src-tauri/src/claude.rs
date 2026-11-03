//! Watches Claude Code: running `claude` processes (and how busy their process
//! trees are) and session transcripts under ~/.claude/projects. A transcript
//! write or a busy process tree counts as activity. Transcripts also show when
//! a session stopped on a usage limit and whether it picked up after the reset.
//! Codex and Cursor sessions are read the same way, by codex.rs and cursor.rs,
//! and listed alongside.

use crate::{codex, cursor};
use chrono::{DateTime, Local, NaiveTime, TimeDelta, TimeZone};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// CPU in one Claude process tree (percent of one core) that counts as working,
/// e.g. a long build or test run that writes nothing to the transcript. Each
/// session's tree is judged on its own: an idle session uses a percent or two
/// with its tools, and a dozen of them together pass 20% while nothing works.
const BUSY_CPU: f32 = 20.0;
/// How long a tree has to stay over BUSY_CPU, so a brief spike in an idle
/// session doesn't count as work.
const BUSY_FOR: Duration = Duration::from_secs(15);
/// How far back the session list looks.
const RECENT: Duration = Duration::from_secs(12 * 3600);
const MAX_SESSIONS: usize = 6;
/// Each agent's recent sessions checked, newest first.
const MAX_SCANNED: usize = 30;
/// How long after a reset a session has to pick up before it counts as missed.
pub const RESUME_GRACE: TimeDelta = TimeDelta::minutes(10);
/// Missed resets older than this are old news.
const MISSED_FOR: TimeDelta = TimeDelta::hours(2);
/// Tools that exist to ask the user something: a question, or a plan to approve.
const ASKING_TOOLS: &[&str] = &["AskUserQuestion", "ExitPlanMode"];
/// The Claude app's end-of-turn box. A question in it (`ask`) leaves the
/// turn ended but waiting on the user's answer.
const WRAP_UP_TOOL: &str = "mcp__ccd_turn__wrap_up";
/// File edits finish within seconds (2 of about 4,700 took over 30 s), so one
/// still open after APPROVAL_AFTER is waiting at the permission prompt. Other
/// tools can't be told apart this way: a quiet Bash command can run for many
/// minutes without writing anything.
const QUICK_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];
const APPROVAL_AFTER: Duration = Duration::from_secs(60);
/// Any other open tool call counts as running for this long, even with
/// nothing written: tool calls have taken up to about 70 minutes. After
/// that the session most likely stopped mid-call.
const RUNNING_FOR: Duration = Duration::from_secs(2 * 3600);
/// A turn can end with background work still going: a command, a subagent or
/// a monitor whose result starts the next turn by itself. That counts as
/// working for this long after the turn ended; a dev server left running
/// never reports back.
const BACKGROUND_FOR: Duration = Duration::from_secs(2 * 3600);

/// For pasting into a Claude session when sessions don't continue after a reset.
pub const FIX_PROMPT: &str = "My Claude Code sessions stop when they hit the usage limit and don't continue after it resets, even though my Mac stays awake overnight. Please check and fix this:
1. In ~/.claude/settings.json, make sure \"autoContinueAtUsageLimit\" isn't false (it defaults to true) and that the \"env\" block has \"CLAUDE_CODE_RETRY_WATCHDOG\": \"1\".
2. Look for project or managed settings that turn either of these off.
3. Check that my Claude Code version is 2.1.234 or later, which auto-continue needs.
Show me each change before you save it.";
pub(crate) const TAIL_BYTES: u64 = 256 * 1024;
pub(crate) const HEAD_BYTES: u64 = 64 * 1024;

/// The coding agents whose sessions Overnight follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Agent {
    Claude,
    Codex,
    Cursor,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
            Agent::Cursor => "Cursor",
        }
    }

    /// Whether a busy process tree counts as work. Cursor is an editor: its
    /// app is busy indexing and drawing whether or not its agent is working,
    /// so only its transcripts count.
    fn busy_counts(self) -> bool {
        self != Agent::Cursor
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Session {
    pub agent: Agent,
    pub title: String,
    pub project: String,
    /// Where to answer it, e.g. "Terminal" or "the Claude app", when the
    /// transcript says.
    pub place: Option<&'static str>,
    pub idle_secs: u64,
    /// Working: a tool call is running, or there was other activity within
    /// the idle time. Never while it's waiting on the user or done.
    pub active: bool,
    pub limit: Option<LimitState>,
    pub turn: Option<Turn>,
}

/// Where a session's turn stands, read from the end of its transcript.
/// None when the records don't say, e.g. a request in flight: then the idle
/// time decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Turn {
    /// It asked a question, or wants a plan approved.
    Question,
    /// A file edit has waited long enough that it must be at the permission prompt.
    Approval,
    /// A tool call is running.
    Running,
    /// Its reply ended the turn, but background work it started is still
    /// going, and its result starts the next turn.
    Background,
    /// Its reply ended the turn, or the user interrupted it.
    Done,
}

/// A session waiting for an answer or an approval, for notifications.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub session: PathBuf,
    pub agent: Agent,
    pub turn: Turn,
    /// When it asked.
    pub since: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitState {
    /// Stopped on a usage limit that hasn't reset yet (or only just has).
    Waiting,
    /// The reset passed a while ago and the session didn't continue,
    /// or the limit has no reset time (e.g. a per-model limit).
    Missed,
}

/// Sessions stopped on a usage limit, summed up.
#[derive(Clone, Debug, Serialize)]
pub struct Limit {
    pub state: LimitState,
    pub resets_at: Option<DateTime<Local>>,
    /// What Claude Code showed, e.g. "You've hit your session limit · resets 1:40pm (Europe/London)".
    pub message: String,
    pub sessions: usize,
}

/// A usage-limit error with no normal reply after it.
#[derive(Clone, Debug)]
pub(crate) struct LimitHit {
    resets_at: Option<DateTime<Local>>,
    message: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ClaudeInfo {
    /// Every agent's processes together. For Cursor that's the app itself.
    pub processes: usize,
    /// The same, by agent.
    pub running: BTreeMap<Agent, usize>,
    /// All Claude and Codex process trees together, for display. Activity
    /// goes by the busiest single tree instead.
    pub busy_cpu: f32,
    /// Some session is working, or a process tree was busy within the idle time.
    pub active: bool,
    /// Seconds since the last transcript write or busy process tree.
    pub idle_secs: Option<u64>,
    pub sessions: Vec<Session>,
    /// Sessions waiting for an answer or an approval, including ones too old
    /// for the session list.
    pub needs_you: usize,
    #[serde(skip)]
    pub asks: Vec<Ask>,
    /// The agents behind `active`, for saying who is done.
    #[serde(skip)]
    pub working: BTreeSet<Agent>,
    pub limit: Option<Limit>,
    /// `autoContinueAtUsageLimit` from ~/.claude/settings.json (on unless set
    /// to false). The desktop app's own checkbox lives elsewhere.
    pub auto_continue: Option<bool>,
    pub fix_prompt: &'static str,
}

/// What a transcript says, whichever agent wrote it.
#[derive(Clone)]
pub(crate) struct Meta {
    pub(crate) len: u64,
    pub(crate) title: String,
    pub(crate) project: String,
    pub(crate) place: Option<&'static str>,
    pub(crate) limit: Option<LimitHit>,
    pub(crate) last_record: Option<SystemTime>,
    /// See `turn_state`. An Approval here is only a file edit still open;
    /// `poll` decides when it has waited long enough.
    pub(crate) turn: Option<(Turn, SystemTime)>,
    /// See `background_open`.
    pub(crate) background: bool,
}

pub struct Watcher {
    /// None if process info isn't available; activity then comes from transcripts alone.
    sys: Option<System>,
    projects: PathBuf,
    settings: PathBuf,
    codex: codex::Dirs,
    cursor_projects: PathBuf,
    meta: HashMap<PathBuf, Meta>,
    last_busy: Option<SystemTime>,
    /// Whose tree was busy then.
    busy_agent: Option<Agent>,
    /// When some tree last went over BUSY_CPU, while it stays over.
    busy_since: Option<SystemTime>,
}

impl Watcher {
    pub fn new(home: &Path) -> Self {
        Self {
            sys: System::new().ok(),
            projects: home.join(".claude/projects"),
            settings: home.join(".claude/settings.json"),
            codex: codex::Dirs::new(home),
            cursor_projects: home.join(".cursor/projects"),
            meta: HashMap::new(),
            last_busy: None,
            busy_agent: None,
            busy_since: None,
        }
    }

    pub fn poll(&mut self, idle_after: Duration) -> ClaudeInfo {
        let now = SystemTime::now();
        let since = |t: SystemTime| now.duration_since(t).unwrap_or_default();

        let usage = self.processes().unwrap_or_default();
        // Without a running process there is nothing left to wait for.
        let no_sys = self.sys.is_none();
        let running = |a: Agent| no_sys || usage.processes.get(&a).is_some_and(|n| *n > 0);
        if sustained(&mut self.busy_since, usage.busiest, now) {
            self.last_busy = Some(now);
            self.busy_agent = usage.busiest_agent;
        }
        // A session's activity is its newest timestamped record or subagent
        // write, not the file's modified time (see `last_record_time`).
        // Cursor's records carry no time, so for Cursor it is.
        let lists = [
            transcripts(&self.projects),
            codex::rollouts(&self.codex),
            cursor::transcripts(&self.cursor_projects),
        ];
        let mut recent: Vec<(PathBuf, Agent, Meta, SystemTime)> = Vec::new();
        // Each agent's newest, so one agent's touched files can't crowd out another's.
        for list in lists {
            for t in list.into_iter().filter(|t| since(t.written) < RECENT).take(MAX_SCANNED) {
                let meta = self.describe(&t);
                if let Some(at) = meta.last_record.max(t.subagents).filter(|at| since(*at) < RECENT) {
                    recent.push((t.path, t.agent, meta, at));
                }
            }
        }
        recent.sort_by(|a, b| b.3.cmp(&a.3));
        let last = recent.first().map(|r| r.3).max(self.last_busy);
        let idle_secs = last.map(|t| since(t).as_secs());
        let busy = self.busy_agent.filter(|a| running(*a) && self.last_busy.is_some_and(|t| since(t) < idle_after));
        let mut active = busy.is_some();
        let mut working: BTreeSet<Agent> = busy.into_iter().collect();

        let local_now = Local::now();
        let mut sessions = Vec::new();
        let mut hits = Vec::new();
        let mut asks = Vec::new();
        for (path, agent, meta, at) in recent {
            let running = running(agent);
            let state = meta.limit.as_ref().and_then(|h| limit_state(h, local_now));
            if state.is_some() {
                hits.extend(meta.limit.clone());
            }
            // Without a process, nothing is running or left to answer.
            let turn = meta.turn.filter(|_| running).and_then(|(t, at)| match t {
                Turn::Approval if since(at) < APPROVAL_AFTER => Some((Turn::Running, at)),
                Turn::Running if since(at) >= RUNNING_FOR => None,
                Turn::Done if meta.background && since(at) < BACKGROUND_FOR => Some((Turn::Background, at)),
                t => Some((t, at)),
            });
            if let Some((t @ (Turn::Question | Turn::Approval), asked)) = turn {
                asks.push(Ask { session: path, agent, turn: t, since: asked });
            }
            let session_active = running
                && match turn {
                    Some((Turn::Running | Turn::Background, _)) => true,
                    // It also picks up by itself once its background work reports back.
                    Some((Turn::Question, at)) => meta.background && since(at) < BACKGROUND_FOR,
                    Some(_) => false,
                    None => since(at) < idle_after,
                };
            active |= session_active;
            if session_active {
                working.insert(agent);
            }
            sessions.push(Session {
                agent,
                title: meta.title,
                project: meta.project,
                place: meta.place,
                idle_secs: since(at).as_secs(),
                active: session_active,
                limit: state,
                turn: turn.map(|t| t.0),
            });
        }
        // Sessions that need the user come first, so the list always shows
        // them, then the rest by latest activity.
        sessions.sort_by_key(|s| !matches!(s.turn, Some(Turn::Question | Turn::Approval)) && s.limit.is_none());
        sessions.truncate(MAX_SESSIONS);
        let limit = summarize_limits(&hits, local_now);

        let auto_continue = auto_continue(&self.settings);
        ClaudeInfo {
            processes: usage.processes.values().sum(),
            running: usage.processes,
            busy_cpu: usage.total,
            active,
            idle_secs,
            sessions,
            needs_you: asks.len(),
            asks,
            working,
            limit,
            auto_continue,
            fix_prompt: FIX_PROMPT,
        }
    }

    /// Counts agent processes and sums CPU across the Claude Code and Codex
    /// ones and everything they started (shells, builds, test runners, MCP
    /// servers).
    fn processes(&mut self) -> Option<Usage> {
        let sys = self.sys.as_mut()?;
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet),
        );
        let procs: Vec<Proc> = sys
            .processes()
            .iter()
            .map(|(pid, p)| Proc {
                pid: pid.as_u32(),
                parent: p.parent().map(Pid::as_u32),
                cpu: p.cpu_usage(),
                agent: agent_of(p),
            })
            .collect();
        Some(tree_usage(&procs))
    }

    /// Session title, project folder, where its turn stands and any pending
    /// usage limit, cached until the transcript grows.
    fn describe(&mut self, t: &Transcript) -> Meta {
        let path = t.path.as_path();
        let len = fs::metadata(path).map_or(0, |m| m.len());
        if let Some(m) = self.meta.get(path).filter(|m| m.len == len) {
            return m.clone();
        }
        let meta = match t.agent {
            Agent::Claude => describe_claude(path, len),
            Agent::Codex => codex::describe(path, len, self.codex.titles()),
            Agent::Cursor => cursor::describe(path, len),
        };
        self.meta.insert(path.to_path_buf(), meta.clone());
        meta
    }
}

/// A Claude Code transcript's title, project folder and the rest of `Meta`.
fn describe_claude(path: &Path, len: u64) -> Meta {
    let head = read_range(path, 0, HEAD_BYTES);
    let mut info = TranscriptInfo {
        // Where the session started; later records follow any `cd`.
        cwd: head.lines().find_map(|l| string_field(l, "cwd")),
        ..Default::default()
    };
    let tail = read_range(path, len.saturating_sub(TAIL_BYTES), TAIL_BYTES);
    info.scan(&tail);
    if info.title().is_none() {
        info.scan(&head);
    }
    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let title = info
        .title()
        .unwrap_or_else(|| format!("Session {}", id.chars().take(8).collect::<String>()));
    let project = info
        .cwd
        .as_deref()
        .and_then(|c| Path::new(c).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let place = match head.lines().find_map(|l| string_field(l, "entrypoint")).as_deref() {
        Some("cli") => Some("Terminal"),
        Some("claude-desktop") => Some("the Claude app"),
        _ => None,
    };
    Meta {
        len,
        title,
        project,
        place,
        limit: unresumed_limit(&tail),
        last_record: last_record_time(&tail),
        turn: turn_state(&tail),
        background: background_open(&tail),
    }
}

/// `autoContinueAtUsageLimit` defaults to on; None if settings can't be read.
fn auto_continue(settings: &Path) -> Option<bool> {
    let v: serde_json::Value = match fs::read(settings) {
        Ok(bytes) => serde_json::from_slice(&bytes).ok()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(true),
        Err(_) => return None,
    };
    Some(v.get("autoContinueAtUsageLimit").and_then(|b| b.as_bool()).unwrap_or(true))
}

/// The newest usage-limit stop in a transcript tail, unless a normal reply
/// came after it (the session continued). Two record shapes:
/// - the session gave up: an assistant record with
///   `"isApiErrorMessage":true,"error":"rate_limit"` and the reset as epoch
///   seconds in `quotaLimits.resetsAt`;
/// - the session is waiting it out (CLAUDE_CODE_RETRY_WATCHDOG): a system
///   `api_error` record with `error.rateLimits.resetsAt`.
fn unresumed_limit(tail: &str) -> Option<LimitHit> {
    for line in tail.lines().rev() {
        if line.contains("\"subtype\":\"api_error\"") && line.contains("\"resetsAt\"") {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let resets_at = v
                .pointer("/error/rateLimits/resetsAt")
                .and_then(|r| r.as_i64())
                .and_then(|r| Local.timestamp_opt(r, 0).single());
            if resets_at.is_some() {
                let message = "Waiting for the usage limit to reset, then retrying.".to_string();
                return Some(LimitHit { resets_at, message });
            }
            continue;
        }
        if !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        if !line.contains("\"isApiErrorMessage\":true") {
            return None; // a normal reply: nothing pending
        }
        if !line.contains("\"error\":\"rate_limit\"") {
            continue; // some other API error; keep looking
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let message = v
            .pointer("/message/content/0/text")
            .and_then(|t| t.as_str())
            .unwrap_or("Claude hit a usage limit.")
            .to_string();
        let hit_at = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Local));
        let resets_at = v
            .pointer("/quotaLimits/resetsAt")
            .and_then(|r| r.as_i64())
            .and_then(|r| Local.timestamp_opt(r, 0).single())
            .or_else(|| hit_at.and_then(|at| reset_from_text(&message, at)));
        return Some(LimitHit { resets_at, message });
    }
    None
}

/// Fallback for records without `quotaLimits`: reads "resets 3am" or
/// "resets 1:40pm" from the message as the next such local time.
fn reset_from_text(message: &str, hit_at: DateTime<Local>) -> Option<DateTime<Local>> {
    let lower = message.to_lowercase();
    let rest = &lower[lower.rfind("resets ")? + "resets ".len()..];
    let token = rest.split_whitespace().next()?;
    let (num, pm) = match (token.strip_suffix("am"), token.strip_suffix("pm")) {
        (Some(n), _) => (n, false),
        (_, Some(n)) => (n, true),
        _ => return None,
    };
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&h) {
        return None;
    }
    let time = NaiveTime::from_hms_opt(h % 12 + if pm { 12 } else { 0 }, m, 0)?;
    let today = hit_at.date_naive().and_time(time).and_local_timezone(Local).earliest()?;
    Some(if today > hit_at { today } else { today + TimeDelta::days(1) })
}

fn limit_state(hit: &LimitHit, now: DateTime<Local>) -> Option<LimitState> {
    match hit.resets_at {
        None => Some(LimitState::Missed),
        Some(at) if now < at + RESUME_GRACE => Some(LimitState::Waiting),
        Some(at) if now - at < MISSED_FOR => Some(LimitState::Missed),
        Some(_) => None,
    }
}

/// Waiting sessions win over missed ones; the latest reset is the one to wait for.
fn summarize_limits(hits: &[LimitHit], now: DateTime<Local>) -> Option<Limit> {
    let with_state: Vec<(&LimitHit, LimitState)> =
        hits.iter().filter_map(|h| Some((h, limit_state(h, now)?))).collect();
    let state = if with_state.iter().any(|(_, s)| *s == LimitState::Waiting) {
        LimitState::Waiting
    } else {
        LimitState::Missed
    };
    let pool: Vec<&LimitHit> = with_state.iter().filter(|(_, s)| *s == state).map(|(h, _)| *h).collect();
    let latest = pool.iter().max_by_key(|h| h.resets_at)?;
    Some(Limit { state, resets_at: latest.resets_at, message: latest.message.clone(), sessions: pool.len() })
}

/// One process, as far as `tree_usage` cares.
struct Proc {
    pid: u32,
    parent: Option<u32>,
    cpu: f32,
    agent: Option<Agent>,
}

/// How many agent processes run, and the CPU use of the Claude and Codex
/// ones and everything they started.
#[derive(Debug, Default)]
struct Usage {
    processes: BTreeMap<Agent, usize>,
    /// Every tree together, each process counted once.
    total: f32,
    /// The busiest single tree. A tree inside another counts in both.
    busiest: f32,
    busiest_agent: Option<Agent>,
}

fn tree_usage(procs: &[Proc]) -> Usage {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut cpu: HashMap<u32, f32> = HashMap::new();
    let mut usage = Usage::default();
    for p in procs {
        cpu.insert(p.pid, p.cpu);
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(p.pid);
        }
        if let Some(agent) = p.agent {
            *usage.processes.entry(agent).or_default() += 1;
        }
    }
    let roots = procs.iter().filter_map(|p| Some((p.pid, p.agent.filter(|a| a.busy_counts())?)));
    let mut counted = HashSet::new();
    for (root, agent) in roots {
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        let mut tree = 0.0;
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let c = cpu.get(&pid).copied().unwrap_or(0.0);
            tree += c;
            if counted.insert(pid) {
                usage.total += c;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids);
            }
        }
        if tree > usage.busiest || usage.busiest_agent.is_none() {
            usage.busiest = tree;
            usage.busiest_agent = Some(agent);
        }
    }
    usage
}

/// Whether a tree has stayed over BUSY_CPU for BUSY_FOR. `since` remembers
/// when it went over, between polls.
fn sustained(since: &mut Option<SystemTime>, busiest: f32, now: SystemTime) -> bool {
    if busiest < BUSY_CPU {
        *since = None;
        return false;
    }
    let start = *since.get_or_insert(now);
    now.duration_since(start).unwrap_or_default() >= BUSY_FOR
}

fn agent_of(p: &Process) -> Option<Agent> {
    if is_claude(p) {
        Some(Agent::Claude)
    } else if codex::is_codex(p) {
        Some(Agent::Codex)
    } else if cursor::is_cursor(p) {
        Some(Agent::Cursor)
    } else {
        None
    }
}

fn is_claude(p: &Process) -> bool {
    let name = p.name().to_string_lossy();
    if name == "claude" {
        return true;
    }
    let exe = p.exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    if exe.ends_with("/claude") || exe.contains("/claude/versions/") {
        return true;
    }
    (name == "node" || name == "bun")
        && p.cmd().iter().any(|a| a.to_string_lossy().contains("claude-code"))
}

pub(crate) struct Transcript {
    pub(crate) path: PathBuf,
    pub(crate) agent: Agent,
    /// The latest write to the transcript or its subagents. Only narrows the
    /// search: the desktop app touches transcripts nobody is working in.
    pub(crate) written: SystemTime,
    /// The latest write by one of its subagents (`<session>/subagents/*.jsonl`),
    /// which hold nothing but conversation, so this counts as activity.
    pub(crate) subagents: Option<SystemTime>,
}

/// Session transcripts, most recently written first.
fn transcripts(projects: &Path) -> Vec<Transcript> {
    let mut latest: HashMap<PathBuf, Transcript> = HashMap::new();
    let mut bump = |path: PathBuf, t: SystemTime, sub: bool| {
        let e = latest
            .entry(path.clone())
            .or_insert(Transcript { path, agent: Agent::Claude, written: t, subagents: None });
        e.written = e.written.max(t);
        if sub {
            e.subagents = e.subagents.max(Some(t));
        }
    };
    let Ok(dirs) = fs::read_dir(projects) else {
        return Vec::new();
    };
    for project in dirs.flatten() {
        let Ok(entries) = fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(t) = mtime(&path) {
                    bump(path, t, false);
                }
            } else if kind.is_dir() {
                let main = path.with_extension("jsonl");
                if !main.is_file() {
                    continue;
                }
                let Ok(subs) = fs::read_dir(path.join("subagents")) else {
                    continue;
                };
                for sub in subs.flatten() {
                    if let Some(t) = mtime(&sub.path()) {
                        bump(main.clone(), t, true);
                    }
                }
            }
        }
    }
    let mut v: Vec<_> = latest.into_values().collect();
    v.sort_by(|a, b| b.written.cmp(&a.written));
    v
}

/// When the newest timestamped record in a transcript tail was written.
/// Conversation records (prompts, replies, tool results, retries) carry a
/// timestamp; the bookkeeping the desktop app appends (titles, cost, artifact
/// state) doesn't, so this tells real activity apart from a touched file.
pub(crate) fn last_record_time(tail: &str) -> Option<SystemTime> {
    tail.lines().rev().find_map(|line| {
        if !line.contains("\"timestamp\"") {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let ts = v.get("timestamp")?.as_str()?;
        DateTime::parse_from_rfc3339(ts).ok().map(SystemTime::from)
    })
}

/// Where the session's turn stands, and since when, from the newest
/// conversation records. Claude Code writes nothing while a tool runs or a
/// question or permission prompt is open, so an open tool call is a question
/// if the tool asks the user, may be at the permission prompt if it's a file
/// edit, and is running otherwise. A reply that ended the turn, or an
/// interruption, is done. Usage limits are reported separately.
fn turn_state(tail: &str) -> Option<(Turn, SystemTime)> {
    let mut answered: HashSet<String> = HashSet::new();
    // The reply whose tool calls all have results. Its other parts (one
    // record per content block) may still hold an open call.
    let mut reply_id: Option<String> = None;
    let mut lines = tail.lines().rev();
    while let Some(line) = lines.next() {
        // Untimed records are bookkeeping, not conversation.
        if !line.contains("\"timestamp\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let blocks = v.pointer("/message/content").and_then(|c| c.as_array());
        let blocks = blocks.map(Vec::as_slice).unwrap_or_default();
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "assistant" => {
                if v.get("isApiErrorMessage").and_then(|e| e.as_bool()) == Some(true) {
                    return None;
                }
                let id = v.pointer("/message/id").and_then(|i| i.as_str());
                if reply_id.is_some() && reply_id.as_deref() != id {
                    return None; // an earlier reply: its results are in
                }
                let at = timestamp(&v)?;
                let mut calls = blocks.iter().filter(|b| b["type"] == "tool_use").peekable();
                if calls.peek().is_none() {
                    if reply_id.is_some() {
                        continue;
                    }
                    if v.pointer("/message/stop_reason").and_then(|r| r.as_str()) != Some("end_turn") {
                        return None;
                    }
                    let turn = if closing_ask(&mut lines, id) { Turn::Question } else { Turn::Done };
                    return Some((turn, at));
                }
                for call in calls {
                    if answered.contains(call["id"].as_str().unwrap_or_default()) {
                        continue;
                    }
                    let name = call["name"].as_str().unwrap_or_default();
                    return Some((
                        if ASKING_TOOLS.contains(&name) {
                            Turn::Question
                        } else if QUICK_TOOLS.contains(&name) {
                            Turn::Approval
                        } else {
                            Turn::Running
                        },
                        at,
                    ));
                }
                reply_id = Some(id.unwrap_or_default().to_string());
            }
            "user" => {
                let results: Vec<_> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_result")
                    .filter_map(|b| b["tool_use_id"].as_str())
                    .collect();
                // Pressing Esc writes this, alongside any cut-off tool result.
                let interrupted = blocks.iter().any(|b| {
                    b["type"] == "text" && b["text"].as_str().is_some_and(|t| t.starts_with("[Request interrupted by user"))
                });
                if interrupted {
                    return timestamp(&v).map(|at| (Turn::Done, at));
                }
                if results.is_empty() || reply_id.is_some() {
                    return None; // a prompt Claude is working on
                }
                answered.extend(results.into_iter().map(str::to_owned));
            }
            // Written after a reply that ends the turn.
            "system" if matches!(v["subtype"].as_str(), Some("stop_hook_summary" | "turn_duration")) => {}
            "system" => return None,
            // A request in flight, or a message waiting its turn.
            t if t.starts_with("api-request") || t == "queue-operation" => return None,
            // Hook results, file snapshots and the like.
            _ => {}
        }
    }
    None
}

/// Is a background task the session started still going? Background
/// commands and subagents name their task in the tool result, monitors too;
/// each ends with a task notification that has a status, or a TaskStop.
fn background_open(tail: &str) -> bool {
    let mut starts: HashSet<String> = HashSet::new();
    let mut open: HashSet<String> = HashSet::new();
    for line in tail.lines() {
        if !line.contains("\"timestamp\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = v.pointer("/message/content");
        let blocks = content.and_then(|c| c.as_array()).map(Vec::as_slice).unwrap_or_default();
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "assistant" => {
                for call in blocks.iter().filter(|b| b["type"] == "tool_use") {
                    let input = &call["input"];
                    match call["name"].as_str().unwrap_or_default() {
                        "Bash" if input["run_in_background"] == true => {}
                        // Subagents launch in the background with or without the flag.
                        "Agent" | "Monitor" => {}
                        "TaskStop" => {
                            open.remove(input["task_id"].as_str().unwrap_or_default());
                            continue;
                        }
                        _ => continue,
                    }
                    starts.insert(call["id"].as_str().unwrap_or_default().to_string());
                }
            }
            "user" => {
                for b in blocks {
                    if b["type"] == "tool_result" {
                        let started = b["tool_use_id"].as_str().is_some_and(|id| starts.remove(id));
                        if let Some(task) = started.then(|| started_task(&b["content"])).flatten() {
                            open.insert(task);
                        }
                    } else if let Some(text) = b["text"].as_str() {
                        close_notified(text, &mut open);
                    }
                }
                if let Some(text) = content.and_then(|c| c.as_str()) {
                    close_notified(text, &mut open);
                }
            }
            // A notification that arrived while Claude was busy.
            "attachment" if v.pointer("/attachment/type").and_then(|t| t.as_str()) == Some("queued_command") => {
                if let Some(text) = v.pointer("/attachment/prompt").and_then(|p| p.as_str()) {
                    close_notified(text, &mut open);
                }
            }
            _ => {}
        }
    }
    !open.is_empty()
}

/// The task a background start names: "Command running in background with
/// ID: b8s7p075a.", "Async agent launched … agentId: a0f94c8b4e0c75ede",
/// "Monitor started (task b4ked3ohd, …". A subagent that ran in the
/// foreground also names its agentId, next to its report.
fn started_task(content: &serde_json::Value) -> Option<String> {
    let text = match content {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Array(parts) => parts.iter().find_map(|p| p["text"].as_str())?,
        _ => return None,
    };
    let agent = text.starts_with("Async agent launched").then_some("agentId: ");
    ["with ID: ", "Monitor started (task "].into_iter().chain(agent).find_map(|mark| {
        let rest = &text[text.find(mark)? + mark.len()..];
        let id: String = rest.chars().take_while(char::is_ascii_alphanumeric).collect();
        (!id.is_empty()).then_some(id)
    })
}

/// Closes each task whose notification in `text` has a status. A monitor's
/// events come without one.
fn close_notified(text: &str, open: &mut HashSet<String>) {
    for note in text.split("<task-notification>").skip(1) {
        let note = note.split("</task-notification>").next().unwrap_or_default();
        let tag = |name: &str| {
            let rest = &note[note.find(&format!("<{name}>"))? + name.len() + 2..];
            Some(rest[..rest.find('<')?].trim().to_string())
        };
        if let (Some(task), Some(_)) = (tag("task-id"), tag("status")) {
            open.remove(&task);
        }
    }
}

/// Did the turn that ends in reply `end_id` ask its question in the app's
/// end-of-turn box? Reads back from just before that reply to the turn's last
/// tool call.
fn closing_ask<'a>(lines: &mut impl Iterator<Item = &'a str>, end_id: Option<&str>) -> bool {
    for line in lines {
        if !line.contains("\"timestamp\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let blocks = v.pointer("/message/content").and_then(|c| c.as_array());
        let blocks = blocks.map(Vec::as_slice).unwrap_or_default();
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "assistant" => {
                let mut calls = blocks.iter().filter(|b| b["type"] == "tool_use").peekable();
                if calls.peek().is_some() {
                    return calls.any(|c| {
                        c["name"] == WRAP_UP_TOOL && c.pointer("/input/ask").and_then(|a| a.as_str()).is_some_and(|a| !a.trim().is_empty())
                    });
                }
                if v.pointer("/message/id").and_then(|i| i.as_str()) != end_id {
                    return false; // an earlier reply with no tool calls
                }
            }
            // The wrap-up's own result; anything else is the user's prompt.
            "user" if blocks.iter().any(|b| b["type"] == "tool_result") => {}
            "user" | "system" => return false,
            _ => {}
        }
    }
    false
}

pub(crate) fn timestamp(v: &serde_json::Value) -> Option<SystemTime> {
    let ts = v.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(ts).ok().map(SystemTime::from)
}

pub(crate) fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The folders in `dir`, if it can be read.
pub(crate) fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

pub(crate) fn read_range(path: &Path, start: u64, max: u64) -> String {
    let mut buf = Vec::new();
    if let Ok(mut f) = fs::File::open(path) {
        if f.seek(SeekFrom::Start(start)).is_ok() {
            let _ = f.take(max).read_to_end(&mut buf);
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[derive(Default)]
struct TranscriptInfo {
    custom: Option<String>,
    agent: Option<String>,
    ai: Option<String>,
    cwd: Option<String>,
}

impl TranscriptInfo {
    /// Reads title records and the working directory, latest lines first.
    /// Only the small title lines are parsed as JSON.
    fn scan(&mut self, text: &str) {
        for line in text.lines().rev() {
            if self.cwd.is_none() {
                self.cwd = string_field(line, "cwd");
            }
            for (key, slot) in [
                ("customTitle", &mut self.custom),
                ("agentName", &mut self.agent),
                ("aiTitle", &mut self.ai),
            ] {
                if slot.is_none() && line.contains(key) {
                    *slot = serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|v| v.get(key)?.as_str().map(str::to_owned))
                        .filter(|s| !s.trim().is_empty());
                }
            }
        }
    }

    fn title(&self) -> Option<String> {
        self.custom.clone().or_else(|| self.agent.clone()).or_else(|| self.ai.clone())
    }
}

/// Pulls `"key":"value"` out of a JSON line without parsing all of it.
pub(crate) fn string_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len();
    let end = line[start..].find('"')?;
    Some(line[start..start + end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32, cpu: f32, claude: bool) -> Proc {
        Proc { pid, parent: Some(parent), cpu, agent: claude.then_some(Agent::Claude) }
    }

    /// A dozen idle sessions, each with a few tools, add up to more than 20%
    /// of a core. That kept Overnight saying Claude was working all night.
    #[test]
    fn idle_sessions_together_are_not_busy() {
        let mut procs = Vec::new();
        for s in 0..13 {
            let root = 1000 + s * 10;
            procs.push(proc(root, 1, 1.2, true));
            for t in 1..=3 {
                procs.push(proc(root + t, root, 0.6, false));
            }
        }
        let usage = tree_usage(&procs);
        assert_eq!(usage.processes[&Agent::Claude], 13);
        assert!(usage.total > BUSY_CPU, "together {}", usage.total);
        assert!(usage.busiest < BUSY_CPU, "busiest {}", usage.busiest);
    }

    #[test]
    fn one_session_building_is_busy() {
        let procs = vec![
            proc(10, 1, 1.0, true),
            proc(11, 10, 0.5, false),
            proc(12, 11, 85.0, false), // cargo, two levels down
            proc(20, 1, 1.0, true),
            proc(30, 1, 40.0, false), // not Claude's
        ];
        let usage = tree_usage(&procs);
        assert_eq!(usage.processes[&Agent::Claude], 2);
        assert!((usage.busiest - 86.5).abs() < 0.01);
        assert!((usage.total - 87.5).abs() < 0.01);
    }

    /// A Codex command counts like a Claude one. Cursor's own tree doesn't:
    /// the editor is busy whether or not its agent works.
    #[test]
    fn codex_trees_count_and_cursor_runs_without() {
        let agent = |pid: u32, cpu: f32, a: Agent| Proc { pid, parent: Some(1), cpu, agent: Some(a) };
        let procs = vec![
            agent(10, 0.5, Agent::Claude),
            agent(20, 0.5, Agent::Codex),
            proc(21, 20, 60.0, false), // cargo test, run by Codex
            agent(30, 95.0, Agent::Cursor),
            proc(31, 30, 40.0, false), // indexing
        ];
        let usage = tree_usage(&procs);
        assert_eq!(usage.processes.values().sum::<usize>(), 3);
        assert_eq!(usage.processes[&Agent::Cursor], 1);
        assert_eq!(usage.busiest_agent, Some(Agent::Codex));
        assert!((usage.busiest - 60.5).abs() < 0.01);
        assert!((usage.total - 61.0).abs() < 0.01);
    }

    /// `claude` started from inside another session's shell.
    #[test]
    fn nested_session_is_counted_once_in_total() {
        let procs = vec![proc(10, 1, 2.0, true), proc(11, 10, 1.0, false), proc(12, 11, 30.0, true)];
        let usage = tree_usage(&procs);
        assert_eq!(usage.processes[&Agent::Claude], 2);
        assert!((usage.total - 33.0).abs() < 0.01);
        assert!((usage.busiest - 33.0).abs() < 0.01);
    }

    #[test]
    fn a_spike_is_not_work() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = |s: u64| t + Duration::from_secs(s);
        let mut since = None;
        assert!(!sustained(&mut since, 45.0, at(0)));
        assert!(!sustained(&mut since, 3.0, at(10)));
        assert!(!sustained(&mut since, 45.0, at(20)));
        assert!(!sustained(&mut since, 30.0, at(30)));
        assert!(sustained(&mut since, 30.0, at(40)));
        assert!(sustained(&mut since, 90.0, at(50)));
        assert!(!sustained(&mut since, 1.0, at(60)));
        assert_eq!(since, None);
    }

    /// The desktop app keeps appending untimed bookkeeping to idle sessions,
    /// which made a session that last spoke yesterday look busy.
    #[test]
    fn activity_ignores_untimed_bookkeeping() {
        let tail = concat!(
            "fragment of a cut-off line\n",
            r#"{"type":"assistant","timestamp":"2026-10-29T04:29:58.000Z","message":{"content":[]}}"#, "\n",
            r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-10-29T04:30:20.604Z"}"#, "\n",
            r#"{"type":"custom-title","customTitle":"Micro-app platform v1 inventory"}"#, "\n",
            r#"{"type":"cost-state","sessionId":"x"}"#, "\n",
            r#"{"type":"artifact-autoreact-ledger","entries":[{"note":"has a \"timestamp\" inside"}]}"#, "\n",
        );
        let want = DateTime::parse_from_rfc3339("2026-10-29T04:30:20.604Z").unwrap();
        assert_eq!(last_record_time(tail), Some(SystemTime::from(want)));
        assert_eq!(last_record_time(r#"{"type":"custom-title","customTitle":"t"}"#), None);
    }

    #[test]
    fn prefers_custom_title_and_finds_cwd() {
        let mut info = TranscriptInfo::default();
        info.scan(concat!(
            r#"{"type":"user","cwd":"/Users/me/Git/app","message":"hi"}"#, "\n",
            r#"{"type":"ai-title","aiTitle":"Generated","sessionId":"x"}"#, "\n",
            r#"{"type":"custom-title","customTitle":"Mine","sessionId":"x"}"#, "\n",
        ));
        assert_eq!(info.title().as_deref(), Some("Mine"));
        assert_eq!(info.cwd.as_deref(), Some("/Users/me/Git/app"));
    }

    const LIMIT: &str = r#"{"type":"assistant","timestamp":"2026-08-27T06:25:06.782Z","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 1:40pm (Europe/London)"}]},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"quotaLimits":{"status":"rejected","resetsAt":1787812800,"rateLimitType":"five_hour"}}"#;
    const REPLY: &str = r#"{"type":"assistant","timestamp":"2026-08-27T06:43:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Picking up where I left off."}]}}"#;
    const OTHER_ERROR: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"API Error: 529 Overloaded"}]},"error":"overloaded","isApiErrorMessage":true}"#;

    #[test]
    fn finds_pending_limit_with_reset_time() {
        let tail = format!("{REPLY}\n{LIMIT}\n{{\"type\":\"last-prompt\"}}\n{OTHER_ERROR}\n");
        let hit = unresumed_limit(&tail).expect("limit");
        assert_eq!(hit.resets_at, Local.timestamp_opt(1787812800, 0).single());
        assert!(hit.message.contains("session limit"));
    }

    #[test]
    fn retry_wait_counts_as_waiting() {
        let retry = r#"{"type":"system","subtype":"api_error","level":"error","error":{"formatted":"429 ...","rateLimits":{"rateLimitType":"five_hour","resetsAt":1792570800}},"retryInMs":10697801,"retryAttempt":1,"maxRetries":300}"#;
        let hit = unresumed_limit(&format!("{REPLY}\n{retry}\n")).expect("waiting");
        assert_eq!(hit.resets_at, Local.timestamp_opt(1792570800, 0).single());
        assert!(unresumed_limit(&format!("{retry}\n{REPLY}\n")).is_none());
    }

    #[test]
    fn reads_auto_continue_setting() {
        let dir = std::env::temp_dir().join(format!("overnight-settings-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        assert_eq!(auto_continue(&file), Some(true)); // no file: default on
        fs::write(&file, r#"{"env":{"CLAUDE_CODE_RETRY_WATCHDOG":"1"}}"#).unwrap();
        assert_eq!(auto_continue(&file), Some(true));
        fs::write(&file, r#"{"autoContinueAtUsageLimit":false}"#).unwrap();
        assert_eq!(auto_continue(&file), Some(false));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reply_after_limit_means_resumed() {
        assert!(unresumed_limit(&format!("{LIMIT}\n{REPLY}\n")).is_none());
    }

    #[test]
    fn reset_time_from_text() {
        let at = Local.with_ymd_and_hms(2026, 10, 30, 13, 25, 0).unwrap();
        let msg = "You've hit your session limit · resets 1:40pm (Europe/London)";
        assert_eq!(reset_from_text(msg, at), Local.with_ymd_and_hms(2026, 10, 30, 13, 40, 0).single());
        let late = Local.with_ymd_and_hms(2026, 10, 30, 23, 10, 0).unwrap();
        assert_eq!(reset_from_text("... resets 2am (Europe/London)", late), Local.with_ymd_and_hms(2026, 10, 31, 2, 0, 0).single());
        assert_eq!(reset_from_text("You've reached your Fable limit. Switch to another model to continue.", at), None);
    }

    #[test]
    fn limit_states_over_time() {
        let reset = Local.with_ymd_and_hms(2026, 10, 31, 2, 0, 0).unwrap();
        let hit = LimitHit { resets_at: Some(reset), message: String::new() };
        let mins = |m| reset + TimeDelta::minutes(m);
        assert_eq!(limit_state(&hit, mins(-60)), Some(LimitState::Waiting));
        assert_eq!(limit_state(&hit, mins(5)), Some(LimitState::Waiting)); // grace to pick up
        assert_eq!(limit_state(&hit, mins(15)), Some(LimitState::Missed));
        assert_eq!(limit_state(&hit, mins(180)), None);
        let model = LimitHit { resets_at: None, message: String::new() };
        assert_eq!(limit_state(&model, mins(0)), Some(LimitState::Missed));
        let s = summarize_limits(&[hit.clone(), model], mins(-30)).unwrap();
        assert_eq!((s.state, s.sessions, s.resets_at), (LimitState::Waiting, 1, Some(reset)));
    }

    /// Records shaped like Claude Code's, one per content block.
    fn call(id: &str, name: &str, at: &str) -> String {
        format!(r#"{{"type":"assistant","timestamp":"2026-11-02T13:{at}.000Z","message":{{"id":"msg_{at}","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}],"stop_reason":"tool_use"}}}}"#)
    }
    fn result(id: &str) -> String {
        format!(r#"{{"type":"user","timestamp":"2026-11-02T13:30:00.000Z","message":{{"role":"user","content":[{{"tool_use_id":"{id}","type":"tool_result","content":"ok"}}]}}}}"#)
    }
    const HOOK: &str = r#"{"type":"attachment","attachment":{"type":"hook_success","hookEvent":"PreToolUse"},"timestamp":"2026-11-02T13:22:47.000Z"}"#;
    const ENDED: &str = r#"{"type":"assistant","timestamp":"2026-11-02T13:27:40.000Z","message":{"id":"msg_end","content":[{"type":"text","text":"Done. Want me to push it?"}],"stop_reason":"end_turn"}}"#;
    const STOP_HOOKS: &str = r#"{"type":"system","subtype":"stop_hook_summary","timestamp":"2026-11-02T13:27:43.000Z"}"#;
    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-11-02T13:40:00.000Z","message":{"role":"user","content":"yes, push it"}}"#;
    const REQUEST: &str = r#"{"type":"api-request","timestamp":"2026-11-02T13:30:01.000Z"}"#;

    fn turn(lines: &[&str]) -> Option<Turn> {
        turn_state(&(lines.join("\n") + "\n")).map(|t| t.0)
    }

    /// The "Roleplay request scenario" session: a question nobody answered
    /// yet looked like work for 15 minutes, then like idle.
    #[test]
    fn open_question_waits_for_the_user() {
        let ask = call("toolu_q", "AskUserQuestion", "22:46");
        let tail = format!("{ask}\n{HOOK}\n{{\"type\":\"custom-title\",\"customTitle\":\"Roleplay\"}}\n");
        let (t, at) = turn_state(&tail).unwrap();
        assert_eq!(t, Turn::Question);
        assert_eq!(at, SystemTime::from(DateTime::parse_from_rfc3339("2026-11-02T13:22:46Z").unwrap()));
        assert_eq!(turn(&[&call("toolu_p", "ExitPlanMode", "22:46")]), Some(Turn::Question));
        assert_eq!(turn(&[&ask, &result("toolu_q"), REQUEST]), None);
    }

    #[test]
    fn open_file_edit_may_need_approval_but_a_command_is_running() {
        assert_eq!(turn(&[&call("toolu_w", "Write", "28:53"), HOOK]), Some(Turn::Approval));
        assert_eq!(turn(&[&call("toolu_b", "Bash", "28:53"), HOOK]), Some(Turn::Running));
        assert_eq!(turn(&[&call("toolu_w", "Write", "28:53"), &result("toolu_w")]), None);
    }

    /// The "Insomnia with AI twist" session: Esc during a command.
    #[test]
    fn interrupted_turn_is_done() {
        let esc = r#"{"type":"user","timestamp":"2026-10-31T08:51:39.000Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;
        let bash = call("toolu_b", "Bash", "28:53");
        assert_eq!(turn(&[&bash, &result("toolu_b"), esc]), Some(Turn::Done));
    }

    #[test]
    fn ended_turn_is_done() {
        assert_eq!(turn(&[ENDED, STOP_HOOKS, r#"{"type":"cost-state"}"#]), Some(Turn::Done));
        assert_eq!(turn(&[ENDED, STOP_HOOKS, PROMPT]), None);
        assert_eq!(turn(&[ENDED, REQUEST]), None);
        assert_eq!(turn(&[LIMIT]), None); // reported as a usage limit instead
    }

    #[test]
    fn question_in_the_closing_box_waits_for_the_user() {
        let wrap = |input: &str| {
            format!(r#"{{"type":"assistant","timestamp":"2026-11-02T13:27:30.000Z","message":{{"id":"msg_wrap","content":[{{"type":"tool_use","id":"toolu_w","name":"mcp__ccd_turn__wrap_up","input":{input}}}],"stop_reason":"tool_use"}}}}"#)
        };
        let asked = wrap(r#"{"recap":"Tests pass.","ask":"Run the plan next?","actions":[{"label":"Yes"}]}"#);
        let told = wrap(r#"{"recap":"Tests pass."}"#);
        assert_eq!(turn(&[&asked, &result("toolu_w"), REQUEST, ENDED, STOP_HOOKS]), Some(Turn::Question));
        assert_eq!(turn(&[&told, &result("toolu_w"), REQUEST, ENDED, STOP_HOOKS]), Some(Turn::Done));
        // A later turn that ends without the box doesn't inherit the question.
        assert_eq!(turn(&[&asked, &result("toolu_w"), ENDED, PROMPT, ENDED]), Some(Turn::Done));
        assert_eq!(turn(&[&asked, &result("toolu_w"), ENDED, PROMPT]), None);
    }

    #[test]
    fn background_work_keeps_going_after_the_turn() {
        let start = |id: &str, name: &str, input: &str| {
            format!(r#"{{"type":"assistant","timestamp":"2026-11-02T13:20:00.000Z","message":{{"id":"msg_{id}","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}],"stop_reason":"tool_use"}}}}"#)
        };
        let started = |id: &str, text: &str| {
            format!(r#"{{"type":"user","timestamp":"2026-11-02T13:20:01.000Z","message":{{"role":"user","content":[{{"tool_use_id":"{id}","type":"tool_result","content":[{{"type":"text","text":"{text}"}}]}}]}}}}"#)
        };
        let note = |task: &str, status: &str| {
            format!(r#"{{"type":"user","timestamp":"2026-11-02T13:40:00.000Z","message":{{"role":"user","content":"<task-notification>\n<task-id>{task}</task-id>\n{status}<summary>Done</summary>\n</task-notification>"}}}}"#)
        };
        let open = |lines: &[&str]| background_open(&(lines.join("\n") + "\n"));
        let bash = start("toolu_s", "Bash", r#"{"command":"make","run_in_background":true}"#);
        let bash_up = started("toolu_s", "Command running in background with ID: b8s7p075a. Output is being written to: /tmp/x");
        assert!(open(&[&bash, &bash_up, ENDED]));
        assert!(!open(&[&bash, &bash_up, ENDED, &note("b8s7p075a", "<status>completed</status>")]));
        // A command in the foreground isn't background work.
        assert!(!open(&[&start("toolu_s", "Bash", r#"{"command":"make"}"#), &result("toolu_s"), ENDED]));
        let report = started("toolu_a", "Here's what I found. agentId: a0f94c8b4e0c75ede");
        assert!(!open(&[&start("toolu_a", "Agent", r#"{"prompt":"look"}"#), &report, ENDED]));

        let agent = start("toolu_a", "Agent", r#"{"prompt":"look"}"#);
        let agent_up = started("toolu_a", "Async agent launched successfully. agentId: a0f94c8b4e0c75ede (internal ID)");
        assert!(open(&[&agent, &agent_up, ENDED]));
        let stop = start("toolu_t", "TaskStop", r#"{"task_id":"a0f94c8b4e0c75ede"}"#);
        assert!(!open(&[&agent, &agent_up, &stop, &result("toolu_t"), ENDED]));

        // A monitor's events don't end it; the notice with a status does.
        let monitor = start("toolu_m", "Monitor", r#"{"command":"watch ci"}"#);
        let monitor_up = started("toolu_m", "Monitor started (task b4ked3ohd, expires in 30m)");
        assert!(open(&[&monitor, &monitor_up, &note("b4ked3ohd", "<event>build: ok</event>"), ENDED]));
        assert!(!open(&[&monitor, &monitor_up, &note("b4ked3ohd", "<status>completed</status>"), ENDED]));
        // Arrived while Claude was busy, so it was queued.
        let queued = r#"{"type":"attachment","timestamp":"2026-11-02T13:40:00.000Z","attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>b4ked3ohd</task-id>\n<status>failed</status>\n</task-notification>"}}"#;
        assert!(!open(&[&monitor, &monitor_up, queued, ENDED]));
    }

    /// Two calls in one reply, written as two records: the first is still open.
    #[test]
    fn parallel_calls_wait_for_the_open_one() {
        let one = call("toolu_1", "AskUserQuestion", "29:00");
        let two = call("toolu_2", "Read", "29:00");
        assert_eq!(turn(&[&one, &two, &result("toolu_2")]), Some(Turn::Question));
        assert_eq!(turn(&[&one, &two, &result("toolu_2"), &result("toolu_1")]), None);
    }

    #[test]
    fn falls_back_to_agent_name() {
        let mut info = TranscriptInfo::default();
        info.scan(r#"{"type":"agent-name","agentName":"Lid sleep","sessionId":"x"}"#);
        assert_eq!(info.title().as_deref(), Some("Lid sleep"));
    }
}
