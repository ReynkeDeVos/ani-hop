use crate::anilist::Media;
use crate::{App, Cover, Input, PIN_REDIRECT, Pending, View, ani, list_label};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, HighlightSpacing, List, ListItem, Paragraph, Wrap};
use ratatui_image::Image;
use ratatui_image::picker::Capability;
use std::time::{Duration, Instant};
use tachyonfx::fx::{self, EvolveSymbolSet};
use tachyonfx::pattern::SweepPattern;
use tachyonfx::{CellFilter, Effect, Interpolation};

// ANSI colors on purpose: they follow the terminal (and Omarchy) theme.
const ACCENT: Color = Color::Magenta;
const MATCH: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 2] = ["▄▀█ █▄ █ █ ▄▄ █ █ █▀█ █▀█", "█▀█ █ ▀█ █    █▀█ █▄█ █▀▀"];

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
    // effects run on top of the finished frame; long idle gaps count as one frame
    let dt = app.last_frame.elapsed().min(Duration::from_millis(50));
    app.last_frame = Instant::now();
    let screen = f.area();
    app.fx.process_effects(dt.into(), f.buffer_mut(), screen);
}

// Effects never hide content: speed first, the shine comes on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fx {
    #[default]
    Boot,
    Glint,
}

/// Effects must never touch poster cells: the Kitty image travels inside the first
/// placeholder cell and is only sent once.
fn not_image() -> CellFilter {
    CellFilter::AllOf(vec![
        CellFilter::NonEmpty,
        CellFilter::eval_cell(|c| !c.symbol().contains('\u{10EEEE}')),
    ])
}

/// The UI materializes through ░▒▓█, left to right; the menu is usable from frame one.
pub fn boot_fx() -> Effect {
    fx::evolve_into(EvolveSymbolSet::Shaded, (500, Interpolation::QuadOut))
        .with_pattern(SweepPattern::left_to_right(25))
        .with_filter(not_image())
}

/// A light sweep across the title in its cover color, text readable throughout.
fn glint(title: Color, area: Rect) -> Effect {
    fx::fade_from_fg(Color::White, (450, Interpolation::QuadOut))
        .with_pattern(SweepPattern::left_to_right(12))
        .with_area(area)
        .with_filter(CellFilter::FgColor(title))
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
        None => Line::from(Span::styled("○ AniList not connected ", DIM)),
    };
    let [crumbs_area, user_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(user.width() as u16)])
            .areas(area);
    let mut crumbs = vec![Span::styled(
        " ani-hop ",
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];
    for s in app.stack.iter().skip(1) {
        crumbs.push(Span::styled(" › ", DIM));
        crumbs.push(Span::raw(s.title.as_str()));
    }
    f.render_widget(Line::from(crumbs), crumbs_area);
    f.render_widget(user, user_area);
}

