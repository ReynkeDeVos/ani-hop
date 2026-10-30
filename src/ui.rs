use crate::anilist::Media;
use crate::kitty;
use crate::preview::{self, Shown};
use crate::{App, EpMode, Input, PIN_REDIRECT, Pending, View, ani};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, Paragraph, Wrap,
};

// ANSI colors on purpose: they follow the terminal (and Omarchy) theme.
pub const ACCENT: Color = Color::Magenta;
pub const MATCH: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
/// Text to read that steps back: faint main text is easier on the eyes than DIM in most
/// themes (Omarchy's color 7 is the main text itself, so there's no ANSI shade between).
pub const SOFT: Style = Style::new().add_modifier(Modifier::DIM);
/// the row under the cursor
const SELECTED: Style = Style::new()
    .add_modifier(Modifier::BOLD)
    .remove_modifier(Modifier::DIM);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 2] = ["▄▀█ █▄ █ █ ▄▄ █ █ █▀█ █▀█", "█▀█ █ ▀█ █    █▀█ █▄█ █▀▀"];
const SHADES: [&str; 4] = ["░", "▒", "▓", "█"];
const BOOT_MS: u128 = 500;
const POP_MS: u128 = 160;

pub fn render(app: &mut App, f: &mut Frame) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let screen = app.stack.last().unwrap();
    let (left, right) = split(body, question(screen).filter(|_| widens(&screen.view)));
    header(app, f, head);
    menu(app, f, left);
    if let Some(right) = right {
        preview(app, f, right);
    }
    footer(app, f, foot);
    starting(app, f);
    boot(app, f.buffer_mut());
}

// Effects never hide content for long: speed first, the shine comes on top.
pub fn effects_running(app: &App) -> bool {
    app.started.elapsed().as_millis() < BOOT_MS
        || app.preview.animating()
        || app.playback.now().is_some_and(|p| p.step.is_some())
}

