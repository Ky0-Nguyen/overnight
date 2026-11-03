//! Reads Codex sessions: the rollout files that the Codex app and the `codex`
//! CLI write under ~/.codex/sessions/<year>/<month>/<day>/, one per session.
//! Codex brackets every turn with `task_started` and `task_complete`, or
//! `turn_aborted` when you stop it, so where a turn stands comes straight from
//! those. Every record is timestamped.

use crate::claude::{
    last_record_time, mtime, read_range, string_field, subdirs, timestamp, Agent, Meta, Transcript, Turn,
    HEAD_BYTES, TAIL_BYTES,
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};
use sysinfo::Process;

/// Asks you something and waits for the answer, like Claude's
/// AskUserQuestion. The Codex app's `request_user_input_async` returns at
/// once and the turn goes on, so it doesn't count.
const ASKING_TOOL: &str = "request_user_input";

/// The Codex app runs this binary too, inside ChatGPT.app.
pub(crate) fn is_codex(p: &Process) -> bool {
    p.name() == "codex" || p.exe().is_some_and(|e| e.ends_with("codex"))
}

/// Where Codex keeps its sessions, and the names it gives them.
pub(crate) struct Dirs {
    sessions: PathBuf,
    index: PathBuf,
    /// `index` as last read, with its modified time then.
    titles: Option<(Option<SystemTime>, HashMap<String, String>)>,
}

impl Dirs {
    pub(crate) fn new(home: &Path) -> Self {
        Self {
            sessions: home.join(".codex/sessions"),
            index: home.join(".codex/session_index.jsonl"),
            titles: None,
        }
    }

    /// Session names by session ID, read again whenever the index changes.
    pub(crate) fn titles(&mut self) -> &HashMap<String, String> {
        let at = mtime(&self.index);
        if self.titles.as_ref().is_some_and(|(seen, _)| *seen != at) {
            self.titles = None;
        }
        let index = &self.index;
        &self.titles.get_or_insert_with(|| (at, read_titles(index))).1
    }
}

/// The index gets a line each time a session is named; the latest wins.
fn read_titles(index: &Path) -> HashMap<String, String> {
    let text = fs::read_to_string(index).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| Some((v["id"].as_str()?.to_owned(), v["thread_name"].as_str()?.trim().to_owned())))
        .filter(|(_, name)| !name.is_empty())
        .collect()
}

/// Rollout files, most recently written first. A resumed session goes on
/// writing to its old file, so every day's folder is looked at, not just
/// recent ones.
pub(crate) fn rollouts(dirs: &Dirs) -> Vec<Transcript> {
    let mut found = Vec::new();
    for day in subdirs(&dirs.sessions).iter().flat_map(|y| subdirs(y)).flat_map(|m| subdirs(&m)) {
        let Ok(entries) = fs::read_dir(&day) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            let rollout = path.extension().is_some_and(|e| e == "jsonl")
                && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-"));
            if let Some(written) = rollout.then(|| mtime(&path)).flatten() {
                found.push(Transcript { path, agent: Agent::Codex, written, subagents: None });
            }
        }
    }
    found.sort_by(|a, b| b.written.cmp(&a.written));
    found
}

/// A rollout's name, project folder and where its turn stands.
pub(crate) fn describe(path: &Path, len: u64, titles: &HashMap<String, String>) -> Meta {
    // The first record says which session this is, where it started and in
    // which app. It holds the instructions too, so it may be cut off here.
    let head = read_range(path, 0, HEAD_BYTES);
    let first = head.lines().next().unwrap_or_default();
    let tail = read_range(path, len.saturating_sub(TAIL_BYTES), TAIL_BYTES);
    let id = string_field(first, "id").unwrap_or_default();
    let title = titles
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("Session {}", id.chars().take(8).collect::<String>()));
    let project = string_field(first, "cwd")
        .and_then(|c| Path::new(&c).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let place = if string_field(first, "originator").as_deref() == Some("Codex Desktop") {
        Some("the Codex app")
    } else if string_field(first, "source").as_deref() == Some("cli") {
        Some("Terminal")
    } else {
        None
    };
    Meta {
        len,
        title,
        project,
        place,
        limit: None,
        last_record: last_record_time(&tail),
        turn: turn_state(&tail),
        background: false,
    }
}

