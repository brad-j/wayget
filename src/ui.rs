//! Drawing. Records hit-test regions back into `app.areas` as it goes.

use std::{collections::HashMap, f32::consts::TAU, time::Duration};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Bar, BarChart, BarGroup, Block as TuiBlock, BorderType, Clear, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
};
use tachyonfx::{Interpolation, fx};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{Action, App, Border, Focus, Hover, Link, View, ago, clock, tilde},
    index::{Session, one_line},
    markdown::{self, Block},
    theme,
    transcript::tool_label,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App, elapsed: Duration) {
    let area = f.area();
    let [top, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    app.areas.top = top;
    app.areas.body = body;
    app.areas.menu = None;

    draw_top(f, app, top);
    match app.view {
        View::Board => draw_board(f, app, body),
        View::Transcript => draw_transcript(f, app, body),
        View::Stats => draw_stats(f, app, body),
    }
    draw_status(f, app, status);
    if app.menu.is_some() {
        draw_menu(f, app, area);
    }
    let toast_rect = draw_toast(f, app, area);
    if app.tips && app.view == View::Board {
        draw_tips(f, area);
    }
    if app.help {
        draw_help(f, area);
    }

    // Effects run over whatever was drawn this frame.
    if app.intro {
        app.intro = false;
        let a = &app.areas;
        let pane = |r: Rect, delay: u32| {
            fx::delay(delay, fx::coalesce((420, Interpolation::CubicOut))).with_area(r)
        };
        app.fx.add_effect(fx::parallel(&[
            pane(a.left, 0),
            pane(a.mid, 70),
            pane(a.right, 140),
            fx::fade_from_fg(theme::FADE_FROM, (500, Interpolation::QuadOut)).with_area(top),
        ]));
    }
    if app.view_fx {
        app.view_fx = false;
        app.fx.add_unique_effect("view", fx::coalesce((240, Interpolation::QuadOut)).with_area(body));
    }
    if app.detail_fx {
        app.detail_fx = false;
        app.fx.add_unique_effect(
            "detail",
            fx::fade_from_fg(theme::FADE_FROM, (220, Interpolation::QuadOut)).with_area(app.areas.right_text),
        );
    }
    if let (Some(t), Some(r)) = (app.toast.as_mut(), toast_rect)
        && t.fresh {
            t.fresh = false;
            app.fx.add_unique_effect(
                "toast",
                fx::fade_from_fg(theme::MENU_BG, (260, Interpolation::QuadOut)).with_area(r),
            );
        }
    app.fx.process_effects(elapsed.into(), f.buffer_mut(), area);
}

// ---------------------------------------------------------------------------
// Chrome

fn draw_top(f: &mut Frame, app: &mut App, area: Rect) {
    let waiting = app.waiting_count();
    let live = app.live_count();
    let mut left = vec![
        Span::styled(" ◆ wayget ", theme::label()),
        Span::styled(format!("  {} sessions", app.sessions.len()), theme::dim()),
    ];
    if waiting > 0 {
        left.push(Span::styled(" · ", theme::faint()));
        left.push(Span::styled(format!("{waiting} waiting"), Style::new().fg(theme::WAIT)));
    }
    if live > 0 {
        left.push(Span::styled(" · ", theme::faint()));
        left.push(Span::styled(format!("{live} live"), Style::new().fg(pulse(app))));
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);

    let right: Vec<Span> = if app.searching || !app.query.is_empty() {
        let cursor = if app.searching && (app.started.elapsed().as_millis() / 530).is_multiple_of(2) { "▏" } else { " " };
        vec![
            Span::styled(" / ", theme::accent()),
            Span::styled(app.query.clone(), theme::bright()),
            Span::styled(cursor, theme::accent()),
            Span::styled(format!(" {} ", app.visible.len()), theme::faint()),
        ]
    } else {
        vec![Span::styled(" / search   ? help ", theme::faint())]
    };
    let w = right.iter().map(|s| s.width() as u16).sum::<u16>().max(24).min(area.width / 2);
    let r = Rect { x: area.right() - w, y: area.y, width: w, height: 1 };
    app.areas.search = r;
    f.render_widget(Paragraph::new(Line::from(right)).right_aligned(), r);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let hints: &[(&str, &str)] = if app.searching {
        &[("type", "to filter"), ("↵", "keep"), ("esc", "clear")]
    } else {
        match app.view {
            View::Board => &[
                ("↵", "resume"),
                ("R", "here"),
                ("/", "search"),
                ("w", "waiting"),
                ("d", "done"),
                ("t", "transcript"),
                ("c", "catch up"),
                ("s", "stats"),
                ("m", "menu"),
                ("?", "help"),
                ("q", "quit"),
            ],
            View::Transcript => &[
                ("j/k", "scroll"),
                ("space", "page"),
                ("n/p", "next/prev prompt"),
                ("e", "expand all"),
                ("click", "expand a tool"),
                ("↵", "resume"),
                ("esc", "back"),
            ],
            View::Stats => &[("esc", "back")],
        }
    };
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in hints {
        spans.push(Span::styled(*k, theme::accent()));
        spans.push(Span::styled(format!(" {d}   "), theme::faint()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pane(title: &str, focused: bool, border_hot: bool) -> TuiBlock<'static> {
    let color = if border_hot {
        theme::ACCENT
    } else if focused {
        theme::lerp(theme::FAINT, theme::ACCENT, 0.75)
    } else {
        theme::FAINT
    };
    let title_style = if focused { theme::label() } else { theme::dim() };
    TuiBlock::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(Span::styled(format!(" {title} "), title_style)))
}

fn pulse(app: &App) -> Color {
    let t = app.started.elapsed().as_secs_f32();
    theme::lerp(theme::LIVE_DIM, theme::LIVE, ((t * TAU / 1.6).sin() + 1.0) / 2.0)
}

// ---------------------------------------------------------------------------
// Board

fn draw_board(f: &mut Frame, app: &mut App, body: Rect) {
    let show_left = body.width >= 90;
    let left_w = if show_left {
        app.store.left.unwrap_or(40).clamp(18, body.width.saturating_sub(60).max(18))
    } else {
        0
    };
    let right_w = app
        .store
        .right
        .unwrap_or(body.width * 45 / 100)
        .clamp(28, body.width.saturating_sub(left_w + 30).max(28));
    let [left, mid, right] = Layout::horizontal([
        Constraint::Length(left_w),
        Constraint::Min(30),
        Constraint::Length(right_w),
    ])
    .areas(body);
    app.areas.left = left;
    app.areas.mid = mid;
    app.areas.right = right;

    let hot = |b: Border| app.drag == Some(b) || app.hover == Some(Hover::Border(b));
    let (hot_l, hot_r) = (hot(Border::Left), hot(Border::Right));
    if show_left {
        draw_projects(f, app, left, hot_l);
    } else {
        app.areas.left_fixed = Rect::default();
        app.areas.left_list = Rect::default();
    }
    draw_sessions(f, app, mid, hot_l || hot_r);
    draw_detail(f, app, right, hot_r);
    if show_left {
        draw_grip(f, mid.x, body, hot_l);
    }
    draw_grip(f, right.x, body, hot_r);
}

/// A short dotted stretch on a draggable border, so it reads as a handle.
/// Drawn on the left border of the pane to its right, which never carries a
/// scrollbar.
fn draw_grip(f: &mut Frame, x: u16, body: Rect, hot: bool) {
    if body.height < 8 {
        return;
    }
    let color = if hot { theme::ACCENT } else { theme::DIM };
    let mid = body.y + body.height / 2;
    for y in mid - 1..=mid + 1 {
        if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
            cell.set_char('┇').set_fg(color);
        }
    }
}

fn row_style(selected: bool, hovered: bool) -> Style {
    if selected {
        Style::new().bg(theme::SEL_BG)
    } else if hovered {
        Style::new().bg(theme::HOVER_BG)
    } else {
        Style::new()
    }
}

fn draw_projects(f: &mut Frame, app: &mut App, area: Rect, hot: bool) {
    let focused = app.focus == Focus::Projects;
    let block = pane("Projects", focused, hot);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }
    let fixed = Rect { height: 2, ..inner };
    let list = Rect { y: inner.y + 3, height: inner.height - 3, ..inner };
    app.areas.left_fixed = fixed;
    app.areas.left_list = list;

    let hover = app.hover;

    let waiting = app.waiting_count();
    let rows = [
        (Span::styled("●", Style::new().fg(if waiting > 0 { theme::WAIT } else { theme::FAINT })), "Waiting on me", waiting),
        (Span::styled("◇", theme::dim()), "All sessions", app.sessions.len()),
    ];
    for (i, (icon, name, count)) in rows.into_iter().enumerate() {
        let line = left_row(icon, name, count, inner.width, app.left_sel == i, false);
        let r = Rect { y: fixed.y + i as u16, height: 1, ..fixed };
        f.render_widget(
            Paragraph::new(line).style(row_style(app.left_sel == i, hover == Some(Hover::Left(i)))),
            r,
        );
    }
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(inner.width as usize), theme::faint())),
        Rect { y: inner.y + 2, height: 1, ..inner },
    );

    let h = list.height as usize;
    let sel = app.left_sel.checked_sub(2);
    let mut off = app.areas.proj_offset;
    if let Some(s) = sel {
        if s < off {
            off = s;
        } else if s >= off + h {
            off = s + 1 - h;
        }
    }
    off = off.min(app.projects.len().saturating_sub(h));
    app.areas.proj_offset = off;

    for (row, (i, p)) in app.projects.iter().enumerate().skip(off).take(h).enumerate() {
        let icon = if p.live > 0 {
            Span::styled("●", Style::new().fg(pulse(app)))
        } else if p.waiting > 0 {
            Span::styled("●", Style::new().fg(theme::WAIT))
        } else {
            Span::raw(" ")
        };
        let selected = sel == Some(i);
        let line = left_row(icon, &p.name, p.count, inner.width, selected, false);
        let r = Rect { y: list.y + row as u16, height: 1, ..list };
        f.render_widget(
            Paragraph::new(line).style(row_style(selected, hover == Some(Hover::Left(i + 2)))),
            r,
        );
    }
}

