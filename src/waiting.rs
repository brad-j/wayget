//! "Waiting on me" judged by Jev, TypeSafe's yes/no model, instead of by
//! question marks alone. Off unless `WAYGET_JEV_KEY` is set. For each
//! recent session where Claude spoke last, sends the end of Claude's reply,
//! with anything secret-shaped masked, once per session update, and caches the
//! answer. Sessions not judged yet, or whose judgment failed, fall back to the
//! question-mark check.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::index::Session;

const DEFAULT_URL: &str = "https://api.orcarouter.ai/v1/systemone";
const DEFAULT_MODEL: &str = "typesafe/jev-1.13";
/// Jev's probability that Claude is blocked on you, at or above which the session waits.
const THRESHOLD: f32 = 0.5;
/// Requests to the user sit at the end of a reply, so little more is needed.
const REPLY_MAX: usize = 1_000;
const TIMEOUT: Duration = Duration::from_secs(10);

const INSTRUCTIONS: &str = "`reply` is the end of the last message a coding agent sent \
to the user it works for. Does the agent now need something from the user before the \
work can go on?";
const YES: &str = "The reply asks a question the user has to answer, asks the user to \
choose between options, asks for permission or approval to go ahead, or asks the user \
to do something (run a command, log in, test, supply a file or value) and come back.";
const NO: &str = "The reply reports finished work or a stopping point and needs nothing \
back. Optional offers of more work such as \"Want me to also...?\", rhetorical questions, \
and suggestions the user can follow on their own count as no.";

pub struct Config {
    key: String,
    url: String,
    model: String,
}

impl Config {
    pub fn from_env() -> Option<Self> {
        let var = |name: &str| crate::env(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Some(Self {
            key: var("JEV_KEY")?,
            url: var("JEV_URL").unwrap_or_else(|| DEFAULT_URL.into()),
            model: var("JEV_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
        })
    }
}

// ---------------------------------------------------------------------------
// Cache

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Judgment {
    /// The session's `updated` time when judged. Newer activity makes it stale.
    pub updated: i64,
    pub p: f32,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    sessions: HashMap<String, Judgment>,
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("wayget").join("waiting.json"))
}

impl Cache {
    pub fn load() -> Self {
        cache_path()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = cache_path() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec(self) {
            let tmp = p.with_extension("tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(tmp, p);
            }
        }
    }

    /// Jev's probability for this session as it stands, if it has been judged.
    pub fn get(&self, s: &Session) -> Option<f32> {
        self.sessions
            .get(&s.id)
            .filter(|j| j.updated == s.updated && s.ends_with_reply)
            .map(|j| j.p)
    }

    pub fn insert(&mut self, id: String, j: Judgment) {
        self.sessions.insert(id, j);
    }

    /// Drops judgments for sessions last updated before `oldest`.
    pub fn prune(&mut self, oldest: i64) {
        self.sessions.retain(|_, j| j.updated >= oldest);
    }

    /// Whether Claude is waiting on you: Jev's answer when there is one,
    /// otherwise the question-mark check.
    pub fn awaits(&self, s: &Session) -> bool {
        match self.get(s) {
            Some(p) => p >= THRESHOLD,
            None => s.awaiting,
        }
    }
}

// ---------------------------------------------------------------------------
// Background judge

struct Job {
    id: String,
    updated: i64,
    reply: String,
}

pub enum Failure {
    /// The key or endpoint was refused; asking again will not help.
    Refused(String),
    Other(String),
}

pub struct Done {
    pub id: String,
    pub updated: i64,
    pub result: Result<f32, Failure>,
}

/// Asks Jev one session at a time on a background thread.
pub struct Judge {
    tx: Sender<Job>,
    rx: Receiver<Done>,
    /// Session versions already sent, so each is asked once per run.
    asked: HashSet<(String, i64)>,
}

impl Judge {
    pub fn spawn(cfg: Config) -> Self {
        let (tx, jobs) = mpsc::channel::<Job>();
        let (done_tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let agent = ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .build()
                .new_agent();
            for job in jobs {
                let result = judge(&agent, &cfg, &job.reply);
                let refused = matches!(result, Err(Failure::Refused(_)));
                if done_tx.send(Done { id: job.id, updated: job.updated, result }).is_err() || refused {
                    break;
                }
            }
        });
        Self { tx, rx, asked: HashSet::new() }
    }

