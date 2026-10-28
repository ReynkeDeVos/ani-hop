//! The Preview: the right-hand pane's show, or episode of it. What it says (the show's facts,
//! list status, parts, next airing and source, or the episode's title and summary once it's
//! watched, so nothing spoils) is put together from what App knows (`Shown` into `page`), then
//! laid out with the show's poster beside it and its description flowing around that; what
//! doesn't fit pages with shift+↑↓. The Preview keeps the posters, which page is read, and
//! the title's glint.

use crate::anilist::{EpInfo, Media};
use crate::entries::Entries;
use crate::kitty::{self, Posters};
use crate::list_label;
use crate::progress::Progress;
use crate::source::Source;
use crate::ui::{ACCENT, DIM, MATCH, SOFT, hex, swept};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, Wrap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

const GLINT_MS: u128 = 450;

/// What App knows of the show on screen, and of the episode under the cursor on its episode
/// list, for `page`.
pub struct Shown<'a> {
    pub show: &'a Media,
    pub ep: Option<&'a str>,
    pub progress: Progress<'a>,
    pub entries: &'a Entries,
    pub source: Option<&'a Source>,
    /// its episodes on disk
    pub on_disk: usize,
    /// what the episode is called and about, as ani.zip has it
    pub info: Option<&'a EpInfo>,
}

/// What the Preview says, before it's laid out.
pub struct Page {
    /// whose poster, colors and description it has: on an episode of a part, the part's
    media: Media,
    head: Vec<Line<'static>>,
    /// the episode's summary, once it's watched
    about: String,
}

/// What the Preview says of `s`. On an episode of a sequel the provider runs on into, it's
/// that sequel's poster, facts, status and description; what's on disk, ani-cli's entry and
/// the episode stay the show's.
pub fn page(s: &Shown) -> Page {
    let (show, ep) = (s.show, s.ep);
    let part = ep
        .map(|ep| s.progress.entry(ep).0)
        .filter(|p| p.id != show.id);
    let m = part.unwrap_or(show);
    let accent = accent(m);
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
        let e = s.entries.entry(p)?;
        let total = p.episodes.map_or("?".into(), |t| t.to_string());
        let line = format!("● {} · {}/{total}", list_label(&e.status), e.progress);
        Some(Line::styled(line, Color::Green))
    };
    lines.extend(status(m));
    // the provider's episodes past the show's own count toward a sequel on AniList; an
    // episode list says so with a rule between them (ui::menu)
    let parts = if ep.is_none() {
        s.progress.parts()
    } else {
        &[]
    };
    for (before, p) in parts {
        let eps = p.episodes.map_or(format!("{}+", before + 1), |n| {
            format!("{}–{}", before + 1, before + n)
        });
        // as the episode list's rule calls it: "Part 2"
        let name = s.progress.part_name(p);
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
    if s.on_disk > 0 && ep.is_none() {
        lines.push(Line::from(format!("↓ {} on disk", s.on_disk)));
    }
    if let Some(src) = s.source {
        let n = episodes(src.episodes.len());
        // a wrong match keeps its title, so it stands out
        let line = match same_title(&src.title, m) {
            true => format!("ani-cli: {n}"),
            false => format!("ani-cli: {} · {n}", src.title),
        };
        lines.push(Line::styled(line, DIM));
    }
    let mut about = String::new();
    if let Some(ep) = ep
        && let Some(i) = s.info
    {
        lines.push("".into());
        let (title, summary) = i.told(s.progress.watched(ep));
        let heading = match title {
            "" => format!("Episode {ep}"),
            title => format!("Episode {ep} · {title}"),
        };
        lines.push(Line::styled(heading, Style::new().bold()));
        if let Some(d) = i.aired.as_deref().filter(|d| !d.is_empty()) {
            lines.push(Line::styled(format!("aired {}", day(d)), DIM));
        }
        about = summary.to_string();
    }
    // the gap before the description is one row, also when nothing follows the genres
    if lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    Page {
        media: m.clone(),
        head: lines,
        about,
    }
}

/// The cover's dominant color, for the title and while the poster loads.
fn accent(m: &Media) -> Color {
    (m.cover_image.color.as_deref())
        .and_then(hex)
        .unwrap_or(ACCENT)
}

pub struct Preview {
    posters: Posters,
    /// posters prepared in the background
    loaded: (Sender<kitty::Loaded>, Receiver<kitty::Loaded>),
    /// the show and episode shown, and how many pages down it's read; another starts at its top
    scroll: (Option<(u64, Option<String>)>, u16),
    /// the show whose title glint plays, and since when
    glint: (u64, Instant),
}

impl Preview {
    pub fn new(posters: Posters) -> Self {
        Preview {
            posters,
            loaded: mpsc::channel(),
            scroll: (None, 0),
            glint: (0, Instant::now()),
        }
    }

    /// A page down, or back up (shift+↓↑).
    pub fn scroll(&mut self, down: bool) {
        let n = &mut self.scroll.1;
        *n = match down {
            true => n.saturating_add(1),
            false => n.saturating_sub(1),
        };
    }

    /// When the title glint started; it plays for a moment.
    pub fn glint(&self) -> Instant {
        self.glint.1
    }

    pub fn animating(&self) -> bool {
        self.glint.1.elapsed().as_millis() < GLINT_MS
    }

    /// Hands the terminal the posters prepared since; (any of them, whether posters went off
    /// as the terminal didn't take them).
    pub fn tick(&mut self, now: Instant) -> (bool, bool) {
        let mut any = false;
        while let Ok(loaded) = self.loaded.1.try_recv() {
            self.posters.loaded(loaded);
            any = true;
        }
        (any, self.posters.check(now))
    }

