use crate::anilist::Media;
use crate::kitty;
use crate::{App, Cover, EpMode, Feed, Input, PIN_REDIRECT, Pending, View, ani, list_label};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, Paragraph, Wrap,
};

// ANSI colors on purpose: they follow the terminal (and Omarchy) theme.
const ACCENT: Color = Color::Magenta;
const MATCH: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
/// Text to read that steps back: faint main text is easier on the eyes than DIM in most
/// themes (Omarchy's color 7 is the main text itself, so there's no ANSI shade between).
const SOFT: Style = Style::new().add_modifier(Modifier::DIM);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 2] = ["▄▀█ █▄ █ █ ▄▄ █ █ █▀█ █▀█", "█▀█ █ ▀█ █    █▀█ █▄█ █▀▀"];
const SHADES: [&str; 4] = ["░", "▒", "▓", "█"];
const BOOT_MS: u128 = 500;
const GLINT_MS: u128 = 450;
const POP_MS: u128 = 160;

pub fn render(app: &mut App, f: &mut Frame) {
    app.cell = kitty::cell_px();
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(body);
    header(app, f, head);
    menu(app, f, left);
    preview(app, f, right);
    footer(app, f, foot);
    starting(app, f);
    boot(app, f.buffer_mut());
}

// Effects never hide content for long: speed first, the shine comes on top.
pub fn effects_running(app: &App) -> bool {
    app.started.elapsed().as_millis() < BOOT_MS
        || app.glint.1.elapsed().as_millis() < GLINT_MS
        || app.playing.as_ref().is_some_and(|p| p.step.is_some())
}

/// While an episode starts, a box pops up in the middle with a running bar, so enter
/// visibly did something at once. It goes when mpv shows the episode.
fn starting(app: &App, f: &mut Frame) {
    let Some((p, step)) = app.playing.as_ref().and_then(|p| Some((p, p.step?))) else {
        return;
    };
    // the cover color, unless it's too dark to see
    let color = match p.color.as_deref().and_then(hex) {
        Some(Color::Rgb(r, g, b)) if r as u16 + g as u16 + b as u16 > 180 => Color::Rgb(r, g, b),
        _ => ACCENT,
    };
    // grows out of the middle, fast at first
    let t = (p.since.elapsed().as_millis() as f32 / POP_MS as f32).min(1.0);
    let grow = 1.0 - (1.0 - t).powi(3);
    let (w, h) = (f.area().width.min(56) as f32, f.area().height.min(7) as f32);
    let area = f.area().centered(
        Constraint::Length((w * (0.5 + 0.5 * grow)) as u16),
        Constraint::Length((h * (0.4 + 0.6 * grow)).max(3.0) as u16),
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(color)
        .title(Span::styled(" ▶ Starting ", Style::new().fg(color).bold()));
    let inner = block.inner(area).inner(Margin::new(2, 1));
    // a light band runs over the bar, like over the loading lists
    let band = (app.started.elapsed().as_millis() / 20) as usize % (inner.width as usize + 10);
    let bar: Vec<Span> = (0..inner.width as usize)
        .map(|x| {
            Span::styled(
                "━",
                if (x + 5).abs_diff(band) < 5 {
                    color
                } else {
                    DIM
                },
            )
        })
        .collect();
    let lines = vec![
        Line::styled(p.title.as_str(), Style::new().bold()),
        Line::from(vec![
            Span::styled(format!("{} ", spinner(app)), color),
            Span::styled(format!("{step}…"), DIM),
        ]),
        Line::from(bar),
    ];
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

/// How far a left-to-right front with a `band`-wide tail has passed `x`, at progress `t`.
fn swept(t: f32, x: u16, width: u16, band: f32) -> f32 {
    t * (width as f32 + band) - x as f32
}

/// Start-up: every glyph runs ░▒▓█ behind a sweeping front, then turns into the real UI.
/// Image cells stay untouched: the Kitty image travels inside them and is sent only once.
fn boot(app: &App, buf: &mut Buffer) {
    let t = app.started.elapsed().as_millis() as f32 / BOOT_MS as f32;
    let area = buf.area;
    if t >= 1.0 {
        return;
    }
    for (x, y) in area.positions().map(|p| (p.x, p.y)) {
        let cell = &mut buf[(x, y)];
        let d = swept(t, x, area.width, 25.0);
        if d < 25.0 && cell.symbol() != " " && !cell.symbol().contains('\u{10EEEE}') {
            cell.set_symbol(SHADES[(d.max(0.0) / 25.0 * 4.0) as usize]);
        }
    }
}

/// A white sweep across the title that settles into its cover color.
fn glint(app: &App, buf: &mut Buffer, area: Rect, (r, g, b): (u8, u8, u8)) {
    let t = app.glint.1.elapsed().as_millis() as f32 / GLINT_MS as f32;
    if t >= 1.0 {
        return;
    }
    let lerp = |c: u8, w: f32| (c as f32 + (255.0 - c as f32) * w) as u8;
    for pos in area.positions() {
        let cell = &mut buf[pos];
        if cell.fg == Color::Rgb(r, g, b) {
            let w = 1.0 - (swept(t, pos.x - area.x, area.width, 12.0) / 12.0).clamp(0.0, 1.0);
            cell.fg = Color::Rgb(lerp(r, w), lerp(g, w), lerp(b, w));
        }
    }
}

fn logo() -> impl Iterator<Item = Line<'static>> {
    LOGO.iter()
        .map(|l| Line::styled(*l, Style::new().fg(ACCENT).bold()))
}

/// Short name for the breadcrumbs; questions are shown in full in the menu.
fn crumb(s: &crate::Screen) -> &str {
    match s.view {
        View::Confirm(_) => "Confirm",
        _ => &s.title,
    }
}

fn panel() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(DIM)
}

