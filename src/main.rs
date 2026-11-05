//! wayget: a terminal board for your Claude Code and pi sessions.

mod app;
mod index;
mod launch;
mod markdown;
mod pi;
mod store;
mod summary;
mod theme;
mod transcript;
mod ui;
mod waiting;

use std::{
    io::{IsTerminal, Write, stdout},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use ratatui::{
    DefaultTerminal,
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
        execute,
    },
};

use crate::{app::App, index::Indexer};

const HELP: &str = "\
wayget: a terminal board for your Claude Code and pi sessions

Usage:
  wayget              open the board
  wayget --list       print sessions as plain text, newest first
  wayget --reindex    rebuild the index from scratch, then open the board
  wayget --help       this help
  wayget --version    print the version

Environment:
  CLAUDE_CONFIG_DIR             where Claude Code keeps its data (default ~/.claude)
  PI_CODING_AGENT_SESSION_DIR   where pi keeps sessions (default ~/.pi/agent/sessions)
  WAYGET_CLAUDE                 the claude binary to launch (default claude)
  WAYGET_PI                     the pi binary to launch (default pi)
  WAYGET_RESUME=here            always resume in place instead of a new tab
  WAYGET_SUMMARY_MODEL          model for catch-me-up summaries (default haiku)
  WAYGET_JEV_KEY                judge Waiting on me with Jev (an OrcaRouter key by default)
  WAYGET_JEV_URL                Jev endpoint (default https://api.orcarouter.ai/v1/systemone)
  WAYGET_JEV_MODEL              Jev model (default typesafe/jev-1.13)

The older WAYGATE_CS_* names still work.

Press ? inside the board for keys and mouse controls.";

/// `WAYGET_<name>`, falling back to the name used before the rename.
pub fn env(name: &str) -> Option<String> {
    std::env::var(format!("WAYGET_{name}"))
        .or_else(|_| std::env::var(format!("WAYGATE_CS_{name}")))
        .ok()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut force = false;
    for a in &args {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("wayget {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--list" => return list(),
            "--reindex" => force = true,
            other => bail!("unknown argument: {other} (try wayget --help)"),
        }
    }

    let indexer = Indexer::load(force)?;
    if !stdout().is_terminal() {
        bail!("wayget needs a terminal; use wayget --list for plain output");
    }
    if indexer.sessions().is_empty() {
        let roots: Vec<String> = indexer.roots().map(|r| r.display().to_string()).collect();
        println!("No sessions found in {}", roots.join(" or "));
        return Ok(());
    }
    let mut app = App::new(indexer);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result?;

    if let Some((cwd, cmd)) = app.exec_on_exit.take() {
        println!("Running {} in {}", cmd.line(), app::tilde(&cwd));
        launch::resume_here(&cwd, &cmd)?;
    }
    Ok(())
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last = Instant::now();
    loop {
        let elapsed = last.elapsed();
        last = Instant::now();
        terminal.draw(|f| ui::draw(f, app, elapsed))?;
        if app.quit {
            return Ok(());
        }
        let timeout = if app.animating() { Duration::from_millis(33) } else { Duration::from_millis(400) };
        if event::poll(timeout)? {
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
    }
}

fn list() -> Result<()> {
    let indexer = Indexer::load(false)?;
    let judged = waiting::Cache::load();
    let now = app::now_ms();
    let mut out = stdout().lock();
    for s in indexer.sessions() {
        let flag = if judged.awaits(&s) { "?" } else { " " };
        let line = writeln!(
            out,
            "{:>7}  {}  {:<24}  {}  {}",
            app::ago(s.updated, now),
            flag,
            index::one_line(&app::tilde(&s.cwd), 24),
            s.id,
            index::one_line(&s.title, 70)
        );
        // A closed pipe (wayget --list | head) just means the reader has enough.
        if line.is_err() {
            break;
        }
    }
    Ok(())
}
