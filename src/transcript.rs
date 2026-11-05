//! Full conversation view, loaded on demand from one transcript file.

use std::{collections::HashMap, collections::HashSet, fs, path::Path};

use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use serde_json::Value;

use crate::{
    index::{Agent, clean_prompt, one_line, parse_ts},
    markdown::{self, Block},
    theme,
};

pub enum Entry {
    User { text: String, ts: Option<i64> },
    Claude { text: String },
    Tool { name: String, summary: String, input: String, result: Option<String>, error: bool },
}

pub struct Transcript {
    pub title: String,
    agent: Agent,
    pub entries: Vec<Entry>,
    pub expanded: HashSet<usize>,
    pub expand_all: bool,
    pub scroll: usize,
    pub rows: Vec<Line<'static>>,
    pub row_entry: Vec<Option<usize>>,
    pub prompt_rows: Vec<usize>,
    width: u16,
    dirty: bool,
    jumped: bool,
}

impl Transcript {
    pub fn load(agent: Agent, path: &Path, title: &str) -> anyhow::Result<Self> {
        Ok(Self {
            title: title.to_string(),
            agent,
            entries: load_entries(agent, path)?,
            expanded: HashSet::new(),
            expand_all: false,
            scroll: 0,
            rows: Vec::new(),
            row_entry: Vec::new(),
            prompt_rows: Vec::new(),
            width: 0,
            dirty: true,
            jumped: false,
        })
    }

    pub fn toggle(&mut self, entry: usize) {
        if !matches!(self.entries.get(entry), Some(Entry::Tool { .. })) {
            return;
        }
        if !self.expanded.remove(&entry) {
            self.expanded.insert(entry);
        }
        self.dirty = true;
    }

    pub fn toggle_all(&mut self) {
        self.expand_all = !self.expand_all;
        self.expanded.clear();
        self.dirty = true;
    }

    /// Re-lays out rows for `width`; on first layout, scrolls to the last prompt.
    pub fn layout(&mut self, width: u16, height: u16) {
        if !self.dirty && width == self.width {
            return;
        }
        let anchor = self.row_entry.get(self.scroll).copied().flatten();
        self.width = width;
        self.dirty = false;
        self.rows.clear();
        self.row_entry.clear();
        self.prompt_rows.clear();
        let mut prev_tool = false;
        for (i, e) in self.entries.iter().enumerate() {
            let is_tool = matches!(e, Entry::Tool { .. });
            if !self.rows.is_empty() && !(is_tool && prev_tool) {
                self.rows.push(Line::default());
                self.row_entry.push(None);
            }
            prev_tool = is_tool;
            let blocks = self.blocks_for(i, e);
            if matches!(e, Entry::User { .. }) {
                self.prompt_rows.push(self.rows.len());
            }
            for b in blocks {
                for row in markdown::wrap(&b, width) {
                    self.rows.push(row);
                    self.row_entry.push(Some(i));
                }
            }
        }
        if !self.jumped {
            self.jumped = true;
            self.scroll = self.prompt_rows.last().copied().unwrap_or(0);
        } else if let Some(a) = anchor
            && let Some(r) = self.row_entry.iter().position(|e| *e == Some(a)) {
                self.scroll = r;
            }
        self.clamp(height);
    }

    pub fn clamp(&mut self, height: u16) {
        let max = self.rows.len().saturating_sub(height as usize);
        self.scroll = self.scroll.min(max);
    }

