//! Posters, through the Kitty graphics protocol (Kitty, Ghostty). A poster's pixels go into
//! a file in /dev/shm and only its path travels through the terminal (~100 bytes instead of
//! megabytes of base64), so posters can be sent ahead of time and switching between them
//! costs nothing. The terminal shows an image wherever its Unicode placeholder cells are
//! drawn.

use crate::anilist::{self, Media};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Rect, Size};
use ratatui::style::Color;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::num::NonZeroU16;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Set by `shutdown`; `prepare` won't write a file after that, so none outlive the app.
static CLOSED: Mutex<bool> = Mutex::new(false);

const PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column markers, the first 64 of kitty's rowcolumn-diacritics.txt (posters stay smaller).
const DIACRITICS: [char; 64] = [
    '\u{305}', '\u{30D}', '\u{30E}', '\u{310}', '\u{312}', '\u{33D}', '\u{33E}', '\u{33F}',
    '\u{346}', '\u{34A}', '\u{34B}', '\u{34C}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35B}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36A}', '\u{36B}', '\u{36C}', '\u{36D}', '\u{36E}', '\u{36F}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59C}', '\u{59D}', '\u{59E}', '\u{59F}', '\u{5A0}', '\u{5A1}',
    '\u{5A8}', '\u{5A9}', '\u{5AB}', '\u{5AC}', '\u{5AF}', '\u{5C4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
];

/// A poster the terminal already holds, `size` in cells.
#[derive(Clone, Copy)]
struct Poster {
    id: u32,
    size: Size,
}

/// A poster is made for a size in cells and the pixels per cell (font zoom changes those).
pub type Key = (Size, (u16, u16));

enum Cover {
    Loading(Key),
    Ready(Key, Poster),
    Failed,
}

/// What the terminal is told: kitty's escape sequences, or a recording in tests.
pub trait Terminal {
    /// Hands a prepared poster's pixel file to the terminal as image `id`.
    fn transmit(&mut self, id: u32, file: &Path, pixels: (u32, u32), cells: Size);
    /// Frees image `id`.
    fn delete(&mut self, id: u32);
    /// Removes pixel files the terminal never read.
    fn cleanup(&mut self);
}

/// The terminal ani-hop runs in.
pub struct Tty;

impl Terminal for Tty {
    fn transmit(&mut self, id: u32, file: &Path, (w, h): (u32, u32), cells: Size) {
        let file = base64(file.as_os_str().as_bytes());
        let (c, r) = (cells.width, cells.height);
        send(&format!(
            "\x1b_Gq=2,a=T,U=1,i={id},f=32,t=t,s={w},v={h},c={c},r={r};{file}\x1b\\"
        ));
    }

    fn delete(&mut self, id: u32) {
        send(&format!("\x1b_Gq=2,a=d,d=I,i={id}\x1b\\"));
    }

    fn cleanup(&mut self) {
        cleanup();
    }
}

/// A poster being prepared in the background: (show, key, pixel file, its size, cells).
pub type Loaded = (u64, Key, Result<(PathBuf, u32, u32, Size), String>);

/// Fetching a show's cover (from the disk cache, else AniList) and scaling it, to run in the
/// background.
pub struct Load {
    id: u64,
    url: String,
    key: Key,
}

impl Load {
    pub fn run(self) -> Loaded {
        let path = crate::ani::cache_dir()
            .join("covers")
            .join(format!("{}.jpg", self.id));
        let (size, cell) = self.key;
        // a cover cut short (a crash while it was written) is fetched again
        let cached = fs::read(&path).ok();
        let res = match cached.map(|b| prepare(&b, size, cell)) {
            Some(Ok(done)) => Ok(done),
            _ => anilist::fetch(&self.url).and_then(|bytes| {
                let _ = crate::ani::write_bytes(&path, &bytes);
                prepare(&bytes, size, cell)
            }),
        };
        (self.id, self.key, res)
    }
}

/// The posters of the shows on screen and just below: loading, held by the terminal, or
/// failed (the cover's color stands in).
pub struct Posters<T: Terminal = Tty> {
    covers: HashMap<u64, Cover>,
    /// poster size for the next ones, from the last layout, and the pixels per cell
    size: Size,
    cell: (u16, u16),
    /// handed over as files: (file, when); one still there later is unreadable
    sent: Vec<(PathBuf, Instant)>,
    next_image: u32,
    /// no posters this session: no graphics, or the terminal didn't read one
    off: bool,
    term: T,
}

