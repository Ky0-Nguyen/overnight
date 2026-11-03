//! Reads Cursor's agent chats: the transcripts Cursor writes to
//! ~/.cursor/projects/<project>/agent-transcripts/<chat>/<chat>.jsonl, with
//! its subagents' in `subagents/` beside them. Their records carry no
//! timestamps and no tool results, so activity is the file's modified time,
//! and a tool call is open for as long as its reply is the newest record.

use crate::claude::{mtime, read_range, subdirs, Agent, Meta, Transcript, Turn, HEAD_BYTES, TAIL_BYTES};
use std::{
    fs,
    path::{Path, PathBuf},
};
use sysinfo::Process;

/// Tools that ask you something: a question, or a plan to build.
const ASKING_TOOLS: &[&str] = &["AskQuestion", "CreatePlan"];
/// A chat's title is its first prompt, cut to this many characters.
const TITLE_CHARS: usize = 80;

/// The Cursor app, which its agent runs inside.
pub(crate) fn is_cursor(p: &Process) -> bool {
    p.name() == "Cursor"
}

/// Agent chat transcripts, most recently written first.
pub(crate) fn transcripts(projects: &Path) -> Vec<Transcript> {
    let mut found = Vec::new();
    for project in subdirs(projects) {
        for chat in subdirs(&project.join("agent-transcripts")) {
            let Some(name) = chat.file_name() else {
                continue;
            };
            let path = chat.join(format!("{}.jsonl", name.to_string_lossy()));
            let Some(written) = mtime(&path) else {
                continue;
            };
            let subagents = fs::read_dir(chat.join("subagents"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| mtime(&e.path()))
                .max();
            let written = subagents.map_or(written, |s| s.max(written));
            found.push(Transcript { path, agent: Agent::Cursor, written, subagents });
        }
    }
    found.sort_by(|a, b| b.written.cmp(&a.written));
    found
}

/// A chat's title, project folder and where its turn stands.
pub(crate) fn describe(path: &Path, len: u64) -> Meta {
    let head = read_range(path, 0, HEAD_BYTES);
    let tail = read_range(path, len.saturating_sub(TAIL_BYTES), TAIL_BYTES);
    let written = mtime(path);
    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let title = first_prompt(&head).unwrap_or_else(|| format!("Session {}", id.chars().take(8).collect::<String>()));
    // <project>/agent-transcripts/<chat>/<chat>.jsonl
    let project = path
        .ancestors()
        .nth(3)
        .and_then(|p| p.file_name())
        .map(|slug| project_name(&slug.to_string_lossy(), Path::new("/")))
        .unwrap_or_default();
    Meta {
        len,
        title,
        project,
        place: Some("Cursor"),
        limit: None,
        last_record: written,
        turn: turn_state(&tail).zip(written),
        background: false,
    }
}

/// The first line of the chat's first prompt.
fn first_prompt(head: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v["role"] != "user" {
            return None;
        }
        // Cursor wraps what you typed in <user_query>, next to context it adds.
        let text = v.pointer("/message/content/0/text")?.as_str()?;
        let query = text.split("<user_query>").nth(1)?;
        let query = query.split("</user_query>").next().unwrap_or(query);
        let first = query.lines().map(str::trim).find(|l| !l.is_empty())?;
        Some(if first.chars().count() > TITLE_CHARS {
            format!("{}…", first.chars().take(TITLE_CHARS).collect::<String>().trim_end())
        } else {
            first.to_string()
        })
    })
}

/// Cursor names a project's folder after its path with each "/" turned into
/// "-" ("Users-me-Git-my-app"), so a dash in a folder name looks the same as
/// a slash. Finds the folder under `root` that it stands for, or failing
/// that, takes the last part.
fn project_name(slug: &str, root: &Path) -> String {
    let parts: Vec<&str> = slug.split('-').collect();
    find_folder(root, &parts)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| parts.last().copied().unwrap_or_default().to_string())
}

fn find_folder(dir: &Path, parts: &[&str]) -> Option<PathBuf> {
    if parts.is_empty() {
        return Some(dir.to_path_buf());
    }
    (1..=parts.len()).find_map(|n| {
        let next = dir.join(parts[..n].join("-"));
        next.is_dir().then(|| find_folder(&next, &parts[n..])).flatten()
    })
}