fn left_row(
    icon: Span<'static>,
    name: &str,
    count: usize,
    width: u16,
    selected: bool,
    _hover: bool,
) -> Line<'static> {
    let count_s = format!("{count:>3} ");
    let name_w = (width as usize).saturating_sub(3 + count_s.width());
    let name_style = if selected { theme::bright() } else { theme::text() };
    let mut spans = vec![Span::raw(" "), icon, Span::raw(" "), Span::styled(pad(&trunc(name, name_w), name_w), name_style)];
    spans.push(Span::styled(count_s, theme::faint()));
    Line::from(spans)
}

fn draw_sessions(f: &mut Frame, app: &mut App, area: Rect, hot: bool) {
    let focused = app.focus == Focus::Sessions;
    let title = format!("{} · {}", app.scope_label(), app.visible.len());
    let block = pane(&title, focused, hot);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.areas.mid_list = inner;

    if app.visible.is_empty() {
        let msg = if !app.query.is_empty() {
            "No sessions match."
        } else if app.left_sel == 0 {
            "Nothing is waiting on you."
        } else {
            "No sessions yet."
        };
        f.render_widget(
            Paragraph::new(vec![Line::default(), Line::from(Span::styled(format!("  {msg}"), theme::dim()))]),
            inner,
        );
        return;
    }

    let rows = (inner.height / 2).max(1) as usize;
    let mut off = app.areas.sess_offset;
    if app.sess_sel < off {
        off = app.sess_sel;
    } else if app.sess_sel >= off + rows {
        off = app.sess_sel + 1 - rows;
    }
    off = off.min(app.visible.len().saturating_sub(rows));
    app.areas.sess_offset = off;

    let w = inner.width as usize;
    let show_project = !app.single_project();
    for (row, vi) in (off..app.visible.len()).take(rows).enumerate() {
        let s = &app.sessions[app.visible[vi]];
        let selected = vi == app.sess_sel;
        let hovered = app.hover == Some(Hover::Session(vi));
        let bar = if selected {
            Span::styled("▌", Style::new().fg(if focused { theme::ACCENT } else { theme::DIM }))
        } else {
            Span::raw(" ")
        };
        let dot = if app.is_live(s) {
            Span::styled("●", Style::new().fg(pulse(app)))
        } else if app.is_waiting(s) {
            Span::styled("●", Style::new().fg(theme::WAIT))
        } else if app.awaits(s) && app.is_done(s) {
            Span::styled("✓", theme::faint())
        } else {
            Span::raw(" ")
        };
        let when = ago(s.updated, app.now);
        let title_w = w.saturating_sub(4 + when.width() + 1);
        let mut l1 = vec![bar.clone(), dot, Span::raw(" ")];
        let title_style = if selected { theme::bright() } else { theme::text() };
        let hl = app.highlights.get(vi).map(Vec::as_slice).unwrap_or(&[]);
        let (title_spans, used) = highlighted(&s.title, hl, title_w, title_style);
        l1.extend(title_spans);
        l1.push(Span::raw(" ".repeat(title_w.saturating_sub(used) + 1)));
        l1.push(Span::styled(when, theme::faint()));

        let mut meta = String::new();
        if show_project {
            meta.push_str(app.project_name(&s.cwd));
            meta.push_str(" · ");
        }
        meta.push_str(&one_line(&s.last_prompt, 200));
        let l2 = vec![
            bar,
            Span::raw("  "),
            Span::styled(trunc(&meta, w.saturating_sub(4)), theme::dim()),
        ];
        let r = Rect { y: inner.y + row as u16 * 2, height: 2, ..inner };
        f.render_widget(
            Paragraph::new(vec![Line::from(l1), Line::from(l2)]).style(row_style(selected, hovered)),
            r,
        );
    }

    if app.visible.len() > rows {
        let mut st = ScrollbarState::new(app.visible.len().saturating_sub(rows)).position(off);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(theme::dim()),
            Rect { y: area.y + 1, height: area.height.saturating_sub(2), ..area },
            &mut st,
        );
    }
}

