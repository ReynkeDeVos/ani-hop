//! Everything that touches ani-cli, ani-skip, mpv and the download folder.
//!
//! ani-cli has no machine-readable output, so we read its menus instead: with stdin not
//! being a tty it switches to rofi, and we put a stand-in `rofi` first in its PATH that
//! writes the menu to a file. That way the provider scraping stays ani-cli's business and
//! keeps working after `ani-cli -U`.

use crate::anilist::Media;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// The running ani-cli download: pid, show folder, episode.
type Running = (u32, PathBuf, String);

/// (stopped by quitting, the running download). One lock, so quitting or stopping an
/// episode can't race the worker starting the next one.
static DOWNLOAD: Mutex<(bool, Option<Running>)> = Mutex::new((false, None));

const FAKE_ROFI: &str = r#"#!/bin/sh
# ani-hop: captures ani-cli's menu instead of showing one
while [ $# -gt 0 ]; do [ "$1" = -p ] && p="$2"; shift; done
{ printf '%s\n' "$p"; cat; } > "${ANIHOP_MENU_OUT:-/dev/null}"
exit 1
"#;

const FAKE_FFMPEG: &str = r#"#!/bin/sh
# ani-hop: ani-cli downloads a show whose audio is a separate track with ffmpeg, which writes
# the .mp4 as it goes, so one cut short would look finished. This writes .part, like yt-dlp,
# renames it once ffmpeg got all of it and reports how far it got on stdout.
PATH=${PATH#"${0%/*}:"}
for a; do case $a in *.mp4) out=$a ;; esac; done
[ "$1" = -extension_picky ] && [ -n "$out" ] || exec ffmpeg "$@"
for a; do
	shift
	[ "$a" = "$out" ] && set -- "$@" -y -nostdin -nostats -loglevel level+warning -progress pipe:1 -f mp4 && a=$out.part
	set -- "$@" "$a"
done
log=$out.part.log
# ani-hop stops a download by signalling ani-cli's children, and this is one
trap 'kill $pid 2>/dev/null; rm -f "$log"; exit 1' INT HUP TERM
ffmpeg "$@" 2>"$log" &
pid=$!
wait $pid
ok=$?
cat "$log" >&2
# ffmpeg exits 0 after skipping a segment it can't get, which only the input side warns about,
# and after errors in what it got. shortcut: a segment that arrives short or empty with no
# error at all still passes; checking each segment means a downloader of our own.
grep -Eq '^\[(in#|hls|https?|tcp|tls|mpegts)|\[(error|fatal|panic)\]' "$log" && ok=1 && echo "parts of it failed to download" >&2
rm -f "$log"
[ $ok = 0 ] && mv -f "$out.part" "$out"
"#;

/// Where an AniList entry lives on the provider, as ani-cli sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub query: String,
    pub index: usize,
    pub title: String,
    /// which of the entries with this title it is (1: the first); 0 if saved before 0.6.1
    #[serde(default)]
    pub nth: usize,
    pub episodes: Vec<String>,
}

impl Source {
    /// Entry `index` of `hits`, counting which of the entries with its title it is.
    fn at(query: &str, hits: &[String], index: usize, episodes: Vec<String>) -> Source {
        let title = hits[index - 1].clone();
        let nth = hits[..index].iter().filter(|h| **h == title).count();
        Source {
            query: query.into(),
            index,
            title,
            nth,
            episodes,
        }
    }
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// `$var` if it's an absolute path, else `~/fallback` (the XDG base-dir rule).
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// XDG_VIDEOS_DIR from user-dirs.dirs (may be localized, e.g. "$HOME/Vidéos"). Like the
/// dirs crate: only quoted values, and "$HOME/" means disabled.
/// ponytail: no backslash unescaping, xdg-user-dirs-update never needs it for real paths
fn videos_dir(user_dirs: &str, home: &Path) -> PathBuf {
    user_dirs
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_VIDEOS_DIR=\"")?.split_once('"'))
        .map(|(v, _)| PathBuf::from(v.replace("$HOME", &home.to_string_lossy())))
        .filter(|p| p.is_absolute() && p != home)
        .unwrap_or_else(|| home.join("Videos"))
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("ani-hop")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("ani-hop")
}

/// A JSON file, or the default when it's missing or broken.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Written aside and renamed, so a full disk or quitting mid-write can't leave half a file.
pub fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    // through a symlink (dotfiles), as a plain write would
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, &path)
}

fn root_path() -> PathBuf {
    config_dir().join("download-dir")
}

/// The folder from the settings, else ANI_CLI_DOWNLOAD_DIR, else ~/Videos/Anime
/// (~/Movies/Anime on macOS).
pub fn download_root() -> PathBuf {
    fs::read_to_string(root_path())
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_absolute())
        .unwrap_or_else(default_root)
}

pub fn default_root() -> PathBuf {
    match std::env::var("ANI_CLI_DOWNLOAD_DIR") {
        Ok(d) if !d.is_empty() && d != "." => PathBuf::from(d),
        _ if cfg!(target_os = "macos") => home().join("Movies/Anime"),
        _ => {
            let user_dirs = xdg("XDG_CONFIG_HOME", ".config").join("user-dirs.dirs");
            videos_dir(&fs::read_to_string(user_dirs).unwrap_or_default(), &home()).join("Anime")
        }
    }
}

/// Sets the download folder (`~` works); empty goes back to the default.
pub fn set_download_root(dir: &str) -> Result<PathBuf, String> {
    let dir = dir.trim();
    if dir.is_empty() {
        let _ = fs::remove_file(root_path());
        return Ok(default_root());
    }
    let path = match dir.strip_prefix("~") {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => home().join(&rest[1..]),
        _ => PathBuf::from(dir),
    };
    if !path.is_absolute() {
        return Err("Needs a full path, like ~/Videos/Anime".into());
    }
    fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", tilde(&path)))?;
    fs::create_dir_all(config_dir())
        .and_then(|()| fs::write(root_path(), path.to_string_lossy().as_bytes()))
        .map_err(|e| e.to_string())?;
    Ok(path)
}

/// A path as people write it, with ~ for the home folder.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// What ani-hop shells out to: (command, Arch package, what it's for).
const DEPS: [(&str, &str, &str); 7] = [
    ("ani-cli", "ani-cli", "streams and downloads"),
    ("mpv", "mpv", "plays episodes"),
    ("ani-skip", "ani-skip-git", "skips openings and endings"),
    ("yt-dlp", "yt-dlp", "downloads episodes"),
    ("ffmpeg", "ffmpeg", "finishes downloads"),
    ("curl", "curl", "fetches pages for ani-cli"),
    ("xdg-open", "xdg-utils", "opens the AniList login"),
];

pub fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// DEPS on this OS: macOS opens links with its own `open`.
pub fn deps() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    DEPS.into_iter()
        .filter(|d| !(cfg!(target_os = "macos") && d.0 == "xdg-open"))
}

pub fn missing_deps() -> Vec<(&'static str, &'static str, &'static str)> {
    deps().filter(|d| !have(d.0)).collect()
}

/// Installs or updates ani-hop and everything it needs on Debian/Ubuntu, Fedora and Arch.
pub const INSTALL_SH: &str =
    "curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh";