/// Where the newest turn stands, and since when. A turn that started and
/// hasn't ended is open: Codex writes nothing while a command runs or an
/// approval prompt is up, so it counts as running, unless its newest call
/// asks you something. With no turn in the tail, the idle time decides.
fn turn_state(tail: &str) -> Option<(Turn, SystemTime)> {
    let mut newest = None;
    let mut seen_item = false;
    // When the newest reply item asked you something, if it did.
    let mut asked = None;
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let at = timestamp(&v);
        newest = newest.or(at);
        let payload = &v["payload"];
        match (v["type"].as_str().unwrap_or_default(), payload["type"].as_str().unwrap_or_default()) {
            ("event_msg", "task_complete" | "turn_aborted") => return at.map(|at| (Turn::Done, at)),
            ("event_msg", "task_started") => {
                return match asked {
                    Some(at) => Some((Turn::Question, at)),
                    None => newest.map(|at| (Turn::Running, at)),
                };
            }
            ("response_item", kind) if !seen_item => {
                seen_item = true;
                if kind == "function_call" && payload["name"] == ASKING_TOOL {
                    asked = at;
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    /// Records shaped like a Codex rollout's.
    fn event(kind: &str, at: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-01T10:{at}.000Z","type":"event_msg","payload":{{"type":"{kind}","turn_id":"t1"}}}}"#)
    }
    fn call(name: &str, at: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-01T10:{at}.000Z","type":"response_item","payload":{{"type":"function_call","name":"{name}","arguments":"{{}}","call_id":"call_1"}}}}"#)
    }
    fn output(at: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-01T10:{at}.000Z","type":"response_item","payload":{{"type":"function_call_output","call_id":"call_1","output":"ok"}}}}"#)
    }
    fn at(s: &str) -> SystemTime {
        SystemTime::from(DateTime::parse_from_rfc3339(&format!("2026-11-01T10:{s}Z")).unwrap())
    }
    fn turn(lines: &[&str]) -> Option<(Turn, SystemTime)> {
        turn_state(&(lines.join("\n") + "\n"))
    }

    #[test]
    fn open_turn_is_running_until_it_ends() {
        let started = event("task_started", "00:00");
        let exec = call("exec", "00:05");
        let tokens = event("token_count", "00:06");
        assert_eq!(turn(&[&started, &exec, &tokens]), Some((Turn::Running, at("00:06"))));
        let done = event("task_complete", "03:00");
        assert_eq!(turn(&[&started, &exec, &output("02:00"), &done]), Some((Turn::Done, at("03:00"))));
        let stopped = event("turn_aborted", "01:00");
        assert_eq!(turn(&[&started, &exec, &stopped]), Some((Turn::Done, at("01:00"))));
        // A new turn after an ended one.
        assert_eq!(turn(&[&done, &event("task_started", "04:00")]), Some((Turn::Running, at("04:00"))));
        // A long turn whose start is out of the tail.
        assert_eq!(turn(&[&exec, &tokens]), None);
    }

    #[test]
    fn open_question_waits_for_the_answer() {
        let started = event("task_started", "00:00");
        let ask = call("request_user_input", "00:10");
        let tokens = event("token_count", "00:11");
        assert_eq!(turn(&[&started, &ask, &tokens]), Some((Turn::Question, at("00:10"))));
        assert_eq!(turn(&[&started, &ask, &output("02:00")]), Some((Turn::Running, at("02:00"))));
        // The Codex app's version doesn't wait.
        let async_ask = call("request_user_input_async", "00:10");
        assert_eq!(turn(&[&started, &async_ask, &tokens]), Some((Turn::Running, at("00:11"))));
    }

    #[test]
    fn reads_name_project_and_app() {
        let dir = std::env::temp_dir().join(format!("overnight-codex-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-11-01T17-35-21-01a196ad-99db-7902-af23-bca5aab0f023.jsonl");
        let meta_line = r#"{"timestamp":"2026-11-01T10:35:22.412Z","type":"session_meta","payload":{"session_id":"01a196ad-99db-7902-af23-bca5aab0f023","id":"01a196ad-99db-7902-af23-bca5aab0f023","cwd":"/Users/me/Git/overnight","originator":"Codex Desktop","source":"vscode"}}"#;
        let text = [meta_line, &event("task_started", "36:00"), &event("task_complete", "40:00")].join("\n") + "\n";
        fs::write(&path, &text).unwrap();
        let index = dir.join("session_index.jsonl");
        fs::write(
            &index,
            concat!(
                r#"{"id":"01a196ad-99db-7902-af23-bca5aab0f023","thread_name":"First name","updated_at":"x"}"#, "\n",
                r#"{"id":"01a196ad-99db-7902-af23-bca5aab0f023","thread_name":"Lid sleep fix","updated_at":"y"}"#, "\n",
            ),
        )
        .unwrap();
        let meta = describe(&path, text.len() as u64, &read_titles(&index));
        assert_eq!(meta.title, "Lid sleep fix");
        assert_eq!(meta.project, "overnight");
        assert_eq!(meta.place, Some("the Codex app"));
        assert_eq!(meta.turn, Some((Turn::Done, at("40:00"))));
        assert_eq!(meta.last_record, Some(at("40:00")));
        assert_eq!(describe(&path, text.len() as u64, &HashMap::new()).title, "Session 01a196ad");
        fs::remove_dir_all(&dir).unwrap();
    }
}