/// A paused download's row: how far it got, without the stale speed of its last line, when
/// that's kept (yt-dlp; ffmpeg starts over).
fn paused(n: &crate::DlNow) -> String {
    let at = crate::got_so_far(&n.text).filter(|_| n.resumes);
    at.map_or("‖ paused".into(), |p| format!("‖ paused {p}"))
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

fn header(app: &App, f: &mut Frame, area: Rect) {
    let user = match &app.user {
        Some((_, name)) => Line::from(vec![
            Span::styled("● ", Color::Green),
            Span::raw(name.as_str()),
            " ".into(),
        ]),
        None => Line::from(Span::styled("○ not logged in to AniList ", DIM)),
    };
    let [crumbs_area, user_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(user.width() as u16)])
            .areas(area);
    // newest crumbs first; whatever doesn't fit folds into "…" (the current one never does)
    let mut room = (crumbs_area.width as usize).saturating_sub(10);
    let mut shown = Vec::new();
    for t in app.stack.iter().skip(1).rev().map(crumb) {
        let w = Span::raw(t).width() + 3;
        if w > room && !shown.is_empty() {
            shown.push("…");
            break;
        }
        room = room.saturating_sub(w);
        shown.push(t);
    }
    let mut crumbs = vec![Span::styled(
        " ani-hop ",
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];
    for t in shown.into_iter().rev() {
        crumbs.push(Span::styled(" › ", DIM));
        crumbs.push(Span::raw(t));
    }
    f.render_widget(Line::from(crumbs), crumbs_area);
    f.render_widget(user, user_area);
}

fn menu(app: &mut App, f: &mut Frame, area: Rect) {
    let spin = spinner(app);
    let shimmer = (app.started.elapsed().as_millis() / 30) as usize;
    let screen = app.stack.last_mut().unwrap();
    let block = panel();
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    // a confirmation shows its question where the filter prompt would be, wrapped
    let question = matches!(screen.view, View::Confirm(_)).then_some(screen.title.as_str());
    let rows = question.map_or(1, |q| {
        (Line::raw(q).width() as u16 / inner.width.max(1) + 1).min(4)
    });
    let [prompt_area, info_area, list_area] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let label = match &screen.view {
        View::Input(Input::Search) => "search ",
        View::Input(Input::ClientId) => "client id ",
        View::Input(Input::Token) => "token ",
        View::Input(Input::SourceQuery(_)) => "ani-cli search ",
        View::Input(Input::DownloadDir) => "folder ",
        _ => "",
    };
    let prompt = Line::from(vec![
        Span::styled(label, ACCENT),
        Span::styled("❯ ", Style::new().fg(ACCENT).bold()),
        Span::raw(screen.menu.query.as_str()),
    ]);
    let cursor_x =
        (prompt_area.x + prompt.width() as u16).min(prompt_area.right().saturating_sub(1));
    match question {
        Some(q) => f.render_widget(
            Paragraph::new(Line::styled(q, Style::new().bold())).wrap(Wrap { trim: true }),
            prompt_area,
        ),
        None => {
            f.render_widget(prompt, prompt_area);
            f.set_cursor_position((cursor_x, prompt_area.y));
        }
    }

    if let View::Input(input) = &screen.view {
        f.render_widget(
            Paragraph::new(input_help(input)).wrap(Wrap { trim: false }),
            list_area,
        );
        return;
    }

    let m = &screen.menu;
    // how many there are, and while filtering how many of them match
    let picks = |i: &usize| !m.items[*i].action;
    let all = (0..m.items.len()).filter(picks).count();
    let count = match m.query.is_empty() {
        true => all.to_string(),
        false => format!(
            "{}/{all}",
            m.matches.iter().filter(|(i, _)| picks(i)).count()
        ),
    };
    let mut info = vec![Span::styled(
        if m.items.is_empty() && screen.loading {
            String::new()
        } else {
            count
        },
        DIM,
    )];
    if screen.loading {
        info.push(Span::styled(format!(" {spin}"), ACCENT));
    }
    if screen.loading && m.items.is_empty() {
        f.render_widget(skeleton(list_area, shimmer), list_area);
    }
    if !m.marked.is_empty() {
        info.push(Span::styled(
            format!(" ({} marked)", m.marked.len()),
            Color::Green,
        ));
    }
    let used: usize = info.iter().map(|s| s.width()).sum();
    info.push(Span::styled(
        format!(
            " {}",
            "─".repeat((info_area.width as usize).saturating_sub(used + 1))
        ),
        DIM,
    ));
    f.render_widget(Line::from(info), info_area);
    // an empty list says so, instead of a bare 0
    let empty = match &screen.view {
        _ if !m.items.is_empty() || screen.loading => None,
        View::Shows(_, Feed::Local) => Some("Nothing downloaded yet"),
        View::Shows(..) => Some("No shows found"),
        View::Episodes(..) => Some("No episodes found"),
        _ => None,
    };
    if let Some(text) = empty {
        let line = Line::from(vec!["   ".into(), Span::styled(text, DIM)]);
        return f.render_widget(line, list_area);
    }

    // the episode being started shows it right where enter was pressed
    let busy = app.playing.as_ref().and_then(|p| {
        let key = match &screen.view {
            View::Episodes(m, EpMode::Play) if m.id == p.id => p.ep.clone(),
            View::Show(m) if m.id == p.id && p.ep == (m.progress() + 1).to_string() => {
                "next".into()
            }
            _ => return None,
        };
        Some((
            key,
            p.step
                .map_or("▶ playing".into(), |s| format!("{spin} {s}…")),
        ))
    });
    // and the next page right on "More…"
    let fetching = matches!(screen.view, View::Shows(_, Feed::Pages(_, _, true)));
    let busy = busy.or_else(|| fetching.then(|| ("more".to_string(), format!("{spin} loading…"))));
    // and the episode downloading right now on its row, or how far it got when paused
    let busy = busy.or_else(|| match (&screen.view, &app.dl_now) {
        (View::Episodes(m, _), Some(n)) if m.id == n.id => Some((
            n.ep.clone(),
            match ani::paused() {
                true => paused(n),
                false => format!("↓ {}", n.text),
            },
        )),
        _ => None,
    });
    let width = list_area.width.saturating_sub(1) as usize; // minus the pointer column
    let rows: Vec<ListItem> = m
        .matches
        .iter()
        .map(|(i, hits)| {
            let item = &m.items[*i];
            let mark = match (m.marked.contains(i), item.done) {
                (true, _) => "●",
                (false, true) => "✓",
                _ => " ",
            };
            let mut spans = vec![Span::styled(mark, Color::Green), " ".into()];
            spans.extend(item.label.chars().enumerate().map(|(ci, ch)| {
                let hit = hits.binary_search(&(ci as u32)).is_ok();
                Span::styled(
                    ch.to_string(),
                    match (hit, item.done) {
                        (true, _) => Style::new().fg(MATCH).bold(),
                        (false, true) => SOFT,
                        _ => Style::new(),
                    },
                )
            }));
            let mut used: usize = spans.iter().map(|s| s.width()).sum();
            // a title too long for the row ends in "…", like a cut hint
            if used > width {
                while used >= width
                    && let Some(s) = spans.pop()
                {
                    used -= s.width();
                }
                spans.push(Span::styled(
                    "…",
                    if item.done { SOFT } else { Style::new() },
                ));
                used += 1;
            }
            let room = width.saturating_sub(used + 2);
            let (hint, color) = match &busy {
                Some((key, text)) if *key == item.key => (text.clone(), ACCENT),
                _ => (item.hint.clone(), DIM),
            };
            if !hint.is_empty() && room > 3 {
                let hint = Span::styled(fit(hint, room), color);
                spans.push(" ".repeat(width.saturating_sub(used + hint.width())).into());
                spans.push(hint);
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(rows)
        .highlight_symbol(Line::styled("▌", ACCENT))
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::new().bold().not_dim());
    f.render_stateful_widget(list, list_area, &mut screen.menu.state);
}

/// Placeholder rows with a light band running over them while the first fetch is out.
fn skeleton(area: Rect, tick: usize) -> Text<'static> {
    let band = tick % (area.width as usize + 30);
    (0..area.height.min(14) as usize)
        .map(|row| {
            let len = 14 + (row * 7919 % 23); // stable pseudo-random widths
            let spans: Vec<Span> = (0..len)
                .map(|x| {
                    let lit = (x + 3 + row).abs_diff(band) < 4;
                    Span::styled("━", if lit { Color::Gray } else { DIM })
                })
                .collect();
            Line::from([vec!["   ".into()], spans].concat())
        })
        .collect::<Vec<_>>()
        .into()
}

fn input_help(input: &Input) -> Text<'static> {
    let lines: Vec<Line> = match input {
        Input::Search => vec!["Type a title and press enter.".into()],
        Input::ClientId => vec![
            "One-time setup: ani-hop needs your own AniList API client.".into(),
            "".into(),
            "1. Open anilist.co/settings/developer".into(),
            "   → Create New Client".into(),
            "2. Name: ani-hop".into(),
            "   Redirect URL:".into(),
            Line::from(vec![
                "   ".into(),
                Span::styled(PIN_REDIRECT, Style::new().fg(ACCENT).not_dim()),
            ]),
            "3. Paste the client ID here and press enter.".into(),
        ],
        Input::Token => vec![
            "Approve ani-hop in your browser, then paste".into(),
            "the token AniList shows and press enter.".into(),
            "".into(),
            Line::styled(
                "Pasting the whole redirect URL works too.",
                Style::new().italic(),
            ),
        ],
        Input::SourceQuery(_) => vec![
            "Search ani-cli with this text, then pick the".into(),
            "right entry. ani-hop remembers the choice.".into(),
        ],
        Input::DownloadDir => vec![
            "Where new downloads go. ~ works.".into(),
            format!("Empty goes back to {}.", ani::tilde(&ani::default_root())).into(),
            "".into(),
            "Shows already downloaded stay in the old folder.".into(),
        ],
    };
    Text::from(lines).style(SOFT)
}

fn preview(app: &mut App, f: &mut Frame, area: Rect) {
    let block = panel();
    let inner = block.inner(area).inner(Margin::new(2, 1));
    f.render_widget(block, area);
    let screen = app.stack.last().unwrap();
    let media = match &screen.view {
        View::Shows(list, _) => screen
            .menu
            .current()
            .and_then(|it| list.iter().find(|m| m.id.to_string() == it.key))
            .cloned(),
        View::Show(m)
        | View::Episodes(m, _)
        | View::DlChoice(m, _)
        | View::Status(m)
        | View::Sources(m, ..)
        | View::Input(Input::SourceQuery(m))
        | View::Audio(Some(m))
        | View::Confirm(Pending::Progress(m, _) | Pending::Stop(m, _) | Pending::Pause(m, _)) => {
            Some(m.clone())
        }
        _ => None,
    };
    // posters just below the cursor load ahead, so scrolling shows them instantly
    let ahead: Vec<Media> = match &screen.view {
        View::Shows(list, _) => {
            let cur = screen.menu.state.selected().unwrap_or(0);
            screen
                .menu
                .matches
                .iter()
                .skip(cur + 1)
                .take(6)
                .filter_map(|(i, _)| list.get(*i).cloned())
                .collect()
        }
        _ => Vec::new(),
    };
    // episode titles load while the show is open, so its episode list has them right away
    let shows_eps = matches!(screen.view, View::Show(_) | View::Episodes(..));
    let ep = match &screen.view {
        View::Episodes(_, EpMode::Play | EpMode::Download) => screen.menu.current(),
        _ => None,
    };
    let ep = ep.map(|it| it.key.clone());
    if let Some(m) = media {
        if shows_eps {
            app.want_episodes(&m);
        }
        media_preview(app, f, inner, &m, ep.as_deref());
        return ahead.iter().for_each(|m| app.want_cover(m, false));
    }
    let lines: Vec<Line> = match &screen.view {
        View::Bulk(list) => list
            .iter()
            .map(|m| Line::from(format!("• {}", m.name())))
            .collect(),
        View::Confirm(Pending::Delete(paths)) => paths
            .iter()
            .map(|p| {
                Line::from(format!(
                    "✗ {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ))
            })
            .collect(),
        View::Confirm(Pending::Resume(jobs, _)) => jobs
            .iter()
            .map(|j| {
                let eps = match j.eps.as_deref() {
                    Some([one]) => format!("episode {one}"),
                    Some(e @ [first, .., last]) => format!("{} episodes ({first}–{last})", e.len()),
                    _ => "unwatched episodes".into(),
                };
                Line::from(format!("↓ {} · {eps}", j.media.name()))
            })
            .collect(),
        View::Setup => setup(app),
        _ => banner(app),
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn setup(app: &App) -> Vec<Line<'static>> {
    let mark = |good: bool| match good {
        true => Span::styled("  ✓ ", Color::Green),
        false => Span::styled("  ✗ ", Color::Red),
    };
    let row = |good: bool, name: &str, what: &'static str| {
        // extras: without them ani-hop works, just plainer
        let mark = match good || !matches!(name, "ani-skip" | "graphics") {
            true => mark(good),
            false => Span::styled("  ○ ", Color::Yellow),
        };
        Line::from(vec![
            mark,
            format!("{name:<10}").into(),
            Span::styled(what, DIM),
        ])
    };
    // enter on Install or Update runs this, which the hint shows cut short
    let command =
        |cmd: String| Line::from(vec![Span::styled("❯ ", ACCENT), Span::styled(cmd, DIM)]);
    let mut v: Vec<Line> = logo().collect();
    // what to do first: a long checklist must not push it out of a small terminal
    let pkgs: Vec<&str> = app.missing.iter().map(|d| d.1).collect();
    if let Some(cmd) = ani::install_command(&pkgs).filter(|_| !pkgs.is_empty()) {
        v.extend(["".into(), command(cmd)]);
    } else if !pkgs.is_empty() {
        // shortcut: worded for Homebrew, the only way to land here on Linux or macOS; word
        // it per OS if another one is added
        let names: Vec<&str> = app.missing.iter().map(|d| d.0).collect();
        let brew: Vec<&str> = (names.iter().copied())
            .filter(|n| !matches!(*n, "ani-skip" | "xdg-open"))
            .collect();
        v.push("".into());
        if !brew.is_empty() {
            let text = format!("Install with Homebrew (brew.sh): {}", brew.join(" "));
            v.push(Line::styled(text, Color::Yellow));
        }
        if names.contains(&"ani-skip") {
            let text = "ani-skip (optional): github.com/synacktraa/ani-skip";
            v.push(Line::styled(text, DIM));
        }
        if names.contains(&"xdg-open") {
            // only on Linux (deps() leaves it out on macOS), and Homebrew has no xdg-utils
            v.push(Line::styled("xdg-open: xdg-utils from your distro", DIM));
        }
    }
    if let Some(old) = &app.old_ani_cli {
        v.push("".into());
        v.push(Line::styled(
            format!(
                "ani-cli {old} is too old, ani-hop needs {}",
                ani::TESTED_ANI_CLI
            ),
            Color::Yellow,
        ));
        v.push(match ani::ani_cli_update() {
            Some(cmd) => command(cmd),
            None => Line::styled("Update it with your package manager", DIM),
        });
    }
    v.extend(["".into(), Line::styled("Tools", Style::new().bold())]);
    for (bin, _, what) in ani::deps() {
        let old = bin == "ani-cli" && app.old_ani_cli.is_some();
        v.push(row(
            !old && !app.missing.iter().any(|d| d.0 == bin),
            bin,
            what,
        ));
    }
    v.push(row(
        kitty::supported(),
        "graphics",
        "posters need Ghostty or kitty",
    ));
    v.extend(["".into(), Line::styled("AniList", Style::new().bold())]);
    match &app.user {
        Some((_, name)) => v.push(Line::from(vec![
            mark(true),
            format!("logged in as {name}").into(),
        ])),
        None => v.extend([
            Line::styled("  Log in to get Watching/Planning first", DIM),
            Line::styled("  and progress saved after each episode.", DIM),
        ]),
    }
    v
}

fn banner(app: &App) -> Vec<Line<'static>> {
    let key = |k: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(format!("{k:>10}  "), ACCENT),
            Span::styled(what, DIM),
        ])
    };
    let mut v: Vec<Line> = logo().collect();
    v.push("".into());
    v.push(Line::styled(
        format!("↓ {}", ani::tilde(&ani::download_root())),
        DIM,
    ));
    if let Some((new, cmd)) = &app.release {
        v.push(Line::styled(
            format!("↑ ani-hop {new} is out: {cmd}"),
            ACCENT,
        ));
    }
    // on an input typing is the text itself, and there's nothing to mark
    if matches!(app.stack.last().unwrap().view, View::Input(_)) {
        return v;
    }
    v.push("".into());
    v.extend([
        key("type", "filter, fzf syntax:"),
        key("", "'exact ^start end$ !not"),
        key("←↓↑→ ^hjkl", "back, down, up, select"),
        key("tab", "mark for bulk actions"),
        key("^a", "mark all"),
        key("^f", "more of a ranking"),
        key("enter", "select"),
        key("esc", "clear filter / back"),
        key("^c", "quit"),
    ]);
    v
}