impl<T: Terminal> Posters<T> {
    pub fn new(term: T, on: bool) -> Self {
        Posters {
            covers: HashMap::new(),
            size: Size::new(0, 0),
            cell: (10, 20),
            sent: Vec::new(),
            // instances sharing one terminal (tmux panes) must not reuse each other's ids
            next_image: std::process::id().wrapping_mul(2_654_435_761) & 0xFF_FFFF,
            off: !on,
            term,
        }
    }

    /// The poster size for `rows` rows at most `cols` wide, with `cell` pixels per cell
    /// (font zoom changes those): a cover's 0.7 aspect. The posters asked for next get it.
    pub fn fit(&mut self, rows: u16, cols: u16, cell: (u16, u16)) -> Size {
        let w = ((rows as f32 * cell.1 as f32 * 0.7) / cell.0.max(1) as f32).round() as u16;
        (self.size, self.cell) = (Size::new(w.min(cols), rows), cell);
        self.size
    }

    /// Starts loading `m`'s poster at the size from `fit`, unless it's there or loading.
    /// `urgent` (the one on screen) goes first; look-ahead ones wait while 4 are on their way
    /// and get asked for again next frame. What to run in the background, if anything.
    pub fn want(&mut self, m: &Media, urgent: bool) -> Option<Load> {
        let key = (self.size, self.cell);
        let fresh = match self.covers.get(&m.id) {
            Some(Cover::Loading(k) | Cover::Ready(k, _)) => *k == key,
            Some(Cover::Failed) => true,
            None => false,
        };
        let busy = (self.covers.values())
            .filter(|c| matches!(c, Cover::Loading(_)))
            .count();
        if fresh || self.size.width == 0 || (busy >= 4 && !urgent) {
            return None;
        }
        // nobody would read (and delete) the pixel files; the cover color stands in
        if self.off {
            self.covers.insert(m.id, Cover::Failed);
            return None;
        }
        // shortcut: the terminal keeps ~3 MB per poster; past 60 they all go and reload from
        // the disk cache in ms
        if self.covers.len() > 60 {
            self.forget();
        }
        if let Some(Cover::Ready(_, p)) = self.covers.get(&m.id) {
            self.term.delete(p.id); // resized: the old one is never drawn again
        }
        let Some(url) = m.cover_image.extra_large.clone() else {
            self.covers.insert(m.id, Cover::Failed);
            return None;
        };
        self.covers.insert(m.id, Cover::Loading(key));
        Some(Load { id: m.id, url, key })
    }

    /// A poster finished loading: sent to the terminal at once, even a look-ahead one, as
    /// showing it later is free. One for another size, or no longer wanted, goes.
    pub fn loaded(&mut self, (id, key, res): Loaded) {
        let wanted = matches!(self.covers.get(&id), Some(Cover::Loading(k)) if *k == key);
        match res {
            Ok((file, w, h, cells)) if wanted => {
                self.next_image = self.next_image % 0xFF_FFFF + 1; // 24-bit ids, never 0
                self.term.transmit(self.next_image, &file, (w, h), cells);
                self.sent.push((file, Instant::now()));
                let poster = Poster {
                    id: self.next_image,
                    size: cells,
                };
                self.covers.insert(id, Cover::Ready(key, poster));
            }
            Ok((file, ..)) => _ = fs::remove_file(file), // stale, never sent
            Err(_) if wanted => _ = self.covers.insert(id, Cover::Failed),
            Err(_) => {}
        }
    }

    /// Draws show `id`'s poster into `area`; false while there's none to draw.
    pub fn draw(&self, id: u64, area: Rect, buf: &mut Buffer) -> bool {
        match self.covers.get(&id) {
            Some(Cover::Ready(_, p)) => {
                render(p, area, buf);
                true
            }
            _ => false,
        }
    }

    /// The terminal deletes a poster file once it read it. One still around after 2 s means
    /// it never will (tmux without passthrough, ssh, ...): posters go off for the session,
    /// and true says so once.
    pub fn check(&mut self, now: Instant) -> bool {
        let old = |s: &(PathBuf, Instant)| now.duration_since(s.1) > Duration::from_secs(2);
        if !self.sent.iter().any(|s| old(s) && s.0.exists()) {
            self.sent.retain(|s| !old(s));
            return false;
        }
        self.off = true;
        self.forget();
        true
    }

    /// Frees every poster in the terminal.
    pub fn forget(&mut self) {
        for c in self.covers.values() {
            if let Cover::Ready(_, p) = c {
                self.term.delete(p.id);
            }
        }
        self.covers.clear();
        self.sent.clear();
        self.term.cleanup(); // files of the in-flight ones come back unwanted and go too
    }
}

