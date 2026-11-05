//! Reads session transcripts into compact summaries, with an on-disk cache
//! keyed by file size and mtime. Claude Code keeps them in
//! `~/.claude/projects/*/*.jsonl`; pi's are read by `crate::pi`.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

const CACHE_VERSION: u32 = 5;
pub const MAX_FILES: usize = 60;

/// The coding agent that wrote a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Agent {
    #[default]
    Claude,
    Pi,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub agent: Agent,
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: String,
    pub first_prompt: String,
    pub last_prompt: String,
    /// Everything Claude said in the final turn.
    pub last_reply: String,
    pub started: i64,
    pub updated: i64,
    pub prompts: u32,
    /// Millisecond timestamps of each human prompt, for stats.
    pub prompt_times: Vec<i64>,
    pub cost: f64,
    pub files: Vec<String>,
    pub artifacts: Vec<Artifact>,
    pub tools: Vec<(String, u32)>,
    /// Claude spoke last and ended on a question.
    pub awaiting: bool,
    /// Claude spoke last, so the session may be waiting on the user.
    pub ends_with_reply: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub title: String,
    pub url: String,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    sessions: Vec<Session>,
}

pub struct Indexer {
    roots: Vec<(PathBuf, Agent)>,
    by_path: HashMap<PathBuf, Session>,
}

impl Indexer {
    pub fn load(force: bool) -> Result<Self> {
        let roots = vec![(projects_root(), Agent::Claude), (crate::pi::sessions_root(), Agent::Pi)];
        let mut by_path = HashMap::new();
        if !force
            && let Some(cache) = read_cache() {
                for s in cache.sessions {
                    by_path.insert(s.path.clone(), s);
                }
            }
        let mut me = Self { roots, by_path };
        let cold = me.by_path.is_empty();
        if cold {
            eprint!("wayget: indexing sessions...");
        }
        if me.refresh()? || force {
            me.save();
        }
        if cold {
            eprintln!(" done");
        }
        Ok(me)
    }

    /// The folders sessions are read from.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|(p, _)| p.as_path())
    }

    /// Re-reads any transcript whose size or mtime changed. Returns true if anything did.
    pub fn refresh(&mut self) -> Result<bool> {
        let mut files = Vec::new();
        for (root, agent) in &self.roots {
            files.extend(list_files(root, *agent)?);
        }
        let mut changed = false;
        let keep: std::collections::HashSet<&PathBuf> = files.iter().map(|f| &f.0).collect();
        let before = self.by_path.len();
        self.by_path.retain(|p, _| keep.contains(p));
        changed |= self.by_path.len() != before;

        let stale: Vec<&(PathBuf, u64, i64, Agent)> = files
            .iter()
            .filter(|(p, size, mtime, _)| {
                self.by_path
                    .get(p)
                    .is_none_or(|s| s.size != *size || s.mtime != *mtime)
            })
            .collect();
        if !stale.is_empty() {
            changed = true;
            let parsed: Vec<Session> = stale
                .par_iter()
                .filter_map(|(p, size, mtime, agent)| match agent {
                    Agent::Claude => parse_file(p, *size, *mtime).ok(),
                    Agent::Pi => crate::pi::parse_file(p, *size, *mtime).ok(),
                })
                .collect();
            for s in parsed {
                self.by_path.insert(s.path.clone(), s);
            }
        }
        Ok(changed)
    }

    pub fn save(&self) {
        let sessions: Vec<Session> = self.by_path.values().cloned().collect();
        let cache = CacheFile { version: CACHE_VERSION, sessions };
        if let Some(path) = cache_path() {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_vec(&cache) {
                let tmp = path.with_extension("tmp");
                if fs::write(&tmp, json).is_ok() {
                    let _ = fs::rename(tmp, path);
                }
            }
        }
    }

    /// Sessions with at least one human prompt, newest first.
    pub fn sessions(&self) -> Vec<Session> {
        let mut v: Vec<Session> = self
            .by_path
            .values()
            .filter(|s| s.prompts > 0)
            .cloned()
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated));
        v
    }
}

pub fn claude_home() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty() {
            return PathBuf::from(dir);
        }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

fn projects_root() -> PathBuf {
    claude_home().join("projects")
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("wayget").join("index.json"))
}

