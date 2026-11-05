//! "Catch me up": asks `claude -p` for a three-line summary of a session, in
//! the background. Runs only when the user asks, since it costs a little.

use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::Sender,
    thread,
};

use crate::{index::Agent, launch::claude_bin, transcript};

pub struct Done {
    pub id: String,
    pub updated: i64,
    pub result: Result<String, String>,
}

const PROMPT: &str = "You are helping someone pick a coding agent session back up. \
Read the transcript below and reply with exactly three lines, no preamble, no headings, no bullets:\n\
Line 1 starts with \"Goal:\" and says what the session was working on.\n\
Line 2 starts with \"State:\" and says where it ended: decisions made and what was produced.\n\
Line 3 starts with \"Open:\" and says what is unresolved or waiting on the user, or \"Nothing open.\"\n\
Keep each line under 30 words. Use commas and periods, not dashes.\n\n<transcript>\n";

const SYSTEM: &str = "You write short, plain summaries of coding agent session transcripts.";

pub fn spawn(id: String, updated: i64, agent: Agent, path: PathBuf, tx: Sender<Done>) {
    thread::spawn(move || {
        let result = run(agent, &path);
        let _ = tx.send(Done { id, updated, result });
    });
}

fn run(agent: Agent, path: &std::path::Path) -> Result<String, String> {
    let body = transcript::plain_text(agent, path, 60_000).map_err(|e| e.to_string())?;
    let input = format!("{PROMPT}{body}\n</transcript>");
    let model = crate::env("SUMMARY_MODEL").unwrap_or_else(|| "haiku".into());
    // Strip everything a summary does not need: MCP servers, skills and tools
    // can add hundreds of thousands of tokens to every request otherwise.
    // Claude Code creates an (empty) project folder for whatever directory it
    // runs in, so run from our own cache folder to keep that to one.
    let workdir = dirs::cache_dir().map(|d| d.join("wayget")).unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&workdir);
    let mut child = Command::new(claude_bin())
        .args([
            "-p",
            "--model",
            &model,
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--system-prompt",
            SYSTEM,
        ])
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", claude_bin()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { text } else { err };
        Err(if err.is_empty() { "claude returned nothing".into() } else { err })
    }
}