/// Pixels per cell; Ghostty and Kitty report the window size in pixels.
pub fn cell_px() -> (u16, u16) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(w) if w.width > 0 && w.columns > 0 && w.rows > 0 => {
            (w.width / w.columns, w.height / w.rows)
        }
        _ => (10, 20),
    }
}

/// Best guess without asking the terminal (which would cost a round trip at start-up).
pub fn supported() -> bool {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() // also set inside tmux
        || env("TERM_PROGRAM") == "ghostty"
        || env("TERM").contains("kitty")
        || env("TERM").contains("ghostty")
}

/// Scales a cover into `max` cells (aspect kept) and writes its RGBA pixels to a file that
/// the terminal reads and deletes. Returns (file, pixel width, pixel height, cells).
pub fn prepare(
    bytes: &[u8],
    max: Size,
    cell: (u16, u16),
) -> Result<(PathBuf, u32, u32, Size), String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (bw, bh) = (
        max.width as u32 * cell.0 as u32,
        max.height as u32 * cell.1 as u32,
    );
    let img = img.resize(bw, bh, image::imageops::FilterType::Triangle);
    let (w, h) = (img.width(), img.height());
    let cells = Size::new(
        w.div_ceil(cell.0 as u32) as u16,
        h.div_ceil(cell.1 as u32) as u16,
    );
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = shm_dir().join(format!("{}{n}", prefix()));
    let closed = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
    if *closed {
        return Err("shutting down".into());
    }
    std::fs::write(&path, img.to_rgba8().as_raw()).map_err(|e| e.to_string())?;
    drop(closed);
    Ok((path, w, h, cells))
}

