//! Kitty graphics protocol (Kitty, Ghostty). A poster's pixels go into a file in /dev/shm
//! and only its path travels through the terminal (~100 bytes instead of megabytes of
//! base64), so posters can be sent ahead of time and switching between them costs nothing.
//! The terminal shows an image wherever its Unicode placeholder cells are drawn.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Rect, Size};
use ratatui::style::Color;
use std::io::Write;
use std::num::NonZeroU16;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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
pub struct Poster {
    pub id: u32,
    pub size: Size,
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

/// Hands a prepared poster to the terminal; it appears wherever `render` draws `id`.
pub fn transmit(id: u32, path: &Path, (w, h): (u32, u32), cells: Size) {
    let file = base64(path.as_os_str().as_bytes());
    let (c, r) = (cells.width, cells.height);
    send(&format!(
        "\x1b_Gq=2,a=T,U=1,i={id},f=32,t=t,s={w},v={h},c={c},r={r};{file}\x1b\\"
    ));
}

/// Frees a poster in the terminal.
pub fn delete(id: u32) {
    send(&format!("\x1b_Gq=2,a=d,d=I,i={id}\x1b\\"));
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
pub fn render(p: &Poster, area: Rect, buf: &mut Buffer) {
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