fn media_preview(app: &mut App, f: &mut Frame, area: Rect, m: &Media, ep: Option<&str>) {
    let (fw, fh) = app.cell;
    let rows = (area.height * 3 / 5).clamp(6, 26).min(area.height);
    let cols = ((rows as f32 * fh as f32 * 0.7) / fw.max(1) as f32).round() as u16;
    app.poster = Size::new(cols.min(area.width / 2), rows);
    app.want_cover(m, true);

    let [top, _, bottom] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [poster, _, info] = Layout::horizontal([
        Constraint::Length(app.poster.width),
        Constraint::Length(2),
        Constraint::Fill(1),
    ])
    .areas(top);
    let accent = m
        .cover_image
        .color
        .as_deref()
        .and_then(hex)
        .unwrap_or(ACCENT);
    if app.glint.0 != m.id {
        app.glint = (m.id, std::time::Instant::now());
    }
    match app.covers.get(&m.id) {
        Some(Cover::Ready(_, p)) => kitty::render(p, poster, f.buffer_mut()),
        // the cover's dominant color stands in while it loads
        _ => f.render_widget(Block::new().bg(accent), poster),
    }

    let mut lines = vec![Line::styled(
        m.name().to_string(),
        Style::new().fg(accent).bold(),
    )];
    let alt = [&m.title.romaji, &m.title.english]
        .into_iter()
        .flatten()
        .find(|t| t.as_str() != m.name());
    if let Some(alt) = alt {
        lines.push(Line::styled(alt.clone(), SOFT.italic()));
    }
    lines.push("".into());
    let mut facts = Vec::new();
    // no-break spaces: wrapping never splits a fact like "10 episodes"
    if let Some(s) = m.average_score {
        facts.push(Span::styled(format!("★\u{a0}{s}%"), Color::Yellow));
    }
    let detail = [
        m.format.as_deref().map(pretty),
        m.episodes.map(|e| format!("{e}\u{a0}episodes")),
        m.status.as_deref().map(pretty),
    ];
    for d in detail.into_iter().flatten() {
        if !facts.is_empty() {
            facts.push(Span::styled(" · ", DIM));
        }
        facts.push(d.into());
    }
    // ani-cli's shows have none of these: no rows of blanks for them
    if !facts.is_empty() {
        lines.push(Line::from(facts));
    }
    if !m.on_anilist() {
        lines.push(Line::styled(
            "Found through ani-cli: no AniList details or progress sync",
            Color::Yellow,
        ));
    }
    let when = [
        m.season.as_deref().map(pretty),
        m.season_year.map(|y| y.to_string()),
    ];
    let when = when.into_iter().flatten().collect::<Vec<_>>().join(" ");
    let studio = m
        .studios
        .nodes
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let made = [when, studio]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if !made.is_empty() {
        lines.push(made.into());
    }
    if !m.genres.is_empty() {
        lines.push(Line::styled(m.genres.join(" · "), MATCH));
    }
    lines.push("".into());
    if let Some(e) = &m.media_list_entry {
        let total = m.episodes.map_or("?".into(), |t| t.to_string());
        lines.push(Line::styled(
            format!("● {} · {}/{total}", list_label(&e.status), e.progress),
            Color::Green,
        ));
    }
    if let Some(a) = &m.next_airing_episode {
        lines.push(Line::from(format!(
            "Episode {} airs in {}",
            a.episode,
            duration(a.time_until_airing)
        )));
    }
    let local = app.local.get(&m.id).map_or(0, Vec::len);
    if local > 0 {
        lines.push(Line::from(format!("↓ {local} on disk")));
    }
    if let Some(s) = app.sources.get(&m.id) {
        lines.push(Line::styled(
            format!("ani-cli: {} · {}\u{a0}episodes", s.title, s.episodes.len()),
            DIM,
        ));
    }
    if let Some(ep) = ep
        && let Some(i) = app.ep_info.get(&m.id).and_then(|i| i.get(ep))
    {
        lines.push("".into());
        if m.watched(ep) {
            let title = [format!("Episode {ep}"), i.title.clone()];
            let title = title
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>();
            lines.push(Line::styled(title.join(" · "), Style::new().bold()));
            lines.extend(i.about.lines().map(|l| Line::styled(l.to_string(), SOFT)));
        } else {
            lines.push(Line::styled(
                format!("Episode {ep} · not watched yet, so no summary"),
                DIM,
            ));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), info);
    // only true RGB titles: a named color has no RGB value to fade back into
    if let Color::Rgb(r, g, b) = accent {
        glint(app, f.buffer_mut(), info, (r, g, b));
    }

    let desc = m.description.as_deref().map(strip_html).unwrap_or_default();
    f.render_widget(Paragraph::new(desc).wrap(Wrap { trim: true }), bottom);
}