fn aur_helper() -> Option<&'static str> {
    ["yay", "paru"].into_iter().find(|h| have(h))
}

/// One command for everything missing (Arch package names, as in DEPS). ani-cli and
/// ani-skip live in the AUR, so plain pacman only qualifies when neither is needed; other
/// distros get install.sh, which fetches both from upstream. macOS, and Linux when
/// ani-hop came from Homebrew, get brew, which has no ani-skip or xdg-utils.
pub fn install_command(pkgs: &[&str]) -> Option<String> {
    let list = pkgs.join(" ");
    // -S without -y: no system upgrade (Omarchy refuses one outside omarchy update), and the
    // package database isn't refreshed, so it installs what fits the system as last upgraded
    if let Some(helper) = aur_helper() {
        return Some(format!("{helper} -S --needed {list}"));
    }
    let aur = pkgs.iter().any(|p| p.starts_with("ani-"));
    if have("pacman") && !aur {
        return Some(format!("sudo pacman -S --needed {list}"));
    }
    if cfg!(target_os = "linux") && !from_brew(&exe()) {
        return Some(INSTALL_SH.into());
    }
    let brew: Vec<&str> = pkgs
        .iter()
        .filter_map(|p| match *p {
            "ani-cli" => Some("pystardust/ani-cli/ani-cli"),
            "ani-skip-git" | "xdg-utils" => None,
            p => Some(p),
        })
        .collect();
    let tap = match pkgs.contains(&"ani-cli") {
        true => "brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git && ",
        false => "",
    };
    (have("brew") && !brew.is_empty()).then(|| format!("{tap}brew install {}", brew.join(" ")))
}

/// The ani-cli ani-hop was tested with; older ones scrape a source that changed.
pub const TESTED_ANI_CLI: &str = "5.1";

/// The installed ani-cli's version, if it's older than TESTED_ANI_CLI.
pub fn old_ani_cli() -> Option<String> {
    let out = Command::new("ani-cli")
        .arg("-V")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?; // it exits early when curl or a player is missing
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    older(&v, TESTED_ANI_CLI).then_some(v)
}

/// Dotted versions compared number by number: 4.10 < 5.1 < 5.1.5, a leading v is ignored.
pub fn older(a: &str, b: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        let s = s.trim().trim_start_matches('v');
        s.split('.').map(|n| n.parse().unwrap_or(0)).collect()
    };
    nums(a) < nums(b)
}

/// Where `bin` lives on PATH, symlinks resolved.
fn located(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())?;
    Some(fs::canonicalize(&found).unwrap_or(found))
}

fn from_brew(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str() == "Cellar")
}

/// How to update ani-cli, judged by where it lives (None: only its package manager knows).
pub fn ani_cli_update() -> Option<String> {
    let path = located("ani-cli")?;
    if from_brew(&path) {
        return Some("brew upgrade ani-cli".into()); // before -U: brew's files are ours too
    }
    if fs::OpenOptions::new().append(true).open(&path).is_ok() {
        return Some("ani-cli -U".into()); // patches its own file, so only where we may write
    }
    if aur_helper().is_some() && have("pacman") {
        return Some(system_update());
    }
    // a distro's old copy: install.sh (or brew, for a brew ani-hop) puts a fresh one first
    (!path.starts_with("/nix"))
        .then(|| install_command(&["ani-cli"]))
        .flatten()
}

/// The system update that takes AUR packages along. Omarchy wants its own and refuses a
/// direct -Syu.
fn system_update() -> String {
    if have("omarchy-update") {
        return "omarchy update".into();
    }
    format!("{} -Syu", aur_helper().unwrap_or("yay"))
}

/// This ani-hop's binary, symlinks resolved.
fn exe() -> PathBuf {
    std::env::current_exe()
        .and_then(fs::canonicalize)
        .unwrap_or_default()
}

/// How to update this ani-hop, judged by where it lives.
pub fn update_command() -> String {
    let exe = exe();
    if exe.starts_with(home().join(".local/bin")) {
        INSTALL_SH.into()
    } else if from_brew(&exe) {
        "brew upgrade ani-hop".into()
    } else if exe.starts_with("/usr") {
        system_update()
    } else {
        "cargo install --locked --force --git https://github.com/ReynkeDeVos/ani-hop".into()
    }
}

/// Our lock in run/, held until we exit (or die): other ani-hops see we're alive by failing
/// to take it. Works where there's no /proc (macOS) and isn't fooled by reused pids.
static ALIVE: OnceLock<fs::File> = OnceLock::new();

fn run_dir() -> PathBuf {
    cache_dir().join("run")
}

pub fn setup() -> std::io::Result<()> {
    let bin = cache_dir().join("bin");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(cache_dir().join("probe"))?;
    for (name, script) in [("rofi", FAKE_ROFI), ("ffmpeg", FAKE_FFMPEG)] {
        fs::write(bin.join(name), script)?;
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755))?;
    }
    let run = run_dir();
    fs::create_dir_all(&run)?;
    let me = std::process::id();
    // locked before it gets its name, so no one ever finds it unlocked and deletes it
    let tmp = run.join(format!("{me}.tmp"));
    let lock = fs::File::create(&tmp)?;
    let _ = lock.lock(); // where locks don't work, held() takes every ani-hop for alive
    fs::rename(&tmp, run.join(format!("{me}.lock")))?;
    let _ = ALIVE.set(lock);
    for p in fs::read_dir(&run)?.flatten().map(|e| e.path()) {
        let pid = p.file_stem().unwrap_or_default().to_string_lossy();
        if p.extension().is_some_and(|x| x == "lock") && !running(&pid) {
            let _ = fs::remove_file(&p);
        }
    }
    Ok(())
}

