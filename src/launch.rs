//! Opening things: resume a session in a new terminal tab, open files and
//! URLs, copy to the clipboard.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use crate::index::Agent;

pub fn claude_bin() -> String {
    crate::env("CLAUDE").unwrap_or_else(|| "claude".into())
}

pub fn pi_bin() -> String {
    crate::env("PI").unwrap_or_else(|| "pi".into())
}

/// A program and its arguments that reopen a session.
#[derive(Clone)]
pub struct Cmd {
    pub bin: String,
    pub args: Vec<String>,
}

impl Cmd {
    /// How the agent that wrote a session resumes it.
    pub fn resume(agent: Agent, id: &str) -> Self {
        match agent {
            Agent::Claude => Cmd { bin: claude_bin(), args: vec!["--resume".into(), id.into()] },
            Agent::Pi => Cmd { bin: pi_bin(), args: vec!["--session".into(), id.into()] },
        }
    }

    pub fn line(&self) -> String {
        std::iter::once(self.bin.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
    }
}

pub enum Resume {
    /// Opened in a new tab or window of the named terminal.
    Opened(&'static str),
    /// No scriptable terminal found: the caller should exit and resume in place.
    Here,
}

/// Runs `cmd` in `cwd` in a new tab of the current terminal.
pub fn resume_in_new_tab(cwd: &str, cmd: &Cmd) -> Result<Resume, String> {
    if crate::env("RESUME").is_some_and(|v| v == "here") {
        return Ok(Resume::Here);
    }
    let cmd = cmd.line();
    if std::env::var_os("TMUX").is_some() {
        return tmux(cwd, &cmd).map(|_| Resume::Opened("tmux"));
    }
    if !cfg!(target_os = "macos") {
        return Ok(Resume::Here);
    }
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let (name, script) = match term.as_str() {
        "ghostty" => ("Ghostty", ghostty_script(cwd, &cmd)),
        "iTerm.app" => ("iTerm", iterm_script(cwd, &cmd)),
        "Apple_Terminal" => ("Terminal", terminal_script(cwd, &cmd)),
        _ => return Ok(Resume::Here),
    };
    osascript(&script).map(|_| Resume::Opened(name))
}

fn ghostty_script(cwd: &str, cmd: &str) -> String {
    format!(
        r#"tell application "Ghostty"
  set cfg to new surface configuration
  set initial working directory of cfg to "{cwd}"
  set initial input of cfg to "{cmd}" & return
  if (count of windows) > 0 then
    new tab in front window with configuration cfg
  else
    new window with configuration cfg
  end if
  activate
end tell"#,
        cwd = as_str(cwd),
        cmd = as_str(cmd)
    )
}

fn iterm_script(cwd: &str, cmd: &str) -> String {
    let line = format!("cd {} && {}", sh_quote(cwd), cmd);
    format!(
        r#"tell application "iTerm2"
  if (count of windows) = 0 then
    create window with default profile
  else
    tell current window to create tab with default profile
  end if
  tell current session of current window to write text "{}"
  activate
end tell"#,
        as_str(&line)
    )
}

fn terminal_script(cwd: &str, cmd: &str) -> String {
    let line = format!("cd {} && {}", sh_quote(cwd), cmd);
    format!(
        r#"tell application "Terminal"
  do script "{}"
  activate
end tell"#,
        as_str(&line)
    )
}

fn osascript(script: &str) -> Result<(), String> {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn tmux(cwd: &str, cmd: &str) -> Result<(), String> {
    let out = Command::new("tmux")
        .args(["new-window", "-P", "-F", "#{pane_id}", "-c", cwd])
        .output()
        .map_err(|e| format!("tmux: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Command::new("tmux")
        .args(["send-keys", "-t", &pane, cmd, "Enter"])
        .status()
        .map_err(|e| format!("tmux: {e}"))?;
    Ok(())
}

/// Replaces this process with `cmd` in `cwd`.
pub fn resume_here(cwd: &str, resume: &Cmd) -> anyhow::Result<()> {
    std::env::set_current_dir(cwd)?;
    let mut cmd = Command::new(&resume.bin);
    cmd.args(&resume.args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec().into())
    }
    #[cfg(not(unix))]
    {
        cmd.status()?;
        Ok(())
    }
}

pub fn resume_command(agent: Agent, cwd: &str, id: &str) -> String {
    format!("cd {} && {}", sh_quote(cwd), Cmd::resume(agent, id).line())
}

pub fn open(target: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(opener)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{opener}: {e}"))
}

pub fn copy(text: &str) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("pbcopy: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        child.wait().map_err(|e| e.to_string())?;
        return Ok(());
    }
    // OSC 52: most modern terminals (and tmux, and ssh) accept this.
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Escapes text for an AppleScript string literal.
fn as_str(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"a"), "YQ==");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(as_str(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
