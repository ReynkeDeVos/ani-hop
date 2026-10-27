use crate::anilist::Media;
use crate::kitty;
use crate::{App, EpMode, Input, PIN_REDIRECT, Pending, View, ani, list_label};
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
/// the row under the cursor
const SELECTED: Style = Style::new()
    .add_modifier(Modifier::BOLD)
    .remove_modifier(Modifier::DIM);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 2] = ["▄▀█ █▄ █ █ ▄▄ █ █ █▀█ █▀█", "█▀█ █ ▀█ █    █▀█ █▄█ █▀▀"];
const SHADES: [&str; 4] = ["░", "▒", "▓", "█"];
const BOOT_MS: u128 = 500;
const GLINT_MS: u128 = 450;
const POP_MS: u128 = 160;

pub fn render(app: &mut App, f: &mut Frame) {
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
fn paused(n: &crate::downloads::Now) -> String {
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
    // what "Continue with" on a show's page plays
    let next = match &app.stack.last().unwrap().view {
        View::Show(m) => Some(app.progress(m).next().to_string()),
        _ => None,
    };
    let unaired =
        matches!(&app.stack.last().unwrap().view, View::Episodes(m, _) if app.upcoming(m));
    let screen = app.stack.last_mut().unwrap();
    let block = panel();
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    // a confirmation shows its question where the filter prompt would be, wrapped
    let question = matches!(screen.view, View::Confirm(_)).then_some(screen.title.as_str());
    let rows = question.map_or(1, |q| {
        let q = Paragraph::new(Line::styled(q, Style::new().bold())).wrap(Wrap { trim: true });
        (q.line_count(inner.width) as u16).clamp(1, 6)
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
    let varies = matches!(
        screen.view,
        View::Shows(..) | View::Episodes(..) | View::Sources(..)
    );
    let count = match m.query.is_empty() {
        true if !varies => String::new(), // a fixed menu: its rule alone
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
    // a gap only after something: a fixed menu's rule starts where its rows do
    let gap = if used > 0 { " " } else { "" };
    let line = "─".repeat((info_area.width as usize).saturating_sub(used + gap.len()));
    info.push(Span::styled(format!("{gap}{line}"), DIM));
    f.render_widget(Line::from(info), info_area);
    // an empty list says so, instead of a bare 0
    let empty = match &screen.view {
        _ if !m.items.is_empty() || screen.loading => None,
        View::Shows(list) if list.is_local() => Some("Nothing downloaded yet"),
        View::Shows(list) if list.is_search() => Some("No shows found"),
        View::Shows(..) => Some("Nothing on this list"),
        View::Episodes(..) if unaired => Some("Not out yet"),
        View::Episodes(..) => Some("No episodes found"),
        // its download row goes when none of its shows is out
        View::Bulk(_) => Some("None of them is out yet"),
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
            View::Show(m) if m.id == p.id && next.as_ref() == Some(&p.ep) => "next".into(),
            _ => return None,
        };
        Some((
            key,
            p.step
                .map_or("▶ playing".into(), |s| format!("{spin} {s}…")),
        ))
    });
    // and the next page right on "More…"
    let fetching = matches!(&screen.view, View::Shows(list) if list.fetching());
    let busy = busy.or_else(|| fetching.then(|| ("more".to_string(), format!("{spin} loading…"))));
    // and the episode downloading right now on its row, or how far it got when paused
    let dl = app.downloads.snapshot();
    let busy = busy.or_else(|| match (&screen.view, &dl.now) {
        (View::Episodes(m, _), Some(n)) if m.id == n.id => Some((
            n.ep.clone(),
            match dl.paused {
                true => paused(n),
                false => format!("↓ {}", n.text),
            },
        )),
        _ => None,
    });
    let width = list_area.width.saturating_sub(1) as usize; // minus the pointer column
    let selected = m.state.selected();
    let rows: Vec<ListItem> = (m.matches.iter().enumerate())
        .map(|(k, (i, hits))| {
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
            let mut hint = hint;
            while Span::raw(hint.as_str()).width() > room
                && let Some((head, _)) = hint.rsplit_once(" · ")
            {
                hint = head.to_string();
            }
            // a hint cut to less than half says nothing ("not l…", "loses pr…"), so it goes
            let w = Span::raw(hint.as_str()).width();
            if !hint.is_empty() && (w <= room || room > 3 && room * 2 >= w) {
                let hint = Span::styled(fit(hint, room), color);
                spans.push(" ".repeat(width.saturating_sub(used + hint.width())).into());
                spans.push(hint);
            }
            // its row only, not a rule under it
            if selected == Some(k) {
                for s in &mut spans {
                    s.style = s.style.patch(SELECTED);
                }
            }
            let mut lines = vec![Line::from(spans)];
            // under the row, so the cursor stays on the episode
            if let Some((next, _)) = m.matches.get(k + 1)
                && m.items[*next].group != item.group
            {
                lines.push(rule(&m.items[*next].group, width));
            }
            ListItem::new(lines)
        })
        .collect();
    let list = List::new(rows)
        .highlight_symbol(Line::styled("▌", ACCENT))
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, list_area, &mut screen.menu.state);
}

/// A faint line where a list's next group starts, with its name if it has one.
fn rule(name: &str, width: usize) -> Line<'static> {
    // in line with the labels, past the check column
    let head = match name {
        "" => "  ".to_string(),
        _ => format!("  ── {} ", fit(name.to_string(), width.saturating_sub(8))),
    };
    let rest = width.saturating_sub(Span::raw(head.as_str()).width());
    Line::styled(head + &"─".repeat(rest), DIM)
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
            "".into(),
            "Wrong client ID? Press enter with this empty.".into(),
        ],
        Input::SourceQuery(_) => vec![
            "Search ani-cli with this text, then pick the".into(),
            "right entry. ani-hop remembers the choice.".into(),
        ],
        Input::DownloadDir => vec![
            "Where new downloads go. ~ works.".into(),
            format!("Empty goes back to {}.", ani::tilde(&ani::default_root())).into(),
            "".into(),
            "Shows downloaded before keep their folder,".into(),
            "new episodes of them too.".into(),
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
        View::Shows(list) => screen
            .menu
            .current()
            .and_then(|it| list.shows.iter().find(|m| m.id.to_string() == it.key))
            .cloned(),
        View::Show(m)
        | View::Episodes(m, _)
        | View::DlChoice(m, _)
        | View::Status(m)
        | View::Sources(m, ..)
        | View::Input(Input::SourceQuery(m))
        | View::Audio(Some(m))
        | View::Confirm(Pending::Remove(m) | Pending::Stop(m, _) | Pending::Pause(m, _)) => {
            Some(m.clone())
        }
        View::Confirm(Pending::Progress(mark)) => Some(mark.entry.clone()),
        _ => None,
    };
    // posters just below the cursor load ahead, so scrolling shows them instantly
    let ahead: Vec<Media> = match &screen.view {
        View::Shows(list) => {
            let cur = screen.menu.state.selected().unwrap_or(0);
            screen
                .menu
                .matches
                .iter()
                .skip(cur + 1)
                .take(6)
                .filter_map(|(i, _)| list.shows.get(*i).cloned())
                .collect()
        }
        // and the sequels an episode list runs on into, whose posters its episodes show
        View::Episodes(m, _) => app.parts(m).iter().map(|(_, p)| p.clone()).collect(),
        _ => Vec::new(),
    };
    // episode titles load while the show is open, so its episode list has them right away
    let shows_eps = matches!(screen.view, View::Show(_) | View::Episodes(..));
    let ep = match &screen.view {
        View::Episodes(..) => screen.menu.current(),
        _ => None,
    };
    let ep = ep.map(|it| it.key.clone());
    // another show or episode, or none, starts at its top
    let shown = media.as_ref().map(|m| (m.id, ep.clone()));
    if app.scroll.0 != shown {
        app.scroll = (shown, 0);
    }
    if let Some(m) = media {
        if shows_eps {
            app.want_episodes(&m);
        }
        let (up, down) = media_preview(app, f, inner, &m, ep.as_deref());
        // a cut is never silent: the border says how to read on, and how to get back up
        let hints = [
            (up, area.y, "shift ↑", " back up "),
            (
                down,
                area.bottom().saturating_sub(1),
                "shift ↓",
                " read on ",
            ),
        ];
        for (_, y, key, what) in hints.into_iter().filter(|h| h.0) {
            let row = Rect::new(area.x, y, area.width, 1).inner(Margin::new(2, 0));
            // a narrow border drops the words, then "shift"
            let arrow = key["shift".len()..].trim_start();
            let hint = [(key, what), (key, " "), (arrow, " ")]
                .map(|(k, w)| {
                    Line::from(vec![
                        " ".into(),
                        Span::styled(k, ACCENT),
                        Span::styled(w, DIM),
                    ])
                })
                .into_iter()
                .find(|h| h.width() <= row.width as usize);
            if let Some(hint) = hint {
                f.render_widget(hint.right_aligned(), row);
            }
        }
        return ahead.iter().for_each(|m| app.want_cover(m, false));
    }
    let lines: Vec<Line> = match &screen.view {
        View::Bulk(list) => list
            .iter()
            .map(|m| {
                let mut line = Line::from(format!("• {}", m.name()));
                if app.upcoming(m) {
                    line.push_span(Span::styled(" · not out yet", DIM));
                }
                line
            })
            .collect(),
        // a show's files under its folder, as one of the user's own may be among them
        View::Confirm(Pending::Delete(paths)) => {
            let name = |p: &std::path::Path| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            };
            (paths.iter())
                .flat_map(|p| {
                    let files = match p.is_dir() {
                        true => ani::show_files(p),
                        false => Vec::new(),
                    };
                    let head = Line::from(format!("✗ {}", name(p)));
                    let files = files
                        .into_iter()
                        .map(move |f| Line::styled(format!("    {}", name(&f)), DIM));
                    std::iter::once(head).chain(files)
                })
                .collect()
        }
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
        _ => banner(app, inner),
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
        let mark = match good || !(ani::optional(name) || name == "graphics") {
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
        "posters: Ghostty or kitty",
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

/// Score, format, episode count and status; when and by whom; genres. No-break spaces keep
/// "12 episodes" or "Slice of Life" whole, and a wrapped line never starts with "·".
fn facts(m: &Media) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut facts = Vec::new();
    if let Some(s) = m.average_score {
        facts.push(Span::styled(format!("★\u{a0}{s}%"), Color::Yellow));
    }
    // a movie's one episode goes without saying
    let movie = m.format.as_deref() == Some("MOVIE");
    let detail = [
        m.format.as_deref().map(pretty),
        m.episodes
            .filter(|&e| !(movie && e == 1))
            .map(|e| episodes(e as usize)),
        // a list cached before it started airing still says it's upcoming
        (m.status.as_deref()).map(|s| match s {
            "NOT_YET_RELEASED" if m.released() > Some(0) => pretty("RELEASING"),
            s => pretty(s),
        }),
    ];
    for d in detail.into_iter().flatten() {
        if !facts.is_empty() {
            facts.push(Span::styled("\u{a0}· ", DIM));
        }
        facts.push(d.into());
    }
    // ani-cli's shows have none of these: no rows of blanks for them
    if !facts.is_empty() {
        lines.push(Line::from(facts));
    }
    let when = [
        m.season.as_deref().map(pretty),
        m.season_year.map(|y| y.to_string()),
    ];
    let when = when
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\u{a0}");
    let studio = m.studios.main().collect::<Vec<_>>().join(", ");
    let made = [when, studio].into_iter().filter(|s| !s.is_empty());
    let made = made.collect::<Vec<_>>().join("\u{a0}· ");
    if !made.is_empty() {
        lines.push(made.into());
    }
    if !m.genres.is_empty() {
        let genres = m.genres.iter().map(|g| g.replace(' ', "\u{a0}"));
        lines.push(Line::styled(
            genres.collect::<Vec<_>>().join("\u{a0}· "),
            MATCH,
        ));
    }
    lines
}