    pub fn ask(&mut self, s: &Session) {
        if self.asked.insert((s.id.clone(), s.updated)) {
            let _ = self.tx.send(Job {
                id: s.id.clone(),
                updated: s.updated,
                // Mask first, so the cut cannot split a secret and hide its prefix.
                reply: tail(&redact(&s.last_reply), REPLY_MAX).to_string(),
            });
        }
    }

    pub fn drain(&self) -> Vec<Done> {
        self.rx.try_iter().collect()
    }
}

fn judge(agent: &ureq::Agent, cfg: &Config, reply: &str) -> Result<f32, Failure> {
    let body = request(&cfg.model, reply).to_string();
    let mut resp = agent
        .post(&cfg.url)
        .header("Authorization", &format!("Bearer {}", cfg.key))
        .content_type("application/json")
        .send(&body)
        .map_err(|e| match e {
            ureq::Error::StatusCode(code @ (401 | 403 | 404)) => Failure::Refused(format!("HTTP {code}")),
            e => Failure::Other(e.to_string()),
        })?;
    let text = resp.body_mut().read_to_string().map_err(|e| Failure::Other(e.to_string()))?;
    parse(&text).map_err(Failure::Other)
}

fn request(model: &str, reply: &str) -> Value {
    json!({
        "model": model,
        "state": { "reply": reply },
        "questions": {
            "blocked_on_user": {
                "type": "noul",
                "instructions": INSTRUCTIONS,
                "criteria": { "true": YES, "false": NO },
            }
        }
    })
}

fn parse(body: &str) -> Result<f32, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("unreadable reply: {e}"))?;
    v.pointer("/answers/blocked_on_user/noul")
        .and_then(Value::as_f64)
        .map(|p| p as f32)
        .ok_or_else(|| "reply had no answer".into())
}

/// The end of a reply is where any request to the user sits.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

// ---------------------------------------------------------------------------
// Masking

const MASK: &str = "[redacted]";
/// Prefixes of common API keys and tokens.
const PREFIXES: &[&str] = &[
    "sk-", "sk_", "pk_live_", "rk_live_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_",
    "glpat-", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "xapp-", "AKIA", "ASIA", "AIza", "ya29.", "eyJ",
    "npm_", "hf_", "tvly-", "re_", "whsec_", "SG.",
];
/// Parts of a name whose value is likely secret, as in `API_KEY=...` or `"password": "..."`.
const SECRET_NAMES: &[&str] = &["key", "token", "secret", "passw", "pwd", "auth", "credential", "private"];

/// Masks values that look like credentials: known key prefixes, `Bearer` and
/// `Basic` tokens, values of secret-sounding names, and long random strings.
/// Catches the common shapes, not every secret.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut r = Redactor::default();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() || "\"'`()<>[]{},;|".contains(c) {
            if let Some(st) = start.take() {
                r.word(&s[st..i], &mut out);
            }
            out.push(c);
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        r.word(&s[st..], &mut out);
    }
    out
}

#[derive(Default)]
struct Redactor {
    /// The next word is a value to mask.
    mask_next: bool,
    /// The last word was a secret-sounding name; a lone `:` or `=` may follow.
    after_name: bool,
}

impl Redactor {
    fn word(&mut self, w: &str, out: &mut String) {
        let after_name = std::mem::take(&mut self.after_name);
        let scheme = ["bearer", "basic"].iter().any(|k| w.eq_ignore_ascii_case(k));
        if self.mask_next {
            if scheme {
                out.push_str(w);
                return;
            }
            self.mask_next = false;
            if w.len() >= 4 && !plain_value(w) {
                out.push_str(MASK);
                return;
            }
        }
        if (after_name && (w == ":" || w == "=")) || scheme {
            out.push_str(w);
            self.mask_next = true;
            return;
        }
        if let Some(masked) = mask_query(w) {
            out.push_str(&masked);
            return;
        }
        if let Some(i) = w.find([':', '='])
            && secret_name(&w[..i])
        {
            let rest = &w[i + 1..];
            out.push_str(&w[..=i]);
            match rest.len() {
                0 => self.mask_next = true,
                _ if rest.len() < 4 || plain_value(rest) => out.push_str(rest),
                _ => out.push_str(MASK),
            }
            return;
        }
        let core = w.trim_end_matches(['.', ',', ':', '!', '?']);
        if looks_secret(core) {
            out.push_str(MASK);
            out.push_str(&w[core.len()..]);
            return;
        }
        self.after_name = secret_name(w);
        out.push_str(w);
    }
}