    fn blocks_for(&self, i: usize, e: &Entry) -> Vec<Block> {
        match e {
            Entry::User { text, ts } => {
                let when = ts
                    .map(crate::app::clock)
                    .map(|t| format!("  {t}"))
                    .unwrap_or_default();
                let mut v = vec![Block::new(vec![
                    Span::styled("❯ You", theme::label()),
                    Span::styled(when, theme::faint()),
                ])];
                for l in text.lines() {
                    v.push(Block::new(vec![Span::raw("  "), Span::styled(l.to_string(), theme::bright())]).indent(2));
                }
                v
            }
            Entry::Claude { text } => {
                let mut v = vec![Block::new(vec![Span::styled(
                    match self.agent {
                        Agent::Claude => "◆ Claude",
                        Agent::Pi => "◆ pi",
                    },
                    theme::text().add_modifier(Modifier::BOLD),
                )])];
                for mut b in markdown::render(text) {
                    b.spans.insert(0, Span::raw("  "));
                    b.indent += 2;
                    v.push(b);
                }
                v
            }
            Entry::Tool { name, summary, input, result, error } => {
                let open = self.expand_all || self.expanded.contains(&i);
                let mark = if open { "▾" } else { "▸" };
                let name_style = if *error { theme::dim().fg(theme::ERR) } else { theme::dim() };
                let mut v = vec![
                    Block::new(vec![
                        Span::styled(format!("  {mark} "), theme::faint()),
                        Span::styled(name.clone(), name_style.add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  {summary}"), theme::faint()),
                    ])
                    .indent(6),
                ];
                if open {
                    push_code(&mut v, input, 40);
                    if let Some(r) = result {
                        v.push(Block::new(vec![Span::styled("    result", theme::faint())]));
                        push_code(&mut v, r, 40);
                    }
                }
                v
            }
        }
    }
}

fn push_code(v: &mut Vec<Block>, text: &str, max: usize) {
    let lines: Vec<&str> = text.lines().collect();
    for l in lines.iter().take(max) {
        v.push(
            Block::new(vec![
                Span::styled("    │ ", theme::faint()),
                Span::styled(l.replace('\t', "    "), theme::code()),
            ])
            .indent(6),
        );
    }
    if lines.len() > max {
        v.push(Block::new(vec![Span::styled(
            format!("    │ … {} more lines", lines.len() - max),
            theme::faint(),
        )]));
    }
}

fn load_entries(agent: Agent, path: &Path) -> anyhow::Result<Vec<Entry>> {
    match agent {
        Agent::Claude => load_claude_entries(path),
        Agent::Pi => crate::pi::load_entries(path),
    }
}

/// A folded tool call, before its result arrives.
pub fn tool_entry(name: &str, input: &Value) -> Entry {
    Entry::Tool {
        name: tool_label(name),
        summary: tool_summary(name, input),
        input: tool_input_text(name, input),
        result: None,
        error: false,
    }
}

fn load_claude_entries(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let data = fs::read_to_string(path)?;
    let mut entries = Vec::new();
    let mut tool_at: HashMap<String, usize> = HashMap::new();
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v["type"].as_str().unwrap_or("");
        if v["isSidechain"].as_bool().unwrap_or(false) || !(kind == "user" || kind == "assistant") {
            continue;
        }
        let content = &v["message"]["content"];
        if kind == "user" {
            if let Some(blocks) = content.as_array() {
                let mut had_result = false;
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    had_result = true;
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    if let Some(&at) = tool_at.get(id)
                        && let Some(Entry::Tool { result, error, .. }) = entries.get_mut(at) {
                            *result = Some(result_text(&b["content"]));
                            *error = b["is_error"].as_bool().unwrap_or(false);
                        }
                }
                if had_result {
                    continue;
                }
            }
            if v["isMeta"].as_bool().unwrap_or(false) || v["isCompactSummary"].as_bool().unwrap_or(false) {
                continue;
            }
            if v["origin"]["kind"].as_str().is_some_and(|k| k != "human") {
                continue;
            }
            let raw = match content {
                Value::String(s) => s.clone(),
                Value::Array(bs) => bs
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            if let Some(text) = clean_prompt(&raw) {
                entries.push(Entry::User { text, ts: v["timestamp"].as_str().and_then(parse_ts) });
            }
        } else if let Some(blocks) = content.as_array() {
            for b in blocks {
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
                    Some("tool_use") => {
                        let name = b["name"].as_str().unwrap_or("tool");
                        let input = &b["input"];
                        if let Some(id) = b["id"].as_str() {
                            tool_at.insert(id.to_string(), entries.len());
                        }
                        entries.push(tool_entry(name, input));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(entries)
}

pub fn result_text(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    truncate_chars(&s, 6000)
}

/// `mcp__claude_ai_Gmail__search_threads` becomes `Gmail · search_threads`.
pub fn tool_label(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let mut parts = rest.splitn(2, "__");
        let server = parts.next().unwrap_or("");
        let tool = parts.next().unwrap_or("");
        let server = server
            .strip_prefix("claude_ai_")
            .or_else(|| server.strip_prefix("plugin_"))
            .unwrap_or(server)
            .replace('_', " ");
        return format!("{server} · {tool}");
    }
    name.to_string()
}

fn tool_summary(name: &str, input: &Value) -> String {
    let get = |k: &str| input[k].as_str().map(str::to_string);
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
    let tilde = |p: String| if !home.is_empty() && p.starts_with(&home) { format!("~{}", &p[home.len()..]) } else { p };
    let s = match name {
        "Bash" => get("description").or_else(|| get("command")),
        "bash" => get("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" => get("file_path").map(tilde),
        "read" | "write" | "edit" => get("path").map(tilde),
        "codemode" => get("code").map(|c| c.lines().find(|l| !l.trim().is_empty() && !l.starts_with("//")).unwrap_or("").to_string()),
        "web_search" | "source_check" => get("query").or_else(|| get("claim")),
        "fetch_content" => get("url"),
        "subagent" => get("agent").or_else(|| get("task")),
        "NotebookEdit" => get("notebook_path").map(tilde),
        "Grep" | "Glob" => get("pattern"),
        "WebFetch" => get("url"),
        "WebSearch" | "ToolSearch" => get("query"),
        "Agent" | "Task" => get("description"),
        "Skill" => get("skill"),
        _ => None,
    }
    .or_else(|| {
        input
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_string)))
    })
    .unwrap_or_default();
    one_line(&s, 120)
}

fn tool_input_text(name: &str, input: &Value) -> String {
    let s = match name {
        "Bash" | "bash" => input["command"].as_str().map(str::to_string),
        "Write" | "write" => input["content"].as_str().map(str::to_string),
        "codemode" => input["code"].as_str().map(str::to_string),
        _ => None,
    }
    .unwrap_or_else(|| serde_json::to_string_pretty(input).unwrap_or_default());
    truncate_chars(&s, 6000)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &s[..end])
}

/// Plain text for the summarizer: prompts and replies, no tool output.
pub fn plain_text(agent: Agent, path: &Path, budget: usize) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for e in load_entries(agent, path)? {
        match e {
            Entry::User { text, .. } => parts.push(format!("USER: {}", truncate_chars(&text, 2500))),
            Entry::Claude { text } => parts.push(format!("CLAUDE: {}", truncate_chars(&text, 2500))),
            Entry::Tool { name, summary, .. } => parts.push(format!("(tool {name}: {summary})")),
        }
    }
    let all = parts.join("\n\n");
    if all.len() <= budget {
        return Ok(all);
    }
    // Keep the opening for context and as much of the ending as fits.
    let head = budget / 6;
    let mut h = head;
    while !all.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = all.len() - (budget - head);
    while !all.is_char_boundary(t) {
        t += 1;
    }
    Ok(format!("{}\n\n[... middle of the session omitted ...]\n\n{}", &all[..h], &all[t..]))
}