fn shm_dir() -> PathBuf {
    let dir = Path::new("/dev/shm");
    if dir.is_dir() {
        dir.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

/// The terminal only reads (and deletes) temp files whose name says what they are.
fn prefix() -> String {
    format!("ani-hop-tty-graphics-protocol-{}-", std::process::id())
}

/// Removes pixel files the terminal never picked up (it deletes the ones it read), ours
/// and those of ani-hop processes that died without cleaning up (closed terminal).
pub fn cleanup() {
    let Ok(dir) = std::fs::read_dir(shm_dir()) else {
        return;
    };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let pid = name
            .strip_prefix("ani-hop-tty-graphics-protocol-")
            .and_then(|r| r.split('-').next());
        let dead = pid.is_some_and(|p| !crate::ani::running(p));
        if name.starts_with(&prefix()) || dead {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Last cleanup on the way out; posters still being prepared are dropped.
pub fn shutdown() {
    *CLOSED.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cleanup();
}

/// tmux drops graphics unless passthrough is on (ratatui-image used to switch it on too).
pub fn init() {
    if std::env::var_os("TMUX").is_some() {
        let _ = std::process::Command::new("tmux")
            .args(["set", "-p", "allow-passthrough", "on"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    cleanup();
}

fn send(seq: &str) {
    // inside tmux it needs the passthrough wrapper (and `allow-passthrough on`)
    let seq = match std::env::var_os("TMUX") {
        Some(_) => format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")),
        None => seq.to_string(),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes()).and_then(|_| out.flush());
}

/// Placeholder cells for a poster; the terminal draws the image over them.
fn render(p: &Poster, area: Rect, buf: &mut Buffer) {
    let fg = Color::Rgb((p.id >> 16) as u8, (p.id >> 8) as u8, p.id as u8);
    let one = CellDiffOption::ForcedWidth(NonZeroU16::MIN);
    for y in 0..p.size.height.min(area.height).min(64) {
        for x in 0..p.size.width.min(area.width) {
            // only a row's first cell needs its position, the rest continue from it
            let mut s = String::from(PLACEHOLDER);
            if x == 0 {
                s.extend([DIACRITICS[y as usize], DIACRITICS[0]]);
            }
            buf[(area.x + x, area.y + y)]
                .set_symbol(&s)
                .set_fg(fg)
                .set_diff_option(one);
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    data.chunks(3)
        .flat_map(|c| {
            let n = c.iter().fold(0u32, |n, &b| n << 8 | b as u32) << (8 * (3 - c.len()));
            (0..4).map(move |i| match i <= c.len() {
                true => T[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"/dev/shm/x"), "L2Rldi9zaG0veA==");
    }

    /// What the terminal was told.
    #[derive(Default)]
    struct Record(Vec<String>);

    impl Terminal for Record {
        fn transmit(&mut self, id: u32, _: &Path, _: (u32, u32), _: Size) {
            self.0.push(format!("show {id}"));
        }
        fn delete(&mut self, id: u32) {
            self.0.push(format!("free {id}"));
        }
        fn cleanup(&mut self) {
            self.0.push("cleanup".into());
        }
    }

    fn show(id: u64) -> Media {
        let mut m = Media {
            id,
            ..Default::default()
        };
        m.cover_image.extra_large = Some(format!("https://img/{id}.jpg"));
        m
    }

    /// A pixel file as `Load::run` leaves it, which the terminal deletes once read.
    fn pixels(dir: &Path, name: &str) -> PathBuf {
        let file = dir.join(name);
        fs::write(&file, "rgba").unwrap();
        file
    }

    #[test]
    fn posters_load_once_per_size_and_stale_ones_go() {
        let dir = std::env::temp_dir().join(format!("ani-hop-posters-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut p = Posters::new(Record::default(), true);
        assert!(p.want(&show(1), true).is_none(), "no size yet");
        let size = p.fit(20, 30, (10, 20));
        assert_eq!(size, Size::new(28, 20));
        let load = p.want(&show(1), true).unwrap();
        assert!(p.want(&show(1), true).is_none(), "on its way");
        let cells = Size::new(28, 20);
        p.loaded((1, load.key, Ok((pixels(&dir, "a"), 280, 400, cells))));
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 20));
        assert!(p.draw(1, buf.area, &mut buf) && !p.draw(2, buf.area, &mut buf));
        // zoomed in: made again for the new size, the old one freed
        p.fit(20, 30, (12, 24));
        let again = p.want(&show(1), true).unwrap();
        let stale = pixels(&dir, "b");
        p.loaded((1, load.key, Ok((stale.clone(), 280, 400, cells))));
        assert!(!stale.exists(), "a poster for the old size is never sent");
        p.loaded((1, again.key, Ok((pixels(&dir, "c"), 280, 400, cells))));
        let first = p.next_image - 1;
        assert_eq!(
            p.term.0,
            [
                format!("show {first}"),
                format!("free {first}"),
                format!("show {}", first + 1)
            ]
        );
        // look-ahead posters wait while four are on their way
        let ahead: Vec<_> = (10..14).map(|id| p.want(&show(id), false)).collect();
        assert!(ahead.iter().all(Option::is_some) && p.want(&show(20), false).is_none());
        assert!(
            p.want(&show(21), true).is_some(),
            "the one on screen doesn't"
        );
        p.loaded((10, ahead[0].as_ref().unwrap().key, Err("HTTP 404".into())));
        assert!(p.want(&show(10), true).is_none(), "failed: its color stays");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_terminal_that_doesnt_read_posters_gets_colors() {
        let dir = std::env::temp_dir().join(format!("ani-hop-unread-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut p = Posters::new(Record::default(), true);
        p.fit(20, 30, (10, 20));
        let load = p.want(&show(1), true).unwrap();
        let file = pixels(&dir, "a");
        p.loaded((1, load.key, Ok((file.clone(), 1, 1, Size::new(1, 1)))));
        let sent = Instant::now();
        assert!(
            !p.check(sent + Duration::from_secs(1)),
            "it has 2 s to read it"
        );
        assert!(p.check(sent + Duration::from_secs(3)), "still there: off");
        assert!(p.term.0.ends_with(&["cleanup".to_string()]));
        assert!(p.want(&show(2), true).is_none() && !p.check(sent + Duration::from_secs(9)));
        // one the terminal read and deleted is fine
        let mut p = Posters::new(Record::default(), true);
        p.fit(20, 30, (10, 20));
        let load = p.want(&show(1), true).unwrap();
        p.loaded((1, load.key, Ok((pixels(&dir, "b"), 1, 1, Size::new(1, 1)))));
        fs::remove_file(dir.join("b")).unwrap();
        assert!(!p.check(Instant::now() + Duration::from_secs(3)));
        // past 60 posters they all go
        for id in 100..162 {
            let load = p.want(&show(id), true).unwrap();
            p.loaded((id, load.key, Err("x".into())));
        }
        assert!(p.covers.len() < 60 && p.term.0.contains(&"cleanup".to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placeholders_carry_id_and_row() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        let p = Poster {
            id: 0x010203,
            size: Size::new(3, 2),
        };
        render(&p, Rect::new(1, 1, 3, 2), &mut buf);
        assert_eq!(buf[(1, 2)].symbol(), "\u{10EEEE}\u{30D}\u{305}");
        assert_eq!(buf[(2, 2)].symbol(), "\u{10EEEE}");
        assert_eq!(buf[(2, 2)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }
}
