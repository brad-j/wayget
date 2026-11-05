//! Reads pi session files (`~/.pi/agent/sessions/--<cwd>--/<time>_<id>.jsonl`).
//!
//! Entries form a tree through `id`/`parentId`. Like pi itself, wayget treats
//! the last entry in the file as the current leaf and reads only the branch
//! from the root to it, so abandoned branches stay out of the board.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::Value;

use crate::{
    index::{Agent, MAX_FILES, Session, asks_question, clean_prompt, one_line, parse_ts},
    transcript::{self, Entry},
};

/// Where pi keeps sessions: `PI_CODING_AGENT_SESSION_DIR`, else
/// `$PI_CODING_AGENT_DIR/sessions`, else `~/.pi/agent/sessions`.
pub fn sessions_root() -> PathBuf {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    var("PI_CODING_AGENT_SESSION_DIR")
        .or_else(|| var("PI_CODING_AGENT_DIR").map(|d| d.join("sessions")))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".pi").join("agent").join("sessions"))
}

struct File {
    id: String,
    cwd: String,
    /// The latest `session_info` name anywhere in the file.
    name: Option<String>,
    /// Entries from the root to the current leaf.
    branch: Vec<Value>,
    /// Usage cost of every entry in the file, all branches included.
    cost: f64,
}