/// "1 episode", "28 episodes", kept on one line.
fn episodes(n: usize) -> String {
    crate::plural(n, "episode").replacen(' ', "\u{a0}", 1)
}

/// Whether ani-cli's title for a source is the show's own, letters and digits alike: then
/// the source line needn't repeat it.
fn same_title(source: &str, show: &Media) -> bool {
    let key = |s: &str| {
        (s.chars().filter(|c| c.is_alphanumeric()))
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let titles = [&show.title.english, &show.title.romaji]
        .into_iter()
        .flatten();
    (titles.map(String::as_str).chain([show.name()])).any(|t| key(t) == key(source))
}

/// "1999-12-15" as "15 Dec 1999", anything else as it is.
fn day(iso: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut p = iso.splitn(3, '-').map(|n| n.parse::<usize>().ok());
    match (p.next().flatten(), p.next().flatten(), p.next().flatten()) {
        (Some(y), Some(m @ 1..=12), Some(d)) => format!("{d} {} {y}", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

/// The start panel in `area`: the logo only where everything else fits under it, as a short
/// screen keeps an update notice (two or three rows at 80 columns) and every key instead.
fn banner(app: &App, area: Rect) -> Vec<Line<'static>> {
    // a key, what it does, and the other keys that do the same
    let key = |k: &'static str, what: &'static str, also: &'static str| {
        // padded only beside other keys: trailing spaces would wrap at 80 columns
        let (what, also) = match also {
            "" => (what.to_string(), String::new()),
            _ => (format!("{what:<18}"), format!("{also:>8}")),
        };
        Line::from(vec![
            Span::styled(format!("{k:>10}  "), ACCENT),
            Span::raw(what),
            Span::styled(also, DIM),
        ])
    };
    let mut v = vec![Line::styled(
        format!("↓ downloads go to {}", ani::tilde(&ani::download_root())),
        DIM,
    )];
    if let Some((new, cmd)) = &app.release {
        v.push(Line::styled(
            format!("↑ ani-hop {new} is out: {cmd}"),
            ACCENT,
        ));
    }
    // on an input typing is the text itself, and there's nothing to mark
    if !matches!(app.stack.last().unwrap().view, View::Input(_)) {
        // grouped by what they're for, the everyday ones first; the arrows lead, as esc
        // clears a filter before it goes back
        v.extend([
            "".into(),
            key("↑ ↓", "move", "^k ^j"),
            key("→", "select", "enter ^l"),
            key("←", "back", "^h"),
            "".into(),
            key("type", "filter the list", ""),
            Line::styled(format!("{:12}'exact ^start end$ !not", ""), DIM),
            key("esc", "clear the filter, then back", ""),
            "".into(),
            key("tab", "mark for bulk actions", ""),
            key("^a", "mark all", ""),
            key("^f", "load more shows", ""),
            key("shift ↑↓", "scroll the preview", ""),
            key("^c", "quit", ""),
        ]);
    }
    let below = Paragraph::new(v.clone()).wrap(Wrap { trim: false });
    let logo_h = LOGO.len() + 1; // and the blank line under it
    if below.line_count(area.width) + logo_h <= area.height as usize {
        v.splice(0..0, logo().chain(["".into()]));
    }
    v
}

/// A show's poster with its facts beside it, and its description flowing around it. What
/// doesn't fit pages with shift+↓↑; returns whether there's more (above, below).
fn media_preview(
    app: &mut App,
    f: &mut Frame,
    area: Rect,
    show: &Media,
    ep: Option<&str>,
) -> (bool, bool) {
    // on an episode of a sequel the provider runs on into, it's that sequel's poster, facts,
    // status and description; what's on disk, ani-cli's entry and the episode stay the show's
    let part = ep.map(|ep| app.progress(show).entry(ep).0.clone());
    let part = part.filter(|p| p.id != show.id);
    let m = part.as_ref().unwrap_or(show);
    let rows = (area.height * 3 / 5).clamp(6, 26).min(area.height);
    // font zoom changes the pixels per cell, so they're read every frame
    let size = app.posters.fit(rows, area.width / 2, kitty::cell_px());
    app.want_cover(m, true);

    let [poster, _, info] = Layout::horizontal([
        Constraint::Length(size.width),
        Constraint::Length(2),
        Constraint::Fill(1),
    ])
    .areas(Rect::new(area.x, area.y, area.width, rows));
    let accent = m
        .cover_image
        .color
        .as_deref()
        .and_then(hex)
        .unwrap_or(ACCENT);
    if app.glint.0 != m.id {
        app.glint = (m.id, std::time::Instant::now());
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
    // on an episode list the show's facts are one esc away, and its episode comes first
    let facts_from = lines.len();
    if ep.is_none() {
        lines.extend(facts(m));
    }
    if !m.on_anilist() {
        lines.push(Line::styled(
            "Found through ani-cli: no AniList details or progress sync",
            Color::Yellow,
        ));
    }
    if lines.len() > facts_from {
        lines.push("".into());
    }
    let status = |p: &Media| {
        let e = p.media_list_entry.as_ref()?;
        let total = p.episodes.map_or("?".into(), |t| t.to_string());
        let line = format!("● {} · {}/{total}", list_label(&e.status), e.progress);
        Some(Line::styled(line, Color::Green))
    };
    lines.extend(status(m));
    // the provider's episodes past the show's own count toward a sequel on AniList; an
    // episode list says so with a rule between them (ui::menu)
    let run = app.progress(show);
    let parts = if ep.is_none() { run.parts() } else { &[] };
    for (before, p) in parts {
        let eps = p.episodes.map_or(format!("{}+", before + 1), |n| {
            format!("{}–{}", before + 1, before + n)
        });
        // as the episode list's rule calls it: "Part 2"
        let name = run.part_name(p);
        // "Part 2" wraps whole; a long name still wraps, just not before its last word
        let name = match name.rsplit_once(' ') {
            Some((head, last)) => format!("{head}\u{a0}{last}"),
            None => name.to_string(),
        };
        lines.push(Line::styled(format!("Episodes\u{a0}{eps} are {name}"), DIM));
        lines.extend(status(p));
    }
    if let Some(a) = &m.next_airing_episode
        && let Some(left) = a.left(crate::anilist::now())
    {
        lines.push(Line::from(format!(
            "Episode {} airs in {}",
            a.episode,
            duration(left)
        )));
    }
    // an episode list says "on disk" on each of its rows
    let local = app.local.get(&show.id).map_or(0, Vec::len);
    if local > 0 && ep.is_none() {
        lines.push(Line::from(format!("↓ {local} on disk")));
    }
    if let Some(s) = app.sources.get(show.id) {
        let n = episodes(s.episodes.len());
        // a wrong match keeps its title, so it stands out
        let line = match same_title(&s.title, m) {
            true => format!("ani-cli: {n}"),
            false => format!("ani-cli: {} · {n}", s.title),
        };
        lines.push(Line::styled(line, DIM));
    }
    let mut about = "";
    if let Some(ep) = ep
        && let Some(i) = app.ep_about(show, ep)
    {
        lines.push("".into());
        // its title and summary only once watched: they give things away, its date doesn't
        let watched = app.progress(show).watched(ep);
        let heading = match watched && !i.title.is_empty() {
            true => format!("Episode {ep} · {}", i.title),
            false => format!("Episode {ep}"),
        };
        lines.push(Line::styled(heading, Style::new().bold()));
        if let Some(d) = i.aired.as_deref().filter(|d| !d.is_empty()) {
            lines.push(Line::styled(format!("aired {}", day(d)), DIM));
        }
        if watched {
            about = &i.about;
        }
    }
    // the gap before the description is one row, also when nothing follows the genres
    if lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    let head = Paragraph::new(lines).wrap(Wrap { trim: true });
    let head_h = head.line_count(info.width) as u16;

    // the episode's summary right under its title, then the description: beside the poster
    // (and in the row under it) first, where the facts leave rows empty, then full width
    let desc = m.description.as_deref().map(strip_html).unwrap_or_default();
    let text = [(about, SOFT), (desc.as_str(), Style::new())];
    let text: Vec<_> = text.into_iter().filter(|t| !t.0.is_empty()).collect();
    let top = head_h + u16::from(about.is_empty());
    let narrow = (rows + 1).saturating_sub(top) as usize;
    let mut text = flow(&text, narrow, info.width, area.width);
    // shortcut: rows end at u16::MAX, which no AniList description gets near even one column
    // wide; page the text in parts if one ever does
    text.truncate(usize::from((u16::MAX - area.y).saturating_sub(top)));
    let end = match text.len() {
        0 => head_h,
        n => top + n as u16,
    };

    // drawn whole, then the rows that are read now go on screen: placeholder cells carry
    // their own poster row, so a poster scrolled halfway shows its lower half
    let page_h = end.max(rows);
    let page_area = Rect::new(area.x, area.y, area.width, page_h.max(area.height));
    let mut page = Buffer::empty(page_area);
    // the cover's dominant color stands in while it loads
    if !app.posters.draw(m.id, poster, &mut page) {
        Block::new().bg(accent).render(poster, &mut page);
    }
    let head_area = Rect::new(info.x, info.y, info.width, head_h);
    head.render(head_area, &mut page);
    // only true RGB titles: a named color has no RGB value to fade back into
    if let Color::Rgb(r, g, b) = accent {
        glint(app, &mut page, head_area, (r, g, b));
    }
    for (i, line) in text.into_iter().enumerate() {
        let column = if i < narrow { info } else { area };
        let y = area.y + top + i as u16;
        line.render(Rect::new(column.x, y, column.width, 1), &mut page);
    }

    // a page is the pane less one row, so the last line read stays on screen
    let step = area.height.saturating_sub(1).max(1);
    let max = page_h.saturating_sub(area.height);
    let n = &mut app.scroll.1;
    *n = (*n).min(max.div_ceil(step));
    let off = (*n * step).min(max);
    let buf = f.buffer_mut();
    for pos in area.positions() {
        buf[pos] = page[(pos.x, pos.y + off)].clone();
    }
    (off > 0, off < max)
}

/// Wraps paragraphs word by word like Paragraph does, the first `narrow` lines to `first`
/// columns and the rest to `rest`, with a blank line between the parts of `text`. A word
/// wider than its line is broken between characters (graphemes), so text without spaces
/// (Japanese, a URL) is never cut.
fn flow(text: &[(&str, Style)], narrow: usize, first: u16, rest: u16) -> Vec<Line<'static>> {
    let width = |s: &str| Span::raw(s).width();
    let room = |n: usize| usize::from(if n < narrow { first } else { rest.max(1) });
    let mut out = Vec::new();
    for (i, &(s, style)) in text.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        for para in s.lines() {
            // the last narrow line is the row under the poster: a paragraph starting there
            // would look indented, so it starts full width below it
            if out.len() + 1 == narrow && !para.trim().is_empty() {
                out.push(Line::default());
            }
            let mut line = String::new();
            // no-break spaces stay inside a word, like in the facts
            for word in para.split(' ').filter(|w| !w.is_empty()) {
                if !line.is_empty() {
                    if width(&line) + 1 + width(word) <= room(out.len()) {
                        line.push(' ');
                        line.push_str(word);
                        continue;
                    }
                    out.push(Line::styled(std::mem::take(&mut line), style));
                }
                let word = Span::raw(word);
                for g in word.styled_graphemes(style).map(|g| g.symbol) {
                    if !line.is_empty() && width(&line) + width(g) > room(out.len()) {
                        out.push(Line::styled(std::mem::take(&mut line), style));
                    }
                    // a narrow line too small for a wide character stays empty
                    while line.is_empty() && out.len() < narrow && width(g) > room(out.len()) {
                        out.push(Line::default());
                    }
                    line.push_str(g);
                }
            }
            out.push(Line::styled(line, style));
        }
    }
    out
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
    let mut keys = vec![("enter", enter)];
    // the main menu has nothing to go back to
    if app.stack.len() > 1 || !screen.menu.query.is_empty() {
        keys.push(("esc", esc));
    }
    if screen.menu.multi && !screen.menu.items.is_empty() {
        keys.splice(0..0, [("tab", "mark"), ("^a", "all")]);
    }
    if matches!(&screen.view, View::Shows(list) if list.more_page().is_some()) {
        let at = keys.iter().position(|k| k.0 == "enter").unwrap();
        keys.insert(at, ("^f", "more"));
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
    // read once: the worker can take the last one off between two reads
    let dl = app.downloads.snapshot();
    let n = dl.pending;
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
        _ if n > 0 => {
            // how far the one downloading got first, so that's what 80 columns keep, then
            // which one it is and what's after it
            let head = match (&dl.now, dl.paused) {
                (Some(d), true) => paused(d),
                (Some(d), false) => format!("{} ↓ {}", spinner(app), d.text),
                (None, true) => format!("‖ paused · {n} queued"),
                (None, false) => format!("{} ↓ {n} queued", spinner(app)),
            };
            let head = format!(" {head} ");
            let tail = (dl.now.as_ref()).map_or(String::new(), |d| match n - 1 {
                0 => format!("· {}", d.label),
                more => format!("· {} · {more} more queued", d.label),
            });
            // cut before the key hints, with a column between
            let room =
                (area.width as usize).saturating_sub(hints.width() + Span::raw(&head).width() + 1);
            Line::from(vec![
                Span::styled(head, ACCENT),
                Span::styled(fit(tail, room), DIM),
            ])
        }
        _ => Line::default(),
    };
    // a toast that doesn't fit beside the hints gets the line until it fades
    let crowded = toast.is_some() && status.width() + hints.width() > area.width as usize;
    let hints_width = if crowded { 0 } else { hints.width() as u16 };
    // a column between them, so a cut status line doesn't run into the keys
    let [status_area, hints_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hints_width)])
            .spacing(1)
            .areas(area);
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