    /// Frees every poster in the terminal, as ani-hop ends.
    pub fn forget(&mut self) {
        self.posters.forget();
    }

    /// Starts preparing `m`'s poster; `urgent` is the one on screen.
    fn want(&mut self, m: &Media, urgent: bool) {
        if let Some(load) = self.posters.want(m, urgent) {
            let tx = self.loaded.0.clone();
            std::thread::spawn(move || _ = tx.send(load.run()));
        }
    }

    /// Which show and episode are read: another one, or none, starts at its top.
    pub fn read(&mut self, shown: Option<(u64, Option<String>)>) {
        if self.scroll.0 != shown {
            self.scroll = (shown, 0);
        }
    }

    /// Draws `page` into `inner`, a pane inside `area`, whose border says when there's more
    /// above or below; posters of the shows `ahead` load meanwhile, so the next ones show at
    /// once.
    pub fn draw(&mut self, page: &Page, ahead: &[Media], f: &mut Frame, area: Rect, inner: Rect) {
        let (up, down) = self.lay_out(page, f, inner);
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
        ahead.iter().for_each(|m| self.want(m, false));
    }

    /// The poster with the head beside it, and the text flowing around it; returns whether
    /// there's more (above, below).
    fn lay_out(&mut self, page: &Page, f: &mut Frame, area: Rect) -> (bool, bool) {
        let m = &page.media;
        let rows = (area.height * 3 / 5).clamp(6, 26).min(area.height);
        // font zoom changes the pixels per cell, so they're read every frame
        let size = self.posters.fit(rows, area.width / 2, kitty::cell_px());
        self.want(m, true);
        let [poster, _, info] = Layout::horizontal([
            Constraint::Length(size.width),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .areas(Rect::new(area.x, area.y, area.width, rows));
        let accent = accent(m);
        if self.glint.0 != m.id {
            self.glint = (m.id, Instant::now());
        }
        let head = Paragraph::new(page.head.clone()).wrap(Wrap { trim: true });
        let head_h = head.line_count(info.width) as u16;

        // the episode's summary right under its title, then the description: beside the poster
        // (and in the row under it) first, where the facts leave rows empty, then full width
        let desc = m.description.as_deref().map(strip_html).unwrap_or_default();
        let text = [(page.about.as_str(), SOFT), (desc.as_str(), Style::new())];
        let text: Vec<_> = text.into_iter().filter(|t| !t.0.is_empty()).collect();
        let top = head_h + u16::from(page.about.is_empty());
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
        let mut buf = Buffer::empty(page_area);
        // the cover's dominant color stands in while it loads
        if !self.posters.draw(m.id, poster, &mut buf) {
            Block::new().bg(accent).render(poster, &mut buf);
        }
        let head_area = Rect::new(info.x, info.y, info.width, head_h);
        head.render(head_area, &mut buf);
        // only true RGB titles: a named color has no RGB value to fade back into
        if let Color::Rgb(r, g, b) = accent {
            self.shine(&mut buf, head_area, (r, g, b));
        }
        for (i, line) in text.into_iter().enumerate() {
            let column = if i < narrow { info } else { area };
            let y = area.y + top + i as u16;
            line.render(Rect::new(column.x, y, column.width, 1), &mut buf);
        }

        // a page is the pane less one row, so the last line read stays on screen
        let step = area.height.saturating_sub(1).max(1);
        let max = page_h.saturating_sub(area.height);
        let n = &mut self.scroll.1;
        *n = (*n).min(max.div_ceil(step));
        let off = (*n * step).min(max);
        let frame = f.buffer_mut();
        for pos in area.positions() {
            frame[pos] = buf[(pos.x, pos.y + off)].clone();
        }
        (off > 0, off < max)
    }

    /// A white sweep across the title that settles into its cover color.
    fn shine(&self, buf: &mut Buffer, area: Rect, (r, g, b): (u8, u8, u8)) {
        let t = self.glint.1.elapsed().as_millis() as f32 / GLINT_MS as f32;
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
    fn an_episode_tells_its_title_and_summary_once_watched() {
        let mut show = Media {
            id: 1,
            episodes: Some(26),
            media_list_entry: Some(crate::anilist::Entry {
                id: 9,
                status: "CURRENT".into(),
                progress: 1,
            }),
            ..Default::default()
        };
        show.title.romaji = Some("Mushishi".into());
        let anilist = std::sync::Arc::new(crate::anilist::Client { token: None });
        let entries = Entries::new(anilist);
        let info = EpInfo {
            title: "The Green Seat".into(),
            about: "A boy's drawings come alive.".into(),
            number: None,
            aired: Some("2005-10-23".into()),
            season: Some(1),
        };
        let told = |ep| {
            let p = page(&Shown {
                show: &show,
                ep: Some(ep),
                progress: entries.progress(&show, &[]),
                entries: &entries,
                source: None,
                on_disk: 0,
                info: Some(&info),
            });
            let head: Vec<String> = p.head.iter().map(Line::to_string).collect();
            (head, p.about)
        };
        let (head, about) = told("1");
        assert!(head.contains(&"Episode 1 · The Green Seat".into()));
        assert_eq!(about, "A boy's drawings come alive.");
        let (head, about) = told("2");
        assert!(
            head.contains(&"Episode 2".into()) && about.is_empty(),
            "nothing spoils"
        );
        assert!(
            head.contains(&"aired 23 Oct 2005".into()),
            "its date doesn't spoil"
        );
    }

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
}