fn read_cache() -> Option<CacheFile> {
    let bytes = fs::read(cache_path()?).ok()?;
    let cache: CacheFile = serde_json::from_slice(&bytes).ok()?;
    (cache.version == CACHE_VERSION).then_some(cache)
}

fn list_files(root: &Path, agent: Agent) -> Result<Vec<(PathBuf, u64, i64, Agent)>> {
    let mut out = Vec::new();
    let dirs = match fs::read_dir(root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    for dir in dirs.flatten() {
        let Ok(entries) = fs::read_dir(dir.path()) else { continue };
        // Only top-level transcripts; subagent sidechains live in subdirectories.
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl")
                && let Ok(md) = e.metadata()
                    && md.is_file() {
                        out.push((p, md.len(), mtime_ms(&md), agent));
                    }
        }
    }
    Ok(out)
}

fn mtime_ms(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------
// Parsing

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rec<'a> {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    is_sidechain: Option<bool>,
    #[serde(default)]
    is_meta: Option<bool>,
    #[serde(default)]
    is_compact_summary: Option<bool>,
    #[serde(default)]
    origin: Option<serde_json::Value>,
    #[serde(borrow, default)]
    message: Option<&'a RawValue>,
    #[serde(default, deserialize_with = "loose_str")]
    ai_title: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    custom_title: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    summary: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    agent_name: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    frame_url: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    title: Option<String>,
    #[serde(default, rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
}

fn loose_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<ToolInput>,
}

#[derive(Deserialize, Default)]
struct ToolInput {
    #[serde(default, deserialize_with = "loose_str")]
    file_path: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    notebook_path: Option<String>,
}

enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

fn parse_content(raw: &RawValue) -> Option<Content> {
    let s = raw.get().trim_start();
    if s.starts_with('"') {
        serde_json::from_str::<String>(raw.get()).ok().map(Content::Text)
    } else if s.starts_with('[') {
        serde_json::from_str::<Vec<Block>>(raw.get())
            .ok()
            .map(Content::Blocks)
    } else {
        None
    }
}