fn menu(app: &mut App, f: &mut Frame, area: Rect) {
    let spin = spinner(app);
    let shimmer = (app.started.elapsed().as_millis() / 30) as usize;
    let screen = app.stack.last_mut().unwrap();
    let block = panel().title(Span::styled(
        format!(" {} ", screen.title),
        Style::new().fg(Color::Reset).bold(),
    ));
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    let [prompt_area, info_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let label = match &screen.view {
        View::Input(Input::Search) => "search ",
        View::Input(Input::ClientId) => "client id ",
        View::Input(Input::Token) => "token ",
        View::Input(Input::SourceQuery(_)) => "ani-cli search ",
        _ => "",
    };
    let prompt = Line::from(vec![
        Span::styled(label, ACCENT),
        Span::styled("❯ ", Style::new().fg(ACCENT).bold()),
        Span::raw(screen.menu.query.as_str()),
    ]);
    let cursor_x =
        (prompt_area.x + prompt.width() as u16).min(prompt_area.right().saturating_sub(1));
    f.render_widget(prompt, prompt_area);
    f.set_cursor_position((cursor_x, prompt_area.y));

    if let View::Input(input) = &screen.view {
        f.render_widget(
            Paragraph::new(input_help(input)).wrap(Wrap { trim: false }),
            list_area,
        );
        return;
    }

    let m = &screen.menu;
    let count = format!("{}/{}", m.matches.len(), m.items.len());
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

    let width = list_area.width.saturating_sub(1) as usize; // minus the pointer column
    let rows: Vec<ListItem> = m
        .matches
        .iter()
        .map(|(i, hits)| {
            let item = &m.items[*i];
            let mut spans = vec![
                Span::styled(if m.marked.contains(i) { "●" } else { " " }, Color::Green),
                " ".into(),
            ];
            spans.extend(item.label.chars().enumerate().map(|(ci, ch)| {
                let hit = hits.binary_search(&(ci as u32)).is_ok();
                Span::styled(
                    ch.to_string(),
                    if hit {
                        Style::new().fg(MATCH).bold()
                    } else {
                        Style::new()
                    },
                )
            }));
            let used: usize = spans.iter().map(|s| s.width()).sum();
            let room = width.saturating_sub(used + 2);
            if !item.hint.is_empty() && room > 3 {
                let mut hint = item.hint.clone();
                if hint.chars().count() > room {
                    hint = hint.chars().take(room - 1).collect::<String>() + "…";
                }
                let hint = Span::styled(hint, DIM);
                spans.push(" ".repeat(width - used - hint.width()).into());
                spans.push(hint);
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(rows)
        .highlight_symbol(Line::styled("▌", ACCENT))
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::new().bold());
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
            "1. Your browser opened anilist.co/settings/developer".into(),
            "   → Create New Client".into(),
            "2. Name: ani-hop".into(),
            Line::from(vec![
                "   Redirect URL: ".into(),
                Span::styled(PIN_REDIRECT, ACCENT),
            ]),
            "3. Paste the client ID here and press enter.".into(),
        ],
        Input::Token => vec![
            "Approve ani-hop in the browser tab that just opened,".into(),
            "then paste the token AniList shows and press enter.".into(),
            "".into(),
            Line::styled("Pasting the whole redirect URL works too.", DIM),
        ],
        Input::SourceQuery(_) => vec![
            "Search ani-cli's provider with this text,".into(),
            "then pick the matching entry. The choice is remembered.".into(),
        ],
    };
    Text::from(lines).fg(DIM)
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
        | View::Status(m)
        | View::Sources(m, ..)
        | View::Input(Input::SourceQuery(m))
        | View::Confirm(Pending::Progress(m, _)) => Some(m.clone()),
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
    if let Some(m) = media {
        media_preview(app, f, inner, &m);
        return ahead.iter().for_each(|m| app.want_cover(m));
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
        Line::from(vec![
            mark(good),
            format!("{name:<10}").into(),
            Span::styled(what, DIM),
        ])
    };
    let mut v: Vec<Line> = LOGO
        .iter()
        .map(|l| Line::styled(*l, Style::new().fg(ACCENT).bold()))
        .collect();
    v.extend(["".into(), Line::styled("Tools", Style::new().bold())]);
    for (bin, _, what) in ani::DEPS {
        v.push(row(!app.missing.iter().any(|d| d.0 == bin), bin, what));
    }
    let kitty = app.picker.capabilities().contains(&Capability::Kitty);
    v.push(row(kitty, "graphics", "posters need Ghostty or Kitty"));
    let pkgs: Vec<&str> = app.missing.iter().map(|d| d.1).collect();
    if !pkgs.is_empty() && ani::install_command(&pkgs).is_none() {
        v.push("".into());
        v.push(Line::styled(
            format!("Install: {}", pkgs.join(" ")),
            Color::Yellow,
        ));
        v.push(Line::styled(
            "ani-cli and ani-skip-git come from the AUR",
            DIM,
        ));
    }
    v.extend(["".into(), Line::styled("AniList", Style::new().bold())]);
    v.push(match &app.user {
        Some((_, name)) => Line::from(vec![mark(true), format!("logged in as {name}").into()]),
        None => Line::styled(
            "  Log in to get Watching/Planning first and progress synced after each episode.",
            DIM,
        ),
    });
    v
}

fn banner(app: &App) -> Vec<Line<'static>> {
    let key = |k: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(format!("{k:>8}  "), ACCENT),
            Span::styled(what, DIM),
        ])
    };
    let mut v: Vec<Line> = LOGO
        .iter()
        .map(|l| Line::styled(*l, Style::new().fg(ACCENT).bold()))
        .collect();
    v.push("".into());
    v.push(match &app.user {
        Some((_, n)) => Line::from(vec![
            Span::styled("● ", Color::Green),
            format!("AniList: {n}").into(),
        ]),
        None => Line::styled("○ AniList: not connected", DIM),
    });
    let root = ani::download_root().display().to_string();
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    v.push(Line::styled(
        format!("↓ {}", root.replacen(&home, "~", 1)),
        DIM,
    ));
    v.push("".into());
    v.extend([
        key("type", "filter, fzf syntax: 'exact ^start end$ !not"),
        key("↑↓ ^j^k", "move"),
        key("tab", "mark for bulk actions"),
        key("^a", "mark all"),
        key("enter", "select"),
        key("esc", "clear filter / back"),
        key("^c", "quit"),
    ]);
    v
}