fn footer(app: &App, f: &mut Frame, area: Rect) {
    let screen = app.stack.last().unwrap();
    let enter = app.dl_verb();
    // esc empties a typed filter (or a prefilled input) before it goes back
    let esc = if screen.menu.query.is_empty() {
        "back"
    } else {
        "clear"
    };
    let mut keys = vec![("enter", enter), ("esc", esc)];
    if screen.menu.multi && !screen.menu.items.is_empty() {
        keys.splice(0..0, [("tab", "mark"), ("^a", "all")]);
    }
    if matches!(screen.view, View::Shows(_, Feed::Pages(_, Some(_), _))) {
        keys.insert(keys.len() - 2, ("^f", "more")); // before enter and esc
    }
    keys.push(("^c", "quit"));
    let hints = Line::from(
        keys.iter()
            .flat_map(|(k, what)| {
                [
                    Span::styled(*k, ACCENT),
                    Span::styled(format!(" {what}  "), DIM),
                ]
            })
            .collect::<Vec<_>>(),
    );
    let toast = app.toast.as_ref().filter(|t| t.2.elapsed().as_secs() < 4);
    let status = match (toast, &app.playing) {
        // shortcut: what's still underway is told apart by its "…"; give toasts a kind once a
        // finished one can end in "…"
        (Some((text, err, _)), _) => Line::styled(
            format!(" {text}"),
            match (*err, text.ends_with('…')) {
                (true, _) => Color::Red,
                (false, true) => ACCENT,
                _ => Color::Green,
            },
        ),
        // while it starts the pop-up says so, and downloads keep their line
        (_, Some(p)) if p.step.is_none() => {
            Line::from(vec![Span::styled(" ▶ ", ACCENT), p.title.as_str().into()])
        }
        _ if crate::dl_pending() > 0 => {
            let (n, paused) = (crate::dl_pending(), ani::paused());
            let left = match paused {
                true => format!(" ‖ paused · {n} left "),
                false => format!(" {} ↓ {n} left ", spinner(app)),
            };
            // paused, the speed in its last line is stale
            let now = (app.dl_now.as_ref()).map_or(String::new(), |d| match paused {
                true => [d.label.clone()]
                    .into_iter()
                    .chain(crate::got_so_far(&d.text).filter(|_| d.resumes))
                    .collect::<Vec<_>>()
                    .join(" · "),
                false => format!("{} · {}", d.label, d.text),
            });
            // cut before the key hints, with a column between
            let room =
                (area.width as usize).saturating_sub(hints.width() + Span::raw(&left).width() + 1);
            Line::from(vec![
                Span::styled(left, ACCENT),
                Span::styled(fit(now, room), DIM),
            ])
        }
        _ => Line::default(),
    };
    // a toast that doesn't fit beside the hints gets the line until it fades
    let crowded = toast.is_some() && status.width() + hints.width() > area.width as usize;
    let hints_width = if crowded { 0 } else { hints.width() as u16 };
    let [status_area, hints_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hints_width)]).areas(area);
    f.render_widget(status, status_area);
    f.render_widget(hints, hints_area);
}