/// Where the newest turn stands. Cursor writes a reply once it's complete,
/// before its tools run, and `turn_ended` once the turn is over (Cursor 3 and
/// later). So a reply with tool calls as the newest record has a call still
/// open: a question if it asks you something, running otherwise. A turn
/// that ended on a question waits for your answer.
fn turn_state(tail: &str) -> Option<Turn> {
    let mut ended = false;
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"] == "turn_ended" {
            ended = true;
            continue;
        }
        match v["role"].as_str() {
            Some("assistant") => {
                let blocks = v.pointer("/message/content").and_then(|c| c.as_array());
                let call = blocks.map(Vec::as_slice).unwrap_or_default().iter().rev().find(|b| b["type"] == "tool_use");
                let asks = call.and_then(|c| c["name"].as_str()).is_some_and(|n| ASKING_TOOLS.contains(&n));
                return match (asks, call.is_some(), ended) {
                    (true, _, _) => Some(Turn::Question),
                    (false, true, false) => Some(Turn::Running),
                    (false, _, true) => Some(Turn::Done),
                    // Between two replies, or a chat from before Cursor wrote turn_ended.
                    (false, false, false) => None,
                };
            }
            // Your prompt: Cursor is on it, unless the turn ended without a reply.
            Some("user") => return ended.then_some(Turn::Done),
            _ => {}
        }
    }
    ended.then_some(Turn::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records shaped like Cursor's.
    fn reply(tools: &[&str]) -> String {
        let mut blocks = vec![r#"{"type":"text","text":"Looking."}"#.to_string()];
        blocks.extend(tools.iter().map(|t| format!(r#"{{"type":"tool_use","name":"{t}","input":{{}}}}"#)));
        format!(r#"{{"role":"assistant","message":{{"content":[{}]}}}}"#, blocks.join(","))
    }
    const PROMPT: &str = r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Monday</timestamp>\n<user_query>\nfix the lid sleep bug\nand add a test\n</user_query>"}]}}"#;
    const ENDED: &str = r#"{"type":"turn_ended","status":"success"}"#;

    fn turn(lines: &[&str]) -> Option<Turn> {
        turn_state(&(lines.join("\n") + "\n"))
    }

    #[test]
    fn open_tool_call_is_running_until_the_turn_ends() {
        assert_eq!(turn(&[PROMPT, &reply(&["Read", "Shell"])]), Some(Turn::Running));
        assert_eq!(turn(&[PROMPT, &reply(&["Shell"]), &reply(&[])]), None);
        assert_eq!(turn(&[PROMPT, &reply(&["Shell"]), &reply(&[]), ENDED]), Some(Turn::Done));
        assert_eq!(turn(&[PROMPT]), None);
        assert_eq!(turn(&[PROMPT, ENDED]), Some(Turn::Done));
        // A new prompt after an ended turn.
        assert_eq!(turn(&[&reply(&[]), ENDED, PROMPT]), None);
    }

    #[test]
    fn a_question_or_a_plan_waits_for_you() {
        assert_eq!(turn(&[PROMPT, &reply(&["AskQuestion"])]), Some(Turn::Question));
        assert_eq!(turn(&[PROMPT, &reply(&["Read", "CreatePlan"]), ENDED]), Some(Turn::Question));
        // Answered: the agent went on.
        assert_eq!(turn(&[PROMPT, &reply(&["AskQuestion"]), &reply(&["Read"])]), Some(Turn::Running));
        assert_eq!(turn(&[PROMPT, &reply(&["AskQuestion"]), &reply(&["Read"]), &reply(&[]), ENDED]), Some(Turn::Done));
    }

    #[test]
    fn title_is_the_first_line_of_the_first_prompt() {
        let reply = reply(&[]);
        assert_eq!(first_prompt(&format!("{PROMPT}\n{reply}\n")).as_deref(), Some("fix the lid sleep bug"));
        let long = format!(r#"{{"role":"user","message":{{"content":[{{"type":"text","text":"<user_query>{}</user_query>"}}]}}}}"#, "word ".repeat(40));
        // 80 characters end on a space, which goes.
        assert_eq!(first_prompt(&long).unwrap(), format!("{}…", "word ".repeat(16).trim_end()));
        assert_eq!(first_prompt(&reply), None);
    }

    #[test]
    fn project_name_follows_the_folders_on_disk() {
        let root = std::env::temp_dir().join(format!("overnight-cursor-{}", std::process::id()));
        fs::create_dir_all(root.join("Users/me/Git/petx/petx-be")).unwrap();
        assert_eq!(project_name("Users-me-Git-petx-petx-be", &root), "petx-be");
        assert_eq!(project_name("Users-me-Git-petx", &root), "petx");
        assert_eq!(project_name("Users-me-Gone-my-app", &root), "app");
        fs::remove_dir_all(&root).unwrap();
    }
}