fn ani_cli() -> Command {
    let path = format!(
        "{}:{}",
        cache_dir().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new("ani-cli");
    c.env("PATH", path).stdin(Stdio::null());
    c
}

/// ani-cli prints its errors with colors and line clears.
fn last_error(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let line = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("ani-cli failed");
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => while chars.next().is_some_and(|c| !c.is_ascii_alphabetic()) {},
            '\r' => out.clear(),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Runs ani-cli until it wants to show a menu and returns that menu as (prompt, lines).
/// `None` means ani-cli never asked (it went on to "play" with the debug player).
fn probe(args: &[&str]) -> Result<Option<(String, Vec<String>)>, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    // one at a time: the provider shouldn't see a burst when several shows are opened
    static TURN: Mutex<()> = Mutex::new(());
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let dir = cache_dir().join("probe");
    let out = dir.join(format!(
        "menu-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let res = ani_cli()
        .args(args)
        .env("ANIHOP_MENU_OUT", &out)
        .env("ANI_CLI_PLAYER", "debug")
        // a single-episode show gets its stream fetched here, and nearly every one has a sub
        .env("ANI_CLI_MODE", "sub")
        .env("ANI_CLI_HIST_DIR", &dir)
        .output()
        .map_err(|e| format!("ani-cli: {e}"))?;
    match fs::read_to_string(&out) {
        Ok(text) => {
            let _ = fs::remove_file(&out);
            let mut lines = text.lines().map(str::to_string);
            let prompt = lines.next().unwrap_or_default();
            Ok(Some((prompt, lines.filter(|l| !l.is_empty()).collect())))
        }
        Err(_) if res.status.success() => Ok(None),
        Err(_) => Err(last_error(&res.stderr)),
    }
}

/// Provider titles in ani-cli's order (index = position + 1). If there is just one hit,
/// ani-cli skips the menu, so we get its episodes instead and not its title.
pub fn search(query: &str) -> Result<(Vec<String>, Option<Vec<String>>), String> {
    match probe(&[query])? {
        Some((p, lines)) if p.starts_with("Select anime") => Ok((
            lines
                .iter()
                .map(|l| l.split_once(' ').map_or(l.as_str(), |(_, t)| t).to_string())
                .collect(),
            None,
        )),
        Some((_, eps)) => Ok((vec![query.to_string()], Some(eps))),
        None => Ok((vec![query.to_string()], Some(vec!["1".into()]))),
    }
}

/// AniList's results, or if AniList failed, ani-cli's: one show per provider entry, equal
/// titles numbered so each gets its own id and folder. `search` is `search`, passed in for tests.
pub fn or_ani_cli(
    anilist: Result<Vec<Media>, String>,
    query: &str,
    search: impl FnOnce(&str) -> Result<(Vec<String>, Option<Vec<String>>), String>,
) -> Result<Vec<Media>, String> {
    let e = match anilist {
        Err(e) => e,
        ok => return ok, // no results is an answer too
    };
    let (hits, _) = search(query).map_err(|a| format!("{e} · ani-cli: {a}"))?;
    let mut taken: HashSet<String> = hits.iter().cloned().collect();
    let mut first = HashSet::new();
    let shows = hits.iter().enumerate().map(|(i, title)| {
        // later entries of a title get the lowest free number: the provider may have its own "X (2)"
        let name = match first.insert(title) {
            true => title.clone(),
            false => (2..)
                .map(|n| format!("{title} ({n})"))
                .find(|n| !taken.contains(n))
                .unwrap(),
        };
        taken.insert(name.clone());
        Media::from_ani_cli(&name, Source::at(query, &hits, i + 1, Vec::new()))
    });
    Ok(shows.collect())
}

fn episodes(query: &str, index: usize) -> Result<Vec<String>, String> {
    match probe(&["-S", &index.to_string(), query])? {
        Some((_, eps)) => Ok(eps),
        None => Ok(vec!["1".into()]), // single-episode show, ani-cli doesn't ask
    }
}

/// Canonical title, so "Jujutsu Kaisen (TV)" = "JUJUTSU KAISEN" and
/// "My Hero Academia 2" = "My Hero Academia Season 2" = "... 2nd Season".
fn title_key(s: &str) -> String {
    let s = s.to_lowercase().replace('×', " x ");
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !matches!(*w, "" | "season" | "part" | "cour" | "the" | "tv"))
        .map(|w| {
            match w
                .strip_suffix(['t', 'd', 'h'])
                .and_then(|w| w.strip_suffix(['s', 'n', 'r', 't']))
            {
                Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => n,
                _ => w,
            }
        })
        .collect()
}

/// Best hits, equally good: same canonical title as on AniList, else the closest title
/// that contains one of AniList's (or is contained), else the provider's top hit.
fn best_matches(m: &Media, hits: &[String]) -> Vec<usize> {
    let titles: Vec<String> = [&m.title.english, &m.title.romaji]
        .into_iter()
        .flatten()
        .chain(&m.synonyms)
        .map(|t| title_key(t))
        .filter(|t| !t.is_empty())
        .collect();
    let hits: Vec<String> = hits.iter().map(|h| title_key(h)).collect();
    let closeness = |h: &String| {
        let near = titles
            .iter()
            .filter(|t| h.contains(t.as_str()) || t.contains(h.as_str()));
        near.map(|t| t.len().abs_diff(h.len())).min()
    };
    let scores: Vec<Option<usize>> = hits.iter().map(closeness).collect();
    match scores.iter().flatten().min() {
        Some(best) => (0..hits.len())
            .filter(|&i| scores[i] == Some(*best))
            .collect(),
        None => vec![0],
    }
}

/// The show `best_matches` compares titles with: one ani-cli found by its provider title, as
/// its shown name may carry a number ("A (2)") that would read as a season.
fn matched(m: &Media) -> Media {
    let mut m = m.clone();
    if let Some(at) = &m.ani_cli {
        m.title.romaji = Some(at.title.clone());
    }
    m
}

fn overrides_path() -> PathBuf {
    config_dir().join("sources.json")
}

/// Manually chosen sources: anilist id -> (search query, provider title).
fn overrides() -> HashMap<String, (String, String)> {
    read_json(&overrides_path())
}

pub fn save_override(id: u64, query: &str, title: &str) -> std::io::Result<()> {
    let mut all = overrides();
    all.insert(id.to_string(), (query.to_string(), title.to_string()));
    write_json(&overrides_path(), &all)
}

pub fn default_query(m: &Media) -> String {
    if let Some(at) = &m.ani_cli {
        return at.query.clone();
    }
    m.title
        .romaji
        .clone()
        .or(m.title.english.clone())
        .unwrap_or_default()
}

fn resolved_path() -> PathBuf {
    cache_dir().join("resolved.json")
}

/// Sources found in earlier sessions, so menus and playback don't wait for ani-cli.
pub fn load_resolved() -> HashMap<u64, Source> {
    read_json(&resolved_path())
}

pub fn save_resolved(all: &HashMap<u64, Source>) {
    let _ = write_json(&resolved_path(), all);
}

/// Re-checks a (possibly cached) source right before ani-cli runs: `-S` is positional and
/// the provider's order shifts when new seasons show up. One search, not a full resolve.
pub fn locate(m: &Media, src: &Source) -> Result<Source, String> {
    let (hits, _) = search(&src.query)?;
    match position(src, &hits) {
        Some(index) => Ok(Source::at(&src.query, &hits, index, src.episodes.clone())),
        None => resolve(m),
    }
}

/// Where `src` is in fresh `hits`: the nth entry with its title, as equal titles (a remake)
/// keep their order when others move in. Saved before 0.6.1: only an unambiguous match.
fn position(src: &Source, hits: &[String]) -> Option<usize> {
    let same: Vec<usize> = (1..=hits.len())
        .filter(|&i| hits[i - 1] == src.title)
        .collect();
    match src.nth {
        0 if same.len() == 1 => Some(same[0]),
        0 => None, // which of the equal titles is unknown: resolve again
        n => same.get(n - 1).copied(),
    }
}

pub fn resolve(m: &Media) -> Result<Source, String> {
    let saved = overrides().remove(&m.id.to_string());
    // picked from ani-cli's own results: that very entry, unless "Change source…" chose another
    let picked = m.ani_cli.as_ref().filter(|_| saved.is_none());
    let mut found = Err("no title to search for".to_string());
    if let Some(at) = picked {
        let (hits, eps) = search(&at.query)?;
        if let Some(eps) = eps {
            // a single hit: ani-cli shows no title to check, and it's the only show there is
            return Ok(Source::at(&at.query, &hits, 1, eps));
        }
        if let Some(index) = position(at, &hits) {
            return Ok(Source::at(
                &at.query,
                &hits,
                index,
                episodes(&at.query, index)?,
            ));
        }
        // gone, or picked as a single hit whose title ani-cli never showed: match titles below
        found = Ok((at.query.clone(), (hits, None)));
    }
    // provider search chokes on some symbols (HUNTER×HUNTER), so fall back to plainer queries
    let plain = |t: &String| {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut queries: Vec<String> = match &saved {
        Some((q, _)) => vec![q.clone()],
        None if picked.is_some() => Vec::new(), // searched above
        None => [
            m.title.romaji.clone(),
            m.title.english.clone(),
            m.title.romaji.as_ref().map(plain),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    queries.dedup();
    for q in queries {
        found = search(&q).map(|r| (q, r));
        if found.is_ok() {
            break;
        }
    }
    let (query, (hits, eps)) = found?;
    if let Some(eps) = eps {
        return Ok(Source::at(&query, &hits, 1, eps));
    }
    let exact = saved.map(|(_, t)| t).or(picked.map(|at| at.title.clone()));
    let candidates = match exact.and_then(|t| hits.iter().position(|h| *h == t)) {
        Some(pos) => vec![pos],
        None => best_matches(&matched(m), &hits),
    };
    // same title twice (HUNTER×HUNTER 1999/2011): a finished show's episode count decides
    let want = m
        .episodes
        .filter(|_| m.status.as_deref() == Some("FINISHED"));
    let mut pick = None;
    for &i in candidates.iter().take(3) {
        let eps = episodes(&query, i + 1)?;
        let fits = want.is_none_or(|n| eps.len() == n as usize);
        if pick.is_none() || fits {
            pick = Some((i, eps));
        }
        if fits {
            break;
        }
    }
    let (i, episodes) = pick.unwrap();
    Ok(Source::at(&query, &hits, i + 1, episodes))
}

/// Streams one episode with ani-skip; blocks until mpv is closed.
/// Feeds the child's output lines to `line` (also the ones redrawn with \r) until it
/// exits; returns whether it succeeded and what it wrote to stderr.
fn follow(mut child: Child, mut line: impl FnMut(&str)) -> (bool, Vec<u8>) {
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let mut buf = Vec::new();
    for b in BufReader::new(child.stdout.take().unwrap())
        .bytes()
        .map_while(Result::ok)
    {
        if b != b'\r' && b != b'\n' {
            buf.push(b);
            continue;
        }
        line(String::from_utf8_lossy(&buf).trim());
        buf.clear();
    }
    let ok = child.wait().is_ok_and(|s| s.success());
    (ok, err_reader.join().unwrap_or_default())
}

/// mpv prints "VO: [gpu-next] 1920x1080 ..." once its window opens.
fn player_up(line: &str, up: &mut impl FnMut()) {
    if line.starts_with("VO:") {
        up();
    }
}

/// Whether ani-skip jumps over openings and endings (switched in the main menu).
pub fn skip_on() -> bool {
    !config_dir().join("no-skip").exists()
}

pub fn set_skip(on: bool) -> std::io::Result<()> {
    let off = config_dir().join("no-skip");
    match on {
        true => fs::remove_file(off),
        false => fs::create_dir_all(config_dir()).and_then(|()| fs::write(off, "")),
    }
}

fn audio_path() -> PathBuf {
    config_dir().join("audio.json")
}

/// What a show plays with, as (key, label). The provider has Japanese and English audio,
/// both with English subtitles, which mpv shows or keeps hidden (v switches).
pub const AUDIO: [(&str, &str); 4] = [
    ("raw", "Japanese · no subtitles"),
    ("sub", "Japanese · English subtitles"),
    ("dub", "English dub"),
    ("dub+sub", "English dub · English subtitles"),
];

/// Audio chosen per AniList id and for "all" shows, as AUDIO keys.
fn choices() -> HashMap<String, String> {
    read_json(&audio_path())
}

/// A show's audio (None: the default): its own choice, else the default, else ani-cli's
/// ANI_CLI_MODE.
pub fn audio(id: Option<u64>) -> &'static str {
    audio_in(&choices(), id)
}

fn audio_in(all: &HashMap<String, String>, id: Option<u64>) -> &'static str {
    let known = |a: Option<&String>| {
        AUDIO
            .iter()
            .map(|k| k.0)
            .find(|k| a.is_some_and(|a| a == k))
    };
    let env = std::env::var("ANI_CLI_MODE").is_ok_and(|m| m == "dub");
    known(id.and_then(|i| all.get(&i.to_string())))
        .or_else(|| known(all.get("all")))
        .unwrap_or(if env { "dub" } else { "sub" })
}

pub fn audio_label(audio: &str) -> &'static str {
    AUDIO.iter().find(|a| a.0 == audio).map_or("", |a| a.1)
}

pub fn dub(audio: &str) -> bool {
    audio.starts_with("dub")
}

fn subs(audio: &str) -> bool {
    audio.ends_with("sub")
}

pub fn set_audio(id: Option<u64>, audio: &str) -> std::io::Result<()> {
    let mut all = choices();
    choose(&mut all, id, audio);
    write_json(&audio_path(), &all)
}

/// A show keeps a choice of its own only while it differs from the default.
fn choose(all: &mut HashMap<String, String>, id: Option<u64>, audio: &str) {
    match id {
        Some(id) if audio == audio_in(all, None) => _ = all.remove(&id.to_string()),
        Some(id) => _ = all.insert(id.to_string(), audio.into()),
        None => {
            all.retain(|_, a| a != audio);
            all.insert("all".into(), audio.into());
        }
    }
}

fn mode(dub: bool) -> &'static str {
    if dub { "dub" } else { "sub" }
}

/// Streams through ani-cli (with ani-skip); `up` is called once mpv shows the episode.
pub fn stream(src: &Source, ep: &str, audio: &str, mut up: impl FnMut()) -> Result<(), String> {
    let mut flags = std::env::var("ANI_CLI_PLAYER_FLAGS").unwrap_or_default();
    // mpv unless the user picked another: ani-cli would take IINA on macOS and Windows'
    // mpv.exe under WSL
    let player = std::env::var("ANI_CLI_PLAYER")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "mpv".into());
    // an mpv flag, other players ani-cli knows (vlc, syncplay) don't start with it
    if player.contains("mpv") && !subs(audio) {
        flags.push_str(" --sub-visibility=no");
    }
    let child = ani_cli()
        .env("ANI_CLI_PLAYER", player)
        .env("ANI_CLI_MODE", mode(dub(audio)))
        .env("ANI_CLI_PLAYER_FLAGS", flags)
        // ani-cli stops when --skip finds no ani-skip, and ani-skip is optional
        .args((skip_on() && have("ani-skip")).then_some("--skip"))
        .args([
            "--exit-after-play",
            "-S",
            &src.index.to_string(),
            "-e",
            ep,
            &src.query,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ani-cli: {e}"))?;
    match follow(child, |l| player_up(l, &mut up)) {
        (true, _) => Ok(()),
        (false, stderr) => Err(last_error(&stderr)),
    }
}

pub fn play_file(m: &Media, ep: &str, file: &Path, mut up: impl FnMut()) -> Result<(), String> {
    let mut mpv = Command::new("mpv");
    mpv.arg(format!("--force-media-title={} Episode {ep}", m.name()));
    let vtt = file.with_extension("vtt");
    if vtt.exists() {
        mpv.arg(format!("--sub-file={}", vtt.display()));
        if hide_subs(audio(Some(m.id)), is_dub(file)) {
            mpv.arg("--sub-visibility=no");
        }
    }
    if let Some(mal) = m.id_mal.filter(|_| skip_on()) {
        let skip = Command::new("ani-skip")
            .args(["-i", &mal.to_string(), "-e", ep])
            .stderr(Stdio::null())
            .output();
        if let Ok(o) = skip {
            mpv.args(String::from_utf8_lossy(&o.stdout).split_whitespace());
        }
    }
    let child = mpv
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("mpv: {e}"))?;
    match follow(child, |l| player_up(l, &mut up)) {
        (true, _) => Ok(()),
        (false, stderr) => Err(last_error(&stderr)),
    }
}

/// What ani-hop knows about a show folder it downloaded into. It's kept in the config
/// folder, so the download folder holds only episodes and subtitles.
#[derive(Default, Serialize, Deserialize)]
struct Downloaded {
    media: Media,
    /// episode files that are the English dub, ani-cli names both the same
    #[serde(default)]
    dub: Vec<String>,
}

/// By show folder.
type Index = std::collections::BTreeMap<PathBuf, Downloaded>;

#[cfg(not(test))]
fn index_path() -> PathBuf {
    config_dir().join("downloads.json")
}

#[cfg(test)]
fn index_path() -> PathBuf {
    std::env::temp_dir().join("ani-hop-test-index.json")
}

fn index() -> Index {
    read_json(&index_path())
}

/// Changes the index under a lock, as another ani-hop may be changing it too.
fn edit_index(f: impl FnOnce(&mut Index)) -> std::io::Result<()> {
    let path = index_path();
    fs::create_dir_all(path.parent().unwrap())?;
    // its own file, as the rename below swaps the index for a new one
    let lock = fs::File::create(path.with_extension("lock"))?;
    lock.lock()?;
    // a broken index stays as it is, rather than be overwritten with just this change
    let mut all = match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        r => serde_json::from_slice(&r?)?,
    };
    f(&mut all);
    write_json(&path, &all)
}