fn parse_file(path: &Path, size: u64, mtime: i64) -> Result<Session> {
    let data = fs::read_to_string(path)?;
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut s = Session {
        id,
        path: path.to_path_buf(),
        size,
        mtime,
        ..Default::default()
    };
    let mut ai_title = None;
    let mut custom_title = None;
    let mut summary = None;
    let mut agent_name = None;
    let mut turn: Vec<String> = Vec::new();
    let mut last_text_any = String::new();
    let mut claude_last = false;
    let mut tools: HashMap<String, u32> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    let (mut first_ts, mut last_ts) = (i64::MAX, 0i64);

    for line in data.lines() {
        let Ok(rec) = serde_json::from_str::<Rec>(line) else { continue };
        let kind = rec.kind.as_deref().unwrap_or("");
        if let Some(ts) = rec.timestamp.as_deref().and_then(parse_ts) {
            first_ts = first_ts.min(ts);
            last_ts = last_ts.max(ts);
        }
        if s.cwd.is_empty()
            && let Some(c) = &rec.cwd {
                s.cwd = c.clone();
            }
        match kind {
            "ai-title" => ai_title = rec.ai_title.or(ai_title),
            "custom-title" => custom_title = rec.custom_title.or(custom_title),
            "summary" => summary = rec.summary.or(summary),
            "agent-name" => agent_name = rec.agent_name.or(agent_name),
            "frame-link" => {
                if let Some(url) = rec.frame_url {
                    let title = rec.title.unwrap_or_else(|| "Artifact".into());
                    s.artifacts.retain(|a| a.url != url);
                    s.artifacts.push(Artifact { title, url });
                }
            }
            "cost-state" => {
                if let Some(c) = rec.total_cost_usd {
                    s.cost = s.cost.max(c);
                }
            }
            "user" | "assistant" => {
                if rec.is_sidechain.unwrap_or(false) {
                    continue;
                }
                let Some(raw) = rec.message else { continue };
                if kind == "user" {
                    if rec.is_meta.unwrap_or(false) || rec.is_compact_summary.unwrap_or(false) {
                        continue;
                    }
                    let human = rec
                        .origin
                        .as_ref()
                        .and_then(|o| o.get("kind"))
                        .and_then(|k| k.as_str());
                    if human.is_some_and(|k| k != "human") {
                        continue;
                    }
                    if human.is_none() && raw.get().contains(r#""type":"tool_result""#) {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Msg>(raw.get()) else { continue };
                    let text = match msg.content.and_then(parse_content) {
                        Some(Content::Text(t)) => t,
                        Some(Content::Blocks(bs)) => {
                            if bs.iter().any(|b| b.kind == "tool_result") {
                                continue;
                            }
                            bs.iter()
                                .filter(|b| b.kind == "text")
                                .filter_map(|b| b.text.as_deref())
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                        None => continue,
                    };
                    let Some(text) = clean_prompt(&text) else { continue };
                    if s.first_prompt.is_empty() {
                        s.first_prompt = text.clone();
                    }
                    s.last_prompt = text;
                    s.prompts += 1;
                    if let Some(ts) = rec.timestamp.as_deref().and_then(parse_ts) {
                        s.prompt_times.push(ts);
                    }
                    turn.clear();
                    claude_last = false;
                } else {
                    let Ok(msg) = serde_json::from_str::<Msg>(raw.get()) else { continue };
                    let Some(Content::Blocks(bs)) = msg.content.and_then(parse_content) else {
                        continue;
                    };
                    for b in bs {
                        match b.kind.as_str() {
                            "text" => {
                                if let Some(t) = b.text {
                                    let t = t.trim();
                                    if !t.is_empty() {
                                        turn.push(t.to_string());
                                        last_text_any = t.to_string();
                                        claude_last = true;
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = b.name.unwrap_or_default();
                                if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
                                    && let Some(inp) = b.input
                                        && let Some(p) = inp.file_path.or(inp.notebook_path) {
                                            files.retain(|f| f != &p);
                                            files.push(p);
                                        }
                                *tools.entry(name).or_default() += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    s.last_reply = if turn.is_empty() {
        last_text_any
    } else {
        turn.join("\n\n")
    };
    s.ends_with_reply = claude_last;
    s.awaiting = claude_last && asks_question(&s.last_reply);
    s.title = custom_title
        .or(ai_title)
        .or(summary)
        .or(agent_name)
        .unwrap_or_else(|| one_line(&s.first_prompt, 80));
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

pub fn parse_ts(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// Turns a raw user message into the prompt a person typed, or None for
/// tool plumbing, hook output and interruptions.
pub fn clean_prompt(raw: &str) -> Option<String> {
    let text = strip_tag(raw, "system-reminder");
    let text = text.trim();
    if text.is_empty() || text.starts_with("[Request interrupted") {
        return None;
    }
    if text.contains("<command-name>") {
        let name = between(text, "<command-name>", "</command-name>")?.trim();
        let args = between(text, "<command-args>", "</command-args>").unwrap_or("").trim();
        let name = if name.starts_with('/') { name.to_string() } else { format!("/{name}") };
        return Some(if args.is_empty() { name } else { format!("{name} {args}") });
    }
    if text.starts_with('<') || text.starts_with("This session is being continued") {
        return None;
    }
    Some(text.to_string())
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let a = s.find(open)? + open.len();
    let b = s[a..].find(close)? + a;
    Some(&s[a..b])
}

fn strip_tag(s: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(&close) {
            Some(j) => rest = &rest[i + j + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// True when the tail of a reply poses a question to the reader.
pub fn asks_question(reply: &str) -> bool {
    reply
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .take(12)
        .any(|l| {
            let l = l.trim_end_matches(['*', '_', '`', ')', '"', '\'', ' ']);
            l.ends_with('?') || l.contains("? ")
        })
}

pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut t: String = flat.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_cleaned() {
        assert_eq!(clean_prompt("hello").as_deref(), Some("hello"));
        assert_eq!(clean_prompt("[Request interrupted by user]"), None);
        assert_eq!(clean_prompt("<local-command-stdout>x</local-command-stdout>"), None);
        assert_eq!(
            clean_prompt("<command-name>/review</command-name><command-args>42</command-args>").as_deref(),
            Some("/review 42")
        );
        assert_eq!(
            clean_prompt("hi <system-reminder>noise</system-reminder> there").as_deref(),
            Some("hi  there")
        );
    }

    #[test]
    fn questions_are_detected() {
        assert!(asks_question("Done.\n\n1. **Terms:** which one?\n2. OK?"));
        assert!(asks_question("Go?"));
        assert!(!asks_question("All done. Tests pass."));
    }
}