fn section(label: &str, width: u16) -> Block {
    let text = format!("{label} ");
    let rest = (width as usize).saturating_sub(text.width());
    Block::new(vec![
        Span::styled(text, theme::dim().add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(rest), theme::faint()),
    ])
}

fn detail_blocks(app: &App, s: &Session, width: u16) -> Vec<(Block, Option<Link>)> {
    let mut out: Vec<(Block, Option<Link>)> = Vec::new();
    let mut push = |b: Block| out.push((b, None));

    push(Block::new(vec![Span::styled(s.title.clone(), theme::bright())]));
    let mut chips = Vec::new();
    if app.is_live(s) {
        chips.push(Span::styled("● live", Style::new().fg(pulse(app))));
    } else if app.is_waiting(s) {
        chips.push(Span::styled("● waiting on you", Style::new().fg(theme::WAIT)));
    } else if app.awaits(s) && app.is_done(s) {
        chips.push(Span::styled("✓ done", theme::dim()));
    }
    let mut meta = vec![Span::styled(tilde(&s.cwd), theme::dim())];
    if !chips.is_empty() {
        meta.push(Span::styled("   ", theme::faint()));
        meta.extend(chips);
    }
    push(Block::new(meta));
    let cost = if s.cost > 0.0 { format!(" · ${:.2}", s.cost) } else { String::new() };
    let agent = match s.agent {
        crate::index::Agent::Claude => "Claude Code",
        crate::index::Agent::Pi => "pi",
    };
    push(Block::new(vec![Span::styled(
        format!(
            "{agent} · {} prompts{} · started {} · updated {}",
            s.prompts,
            cost,
            clock(s.started),
            ago(s.updated, app.now)
        ),
        theme::faint(),
    )]));
    push(Block::blank());

    let pending = app.pending.contains(&s.id);
    if let Some(sum) = app.summary_for(s).filter(|_| !pending) {
        push(section("catch me up", width));
        for l in sum.text.lines().filter(|l| !l.trim().is_empty()) {
            let l = l.trim();
            let spans = match l.split_once(':') {
                Some((k, v)) if k.len() <= 6 => vec![
                    Span::styled(format!("{k}:"), theme::accent().add_modifier(Modifier::BOLD)),
                    Span::styled(v.to_string(), theme::text()),
                ],
                _ => markdown::inline(l, theme::text()),
            };
            push(Block::new(spans).indent(2));
        }
        if sum.updated < s.updated {
            push(Block::new(vec![Span::styled(
                "Written before the latest activity. Press c to refresh.",
                theme::faint().add_modifier(Modifier::ITALIC),
            )]));
        }
        push(Block::blank());
    } else if pending {
        let frame = SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
        push(section("catch me up", width));
        push(Block::new(vec![
            Span::styled(format!("{frame} "), theme::accent()),
            Span::styled("Asking Claude for a summary…", theme::dim()),
        ]));
        push(Block::blank());
    }

    push(section("where it left off", width));
    if s.last_reply.trim().is_empty() {
        push(Block::new(vec![Span::styled("No reply yet.", theme::faint())]));
    } else {
        for b in markdown::render(&s.last_reply) {
            push(b);
        }
    }
    push(Block::blank());

    let quote = |text: &str, max: usize| -> Vec<Block> {
        let text = if text.chars().count() > max {
            format!("{}…", text.chars().take(max).collect::<String>())
        } else {
            text.to_string()
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                Block::new(vec![
                    Span::styled("▎ ", theme::accent()),
                    Span::styled(l.trim().to_string(), theme::text().add_modifier(Modifier::ITALIC)),
                ])
                .indent(2)
            })
            .collect()
    };
    push(section("you last said", width));
    for b in quote(&s.last_prompt, 700) {
        push(b);
    }
    if s.prompts > 1 && s.first_prompt != s.last_prompt {
        push(Block::blank());
        push(section("it started with", width));
        for b in quote(&s.first_prompt, 400) {
            push(b);
        }
    }

    if !s.artifacts.is_empty() {
        out.push((Block::blank(), None));
        out.push((section("artifacts", width), None));
        for a in s.artifacts.iter().rev() {
            out.push((
                Block::new(vec![
                    Span::styled("↗ ", theme::accent()),
                    Span::styled(a.title.clone(), theme::text().add_modifier(Modifier::UNDERLINED)),
                ])
                .indent(2),
                Some(Link::Url(a.url.clone())),
            ));
        }
    }
    if !s.files.is_empty() {
        out.push((Block::blank(), None));
        out.push((section(&format!("files written · {}", s.files.len()), width), None));
        let prefix = format!("{}/", s.cwd.trim_end_matches('/'));
        for p in s.files.iter().rev().take(12) {
            let shown = p.strip_prefix(&prefix).map(str::to_string).unwrap_or_else(|| tilde(p));
            out.push((
                Block::new(vec![Span::styled("▸ ", theme::faint()), Span::styled(shown, theme::code())]).indent(2),
                Some(Link::Path(p.clone())),
            ));
        }
        if s.files.len() > 12 {
            out.push((
                Block::new(vec![Span::styled(format!("  and {} more", s.files.len() - 12), theme::faint())]),
                None,
            ));
        }
    }
    out.push((Block::blank(), None));
    out.push((section("resume", width), None));
    out.push((
        Block::new(vec![Span::styled(
            crate::launch::resume_command(s.agent, &s.cwd, &s.id),
            theme::faint(),
        )])
        .indent(2),
        None,
    ));
    out
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect, hot: bool) {
    let focused = app.focus == Focus::Detail;
    let block = pane("Where it left off", focused, hot);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(3),
        ..inner
    };
    app.areas.right_text = text;
    app.links.clear();

    let Some(s) = app.selected().cloned() else {
        app.detail_rows = 0;
        return;
    };
    let mut rows: Vec<Line> = Vec::new();
    let mut links = Vec::new();
    for (b, link) in detail_blocks(app, &s, text.width) {
        for line in markdown::wrap(&b, text.width) {
            if let Some(l) = &link {
                links.push((rows.len(), l.clone()));
            }
            rows.push(line);
        }
    }
    app.detail_rows = rows.len();
    let max = rows.len().saturating_sub(text.height as usize);
    app.detail_scroll = app.detail_scroll.min(max);

    // Hovered link rows get a background so they read as clickable.
    if let Some(Hover::Detail(r)) = app.hover
        && links.iter().any(|(row, _)| *row == r)
            && let Some(line) = rows.get_mut(r) {
                line.style = Style::new().bg(theme::HOVER_BG);
            }
    app.links = links;
    let visible: Vec<Line> = rows.into_iter().skip(app.detail_scroll).take(text.height as usize).collect();
    f.render_widget(Paragraph::new(visible), text);

    if max > 0 {
        let mut st = ScrollbarState::new(max).position(app.detail_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(theme::dim()),
            Rect { y: area.y + 1, height: area.height.saturating_sub(2), ..area },
            &mut st,
        );
    }
}