/// Masks secret-named parameters in a URL query, as in `?token=...&page=2`.
fn mask_query(w: &str) -> Option<String> {
    if !w.contains(['?', '&']) {
        return None;
    }
    let mut out = String::with_capacity(w.len());
    let mut changed = false;
    for seg in w.split_inclusive(['?', '&']) {
        let body = seg.trim_end_matches(['?', '&']);
        match body.split_once('=') {
            Some((name, value)) if secret_name(name) && value.len() >= 4 => {
                out.push_str(name);
                out.push('=');
                out.push_str(MASK);
                changed = true;
            }
            _ => out.push_str(body),
        }
        out.push_str(&seg[body.len()..]);
    }
    changed.then_some(out)
}

/// Settings like `PasswordAuthentication = false` name a secret but hold none.
fn plain_value(v: &str) -> bool {
    let v = v.trim_end_matches(['.', ',', ':', '!', '?']);
    ["true", "false", "null", "none", "yes", "no"].iter().any(|k| v.eq_ignore_ascii_case(k))
}

fn secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_NAMES.iter().any(|k| lower.contains(k))
}

/// A known key prefix, or a long run of mixed letters and digits like a hash or token.
fn looks_secret(w: &str) -> bool {
    if PREFIXES.iter().any(|p| w.starts_with(p) && w.len() >= p.len() + 16) {
        return true;
    }
    w.chars().all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c))
        && w.split(['-', '_', '+', '/', '=']).any(|part| {
            part.len() >= 16
                && part.chars().any(|c| c.is_ascii_digit())
                && part.chars().any(|c| c.is_ascii_alphabetic())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_parsed() {
        let body = r#"{"model":"jev-1.13.0","answers":{"blocked_on_user":{"type":"noul","noul":0.93}}}"#;
        assert!((parse(body).ok().unwrap() - 0.93).abs() < 1e-6);
        assert!(parse(r#"{"answers":{}}"#).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn requests_carry_the_reply_tail() {
        let v = request("typesafe/jev-1.13", "Done. Run `make` and tell me?");
        assert_eq!(v["model"], "typesafe/jev-1.13");
        assert_eq!(v["state"]["reply"], "Done. Run `make` and tell me?");
        assert_eq!(v["questions"]["blocked_on_user"]["type"], "noul");
        assert!(v["state"].get("request").is_none());
    }

    #[test]
    fn secrets_are_masked() {
        let cases = [
            ("export OPENAI_API_KEY=sk-proj-abcdef1234567890abcdef", "export OPENAI_API_KEY=[redacted]"),
            ("Authorization: Bearer abcdefghij0123456789", "Authorization: Bearer [redacted]"),
            (r#"{"password": "hunter22", "user": "brad"}"#, r#"{"password": "[redacted]", "user": "brad"}"#),
            ("API_TOKEN = 0f3a9c7d", "API_TOKEN = [redacted]"),
            ("Found `ghp_0123456789abcdefABCDEF0123` in the log.", "Found `[redacted]` in the log."),
            ("curl https://x.io/api?token=abc123def&page=2", "curl https://x.io/api?token=[redacted]&page=2"),
            ("hash 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b.", "hash [redacted]."),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "{input}");
        }
    }

    #[test]
    fn ordinary_text_is_kept() {
        let text = "Run `cargo test` in ~/westwood/Projects/Software/waygate-cs/src/app.rs:317 \
            and tell me what it prints? Commit 6fb3973, session b83882c1-76a1-4f42-9d2c-3eb1abcea2f4, \
            notes in 2026-09-28-homelab-audit.md. Confirm the token color. PasswordAuthentication = false. \
            Want me to also update the README?";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn trimming_respects_char_boundaries() {
        let s = "ab✓cd";
        assert_eq!(tail(s, 3), "cd");
        assert_eq!(tail("short", 10), "short");
    }

    #[test]
    fn stale_judgments_fall_back() {
        let s = Session { id: "a".into(), updated: 2, ends_with_reply: true, awaiting: true, ..Default::default() };
        let mut c = Cache::default();
        assert!(c.awaits(&s));
        c.insert("a".into(), Judgment { updated: 2, p: 0.1 });
        assert!(!c.awaits(&s));
        c.insert("a".into(), Judgment { updated: 1, p: 0.1 });
        assert!(c.awaits(&s));
    }
}