fn read(path: &Path) -> Result<File> {
    let data = fs::read_to_string(path)?;
    let mut f = File { id: String::new(), cwd: String::new(), name: None, branch: Vec::new(), cost: 0.0 };
    let mut entries: Vec<Value> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["type"].as_str() {
            Some("session") => {
                f.id = v["id"].as_str().unwrap_or_default().to_string();
                f.cwd = v["cwd"].as_str().unwrap_or_default().to_string();
                continue;
            }
            Some("session_info") => f.name = v["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(String::from),
            _ => {}
        }
        f.cost += v["message"]["usage"]["cost"]["total"].as_f64().unwrap_or(0.0)
            + v["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
        if let Some(id) = v["id"].as_str() {
            at.insert(id.to_string(), entries.len());
            entries.push(v);
        }
    }
    let mut chain = Vec::new();
    let mut cur = entries.len().checked_sub(1);
    while let Some(i) = cur {
        if chain.len() > entries.len() {
            break; // a parent cycle; give up on the rest
        }
        chain.push(i);
        cur = entries[i]["parentId"].as_str().and_then(|p| at.get(p).copied());
    }
    let mut slots: Vec<Option<Value>> = entries.into_iter().map(Some).collect();
    f.branch = chain.into_iter().rev().filter_map(|i| slots[i].take()).collect();
    if f.id.is_empty() {
        f.id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    Ok(f)
}

/// The text a person typed, with a skill invocation shown as `/skill:name`.
fn prompt_text(content: &Value) -> Option<String> {
    let raw = match content {
        Value::String(s) => s.clone(),
        Value::Array(bs) => bs
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if let Some(rest) = raw.strip_prefix("<skill name=\"")
        && let Some(end) = rest.find('"')
    {
        let name = &rest[..end];
        let after = rest.find("</skill>").map_or("", |i| rest[i + "</skill>".len()..].trim());
        return Some(if after.is_empty() { format!("/skill:{name}") } else { format!("/skill:{name} {after}") });
    }
    clean_prompt(&raw)
}

/// Tool calls made inside a `codemode` script, from its result: name and arguments.
fn nested_calls(msg: &Value) -> Vec<(String, Value)> {
    if let Some(calls) = msg["nestedCalls"]["calls"].as_array() {
        return calls
            .iter()
            .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["arguments"].clone())))
            .collect();
    }
    msg["details"]["calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| {
                    let args = c["args"].as_str().and_then(|a| serde_json::from_str(a).ok()).unwrap_or(Value::Null);
                    Some((c["name"].as_str()?.to_string(), args))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn written_file(name: &str, args: &Value) -> Option<String> {
    matches!(name, "write" | "edit").then(|| args["path"].as_str().map(String::from)).flatten()
}

pub fn parse_file(path: &Path, size: u64, mtime: i64) -> Result<Session> {
    let f = read(path)?;
    let mut s = Session {
        agent: Agent::Pi,
        id: f.id,
        path: path.to_path_buf(),
        cwd: f.cwd,
        cost: f.cost,
        size,
        mtime,
        ..Default::default()
    };
    let mut turn: Vec<String> = Vec::new();
    let mut last_text_any = String::new();
    let mut agent_last = false;
    let mut tools: HashMap<String, u32> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    let add_file = |files: &mut Vec<String>, p: String| {
        files.retain(|x| x != &p);
        files.push(p);
    };
    let (mut first_ts, mut last_ts) = (i64::MAX, 0i64);

    for e in &f.branch {
        let ts = e["timestamp"].as_str().and_then(parse_ts);
        if let Some(t) = ts {
            first_ts = first_ts.min(t);
            last_ts = last_ts.max(t);
        }
        if e["type"] != "message" {
            continue;
        }
        let m = &e["message"];
        match m["role"].as_str() {
            Some("user") => {
                let Some(text) = prompt_text(&m["content"]) else { continue };
                if s.first_prompt.is_empty() {
                    s.first_prompt = text.clone();
                }
                s.last_prompt = text;
                s.prompts += 1;
                if let Some(t) = ts {
                    s.prompt_times.push(t);
                }
                turn.clear();
                agent_last = false;
            }
            Some("assistant") => {
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or("").trim();
                            if !t.is_empty() {
                                turn.push(t.to_string());
                                last_text_any = t.to_string();
                                agent_last = true;
                            }
                        }
                        Some("toolCall") => {
                            let name = b["name"].as_str().unwrap_or_default().to_string();
                            if let Some(p) = written_file(&name, &b["arguments"]) {
                                add_file(&mut files, p);
                            }
                            *tools.entry(name).or_default() += 1;
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => {
                for (name, args) in nested_calls(m) {
                    if let Some(p) = written_file(&name, &args) {
                        add_file(&mut files, p);
                    }
                    *tools.entry(name).or_default() += 1;
                }
            }
            _ => {}
        }
    }

    s.last_reply = if turn.is_empty() { last_text_any } else { turn.join("\n\n") };
    s.ends_with_reply = agent_last;
    s.awaiting = agent_last && asks_question(&s.last_reply);
    s.title = f.name.unwrap_or_else(|| one_line(&s.first_prompt, 80));
    s.started = if first_ts == i64::MAX { mtime } else { first_ts };
    s.updated = if last_ts == 0 { mtime } else { last_ts };
    if files.len() > MAX_FILES {
        files.drain(..files.len() - MAX_FILES);
    }
    s.files = files;
    let mut tools: Vec<(String, u32)> = tools.into_iter().collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.1));
    s.tools = tools;
    Ok(s)
}

/// The current branch as transcript entries.
pub fn load_entries(path: &Path) -> Result<Vec<Entry>> {
    let f = read(path)?;
    let mut entries = Vec::new();
    let mut tool_at: HashMap<String, usize> = HashMap::new();
    for e in &f.branch {
        if e["type"] != "message" {
            continue;
        }
        let m = &e["message"];
        match m["role"].as_str() {
            Some("user") => {
                if let Some(text) = prompt_text(&m["content"]) {
                    entries.push(Entry::User { text, ts: e["timestamp"].as_str().and_then(parse_ts) });
                }
            }
            Some("assistant") => {
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or("").trim();
                            if t.is_empty() {
                                continue;
                            }
                            if let Some(Entry::Claude { text }) = entries.last_mut() {
                                text.push_str("\n\n");
                                text.push_str(t);
                            } else {
                                entries.push(Entry::Claude { text: t.to_string() });
                            }
                        }
                        Some("toolCall") => {
                            let name = b["name"].as_str().unwrap_or("tool");
                            if let Some(id) = b["id"].as_str() {
                                tool_at.insert(id.to_string(), entries.len());
                            }
                            entries.push(transcript::tool_entry(name, &b["arguments"]));
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => {
                let id = m["toolCallId"].as_str().unwrap_or("");
                if let Some(&at) = tool_at.get(id)
                    && let Some(Entry::Tool { result, error, .. }) = entries.get_mut(at)
                {
                    *result = Some(transcript::result_text(&m["content"]));
                    *error = m["isError"].as_bool().unwrap_or(false);
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wayget-pi-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{}.jsonl", lines.len()));
        fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    #[test]
    fn reads_the_current_branch() {
        let p = write(&[
            r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-11-05T10:00:00Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"1","parentId":null,"timestamp":"2026-11-05T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"message","id":"2","parentId":"1","timestamp":"2026-11-05T10:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"old branch"}],"usage":{"cost":{"total":0.5}}}}"#,
            r#"{"type":"message","id":"3","parentId":"1","timestamp":"2026-11-05T10:00:03Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"write","arguments":{"path":"/w/a.rs"}},{"type":"text","text":"Which one?"}],"usage":{"cost":{"total":0.25}}}}"#,
            r#"{"type":"message","id":"4","parentId":"3","timestamp":"2026-11-05T10:00:04Z","message":{"role":"toolResult","toolCallId":"t1","content":[{"type":"text","text":"ok"}],"isError":false}}"#,
            r#"{"type":"session_info","id":"5","parentId":"4","timestamp":"2026-11-05T10:00:05Z","name":"Named"}"#,
        ]);
        let s = parse_file(&p, 0, 0).unwrap();
        assert_eq!(s.agent, Agent::Pi);
        assert_eq!((s.id.as_str(), s.cwd.as_str(), s.title.as_str()), ("abc", "/w", "Named"));
        assert_eq!(s.prompts, 1);
        assert_eq!(s.last_reply, "Which one?");
        assert!(s.awaiting);
        assert_eq!(s.files, vec!["/w/a.rs".to_string()]);
        assert!((s.cost - 0.75).abs() < 1e-9);
        let entries = load_entries(&p).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[1], Entry::Tool { result: Some(r), .. } if r == "ok"));
    }

    #[test]
    fn skill_prompts_are_shortened() {
        let v = Value::String("<skill name=\"handoff\" location=\"/x\">\nbody\n</skill>\n\nWrite it".into());
        assert_eq!(prompt_text(&v).as_deref(), Some("/skill:handoff Write it"));
    }
}