fn name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// A show folder ani-hop downloaded into. One it doesn't know may be the user's own.
pub fn ours(dir: &Path) -> bool {
    dir.is_dir() && index().contains_key(dir)
}

/// Download folder of a show.
pub fn folder(m: &Media) -> PathBuf {
    let existing = local_shows().into_iter().find(|(lm, _)| lm.id == m.id);
    existing
        .map(|(_, p)| p)
        .unwrap_or_else(|| download_root().join(m.name().replace('/', "∕")))
}

pub fn local_shows() -> Vec<(Media, PathBuf)> {
    let root = download_root();
    let mut shows: Vec<_> = index()
        .into_iter()
        .filter(|(p, _)| p.parent() == Some(&root) && p.is_dir())
        .map(|(p, d)| (d.media, p))
        .collect();
    shows.sort_by(|a: &(Media, _), b| a.0.name().cmp(b.0.name()));
    shows
}

/// Downloaded episodes as (episode, file), sorted by episode number.
pub fn local_episodes(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut eps: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mp4"))
        .filter_map(|p| {
            let ep = p
                .file_stem()?
                .to_str()?
                .rsplit_once(" Episode ")?
                .1
                .to_string();
            Some((ep, p))
        })
        .collect();
    eps.sort_by(|a, b| ep_num(&a.0).total_cmp(&ep_num(&b.0)));
    eps
}