/// "74d 6h", "5h 3m", kept on one line.
fn duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d\u{a0}{h}h")
    } else {
        format!("{h}h\u{a0}{m}m")
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
    fn a_source_with_the_shows_own_title_goes_without_it() {
        let mut m = Media::default();
        m.title.english = Some("Frieren: Beyond Journey’s End".into());
        m.title.romaji = Some("Sousou no Frieren".into());
        assert!(same_title("Frieren: Beyond Journey's End", &m)); // ’ and '
        assert!(same_title("SOUSOU NO FRIEREN", &m));
        assert!(!same_title("Aoki Hagane no Arpeggio: Ars Nova Cadenza", &m));
        assert!(!same_title("Frieren: Beyond Journey's End Season 2", &m));
        assert_eq!(episodes(1), "1\u{a0}episode");
    }

    #[test]
    fn a_show_that_aired_since_its_list_was_cached_is_airing() {
        let now = crate::anilist::now();
        let show = |at: Option<i64>| Media {
            format: Some("TV".into()),
            status: Some("NOT_YET_RELEASED".into()),
            next_airing_episode: at.map(|airing_at| crate::anilist::Airing {
                episode: 1,
                airing_at,
            }),
            ..Default::default()
        };
        let status = |m: Media| facts(&m)[0].to_string();
        assert!(status(show(Some(now - 90))).ends_with("Airing"));
        assert!(status(show(Some(now + 90))).ends_with("Upcoming"));
        assert!(
            status(show(Some(0))).ends_with("Upcoming"),
            "a cache from before 0.7"
        );
        assert!(status(show(None)).ends_with("Upcoming"));
    }

    #[test]
    fn writes_air_dates_out() {
        assert_eq!(day("1999-12-15"), "15 Dec 1999");
        assert_eq!(day("2026-01-03"), "3 Jan 2026");
        assert_eq!(day("2026-13-03"), "2026-13-03");
        assert_eq!(day("soon"), "soon");
    }

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
    fn flows_around_the_poster() {
        let text = |lines: Vec<Line>| lines.iter().map(Line::to_string).collect::<Vec<_>>();
        // two lines beside the poster, then full width; paragraphs and parts keep a blank
        let out = flow(
            &[("aa bb cc dd ee\n\nff", SOFT), ("gg", DIM.into())],
            2,
            5,
            8,
        );
        assert_eq!(
            text(out.clone()),
            ["aa bb", "cc dd", "ee", "", "ff", "", "gg"]
        );
        assert_eq!((out[0].style, out[6].style), (SOFT, DIM.into()));
        // words wider than their line are broken, by width: ビデオ is 6 columns
        let out = flow(&[("ビデオビデオ x", Style::new())], 2, 5, 4);
        assert_eq!(text(out), ["ビデ", "オビ", "デオ", "x"]);
        assert_eq!(
            text(flow(&[("a\u{a0}b c", Style::new())], 0, 1, 3)),
            ["a\u{a0}b", "c"]
        );
        assert!(flow(&[], 3, 5, 8).is_empty());
        // a narrow line too small for a wide character is skipped; joined emoji stay whole
        assert_eq!(text(flow(&[("ビデ", SOFT)], 2, 1, 4)), ["", "", "ビデ"]);
        assert_eq!(text(flow(&[("a ビ", SOFT)], 3, 0, 4)), ["", "", "", "a ビ"]);
        let family = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(text(flow(&[(family, SOFT)], 0, 9, 2)), [family]);
        // no paragraph starts on the last narrow line, under the poster
        assert_eq!(text(flow(&[("aa bb", SOFT)], 1, 2, 8)), ["", "aa bb"]);
        let out = flow(&[("aa", SOFT), ("bb", SOFT)], 3, 5, 8);
        assert_eq!(text(out), ["aa", "", "", "bb"]);
    }

    #[test]
    fn a_rule_names_the_next_group_across_the_row() {
        let text = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let named = text(super::rule("Season 3 Part 2", 30));
        assert_eq!(named, "  ── Season 3 Part 2 ─────────");
        assert_eq!(named.chars().count(), 30);
        assert_eq!(text(super::rule("", 6)), "  ────");
        let cut = text(super::rule("Attack on Titan Season 3 Part 2", 20));
        assert!(cut.starts_with("  ── Attack on") && cut.contains('…'));
        assert_eq!(cut.chars().count(), 20);
    }

    #[test]
    fn cuts_hints_by_width_not_chars() {
        // ~/ビデオ is the Japanese videos folder: 11 chars, 14 columns
        assert_eq!(fit("~/ビデオ/Anime".into(), 11), "~/ビデオ/A…");
        assert_eq!(fit("~/ビデオ/Anime".into(), 14), "~/ビデオ/Anime");
        assert_eq!(fit("on".into(), 0), "…");
    }
}