/// `s` cut to `room` columns with "…", measured as drawn: CJK characters take two.
fn fit(mut s: String, room: usize) -> String {
    if Span::raw(s.as_str()).width() > room {
        while Span::raw(s.as_str()).width() >= room && s.pop().is_some() {}
        s.push('…');
    }
    s
}

fn hex(s: &str) -> Option<Color> {
    let v = u32::from_str_radix(s.strip_prefix('#')?, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// AniList enums like TV_SHORT or NOT_YET_RELEASED, for humans.
fn pretty(s: &str) -> String {
    match s {
        "RELEASING" => "Airing".into(),
        "NOT_YET_RELEASED" => "Upcoming".into(),
        "TV_SHORT" => "TV Short".into(),
        s if s.len() <= 3 => s.into(),
        s => s[..1].to_string() + &s[1..].to_lowercase().replace('_', " "),
    }
}

fn duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{h}h {m}m")
    }
}

fn strip_html(s: &str) -> String {
    let s = s
        .replace(['\r', '\n'], "")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&mdash;", "—")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_anilist_descriptions() {
        assert_eq!(
            strip_html("A <i>mage</i>.<br><br>\n<br>(Source: X)"),
            "A mage.\n\n(Source: X)"
        );
        assert_eq!(pretty("NOT_YET_RELEASED"), "Upcoming");
        assert_eq!(pretty("SPECIAL"), "Special");
        assert_eq!(pretty("ONA"), "ONA");
    }

    #[test]
    fn cuts_hints_by_width_not_chars() {
        // ~/ビデオ is the Japanese videos folder: 11 chars, 14 columns
        assert_eq!(fit("~/ビデオ/Anime".into(), 11), "~/ビデオ/A…");
        assert_eq!(fit("~/ビデオ/Anime".into(), 14), "~/ビデオ/Anime");
        assert_eq!(fit("on".into(), 0), "…");
    }
}