pub fn ep_num(ep: &str) -> f64 {
    ep.parse().unwrap_or(f64::MAX)
}

/// Whether a downloaded episode starts with its subtitles hidden. What the file is counts,
/// not what the show is set to now: a Japanese one keeps them unless none are wanted at all.
fn hide_subs(audio: &str, dub_file: bool) -> bool {
    audio == "raw" || (dub_file && !subs(audio))
}

fn is_dub(file: &Path) -> bool {
    let show = index().remove(file.parent().unwrap());
    show.is_some_and(|d| d.dub.contains(&name(file)))
}

/// Whether ffprobe finds no sound in a file: ani-cli 5.1 and older download only the video
/// of a show whose audio is a separate track. Without ffprobe nothing counts as silent.
fn silent(file: &Path) -> bool {
    let mut url = std::ffi::OsString::from("file:"); // a "Title: …" path isn't a protocol
    url.push(file);
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=index",
        ])
        .args(["-of", "csv=p=0"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.is_empty())
}

pub fn delete_episode(file: &Path) -> std::io::Result<()> {
    let _ = fs::remove_file(file.with_extension("vtt"));
    fs::remove_file(file)?;
    drop_if_empty(file.parent().unwrap())
}

/// Deletes what ani-hop downloaded into a show folder (episodes, subtitles, unfinished
/// parts) and the folder if that was all. A same-named folder may hold the user's own too.
pub fn delete_show(dir: &Path) -> std::io::Result<()> {
    for p in fs::read_dir(dir)?.flatten().map(|e| e.path()) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        let ours = name.contains(" Episode ")
            && (name.ends_with(".mp4") || name.ends_with(".vtt") || name.contains(".mp4."));
        if ours {
            fs::remove_file(&p)?;
        }
    }
    edit_index(|all| {
        all.remove(dir);
    })?;
    let _ = fs::remove_dir(dir); // fails while something of the user's is left in it
    Ok(())
}

/// Drops a show folder of ours once nothing is left in it.
fn drop_if_empty(dir: &Path) -> std::io::Result<()> {
    if ours(dir) && fs::read_dir(dir)?.next().is_none() {
        fs::remove_dir(dir)?;
        edit_index(|all| {
            all.remove(dir);
        })?;
    }
    Ok(())
}

/// Deletes what unfinished downloads of `eps` left in a show folder (yt-dlp's .part
/// files, subtitles), and the folder if that was all it held.
pub fn discard(dir: &Path, eps: &[String]) {
    if !ours(dir) {
        return;
    }
    let done: Vec<String> = local_episodes(dir).into_iter().map(|(e, _)| e).collect();
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for p in rd.flatten().map(|e| e.path()) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        let partial = eps.iter().filter(|ep| !done.contains(ep)).any(|ep| {
            name.contains(&format!(" Episode {ep}.mp4."))
                || name.ends_with(&format!(" Episode {ep}.vtt"))
        });
        if partial {
            let _ = fs::remove_file(&p);
        }
    }
    let _ = drop_if_empty(dir);
}

/// Whether `m` may become the show saved for a download folder: one ani-cli found never
/// replaces one AniList knows, whose details and MAL id (for ani-skip) Downloads needs.
fn replaces(m: &Media, saved: &Media) -> bool {
    m.on_anilist() || saved.id == 0 // 0: a new entry
}