fn media_preview(app: &mut App, f: &mut Frame, area: Rect, m: &Media) {
    let font = app.picker.font_size();
    let rows = (area.height * 3 / 5).clamp(6, 26).min(area.height);
    let cols = ((rows as f32 * font.height as f32 * 0.7) / font.width.max(1) as f32).round() as u16;
    app.poster = Size::new(cols.min(area.width / 2), rows);
    app.want_cover(m);

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
    // only true RGB titles: named colors would snap at the end of the fade
    if app.fx_media != m.id && matches!(accent, Color::Rgb(..)) {
        app.fx.add_unique_effect(Fx::Glint, glint(accent, info));
    }
    app.fx_media = m.id;
    match app.covers.get(&m.id) {
        Some(Cover::Ready(_, p)) => f.render_widget(Image::new(p), poster),
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
        lines.push(Line::styled(alt.clone(), Style::new().fg(DIM).italic()));
    }
    lines.push("".into());
    let mut facts = Vec::new();
    if let Some(s) = m.average_score {
        facts.push(Span::styled(format!("★ {s}%"), Color::Yellow));
    }
    let detail = [
        m.format.as_deref().map(pretty),
        m.episodes.map(|e| format!("{e} episodes")),
        m.status.as_deref().map(pretty),
    ];
    for d in detail.into_iter().flatten() {
        if !facts.is_empty() {
            facts.push(Span::styled(" · ", DIM));
        }
        facts.push(d.into());
    }
    lines.push(Line::from(facts));
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
    lines.push(Line::from(
        [when, studio]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
    ));
    lines.push(Line::styled(m.genres.join(" · "), MATCH));
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
    let local = app.local.get(&m.id).copied().unwrap_or(0);
    if local > 0 {
        lines.push(Line::from(format!("↓ {local} on disk")));
    }
    if let Some(s) = app.sources.get(&m.id) {
        lines.push(Line::styled(
            format!("ani-cli: {} · {} episodes", s.title, s.episodes.len()),
            DIM,
        ));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), info);

    let desc = m.description.as_deref().map(strip_html).unwrap_or_default();
    f.render_widget(Paragraph::new(desc).wrap(Wrap { trim: true }), bottom);
}

fn footer(app: &App, f: &mut Frame, area: Rect) {
    let screen = app.stack.last().unwrap();
    let mut keys = vec![("enter", "select"), ("esc", "back")];
    if screen.menu.multi {
        keys.splice(0..0, [("tab", "mark"), ("^a", "all")]);
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
    let [status_area, hints_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(hints.width() as u16),
    ])
    .areas(area);
    let status = match (&app.toast, &app.playing) {
        (Some((text, err, at)), _) if at.elapsed().as_secs() < 4 => Line::styled(
            format!(" {text}"),
            if *err { Color::Red } else { Color::Green },
        ),
        (_, Some(p)) => Line::from(vec![Span::styled(" ▶ ", ACCENT), p.as_str().into()]),
        _ if app.dl_pending > 0 => Line::from(vec![
            Span::styled(
                format!(" {} ↓ {} left ", spinner(app), app.dl_pending),
                ACCENT,
            ),
            Span::styled(app.dl_line.as_str(), DIM),
        ]),
        _ => Line::default(),
    };
    f.render_widget(status, status_area);
    f.render_widget(hints, hints_area);
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
}