/// While an episode starts, a box pops up in the middle with a running bar, so enter
/// visibly did something at once. It goes when mpv shows the episode.
fn starting(app: &App, f: &mut Frame) {
    let Some((p, step)) = app.playback.now().and_then(|p| Some((p, p.step?))) else {
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
pub fn swept(t: f32, x: u16, width: u16, band: f32) -> f32 {
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
            .spacing(1)
            .areas(area);
    // newest crumbs first; whatever doesn't fit folds into "…", and the current one is cut
    // in the middle, keeping a season's or part's name
    let crumbs: Vec<&str> = app.stack.iter().skip(1).rev().map(crumb).collect();
    let mut room = (crumbs_area.width as usize).saturating_sub(10);
    let mut shown: Vec<String> = Vec::new();
    for (i, t) in crumbs.iter().enumerate() {
        // room for " › …" while older ones follow
        let fold = if i + 1 < crumbs.len() { 4 } else { 0 };
        let t = match i {
            0 => fit_middle(t, room.saturating_sub(3 + fold)),
            _ if Span::raw(*t).width() + 3 + fold > room => {
                shown.push("…".into());
                break;
            }
            _ => t.to_string(),
        };
        room = room.saturating_sub(Span::raw(t.as_str()).width() + 3);
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

/// The question a confirmation asks, and how many answers it has.
fn question(screen: &crate::Screen) -> Option<(&str, usize)> {
    let asks = matches!(screen.view, View::Confirm(_));
    asks.then_some((screen.title.as_str(), screen.menu.items.len()))
}

/// Where the menu's rows go in its panel at `area`.
fn menu_inner(area: Rect) -> Rect {
    panel().inner(area).inner(Margin::new(1, 0))
}

/// Rows question `q` gets in `inner`: what it wraps to, but leaving the rule under it and
/// its `answers` on screen; and whether that's all of it.
fn question_rows(q: &str, answers: usize, inner: Rect) -> (u16, bool) {
    let q = Paragraph::new(Line::styled(q, Style::new().bold())).wrap(Wrap { trim: true });
    let need = q.line_count(inner.width) as u16;
    let room = inner.height.saturating_sub(1 + answers as u16).max(1);
    (need.clamp(1, room), need <= room)
}

/// Whether a question too long beside the preview may take its place: not one whose preview
/// lists what it acts on (the files Delete takes, the downloads Resume picks up).
fn widens(view: &View) -> bool {
    !matches!(
        view,
        View::Confirm(Pending::Delete(_) | Pending::Resume(..))
    )
}

/// The menu's side of `body`, and the preview's: a question too long for its side takes the
/// whole width, so what it changes reads in full before it's answered.
fn split(body: Rect, asked: Option<(&str, usize)>) -> (Rect, Option<Rect>) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(body);
    match asked {
        Some((q, answers)) if !question_rows(q, answers, menu_inner(left)).1 => (body, None),
        _ => (left, Some(right)),
    }
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
    // the episode downloading right now on its row, or how far it got when paused
    let dl_row = match &app.stack.last().unwrap().view {
        View::Episodes(m, _) => app.show_downloads(m).row(),
        _ => None,
    };
    let screen = app.stack.last_mut().unwrap();
    let block = panel();
    let inner = menu_inner(area);
    f.render_widget(block, area);
    // a confirmation shows its question where the filter prompt would be, wrapped
    let asked = question(screen);
    let rows = asked.map_or(1, |(q, answers)| question_rows(q, answers, inner).0);
    let question = asked.map(|(q, _)| q);
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
        if m.items.is_empty() && screen.loading() {
            String::new()
        } else {
            count
        },
        DIM,
    )];
    if screen.loading() {
        info.push(Span::styled(format!(" {spin}"), ACCENT));
    }
    if screen.loading() && m.items.is_empty() {
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
        _ if !m.items.is_empty() || screen.loading() => None,
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
    let busy = app.playback.now().and_then(|p| {
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
    let busy = busy.or(dl_row);
    let width = list_area.width.saturating_sub(1).max(2) as usize; // minus the pointer column
    let titles = matches!(screen.view, View::Shows(..) | View::Sources(..));
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
            let plain = if item.done { SOFT } else { Style::new() };
            let room = titles.then_some(width - 2);
            spans.extend(row_label(&item.label, hits, plain, room));
            let mut used: usize = spans.iter().map(|s| s.width()).sum();
            // a title too long for the row ends in "…", like a cut hint
            if used > width {
                while used >= width
                    && let Some(s) = spans.pop()
                {
                    used -= s.width();
                }
                spans.push(Span::styled("…", plain));
                used += 1;
            }
            let room = width.saturating_sub(used + 2);
            let (hint, color) = match &busy {
                Some((key, text)) if *key == item.key => (text.clone(), ACCENT),
                _ => (item.hint.clone(), DIM),
            };
            let mut hint = hint;
            if Span::raw(hint.as_str()).width() > room && !item.short.is_empty() {
                hint = item.short.clone();
            }
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
        Input::Search => {
            // AniList takes the text as it is; fzf's syntax is for the list it finds
            let row = |typed: &'static str, does: &'static str| {
                let typed = format!("{typed:<11}");
                Line::from(vec![
                    Span::styled(typed, Style::new().fg(ACCENT).not_dim()),
                    does.into(),
                ])
            };
            vec![
                "Type a show's name in English or Japanese, or a nickname like aot, and press enter."
                    .into(),
                "".into(),
                "Then filter what it finds:".into(),
                row("'titan", "exact match"),
                row("^attack", "starts with"),
                row("titan$", "ends with"),
                row("!season", "leaves out"),
                row("fri | aot", "either one"),
            ]
        }
        // one line per step or idea, wrapped by the pane, as 80 columns leave it 29
        Input::ClientId => vec![
            "One-time setup: ani-hop needs your own AniList API client.".into(),
            "".into(),
            "1. On anilist.co/settings/developer, choose Create New Client.".into(),
            "2. Name it ani-hop, with this redirect URL:".into(),
            Line::styled(PIN_REDIRECT, Style::new().fg(ACCENT).not_dim()),
            "3. Paste its client ID here and press enter.".into(),
        ],
        Input::Token => vec![
            "Approve ani-hop in your browser, then paste the token AniList shows and press enter."
                .into(),
            "".into(),
            Line::styled(
                "Pasting the whole redirect URL works too.",
                Style::new().italic(),
            ),
            "".into(),
            "Wrong client ID? Press enter with this empty.".into(),
        ],
        Input::SourceQuery(_) => vec![
            "Search ani-cli with this text, then pick the right entry. ani-hop remembers it."
                .into(),
        ],
        Input::DownloadDir => vec![
            "Where downloads go. ~ works.".into(),
            format!("Empty goes back to {}.", ani::tilde(&ani::default_root())).into(),
            "".into(),
            "Shows downloaded before keep their folder, also for new episodes.".into(),
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
    let ep = match &screen.view {
        View::Episodes(..) => screen.menu.current().map(|it| it.key.clone()),
        _ => None,
    };
    app.preview.read(media.as_ref().map(|m| (m.id, ep.clone())));
    if let Some(m) = media {
        let page = preview::page(&Shown {
            show: &m,
            ep: ep.as_deref(),
            progress: app.progress(&m),
            entries: &app.entries,
            source: app.sources.get(m.id),
            on_disk: app.local.get(&m.id).map_or(0, Vec::len),
            info: ep.as_deref().and_then(|ep| app.ep_about(&m, ep)),
        });
        return app.preview.draw(&page, &ahead, f, area, inner);
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

fn footer(app: &App, f: &mut Frame, area: Rect) {
    let screen = app.stack.last().unwrap();
    let enter = app.enter_verb();
    // esc empties a typed filter (or a prefilled input) before it goes back
    let esc = if screen.menu.query.is_empty() {
        "back"
    } else {
        "clear"
    };
    // none when enter does nothing (episodes another ani-hop has queued)
    let mut keys: Vec<(&str, &str)> = enter.map(|v| ("enter", v)).into_iter().collect();
    // the main menu has nothing to go back to
    if app.stack.len() > 1 || !screen.menu.query.is_empty() {
        keys.push(("esc", esc));
    }
    if screen.menu.multi && !screen.menu.items.is_empty() {
        keys.splice(0..0, [("tab", "mark"), ("^a", "all")]);
    }
    if matches!(&screen.view, View::Shows(list) if list.more_page().is_some()) {
        let at = keys
            .iter()
            .position(|k| k.0 == "enter")
            .unwrap_or(keys.len());
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
    let status = match (toast, app.playback.now()) {
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
                (Some(d), true) => crate::show_downloads::paused(d),
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

/// A row's label, a span per grapheme as drawn (joined emoji stay whole), with the filter's
/// matches lit (`hits`, by grapheme, as the filter counts them). With `room`, a show's title too long for it
/// keeps its end, as seasons and parts of one show differ there: "Attack on Titan Fi…Special 2".
fn row_label(text: &str, hits: &[u32], plain: Style, room: Option<usize>) -> Vec<Span<'static>> {
    let cut = room.and_then(|room| middle_cut(text, room));
    let mut spans = Vec::new();
    let (mut at, mut i) = (0, 0u32);
    for g in Span::raw(text)
        .styled_graphemes(Style::new())
        .map(|g| g.symbol)
    {
        let hit = hits.binary_search(&i).is_ok();
        let style = if hit {
            Style::new().fg(MATCH).bold()
        } else {
            plain
        };
        match cut {
            Some((head, tail)) if at >= head && at < tail => {
                if at == head {
                    spans.push(Span::styled("…", plain));
                }
            }
            _ => spans.push(Span::styled(g.to_string(), style)),
        }
        (at, i) = (at + g.len(), i + 1);
    }
    spans
}

/// Where to cut `label` to fit `room` columns, keeping its start and its last two words (or
/// its last one), where a season or part says which it is: (end of the start, start of the
/// end) as byte offsets. None when it fits, or when that end takes over half the room.
fn middle_cut(label: &str, room: usize) -> Option<(usize, usize)> {
    let width = |s: &str| Span::raw(s).width();
    if width(label) <= room {
        return None;
    }
    let starts: Vec<usize> = label.match_indices(' ').map(|(i, _)| i + 1).collect();
    let ends = [starts.iter().rev().nth(1), starts.last()];
    let tail = *(ends.into_iter().flatten())
        .filter(|&&t| !label[t..].trim().is_empty())
        .find(|&&t| width(&label[t..]) <= room / 2)?;
    let mut room = room - width(&label[tail..]) - 1;
    // whole graphemes, as drawn: "❤️" is two characters and two columns
    let mut head = 0;
    for g in Span::raw(label)
        .styled_graphemes(Style::new())
        .map(|g| g.symbol)
    {
        let w = width(g);
        if w > room {
            break;
        }
        (room, head) = (room - w, head + g.len());
    }
    Some((label[..head].trim_end().len(), tail))
}

/// `label` cut in the middle to fit `room` (`middle_cut`), else at its end.
fn fit_middle(label: &str, room: usize) -> String {
    match middle_cut(label, room) {
        Some((head, tail)) => format!("{}…{}", &label[..head], &label[tail..]),
        None => fit(label.to_string(), room),
    }
}

/// `s` cut to `room` columns with "…", measured as drawn: CJK characters take two.
fn fit(s: String, room: usize) -> String {
    let width = |s: &str| Span::raw(s).width();
    if width(&s) <= room {
        return s;
    }
    // whole graphemes, as drawn, and a column for the "…"
    let (mut cut, mut used) = (String::new(), 0);
    for g in Span::raw(s.as_str())
        .styled_graphemes(Style::new())
        .map(|g| g.symbol)
    {
        used += width(g);
        if used >= room {
            break;
        }
        cut.push_str(g);
    }
    cut + "…"
}

pub fn hex(s: &str) -> Option<Color> {
    let v = u32::from_str_radix(s.strip_prefix('#')?, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_reads_in_full_before_its_answers() {
        let q = "Mark episode 2 of Attack on Titan Final Season Special 2 as watched? \
                 Unmarks 3–5 and moves it to Watching.";
        // the screen's body: all but the header and the footer
        let body = |w: u16, h: u16| Rect::new(0, 1, w, h - 2);
        // too long beside the preview in a small terminal: it takes the whole width
        let (menu, preview) = split(body(40, 12), Some((q, 2)));
        assert_eq!((menu, preview), (body(40, 12), None));
        assert_eq!(question_rows(q, 2, menu_inner(menu)), (4, true));
        // beside the preview where it fits
        let (menu, preview) = split(body(80, 24), Some((q, 2)));
        assert!(preview.is_some() && question_rows(q, 2, menu_inner(menu)).1);
        assert!(
            split(body(40, 12), None).1.is_some(),
            "other screens keep it"
        );
        // and questions whose preview lists what they act on
        assert!(!widens(&View::Confirm(Pending::Delete(Vec::new()))));
        assert!(widens(&View::Confirm(Pending::Remove(Media::default()))));
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

    #[test]
    fn rows_draw_whole_graphemes() {
        let family = "👩\u{200d}👩\u{200d}👧";
        let title = family.repeat(8) + " Final Part 2";
        let spans = row_label(&title, &[], Style::new(), Some(26));
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.ends_with("…Part 2") && Span::raw(text.as_str()).width() <= 26,
            "{text}"
        );
        assert!(
            spans.iter().any(|s| s.content == family),
            "a family stays one"
        );
        // what the filter matches is what lights up: "A", not the family before it
        let label = format!("{family} A");
        let item = crate::menu::Item::new("k", label.as_str(), "");
        let mut menu = crate::menu::Menu::new(vec![item], false);
        menu.edit(|q| *q = "A".into());
        let lit = row_label(&label, &menu.matches[0].1, Style::new(), None);
        let lit: Vec<&str> = (lit.iter().filter(|s| s.style.fg == Some(MATCH)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(lit, ["A"]);
        // cut at the end, emoji stay whole too
        assert_eq!(fit("❤️".repeat(30), 9), "❤️❤️❤️❤️…");
    }

    #[test]
    fn long_titles_keep_their_season() {
        let special = "Attack on Titan Final Season THE FINAL CHAPTERS Special 2";
        assert_eq!(fit_middle(special, 26), "Attack on Titan…Special 2");
        let part = "Attack on Titan Final Season Part 2";
        assert_eq!(fit_middle(part, 26), "Attack on Titan Fin…Part 2");
        assert_eq!(fit_middle("Frieren", 26), "Frieren");
        // without spaces there's no season to keep: cut at the end
        assert_eq!(fit_middle("ビデオビデオビデオビデオ", 9), "ビデオビ…");
        // no room at all, or a trailing space, is no season either
        assert_eq!(fit_middle("Title ", 0), "…");
        // emoji are cut whole and measured as drawn
        let hearts = "❤️".repeat(20) + " Final Part 2";
        let cut = fit_middle(&hearts, 26);
        assert!(
            cut.ends_with("…Part 2") && Span::raw(cut.as_str()).width() <= 26,
            "{cut}"
        );
    }
}