/// Downloads one episode, calling `progress` with yt-dlp's status lines. `wanted` is asked
/// right before it starts, under the lock `cancel` takes, so a stop is never missed.
pub fn download(
    m: &Media,
    src: &Source,
    ep: &str,
    dub: bool,
    wanted: impl FnOnce() -> bool,
    mut progress: impl FnMut(&str),
) -> Result<(), String> {
    let dir = folder(m);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    edit_index(|all| {
        let d = all.entry(dir.clone()).or_default();
        if replaces(m, &d.media) {
            d.media = m.clone();
        }
    })
    .map_err(|e| e.to_string())?;
    if local_episodes(&dir).iter().any(|(e, _)| e == ep) {
        return Ok(()); // yt-dlp wouldn't replace it, so it stays the audio it was
    }
    let child = {
        let mut dl = DOWNLOAD.lock().unwrap();
        if dl.0 || !wanted() {
            return Err("stopped".into());
        }
        let child = ani_cli()
            .args(["-d", "-S", &src.index.to_string(), "-e", ep, &src.query])
            .env("ANI_CLI_DOWNLOAD_DIR", &dir)
            .env("ANI_CLI_MODE", mode(dub))
            .env("ANI_CLI_HIST_DIR", cache_dir().join("probe")) // downloading isn't watching
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ani-cli: {e}"))?;
        dl.1 = Some((child.id(), dir.clone(), ep.to_string()));
        child
    };
    let (_, stderr) = follow(child, |l| {
        if let Some(rest) = l.strip_prefix("[download]") {
            progress(rest.trim());
        } else if let Some(t) = l
            .strip_prefix("out_time=")
            .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        {
            // ffmpeg, through FAKE_FFMPEG: how much of the episode it has, without an hour of 00
            let t = t.split('.').next().unwrap_or(t);
            progress(&format!("{} so far", t.strip_prefix("00:").unwrap_or(t)));
        }
    });
    DOWNLOAD.lock().unwrap().1 = None;
    // ani-cli exits 0 even if yt-dlp failed, so the file is the only reliable signal
    match local_episodes(&dir).into_iter().find(|(e, _)| e == ep) {
        Some((_, file)) if silent(&file) => {
            let _ = delete_episode(&file);
            Err("no sound, its audio track needs a newer ani-cli".into())
        }
        Some((_, file)) => {
            let _ = edit_index(|all| {
                if let Some(d) = all.get_mut(&dir) {
                    d.dub.retain(|f| *f != name(&file));
                    if dub {
                        d.dub.push(name(&file));
                    }
                }
            });
            Ok(())
        }
        None => Err(last_error(&stderr)),
    }
}

/// Stops the running download for good; yt-dlp resumes the .part file next time, ffmpeg
/// (FAKE_FFMPEG) starts over. (Closing the terminal needs nothing: the hangup reaches our
/// whole process group.)
pub fn stop_download() {
    let mut dl = DOWNLOAD.lock().unwrap();
    dl.0 = true;
    if let Some((pid, ..)) = dl.1.take() {
        kill(pid);
    }
}

/// Stops episode `ep` of the show in `dir` if it's the one downloading. The caller took it
/// out of the queue first, so if it isn't downloading yet, `wanted` keeps it from starting.
pub fn cancel(dir: &Path, ep: &str) {
    let mut dl = DOWNLOAD.lock().unwrap();
    if let Some((pid, ..)) = dl.1.take_if(|(_, d, e)| d == dir && e == ep) {
        kill(pid);
    }
}

/// Ends an ani-cli download and what it runs.
fn kill(pid: u32) {
    let pid = pid.to_string();
    // The ani-cli shell first: it holds the signal until yt-dlp is gone and then exits.
    // Signalled after yt-dlp, it falls back to ffmpeg and keeps downloading after we quit.
    let _ = Command::new("kill").args(["-TERM", &pid]).status();
    let _ = Command::new("pkill").args(["-TERM", "-P", &pid]).status();
}

/// Whether downloads were stopped by quitting, so a failed one was cut short, not broken.
pub fn stopped() -> bool {
    DOWNLOAD.lock().unwrap().0
}

/// Whether `pid` is a running ani-hop: its lock in run/ is taken.
pub fn running(pid: &str) -> bool {
    held(&run_dir().join(format!("{pid}.lock")))
}

