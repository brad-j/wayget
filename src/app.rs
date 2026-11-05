//! Application state and input handling. Drawing lives in `ui.rs`.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use chrono::{Local, TimeZone};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
};
use tachyonfx::EffectManager;

use crate::{
    index::{Agent, Indexer, Session},
    launch::{self, Resume},
    store::{Store, Summary},
    summary,
    transcript::Transcript,
    waiting::{self, Failure, Judge, Judgment},
};

const LIVE_WINDOW_MS: i64 = 90_000;
/// A question left unanswered this long has been dropped, not forgotten.
const WAITING_MAX_AGE_MS: i64 = 14 * 86_400_000;
const POLL_EVERY: Duration = Duration::from_secs(2);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Projects,
    Sessions,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Board,
    Transcript,
    Stats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Border {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hover {
    Left(usize),
    Session(usize),
    Detail(usize),
    Border(Border),
    Menu(usize),
}

#[derive(Clone, Debug)]
pub enum Link {
    Url(String),
    Path(String),
}

impl Link {
    pub fn target(&self) -> &str {
        match self {
            Link::Url(s) | Link::Path(s) => s,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    ResumeTab,
    ResumeHere,
    ResumeAngreal,
    Transcript,
    CatchUp,
    ToggleDone,
    OpenFolder,
    OpenArtifact,
    Copy,
}

impl Action {
    pub const ALL: [Action; 9] = [
        Action::ResumeTab,
        Action::ResumeHere,
        Action::ResumeAngreal,
        Action::Transcript,
        Action::CatchUp,
        Action::ToggleDone,
        Action::OpenFolder,
        Action::OpenArtifact,
        Action::Copy,
    ];
    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            Action::ResumeTab => ("Resume in new tab", "↵"),
            Action::ResumeHere => ("Resume here", "R"),
            Action::ResumeAngreal => ("Resume in angreal", "A"),
            Action::Transcript => ("Transcript", "t"),
            Action::CatchUp => ("Catch me up", "c"),
            Action::ToggleDone => ("Mark done / undone", "d"),
            Action::OpenFolder => ("Open folder", "o"),
            Action::OpenArtifact => ("Open artifact", "a"),
            Action::Copy => ("Copy resume command", "y"),
        }
    }
}

pub struct Menu {
    pub x: u16,
    pub y: u16,
    pub sel: usize,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
    pub fresh: bool,
}

pub struct Project {
    pub cwd: String,
    pub name: String,
    pub count: usize,
    pub waiting: usize,
    pub live: usize,
    pub updated: i64,
}

/// Screen regions from the last draw, for mouse hit-testing.
#[derive(Default, Clone)]
pub struct Areas {
    pub top: Rect,
    pub search: Rect,
    pub body: Rect,
    pub left: Rect,
    pub left_fixed: Rect,
    pub left_list: Rect,
    pub mid: Rect,
    pub mid_list: Rect,
    pub right: Rect,
    pub right_text: Rect,
    pub menu: Option<Rect>,
    pub transcript: Rect,
    pub proj_offset: usize,
    pub sess_offset: usize,
}

pub struct App {
    indexer: Indexer,
    pub sessions: Vec<Session>,
    pub projects: Vec<Project>,
    /// 0 = waiting on me, 1 = all, 2.. = projects.
    pub left_sel: usize,
    prev_left: usize,
    pub visible: Vec<usize>,
    pub highlights: Vec<Vec<u32>>,
    pub sess_sel: usize,
    pub detail_scroll: usize,
    pub detail_rows: usize,
    pub links: Vec<(usize, Link)>,
    pub focus: Focus,
    pub view: View,
    pub help: bool,
    /// The start-up tips overlay.
    pub tips: bool,
    pub query: String,
    pub searching: bool,
    pub store: Store,
    pub toast: Option<Toast>,
    pub menu: Option<Menu>,
    pub hover: Option<Hover>,
    pub drag: Option<Border>,
    last_click: Option<(Instant, usize)>,
    pub areas: Areas,
    pub transcript: Option<Transcript>,
    pub pending: HashSet<String>,
    tx: Sender<summary::Done>,
    rx: Receiver<summary::Done>,
    /// Asks Jev whether sessions are waiting on you, when a key is set.
    judge: Option<Judge>,
    judged: waiting::Cache,
    pub fx: EffectManager<&'static str>,
    pub intro: bool,
    pub detail_fx: bool,
    pub view_fx: bool,
    pub quit: bool,
    /// Folder and command to resume once the board has closed.
    pub exec_on_exit: Option<(String, launch::Cmd)>,
    /// Whether angreal is installed, which adds "Resume in angreal" (`A`).
    pub angreal: bool,
    last_poll: Instant,
    pub now: i64,
    pub started: Instant,
    lower: Vec<String>,
    matcher: Matcher,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn clock(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%b %-d, %H:%M").to_string())
        .unwrap_or_default()
}

pub fn ago(ms: i64, now: i64) -> String {
    let secs = (now - ms).max(0) / 1000;
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => {
            let d = Local.timestamp_millis_opt(ms).single();
            let this_year = Local::now().format("%Y").to_string();
            d.map(|d| {
                if d.format("%Y").to_string() == this_year {
                    d.format("%b %-d").to_string()
                } else {
                    d.format("%Y-%m-%d").to_string()
                }
            })
            .unwrap_or_default()
        }
    }
}

pub fn tilde(path: &str) -> String {
    match dirs::home_dir() {
        Some(h) => {
            let h = h.to_string_lossy();
            if path == h {
                "~".into()
            } else if let Some(rest) = path.strip_prefix(&*h) {
                format!("~{rest}")
            } else {
                path.into()
            }
        }
        None => path.into(),
    }
}

impl App {
    pub fn new(indexer: Indexer) -> Self {
        let (tx, rx) = mpsc::channel();
        let sessions = indexer.sessions();
        let store = Store::load();
        let mut app = Self {
            indexer,
            sessions,
            projects: Vec::new(),
            left_sel: 1,
            prev_left: 1,
            visible: Vec::new(),
            highlights: Vec::new(),
            sess_sel: 0,
            detail_scroll: 0,
            detail_rows: 0,
            links: Vec::new(),
            focus: Focus::Sessions,
            view: View::Board,
            help: false,
            tips: !store.hide_tips,
            store,
            query: String::new(),
            searching: false,
            toast: None,
            menu: None,
            hover: None,
            drag: None,
            last_click: None,
            areas: Areas::default(),
            transcript: None,
            pending: HashSet::new(),
            tx,
            rx,
            judge: waiting::Config::from_env().map(Judge::spawn),
            judged: waiting::Cache::load(),
            fx: EffectManager::default(),
            intro: true,
            detail_fx: false,
            view_fx: false,
            quit: false,
            exec_on_exit: None,
            angreal: launch::on_path(launch::ANGREAL),
            last_poll: Instant::now(),
            now: now_ms(),
            started: Instant::now(),
            lower: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT),
        };
        app.rebuild();
        app.ask_judge();
        app
    }

    // ----- derived state -------------------------------------------------

    pub fn is_live(&self, s: &Session) -> bool {
        self.now - s.mtime < LIVE_WINDOW_MS
    }

    pub fn is_done(&self, s: &Session) -> bool {
        self.store.done.get(&s.id).is_some_and(|&t| s.updated <= t)
    }

    /// Claude spoke last and needs something from you, done or not.
    pub fn awaits(&self, s: &Session) -> bool {
        self.judged.awaits(s)
    }

    pub fn is_waiting(&self, s: &Session) -> bool {
        self.awaits(s) && !self.is_done(s) && !self.is_live(s) && self.now - s.updated < WAITING_MAX_AGE_MS
    }

    pub fn waiting_count(&self) -> usize {
        self.sessions.iter().filter(|s| self.is_waiting(s)).count()
    }

    pub fn live_count(&self) -> usize {
        self.sessions.iter().filter(|s| self.is_live(s)).count()
    }

    pub fn selected(&self) -> Option<&Session> {
        self.visible.get(self.sess_sel).map(|&i| &self.sessions[i])
    }

    pub fn scope_label(&self) -> String {
        match self.left_sel {
            0 => "Waiting on me".into(),
            1 => "All sessions".into(),
            n => self.projects.get(n - 2).map(|p| p.name.clone()).unwrap_or_default(),
        }
    }

    pub fn single_project(&self) -> bool {
        self.left_sel >= 2
    }

    pub fn project_name(&self, cwd: &str) -> &str {
        self.projects
            .iter()
            .find(|p| p.cwd == cwd)
            .map_or("", |p| p.name.as_str())
    }

    pub fn summary_for(&self, s: &Session) -> Option<&Summary> {
        self.store.summaries.get(&s.id)
    }

    fn rebuild(&mut self) {
        let keep_proj = self.projects.get(self.left_sel.wrapping_sub(2)).map(|p| p.cwd.clone());
        self.lower = self
            .sessions
            .iter()
            .map(|s| {
                format!(
                    "{} {} {} {} {}",
                    s.title,
                    s.first_prompt,
                    s.last_prompt,
                    s.last_reply,
                    s.files.join(" ")
                )
                .to_lowercase()
            })
            .collect();

        let mut by_cwd: HashMap<&str, Project> = HashMap::new();
        for s in &self.sessions {
            let waiting = self.is_waiting(s);
            let live = self.is_live(s);
            let p = by_cwd.entry(s.cwd.as_str()).or_insert_with(|| Project {
                cwd: s.cwd.clone(),
                name: String::new(),
                count: 0,
                waiting: 0,
                live: 0,
                updated: 0,
            });
            p.count += 1;
            p.waiting += waiting as usize;
            p.live += live as usize;
            p.updated = p.updated.max(s.updated);
        }
        let mut projects: Vec<Project> = by_cwd.into_values().collect();
        let base = |cwd: &str| -> String {
            let t = tilde(cwd);
            if t == "~" || t.is_empty() {
                return if t.is_empty() { "(unknown)".into() } else { t };
            }
            Path::new(cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(t)
        };
        let mut seen: HashMap<String, usize> = HashMap::new();
        for p in &projects {
            *seen.entry(base(&p.cwd)).or_default() += 1;
        }
        for p in &mut projects {
            let b = base(&p.cwd);
            p.name = if seen[&b] > 1 {
                let parent = Path::new(&p.cwd)
                    .parent()
                    .and_then(|x| x.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{parent}/{b}")
            } else {
                b
            };
        }
        projects.sort_by_key(|p| std::cmp::Reverse(p.updated));
        self.projects = projects;
        if let Some(cwd) = keep_proj {
            self.left_sel = self
                .projects
                .iter()
                .position(|p| p.cwd == cwd)
                .map_or(1, |i| i + 2);
        }
        self.left_sel = self.left_sel.min(self.projects.len() + 1);
        self.refilter();
    }

    pub fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.id.clone());
        let scope_cwd = self.projects.get(self.left_sel.wrapping_sub(2)).map(|p| p.cwd.clone());
        let in_scope = |app: &Self, s: &Session| match app.left_sel {
            0 => app.is_waiting(s),
            1 => true,
            _ => scope_cwd.as_deref() == Some(s.cwd.as_str()),
        };
        let base: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| in_scope(self, &self.sessions[i]))
            .collect();

        if self.query.trim().is_empty() {
            self.highlights = vec![Vec::new(); base.len()];
            self.visible = base;
        } else {
            let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
            let atoms: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, usize, Vec<u32>)> = Vec::new();
            for i in base {
                let s = &self.sessions[i];
                let hay = format!("{} {}", s.title, self.project_name(&s.cwd));
                let head = pattern.score(Utf32Str::new(&hay, &mut buf), &mut self.matcher);
                let score = match head {
                    Some(sc) => Some(sc + 10_000),
                    None => atoms.iter().all(|a| self.lower[i].contains(a.as_str())).then_some(1),
                };
                if let Some(sc) = score {
                    let mut idx = Vec::new();
                    pattern.indices(Utf32Str::new(&s.title, &mut buf), &mut self.matcher, &mut idx);
                    idx.sort_unstable();
                    idx.dedup();
                    scored.push((sc, i, idx));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(self.sessions[b.1].updated.cmp(&self.sessions[a.1].updated)));
            self.visible = scored.iter().map(|x| x.1).collect();
            self.highlights = scored.into_iter().map(|x| x.2).collect();
        }
        let pos = keep.and_then(|id| self.visible.iter().position(|&i| self.sessions[i].id == id));
        match pos {
            Some(p) => self.sess_sel = p,
            None => {
                self.sess_sel = 0;
                self.detail_scroll = 0;
                self.detail_fx = true;
            }
        }
    }

    // ----- ticking -------------------------------------------------------

    pub fn animating(&self) -> bool {
        self.fx.is_running()
            || self.toast.is_some()
            || !self.pending.is_empty()
            || self.drag.is_some()
            || self.sessions.iter().take(40).any(|s| self.is_live(s))
    }

    pub fn tick(&mut self) {
        self.now = now_ms();
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_millis(2800)) {
            self.toast = None;
        }
        while let Ok(done) = self.rx.try_recv() {
            self.pending.remove(&done.id);
            match done.result {
                Ok(text) => {
                    self.store.summaries.insert(
                        done.id.clone(),
                        Summary { updated: done.updated, made: now_ms(), text },
                    );
                    self.store.save();
                    if self.selected().is_some_and(|s| s.id == done.id) {
                        self.detail_scroll = 0;
                        self.detail_fx = true;
                    }
                    self.notify("Summary ready");
                }
                Err(e) => self.error(format!("Summary failed: {}", crate::index::one_line(&e, 80))),
            }
        }
        self.take_judgments();
        if self.last_poll.elapsed() >= POLL_EVERY {
            self.last_poll = Instant::now();
            if self.indexer.refresh().unwrap_or(false) {
                self.indexer.save();
                self.sessions = self.indexer.sessions();
                self.rebuild();
            }
            self.ask_judge();
        }
    }

    /// Sends Jev each recent session that ended on Claude's reply and has no
    /// judgment for its latest activity. Live sessions wait until they settle.
    fn ask_judge(&mut self) {
        if self.judge.is_none() {
            return;
        }
        let want: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                let s = &self.sessions[i];
                s.ends_with_reply
                    && !self.is_live(s)
                    && !self.is_done(s)
                    && self.now - s.updated < WAITING_MAX_AGE_MS
                    && self.judged.get(s).is_none()
            })
            .collect();
        if let Some(judge) = &mut self.judge {
            for i in want {
                judge.ask(&self.sessions[i]);
            }
        }
    }

    fn take_judgments(&mut self) {
        let Some(judge) = &self.judge else { return };
        let done = judge.drain();
        if done.is_empty() {
            return;
        }
        let mut changed = false;
        for d in done {
            match d.result {
                Ok(p) => {
                    self.judged.insert(d.id, Judgment { updated: d.updated, p });
                    changed = true;
                }
                Err(Failure::Refused(e)) => {
                    self.judge = None;
                    self.error(format!("Jev refused the request ({e}); using question marks"));
                }
                // Left unjudged until the session changes, so it keeps the question-mark check.
                Err(Failure::Other(e)) => self.error(format!("Jev failed: {}", crate::index::one_line(&e, 70))),
            }
        }
        if changed {
            self.judged.prune(self.now - WAITING_MAX_AGE_MS);
            self.judged.save();
            self.rebuild();
        }
    }

    pub fn notify(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: false, at: Instant::now(), fresh: true });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: true, at: Instant::now(), fresh: true });
    }

    // ----- movement ------------------------------------------------------

    fn set_left(&mut self, n: usize) {
        let n = n.min(self.projects.len() + 1);
        if n != self.left_sel {
            self.left_sel = n;
            self.sess_sel = 0;
            self.detail_scroll = 0;
            self.detail_fx = true;
            self.refilter();
        }
    }

    fn set_sess(&mut self, n: usize) {
        let n = n.min(self.visible.len().saturating_sub(1));
        if n != self.sess_sel {
            self.sess_sel = n;
            self.detail_scroll = 0;
            self.detail_fx = true;
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        let page = self.areas.right_text.height as usize;
        let max = self.detail_rows.saturating_sub(page);
        self.detail_scroll = (self.detail_scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn step(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                let n = (self.left_sel as isize + delta).clamp(0, self.projects.len() as isize + 1);
                self.set_left(n as usize);
            }
            Focus::Sessions => {
                let n = (self.sess_sel as isize + delta).max(0);
                self.set_sess(n as usize);
            }
            Focus::Detail => self.scroll_detail(delta),
        }
    }

    fn jump(&mut self, end: bool) {
        match self.focus {
            Focus::Projects => self.set_left(if end { usize::MAX } else { 0 }),
            Focus::Sessions => self.set_sess(if end { usize::MAX } else { 0 }),
            Focus::Detail => self.scroll_detail(if end { isize::MAX / 2 } else { isize::MIN / 2 }),
        }
    }

    // ----- actions -------------------------------------------------------

    /// Whether "Resume in angreal" applies: it is installed and the selected
    /// session is Claude Code's, since angreal is a Claude Code front end.
    pub fn angreal_here(&self) -> bool {
        self.angreal && self.selected().is_some_and(|s| s.agent == Agent::Claude)
    }

    /// The actions offered in the menu, leaving out angreal when it does not apply.
    pub fn actions(&self) -> Vec<Action> {
        let angreal = self.angreal_here();
        Action::ALL.into_iter().filter(|&a| a != Action::ResumeAngreal || angreal).collect()
    }

    pub fn act(&mut self, a: Action) {
        let Some(s) = self.selected().cloned() else {
            self.error("No session selected");
            return;
        };
        match a {
            Action::ResumeTab | Action::ResumeHere | Action::ResumeAngreal => {
                if !Path::new(&s.cwd).is_dir() {
                    self.error(format!("Folder is gone: {}", tilde(&s.cwd)));
                    return;
                }
                let bin = (a == Action::ResumeAngreal).then(|| launch::ANGREAL.to_string());
                let cmd = launch::Cmd::resume(s.agent, &s.id, bin);
                if a == Action::ResumeHere {
                    self.exec_on_exit = Some((s.cwd, cmd));
                    self.quit = true;
                    return;
                }
                match launch::resume_in_new_tab(&s.cwd, &cmd) {
                    Ok(Resume::Opened(term)) => self.notify(format!("Resumed in a new {term} tab")),
                    Ok(Resume::Here) => {
                        self.exec_on_exit = Some((s.cwd, cmd));
                        self.quit = true;
                    }
                    Err(e) => self.error(format!(
                        "Could not open a tab ({}). Press R to resume here.",
                        crate::index::one_line(&e, 60)
                    )),
                }
            }
            Action::Transcript => match Transcript::load(s.agent, &s.path, &s.title) {
                Ok(t) => {
                    self.transcript = Some(t);
                    self.view = View::Transcript;
                    self.view_fx = true;
                }
                Err(e) => self.error(format!("Could not read transcript: {e}")),
            },
            Action::CatchUp => {
                if self.pending.contains(&s.id) {
                    self.notify("Already summarizing this one");
                } else if self.summary_for(&s).is_some_and(|x| x.updated >= s.updated) {
                    self.detail_scroll = 0;
                    self.notify("Summary is up to date");
                } else {
                    self.pending.insert(s.id.clone());
                    summary::spawn(s.id.clone(), s.updated, s.agent, s.path.clone(), self.tx.clone());
                    self.detail_scroll = 0;
                    self.notify("Asking Claude for a summary");
                }
            }
            Action::ToggleDone => {
                if self.is_done(&s) {
                    self.store.done.remove(&s.id);
                    self.notify("Marked not done");
                } else {
                    self.store.done.insert(s.id.clone(), s.updated);
                    self.notify("Marked done");
                }
                self.store.save();
                self.rebuild();
            }
            Action::OpenFolder => self.open(&s.cwd.clone()),
            Action::OpenArtifact => match s.artifacts.last() {
                Some(a) => self.open(&a.url.clone()),
                None => self.notify("No artifacts in this session"),
            },
            Action::Copy => match launch::copy(&launch::resume_command(s.agent, &s.cwd, &s.id)) {
                Ok(()) => self.notify("Copied resume command"),
                Err(e) => self.error(format!("Copy failed: {e}")),
            },
        }
    }

    fn open(&mut self, target: &str) {
        if let Err(e) = launch::open(target) {
            self.error(e);
        }
    }

    fn toggle_waiting(&mut self) {
        if self.left_sel == 0 {
            let back = self.prev_left;
            self.set_left(back);
        } else {
            self.prev_left = self.left_sel;
            self.set_left(0);
        }
    }

    fn open_menu_at_selection(&mut self) {
        let a = &self.areas;
        let row = self.sess_sel.saturating_sub(a.sess_offset) as u16 * 2;
        self.menu = Some(Menu { x: a.mid_list.x + 4, y: a.mid_list.y + row + 1, sel: 0 });
    }

    // ----- keyboard ------------------------------------------------------

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.tips {
            self.tips = false;
            if k.code == KeyCode::Char('x') {
                self.store.hide_tips = true;
                self.store.save();
                self.notify("Tips hidden. Press ? for every key");
            }
            return;
        }
        let actions = self.actions();
        let n = actions.len();
        if let Some(menu) = &mut self.menu {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => menu.sel = menu.sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => menu.sel = (menu.sel + 1).min(n - 1),
                KeyCode::Enter => {
                    let a = actions[menu.sel];
                    self.menu = None;
                    self.act(a);
                }
                _ => self.menu = None,
            }
            return;
        }
        match self.view {
            View::Transcript => return self.on_key_transcript(k),
            View::Stats => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s')) {
                    self.view = View::Board;
                    self.view_fx = true;
                }
                return;
            }
            View::Board => {}
        }
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.refilter();
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Tab => self.searching = false,
                KeyCode::Backspace => {
                    if self.query.pop().is_none() {
                        self.searching = false;
                    }
                    self.refilter();
                    self.set_sess(0);
                }
                KeyCode::Char('u') if ctrl => {
                    self.query.clear();
                    self.refilter();
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.refilter();
                    self.set_sess(0);
                }
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refilter();
                }
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('/') => {
                self.searching = true;
                self.focus = Focus::Sessions;
            }
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    Focus::Projects => Focus::Sessions,
                    _ => Focus::Detail,
                }
            }
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.focus = match self.focus {
                    Focus::Detail => Focus::Sessions,
                    _ => Focus::Projects,
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('d') if ctrl => self.scroll_detail(self.areas.right_text.height as isize / 2),
            KeyCode::Char('u') if ctrl => self.scroll_detail(-(self.areas.right_text.height as isize / 2)),
            KeyCode::PageDown => self.step(10),
            KeyCode::PageUp => self.step(-10),
            KeyCode::Char('J') => self.scroll_detail(3),
            KeyCode::Char('K') => self.scroll_detail(-3),
            KeyCode::Char('g') | KeyCode::Home => self.jump(false),
            KeyCode::Char('G') | KeyCode::End => self.jump(true),
            KeyCode::Enter => {
                if self.focus == Focus::Projects {
                    self.focus = Focus::Sessions;
                } else {
                    self.act(Action::ResumeTab);
                }
            }
            KeyCode::Char('R') => self.act(Action::ResumeHere),
            KeyCode::Char('A') if self.angreal_here() => self.act(Action::ResumeAngreal),
            KeyCode::Char('w') => self.toggle_waiting(),
            KeyCode::Char('d') => self.act(Action::ToggleDone),
            KeyCode::Char('o') => self.act(Action::OpenFolder),
            KeyCode::Char('a') => self.act(Action::OpenArtifact),
            KeyCode::Char('y') => self.act(Action::Copy),
            KeyCode::Char('t') => self.act(Action::Transcript),
            KeyCode::Char('c') => self.act(Action::CatchUp),
            KeyCode::Char('m') => self.open_menu_at_selection(),
            KeyCode::Char('s') => {
                self.view = View::Stats;
                self.view_fx = true;
            }
            KeyCode::Char('r') => {
                if let Ok(fresh) = Indexer::load(true) {
                    self.indexer = fresh;
                    self.sessions = self.indexer.sessions();
                    self.rebuild();
                    self.notify(format!("Reindexed {} sessions", self.sessions.len()));
                }
            }
            _ => {}
        }
    }

    fn on_key_transcript(&mut self, k: KeyEvent) {
        let page = self.areas.transcript.height as usize;
        let Some(t) = &mut self.transcript else {
            self.view = View::Board;
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => {
                self.view = View::Board;
                self.view_fx = true;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => t.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => t.scroll = t.scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => t.scroll += page / 2,
            KeyCode::Char('u') if ctrl => t.scroll = t.scroll.saturating_sub(page / 2),
            KeyCode::Char(' ') | KeyCode::PageDown => t.scroll += page.saturating_sub(2),
            KeyCode::PageUp => t.scroll = t.scroll.saturating_sub(page.saturating_sub(2)),
            KeyCode::Char('g') | KeyCode::Home => t.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => t.scroll = usize::MAX / 2,
            KeyCode::Char('e') => t.toggle_all(),
            KeyCode::Char('n') => {
                if let Some(&r) = t.prompt_rows.iter().find(|&&r| r > t.scroll) {
                    t.scroll = r;
                }
            }
            KeyCode::Char('p') => {
                if let Some(&r) = t.prompt_rows.iter().rev().find(|&&r| r < t.scroll) {
                    t.scroll = r;
                }
            }
            KeyCode::Enter => {
                self.view = View::Board;
                self.act(Action::ResumeTab);
                return;
            }
            _ => {}
        }
        t.clamp(page as u16);
    }

    // ----- mouse ---------------------------------------------------------

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        if self.help || self.tips {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
                self.tips = false;
            }
            return;
        }
        match self.view {
            View::Transcript => return self.on_mouse_transcript(m),
            View::Stats => {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    self.view = View::Board;
                    self.view_fx = true;
                }
                return;
            }
            View::Board => {}
        }
        if let Some(rect) = self.areas.menu {
            let inside = hit(rect, x, y);
            let item = inside
                .then(|| (y - rect.y) as usize)
                .and_then(|r| r.checked_sub(1))
                .filter(|&r| r < self.actions().len());
            match m.kind {
                MouseEventKind::Moved => {
                    if let (Some(i), Some(menu)) = (item, self.menu.as_mut()) {
                        menu.sel = i;
                        self.hover = Some(Hover::Menu(i));
                    }
                }
                MouseEventKind::Down(_) => {
                    self.menu = None;
                    if let Some(i) = item {
                        let a = self.actions()[i];
                        self.act(a);
                    }
                }
                _ => {}
            }
            if self.menu.is_some() || matches!(m.kind, MouseEventKind::Down(_)) {
                return;
            }
        }

        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(b) = self.border_at(x, y) {
                    self.drag = Some(b);
                    return;
                }
                if hit(self.areas.search, x, y) || (hit(self.areas.top, x, y) && self.searching) {
                    self.searching = true;
                    self.focus = Focus::Sessions;
                    return;
                }
                self.searching = false;
                match self.target_at(x, y) {
                    Some(Hover::Left(i)) => {
                        self.focus = Focus::Projects;
                        self.set_left(i);
                    }
                    Some(Hover::Session(i)) => {
                        self.focus = Focus::Sessions;
                        let on_dot = x <= self.areas.mid_list.x + 2;
                        let double = self
                            .last_click
                            .is_some_and(|(t, j)| j == i && t.elapsed() < DOUBLE_CLICK);
                        self.set_sess(i);
                        if on_dot && self.selected().is_some_and(|s| self.awaits(s)) {
                            self.act(Action::ToggleDone);
                            self.last_click = None;
                        } else if double {
                            self.last_click = None;
                            self.act(Action::ResumeTab);
                        } else {
                            self.last_click = Some((Instant::now(), i));
                        }
                    }
                    Some(Hover::Detail(row)) => {
                        self.focus = Focus::Detail;
                        let link = self.links.iter().find(|(r, _)| *r == row).map(|(_, l)| l.clone());
                        if let Some(l) = link {
                            self.open(l.target());
                        }
                    }
                    _ => {}
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(Hover::Session(i)) = self.target_at(x, y) {
                    self.focus = Focus::Sessions;
                    self.set_sess(i);
                    self.menu = Some(Menu { x, y, sel: 0 });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(b) = self.drag {
                    self.resize(b, x);
                }
            }
            MouseEventKind::Up(_) => {
                if self.drag.take().is_some() {
                    self.store.save();
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                if hit(self.areas.left, x, y) {
                    let n = (self.left_sel as isize + d).clamp(0, self.projects.len() as isize + 1);
                    self.set_left(n as usize);
                } else if hit(self.areas.mid, x, y) {
                    let n = (self.sess_sel as isize + d).max(0);
                    self.set_sess(n as usize);
                } else if hit(self.areas.right, x, y) {
                    self.scroll_detail(d * 3);
                }
            }
            MouseEventKind::Moved => {
                self.hover = self.border_at(x, y).map(Hover::Border).or_else(|| self.target_at(x, y));
            }
            _ => {}
        }
    }

    fn on_mouse_transcript(&mut self, m: MouseEvent) {
        let area = self.areas.transcript;
        let Some(t) = &mut self.transcript else { return };
        match m.kind {
            MouseEventKind::ScrollDown => t.scroll += 3,
            MouseEventKind::ScrollUp => t.scroll = t.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) if hit(area, m.column, m.row) => {
                let row = t.scroll + (m.row - area.y) as usize;
                if let Some(Some(e)) = t.row_entry.get(row).copied() {
                    t.toggle(e);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.view = View::Board;
                self.view_fx = true;
                return;
            }
            _ => {}
        }
        t.clamp(area.height);
    }

    fn border_at(&self, x: u16, y: u16) -> Option<Border> {
        let a = &self.areas;
        if y <= a.body.y || y + 1 >= a.body.bottom() {
            return None;
        }
        if x + 1 == a.left.right() || x == a.mid.x {
            Some(Border::Left)
        } else if x + 1 == a.mid.right() || x == a.right.x {
            Some(Border::Right)
        } else {
            None
        }
    }

    fn resize(&mut self, b: Border, x: u16) {
        let body = self.areas.body;
        let (left, right) = (self.areas.left.width, self.areas.right.width);
        match b {
            Border::Left => {
                let max = body.width.saturating_sub(right + 30).max(18);
                self.store.left = Some((x.saturating_sub(body.x) + 1).clamp(18, max));
            }
            Border::Right => {
                let max = body.width.saturating_sub(left + 30).max(28);
                self.store.right = Some((body.right().saturating_sub(x)).clamp(28, max));
            }
        }
    }

    fn target_at(&self, x: u16, y: u16) -> Option<Hover> {
        let a = &self.areas;
        if hit(a.left_fixed, x, y) {
            let r = (y - a.left_fixed.y) as usize;
            return (r < 2).then_some(Hover::Left(r));
        }
        if hit(a.left_list, x, y) {
            let i = a.proj_offset + (y - a.left_list.y) as usize;
            return (i < self.projects.len()).then_some(Hover::Left(i + 2));
        }
        if hit(a.mid_list, x, y) {
            let i = a.sess_offset + (y - a.mid_list.y) as usize / 2;
            return (i < self.visible.len()).then_some(Hover::Session(i));
        }
        if hit(a.right_text, x, y) {
            return Some(Hover::Detail(self.detail_scroll + (y - a.right_text.y) as usize));
        }
        None
    }
}

pub fn hit(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