// ---------------------------------------------------------------------------
// Transcript

fn draw_transcript(f: &mut Frame, app: &mut App, body: Rect) {
    let Some(t) = app.transcript.as_mut() else { return };
    let block = pane(&format!("Transcript · {}", trunc(&t.title, 60)), true, false);
    let inner = block.inner(body);
    f.render_widget(block, body);
    let text = Rect { x: inner.x + 1, width: inner.width.saturating_sub(3), ..inner };
    app.areas.transcript = text;
    t.layout(text.width, text.height);
    t.clamp(text.height);
    let rows: Vec<Line> = t.rows.iter().skip(t.scroll).take(text.height as usize).cloned().collect();
    f.render_widget(Paragraph::new(rows), text);
    let max = t.rows.len().saturating_sub(text.height as usize);
    if max > 0 {
        let mut st = ScrollbarState::new(max).position(t.scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(theme::dim()),
            Rect { y: body.y + 1, height: body.height.saturating_sub(2), ..body },
            &mut st,
        );
    }
}

// ---------------------------------------------------------------------------
// Stats

fn draw_stats(f: &mut Frame, app: &mut App, body: Rect) {
    let block = pane("Stats", true, false);
    let inner = block.inner(body);
    f.render_widget(block, body);
    if inner.height < 8 {
        return;
    }
    let prompts: u32 = app.sessions.iter().map(|s| s.prompts).sum();
    let cost: f64 = app.sessions.iter().map(|s| s.cost).sum();
    let head = Line::from(vec![
        Span::styled(format!(" {}", app.sessions.len()), theme::bright()),
        Span::styled(" sessions   ", theme::dim()),
        Span::styled(prompts.to_string(), theme::bright()),
        Span::styled(" prompts   ", theme::dim()),
        Span::styled(app.projects.len().to_string(), theme::bright()),
        Span::styled(" projects   ", theme::dim()),
        Span::styled(format!("${cost:.0}"), theme::bright()),
        Span::styled(" spent", theme::dim()),
    ]);
    let [h, grid] = Layout::vertical([Constraint::Length(2), Constraint::Min(4)]).areas(inner);
    f.render_widget(Paragraph::new(head), h);
    let [top, bottom] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(grid);
    let [tl, tr] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    let [bl, br] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);

    // Sessions per week, last 12 weeks.
    let week = 7 * 86_400_000i64;
    let mut weeks = [0u64; 12];
    for s in &app.sessions {
        let age = (app.now - s.started) / week;
        if (0..12).contains(&age) {
            weeks[11 - age as usize] += 1;
        }
    }
    let labels: Vec<String> = (0..12)
        .map(|i| {
            let t = app.now - (11 - i as i64) * week;
            chrono::TimeZone::timestamp_millis_opt(&chrono::Local, t)
                .single()
                .map(|d| d.format("%-m/%-d").to_string())
                .unwrap_or_default()
        })
        .collect();
    let bars: Vec<Bar> = weeks
        .iter()
        .zip(&labels)
        .map(|(v, l)| {
            Bar::default()
                .value(*v)
                .label(Line::from(l.clone()))
                .text_value(v.to_string())
                .value_style(Style::new().fg(theme::BRIGHT).bg(theme::ACCENT))
        })
        .collect();
    let chart_area = titled(f, tl, "Sessions per week");
    let bw = ((chart_area.width.saturating_sub(12)) / 12).clamp(1, 7);
    f.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(bw)
            .bar_gap(1)
            .bar_style(theme::accent())
            .label_style(theme::faint()),
        chart_area,
    );

    // Cost by project.
    let mut by_proj: HashMap<&str, f64> = HashMap::new();
    for s in &app.sessions {
        *by_proj.entry(app.project_name(&s.cwd)).or_default() += s.cost;
    }
    let mut cost_items: Vec<(String, f64, String)> = by_proj
        .into_iter()
        .filter(|(_, c)| *c > 0.0)
        .map(|(n, c)| (n.to_string(), c, format!("${c:.0}")))
        .collect();
    cost_items.sort_by(|a, b| b.1.total_cmp(&a.1));
    let a = titled(f, tr, "Cost by project");
    cost_items.truncate(a.height as usize);
    f.render_widget(Paragraph::new(hbars(&cost_items, a.width)), a);

    // Tools.
    let mut tools: HashMap<String, u32> = HashMap::new();
    for s in &app.sessions {
        for (n, c) in &s.tools {
            *tools.entry(tool_label(n)).or_default() += c;
        }
    }
    let mut tool_items: Vec<(String, f64, String)> =
        tools.into_iter().map(|(n, c)| (n, c as f64, c.to_string())).collect();
    tool_items.sort_by(|a, b| b.1.total_cmp(&a.1));
    let a = titled(f, bl, "Most used tools");
    tool_items.truncate(a.height as usize);
    f.render_widget(Paragraph::new(hbars(&tool_items, a.width)), a);

    // Longest threads.
    let mut longest: Vec<&Session> = app.sessions.iter().collect();
    longest.sort_by_key(|s| std::cmp::Reverse(s.prompts));
    let a = titled(f, br, "Longest threads");
    let lines: Vec<Line> = longest
        .iter()
        .take(a.height as usize)
        .map(|s| {
            let days = ((s.updated - s.started) / 86_400_000).max(0);
            let span = if days > 0 { format!(" {days}d") } else { String::new() };
            let right = format!("{}{} ", app.project_name(&s.cwd), span);
            let tw = (a.width as usize).saturating_sub(6 + right.width() + 1);
            Line::from(vec![
                Span::styled(format!("{:>4}  ", s.prompts), theme::accent()),
                Span::styled(pad(&trunc(&s.title, tw), tw), theme::text()),
                Span::raw(" "),
                Span::styled(right, theme::faint()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), a);
}

fn titled(f: &mut Frame, area: Rect, title: &str) -> Rect {
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(3),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(
        Paragraph::new(section(title, inner.width).spans.into_iter().collect::<Line>()),
        Rect { height: 1, ..inner },
    );
    Rect { y: inner.y + 2, height: inner.height.saturating_sub(2), ..inner }
}

fn hbars(items: &[(String, f64, String)], width: u16) -> Vec<Line<'static>> {
    let max = items.iter().map(|x| x.1).fold(0.0, f64::max).max(1e-9);
    let name_w = items.iter().map(|x| x.0.width()).max().unwrap_or(0).min(width as usize / 3);
    let val_w = items.iter().map(|x| x.2.width()).max().unwrap_or(0);
    let bar_w = (width as usize).saturating_sub(name_w + val_w + 3);
    items
        .iter()
        .map(|(n, v, label)| {
            let eighths = ((v / max) * bar_w as f64 * 8.0).round() as usize;
            let mut bar = "█".repeat(eighths / 8);
            if !eighths.is_multiple_of(8) {
                bar.push([' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8]);
            }
            let bar_len = bar.chars().count();
            Line::from(vec![
                Span::styled(pad(&trunc(n, name_w), name_w), theme::text()),
                Span::raw(" "),
                Span::styled(bar, theme::accent()),
                Span::raw(" ".repeat(bar_w.saturating_sub(bar_len) + 1)),
                Span::styled(format!("{label:>val_w$}"), theme::dim()),
            ])
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Overlays

fn draw_menu(f: &mut Frame, app: &mut App, screen: Rect) {
    let Some(menu) = &app.menu else { return };
    let w = 30u16;
    let actions = app.actions();
    let h = actions.len() as u16 + 2;
    let x = menu.x.min(screen.right().saturating_sub(w));
    let y = if menu.y + h > screen.bottom() { menu.y.saturating_sub(h) } else { menu.y };
    let rect = Rect { x, y, width: w, height: h };
    app.areas.menu = Some(rect);
    f.render_widget(Clear, rect);
    let block = TuiBlock::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .style(Style::new().bg(theme::MENU_BG));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let done = app.selected().is_some_and(|s| app.is_done(s));
    let no_art = app.selected().is_none_or(|s| s.artifacts.is_empty());
    let lines: Vec<Line> = actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let (mut label, key) = a.label();
            if *a == Action::ToggleDone {
                label = if done { "Mark not done" } else { "Mark done" };
            }
            let disabled = *a == Action::OpenArtifact && no_art;
            let lw = inner.width as usize - 4;
            let style = if disabled { theme::faint() } else { theme::text() };
            Line::from(vec![
                Span::styled(format!(" {}", pad(label, lw - key.width())), style),
                Span::styled(format!("{key} "), theme::faint()),
            ])
            .style(if menu.sel == i { Style::new().bg(theme::SEL_BG) } else { Style::new() })
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_toast(f: &mut Frame, app: &App, screen: Rect) -> Option<Rect> {
    let t = app.toast.as_ref()?;
    let text = trunc(&t.text, screen.width.saturating_sub(8) as usize);
    let w = text.width() as u16 + 4;
    let rect = Rect {
        x: screen.right().saturating_sub(w + 1),
        y: screen.bottom().saturating_sub(4),
        width: w,
        height: 3,
    };
    let color = if t.error { theme::ERR } else { theme::ACCENT };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(text, theme::bright()))).block(
            TuiBlock::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(color))
                .style(Style::new().bg(theme::MENU_BG)),
        )
        .centered(),
        rect,
    );
    Some(rect)
}

fn draw_tips(f: &mut Frame, screen: Rect) {
    let tips: &[(&str, &str)] = &[
        ("drag a border", "resize panes; grab the ┇ handle"),
        ("↵ / double-click", "resume in a new tab; R resumes here"),
        ("amber ●", "the agent asked you something; d clears it"),
        ("/", "search the full text of prompts and replies"),
        ("t", "read the whole transcript"),
        ("c", "short catch-up from Claude, costs a few cents"),
        ("right-click", "more actions for a session"),
        ("?", "every key and mouse control"),
    ];
    let w = 72.min(screen.width);
    let h = (tips.len() as u16 + 5).min(screen.height);
    let rect = Rect {
        x: screen.x + (screen.width - w) / 2,
        y: screen.y + (screen.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let mut lines = vec![Line::default()];
    for (k, d) in tips {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", pad(k, 20)), theme::accent()),
            Span::styled(d.to_string(), theme::text()),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  any key", theme::accent()),
        Span::styled(" closes   ", theme::faint()),
        Span::styled("x", theme::accent()),
        Span::styled(" don't show again", theme::faint()),
    ]));
    f.render_widget(
        Paragraph::new(lines).block(
            TuiBlock::bordered()
                .border_type(BorderType::Rounded)
                .border_style(theme::accent())
                .title(Line::from(Span::styled(" wayget · tips ", theme::label())))
                .style(Style::new().bg(theme::MENU_BG)),
        ),
        rect,
    );
}

fn draw_help(f: &mut Frame, screen: Rect) {
    let keys: Vec<(&str, &str)> = vec![
        ("↵ / double-click", "resume in a new tab"),
        ("R", "resume here, replacing wayget"),
        ("/", "search titles, prompts and replies"),
        ("w", "toggle Waiting on me"),
        ("d / click ●", "mark done or not done"),
        ("t", "transcript"),
        ("c", "catch me up (asks Claude, costs a little)"),
        ("s", "stats"),
        ("o / a / y", "open folder, open artifact, copy command"),
        ("m / right-click", "menu"),
        ("h j k l / arrows", "move; tab switches panes"),
        ("J / K, scroll wheel", "scroll the detail pane"),
        ("drag a border", "resize panes"),
        ("click a link", "open artifact or file"),
        ("r", "reindex everything"),
        ("q", "quit"),
    ];
    let w = 76.min(screen.width);
    let h = (keys.len() as u16 + 6).min(screen.height);
    let rect = Rect {
        x: screen.x + (screen.width - w) / 2,
        y: screen.y + (screen.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let mut lines = vec![Line::default()];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", pad(k, 22)), theme::accent()),
            Span::styled(d.to_string(), theme::text()),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  ● amber: the agent asked you something   ● green: live now",
        theme::faint(),
    )));
    f.render_widget(
        Paragraph::new(lines).block(
            TuiBlock::bordered()
                .border_type(BorderType::Rounded)
                .border_style(theme::accent())
                .title(Line::from(Span::styled(" wayget · keys and mouse ", theme::label())))
                .style(Style::new().bg(theme::MENU_BG)),
        ),
        rect,
    );
}

// ---------------------------------------------------------------------------
// Text helpers

fn trunc(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

fn pad(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width { s.to_string() } else { format!("{s}{}", " ".repeat(width - w)) }
}

/// Renders `s` within `width`, styling the fuzzy-matched char indices.
fn highlighted(s: &str, idx: &[u32], width: usize, base: Style) -> (Vec<Span<'static>>, usize) {
    let text = trunc(s, width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_hl = false;
    for (i, ch) in text.chars().enumerate() {
        let hl = idx.binary_search(&(i as u32)).is_ok();
        if hl != cur_hl && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), if cur_hl { theme::matched() } else { base }));
        }
        cur_hl = hl;
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if cur_hl { theme::matched() } else { base }));
    }
    (spans, text.width())
}