/// Shared, so it works on a read-only handle (NFS insists). Any error counts as held:
/// better to leave a dead ani-hop's queue alone than take a live one's.
fn held(lock: &Path) -> bool {
    fs::File::open(lock).is_ok_and(|f| f.try_lock_shared().is_err())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(romaji: &str, english: &str, hits: &[&str]) -> usize {
        let mut m = Media::default();
        m.title.romaji = Some(romaji.into());
        m.title.english = Some(english.into());
        best_matches(&m, &hits.iter().map(|h| h.to_string()).collect::<Vec<_>>())[0]
    }

    #[test]
    fn matches_provider_titles() {
        let aot = [
            "Attack on Titan: No Regrets",
            "Attack on Titan",
            "Attack on Titan Season 2",
        ];
        assert_eq!(pick("Shingeki no Kyojin", "Attack on Titan", &aot), 1);
        let jjk = [
            "Jujutsu Kaisen 3rd Season: The Culling Game Part 1",
            "Jujutsu Kaisen (TV)",
            "Jujutsu Kaisen 2nd Season",
        ];
        assert_eq!(pick("Jujutsu Kaisen", "JUJUTSU KAISEN", &jjk), 1);
        assert_eq!(
            pick("Jujutsu Kaisen 2nd Season", "JUJUTSU KAISEN Season 2", &jjk),
            2
        );
        let mha = [
            "My Hero Academia the Movie 2: Heroes Rising",
            "My Hero Academia 2",
            "My Hero Academia Season 2: Hero Notebook",
        ];
        assert_eq!(
            pick("Boku no Hero Academia 2", "My Hero Academia Season 2", &mha),
            1
        );
        assert_eq!(
            pick(
                "HUNTER×HUNTER (2011)",
                "Hunter x Hunter (2011)",
                &["Hunter x Hunter (2011)"]
            ),
            0
        );
        let mut hxh = Media::default();
        hxh.title.english = Some("Hunter x Hunter".into());
        let hits = [
            "Hunter x Hunter",
            "Hunter x Hunter",
            "Hunter x Hunter Movie 1: Phantom Rouge",
        ]
        .map(String::from);
        assert_eq!(best_matches(&hxh, &hits), [0, 1]); // tie, resolved by episode count
        // nothing close: trust the provider's ranking
        assert_eq!(
            pick(
                "Kimi no Na wa.",
                "Your Name.",
                &["Weathering With You", "Suzume"]
            ),
            0
        );
    }

    #[test]
    fn falls_back_to_ani_cli_only_when_anilist_fails() {
        let unused = |_: &str| -> Result<(Vec<String>, Option<Vec<String>>), String> {
            panic!("ani-cli searched although AniList answered")
        };
        let list = vec![Media::default()];
        assert_eq!(or_ani_cli(Ok(list), "x", unused).unwrap().len(), 1);
        assert!(or_ani_cli(Ok(vec![]), "x", unused).unwrap().is_empty());

        let hits = |_: &str| Ok((vec!["A".into(), "B".into(), "A".into()], None));
        let shows = or_ani_cli(Err("AniList: down".into()), "a", hits).unwrap();
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["A", "B", "A (2)"]);
        assert!(shows.iter().all(|m| !m.on_anilist()));
        assert!(shows[0].id != shows[2].id && shows[0].id != shows[1].id);
        let at: Vec<(usize, &str, usize)> = (shows.iter())
            .map(|m| m.ani_cli.as_ref().unwrap())
            .map(|s| (s.index, s.title.as_str(), s.nth))
            .collect();
        assert_eq!(at, [(1, "A", 1), (2, "B", 1), (3, "A", 2)]);

        // the provider's own "A (2)" keeps its name, so the second A takes the next number
        let clash = |_: &str| Ok((vec!["A".into(), "A".into(), "A (2)".into()], None));
        let shows = or_ani_cli(Err("e".into()), "a", clash).unwrap();
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["A", "A (3)", "A (2)"]);
        let ids: HashSet<u64> = shows.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 3);
        // the same entries from an earlier search without the provider's "A (2)" keep their ids
        let before = |_: &str| Ok((vec!["A".into(), "A".into()], None));
        let before = or_ani_cli(Err("e".into()), "a", before).unwrap();
        assert_eq!(before[1].name(), "A (2)");
        assert_eq!(before[1].id, shows[1].id); // the second A, now "A (3)"
        assert_ne!(before[1].id, shows[2].id); // not the provider's "A (2)"

        // one hit: ani-cli shows no title, only its episodes
        let single = |q: &str| Ok((vec![q.to_string()], Some(vec!["1".into(), "2".into()])));
        let shows = or_ani_cli(Err("e".into()), "Frieren 2", single).unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].name(), "Frieren 2");
        assert_eq!(shows[0].ani_cli.as_ref().unwrap().index, 1);

        let none = |_: &str| Err("No results found!".to_string());
        let e = or_ani_cli(Err("AniList: down".into()), "a", none).unwrap_err();
        assert_eq!(e, "AniList: down · ani-cli: No results found!");
    }

    #[test]
    fn keeps_the_second_of_two_equal_titles() {
        let hits: Vec<String> = ["HxH", "Fantasy", "HxH"].map(String::from).into();
        let src = Source::at("hxh", &hits, 3, Vec::new());
        assert_eq!((src.title.as_str(), src.nth), ("HxH", 2));
        assert_eq!(position(&src, &hits), Some(3));
        // a new show in front: still the second HxH
        let moved: Vec<String> = ["New", "HxH", "Fantasy", "HxH"].map(String::from).into();
        assert_eq!(position(&src, &moved), Some(4));
        assert_eq!(position(&src, &hits[..2]), None); // the second one is gone
        // saved before 0.6.1: its only match, or resolve again when it's one of several
        let old = Source { nth: 0, ..src };
        assert_eq!(position(&old, &hits), None);
        assert_eq!(position(&old, &hits[1..]), Some(2));
    }

    #[test]
    fn a_single_hit_is_found_again_by_its_title() {
        // picked as the only hit: ani-cli showed no title, so the typed query stands in for it
        let query = "frieren beyond journey's end season 2";
        let at = Source::at(query, &[query.to_string()], 1, Vec::new());
        let m = Media::from_ani_cli(query, at);
        let hits = [
            "Frieren: Beyond Journey's End Season 2 Specials",
            "Frieren: Beyond Journey's End Season 2",
        ]
        .map(String::from);
        assert_eq!(position(m.ani_cli.as_ref().unwrap(), &hits), None);
        assert_eq!(best_matches(&matched(&m), &hits), [1]); // what resolve falls back to

        // a numbered one is matched by its provider title, not "A (2)" (= "A Season 2")
        let hits = ["A", "A", "A Season 2"].map(String::from);
        let at = Source::at("a", &hits, 2, Vec::new());
        let m = Media::from_ani_cli("A (2)", at);
        let renamed = ["A TV", "A Season 2"].map(String::from); // no exact "A" left
        assert_eq!(best_matches(&matched(&m), &renamed), [0]);
        assert_eq!(best_matches(&m, &renamed), [1]);
    }

    #[test]
    fn ani_cli_shows_never_replace_anilist_downloads() {
        let at = Source::at("q", &["Show".into()], 1, Vec::new());
        let found = Media::from_ani_cli("Show", at);
        let anilist = Media {
            id: 21,
            ..Media::default()
        };
        let new_entry = Media::default();
        assert!(replaces(&found, &new_entry));
        assert!(!replaces(&found, &anilist));
        assert!(replaces(&anilist, &found));
        assert!(replaces(&anilist, &anilist));
    }

    #[test]
    fn compares_versions_number_by_number() {
        assert!(older("4.10", "5.1"));
        assert!(older("5.0.2", TESTED_ANI_CLI));
        assert!(!older("5.1.5", TESTED_ANI_CLI));
        assert!(!older("5.1", TESTED_ANI_CLI));
        assert!(older("0.5.0", "v0.6.0"));
        assert!(!older("0.10.0", "v0.9.1"));
        // no release yet: /releases/latest points at /releases
        assert!(!older("0.6.0", "releases"));
    }

    #[test]
    fn a_lock_is_held_until_its_file_closes() {
        let path = std::env::temp_dir().join(format!("ani-hop-lock-{}", std::process::id()));
        let file = fs::File::create(&path).unwrap();
        assert!(!held(&path));
        file.lock().unwrap();
        assert!(held(&path));
        drop(file);
        assert!(!held(&path));
        fs::remove_file(&path).unwrap();
        assert!(!held(&path));
    }

    #[test]
    fn finds_videos_dir_in_user_dirs() {
        let home = Path::new("/home/a");
        let file = "# comment\nXDG_MUSIC_DIR=\"$HOME/Musik\"\nXDG_VIDEOS_DIR=\"$HOME/Vidéos\"\n";
        assert_eq!(videos_dir(file, home), Path::new("/home/a/Vidéos"));
        assert_eq!(
            videos_dir("XDG_VIDEOS_DIR=\"relative\"", home),
            Path::new("/home/a/Videos")
        );
        assert_eq!(videos_dir("", home), Path::new("/home/a/Videos"));
        let commented = "XDG_VIDEOS_DIR=\"$HOME/Vids\" # mine";
        assert_eq!(videos_dir(commented, home), Path::new("/home/a/Vids"));
        let disabled = "XDG_VIDEOS_DIR=\"$HOME/\"";
        assert_eq!(videos_dir(disabled, home), Path::new("/home/a/Videos"));
        let unquoted = "XDG_VIDEOS_DIR=$HOME/Clips";
        assert_eq!(videos_dir(unquoted, home), Path::new("/home/a/Videos"));
    }

    #[test]
    fn discards_only_unfinished_leftovers() {
        let dir = std::env::temp_dir().join(format!("ani-hop-discard-{}", std::process::id()));
        let files = [
            "Show Episode 1.mp4",
            "Show Episode 1.vtt",
            "Show Episode 2.mp4.part-Frag7",
            "Show Episode 2.mp4.ytdl",
            "Show Episode 2.vtt",
            "Show Episode 12.mp4.part",
        ];
        fs::create_dir_all(&dir).unwrap();
        files
            .iter()
            .for_each(|f| fs::write(dir.join(f), "").unwrap());
        edit_index(|all| drop(all.insert(dir.clone(), Downloaded::default()))).unwrap();
        discard(&dir, &["1".into(), "2".into()]);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [files[0], files[1], files[5]]);
        // nothing finished and nothing else left: the folder goes
        fs::remove_file(dir.join(files[0])).unwrap();
        discard(&dir, &["12".into()]);
        assert!(dir.exists(), "episode 1's subtitles are still there");
        fs::remove_file(dir.join(files[1])).unwrap();
        discard(&dir, &[]);
        assert!(!dir.exists() && !index().contains_key(&dir));
        // a same-named folder of the user's own stays as it is
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(files[4]), "").unwrap();
        discard(&dir, &["2".into()]);
        assert!(dir.join(files[4]).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn audio_per_show_only_while_it_differs() {
        let mut all = HashMap::from([("all".to_string(), "sub".to_string())]);
        choose(&mut all, Some(1), "dub");
        assert_eq!(
            (audio_in(&all, Some(1)), audio_in(&all, Some(2))),
            ("dub", "sub")
        );
        choose(&mut all, None, "dub");
        assert_eq!(
            all.len(),
            1,
            "show 1 matches the default now and follows it"
        );
        choose(&mut all, None, "raw");
        assert_eq!(audio_in(&all, Some(1)), "raw");
        choose(&mut all, Some(1), "dub+sub");
        choose(&mut all, Some(1), "raw");
        assert_eq!(all.len(), 1, "back to the default, nothing of its own left");
        all.insert("2".into(), "klingon".into());
        assert_eq!(audio_in(&all, Some(2)), "raw", "unknown choices fall back");
        assert!(dub("dub+sub") && subs("dub+sub") && !subs("dub") && subs("sub") && !dub("raw"));
        // downloaded: Japanese files keep their subtitles under "English dub"
        assert!(!hide_subs("dub", false) && hide_subs("dub", true) && hide_subs("raw", false));
        assert!(!hide_subs("dub+sub", true) && !hide_subs("sub", true));
    }

    #[test]
    fn deletes_only_what_was_downloaded() {
        let root = std::env::temp_dir().join(format!("ani-hop-delete-{}", std::process::id()));
        let dir = root.join("Show");
        let episodes = [
            "Show Episode 1.mp4",
            "Show Episode 1.vtt",
            "Show Episode 2.mp4.part",
        ];
        fs::create_dir_all(&dir).unwrap();
        for f in episodes.iter().chain(&["notes.txt"]) {
            fs::write(dir.join(f), "").unwrap();
        }
        edit_index(|all| drop(all.insert(dir.clone(), Downloaded::default()))).unwrap();
        assert!(ours(&dir), "read back from the file");
        delete_show(&dir).unwrap();
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            left,
            ["notes.txt"],
            "the user's own file stays, and the folder with it"
        );
        assert!(!ours(&dir));
        fs::remove_file(dir.join("notes.txt")).unwrap();
        edit_index(|all| drop(all.insert(dir.clone(), Downloaded::default()))).unwrap();
        delete_show(&dir).unwrap();
        assert!(!dir.exists());
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn writes_json_through_symlinks() {
        let dir = std::env::temp_dir().join(format!("ani-hop-json-{}", std::process::id()));
        let (real, link) = (
            dir.join("dotfiles/audio.json"),
            dir.join("config/audio.json"),
        );
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_json(&link, &HashMap::from([("all", "dub")])).unwrap();
        assert!(
            link.is_symlink(),
            "the link stays, the target gets the content"
        );
        assert_eq!(read_json::<HashMap<String, String>>(&real)["all"], "dub");
        let fresh = dir.join("new/first.json");
        write_json(&fresh, &vec![1]).unwrap();
        assert_eq!(read_json::<Vec<u32>>(&fresh), [1]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fake_ffmpeg_renames_only_finished_downloads() {
        let dir = std::env::temp_dir().join(format!("ani-hop-ffmpeg-{}", std::process::id()));
        let (fake, real) = (dir.join("fake"), dir.join("real"));
        // stands in for the real one: logs its arguments, writes its last one, warns $WARN
        let logger = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LOG\"\nfor a; do o=$a; done\n: > \"$o\"\n[ -z \"$WARN\" ] || echo \"$WARN\" >&2\nexit $CODE\n";
        for (bin, script) in [(&fake, FAKE_FFMPEG), (&real, logger)] {
            fs::create_dir_all(bin).unwrap();
            fs::write(bin.join("ffmpeg"), script).unwrap();
            fs::set_permissions(bin.join("ffmpeg"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (log, out) = (dir.join("log"), dir.join("A Episode 1.mp4"));
        let part = dir.join("A Episode 1.mp4.part");
        let run = |code: &str, warn: &str, args: &[&str]| {
            let _ = fs::remove_file(&out);
            let path = format!("{}:{}:/usr/bin:/bin", fake.display(), real.display());
            let o = Command::new(fake.join("ffmpeg"))
                .args(args)
                .arg(&out)
                .env("PATH", path)
                .env("LOG", &log)
                .env("CODE", code)
                .env("WARN", warn)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            (o.status.success(), fs::read_to_string(&log).unwrap(), err)
        };
        let ani_cli = ["-extension_picky", "0", "-i", "v.m3u8", "-c", "copy"];
        let (ok, args, _) = run("0", "", &ani_cli);
        assert!(ok && out.exists() && !part.exists());
        assert!(args.ends_with(&format!("-f\nmp4\n{}\n", part.display())));
        let (ok, _, _) = run("1", "", &ani_cli);
        assert!(
            !ok && !out.exists() && part.exists(),
            "a failed one stays .part"
        );
        // ffmpeg exits 0 after skipping a segment it couldn't get
        let skipped = "[in#0/hls @ 0x1] Segment 7 of playlist 0 failed too many times, skipping";
        let (ok, _, err) = run("0", skipped, &ani_cli);
        assert!(!ok && !out.exists() && part.exists());
        assert!(err.starts_with(skipped) && err.ends_with("parts of it failed to download\n"));
        let broken = "[NULL @ 0x1] [error] Invalid NAL unit size (1234 > 567).";
        let (ok, _, _) = run("0", broken, &ani_cli);
        assert!(
            !ok && !out.exists() && part.exists(),
            "errors in what it got"
        );
        let (ok, _, _) = run("0", "[mp4 @ 0x1] [warning] Non-monotonic DTS", &ani_cli);
        assert!(ok && out.exists(), "the muxer's warnings lose nothing");
        assert!(!dir.join("A Episode 1.mp4.part.log").exists());
        // yt-dlp's own calls (merging, fixups) go through untouched
        let (ok, args, _) = run("0", "", &["-y", "-i", "v.mp4"]);
        assert!(ok && out.exists());
        assert_eq!(args, format!("-y\n-i\nv.mp4\n{}\n", out.display()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strips_ani_cli_colors() {
        assert_eq!(
            last_error(b"info\n\x1b[2K\r\x1b[1;31mEpisode not released!\x1b[0m\n"),
            "Episode not released!"
        );
    }
}
