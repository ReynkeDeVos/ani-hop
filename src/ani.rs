//! Everything that touches ani-cli, ani-skip, mpv and the download folder.
//!
//! ani-cli has no machine-readable output, so we read its menus instead: with stdin not
//! being a tty it switches to rofi, and we put a stand-in `rofi` first in its PATH that
//! writes the menu to a file. That way the provider scraping stays ani-cli's business and
//! keeps working after `ani-cli -U`.

use crate::anilist::Media;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// (stopped, pid of the running ani-cli download). One lock, so quitting can't race
/// the worker starting the next episode.
static DOWNLOAD: Mutex<(bool, Option<u32>)> = Mutex::new((false, None));

const FAKE_ROFI: &str = r#"#!/bin/sh
# ani-hop: captures ani-cli's menu instead of showing one
while [ $# -gt 0 ]; do [ "$1" = -p ] && p="$2"; shift; done
{ printf '%s\n' "$p"; cat; } > "${ANIHOP_MENU_OUT:-/dev/null}"
exit 1
"#;

/// Where an AniList entry lives on the provider, as ani-cli sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub query: String,
    pub index: usize,
    pub title: String,
    pub episodes: Vec<String>,
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

fn root_path() -> PathBuf {
    config_dir().join("download-dir")
}

/// The folder from the settings, else ANI_CLI_DOWNLOAD_DIR, else ~/Videos/Anime.
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
pub const DEPS: [(&str, &str, &str); 7] = [
    ("ani-cli", "ani-cli", "streams and downloads"),
    ("mpv", "mpv", "plays episodes"),
    ("ani-skip", "ani-skip-git", "skips openings and endings"),
    ("yt-dlp", "yt-dlp", "downloads episodes"),
    ("ffmpeg", "ffmpeg", "finishes downloads"),
    ("curl", "curl", "ani-cli's requests"),
    ("xdg-open", "xdg-utils", "opens the AniList login"),
];

pub fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub fn missing_deps() -> Vec<(&'static str, &'static str, &'static str)> {
    DEPS.into_iter().filter(|d| !have(d.0)).collect()
}

/// One command for everything missing. ani-cli and ani-skip live in the AUR, so plain
/// pacman only qualifies when neither is needed.
pub fn install_command(pkgs: &[&str]) -> Option<String> {
    let list = pkgs.join(" ");
    if let Some(helper) = ["yay", "paru"].into_iter().find(|h| have(h)) {
        return Some(format!("{helper} -S --needed {list}"));
    }
    let aur = pkgs.iter().any(|p| p.starts_with("ani-"));
    (have("pacman") && !aur).then(|| format!("sudo pacman -S --needed {list}"))
}

pub fn setup() -> std::io::Result<()> {
    let bin = cache_dir().join("bin");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(cache_dir().join("probe"))?;
    fs::write(bin.join("rofi"), FAKE_ROFI)?;
    fs::set_permissions(bin.join("rofi"), fs::Permissions::from_mode(0o755))
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

fn overrides_path() -> PathBuf {
    config_dir().join("sources.json")
}

/// Manually chosen sources: anilist id -> (search query, provider title).
fn overrides() -> HashMap<String, (String, String)> {
    fs::read(overrides_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_override(id: u64, query: &str, title: &str) -> std::io::Result<()> {
    let mut all = overrides();
    all.insert(id.to_string(), (query.to_string(), title.to_string()));
    fs::create_dir_all(config_dir())?;
    fs::write(overrides_path(), serde_json::to_vec_pretty(&all)?)
}

pub fn default_query(m: &Media) -> String {
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
    fs::read(resolved_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_resolved(all: &HashMap<u64, Source>) {
    let _ = fs::write(resolved_path(), serde_json::to_vec(all).unwrap_or_default());
}

/// Re-checks a (possibly cached) source right before ani-cli runs: `-S` is positional and
/// the provider's order shifts when new seasons show up. One search, not a full resolve.
pub fn locate(m: &Media, src: &Source) -> Result<Source, String> {
    let (hits, _) = search(&src.query)?;
    match hits.iter().position(|h| *h == src.title) {
        Some(i) => Ok(Source {
            index: i + 1,
            ..src.clone()
        }),
        None => resolve(m),
    }
}

pub fn resolve(m: &Media) -> Result<Source, String> {
    let saved = overrides().remove(&m.id.to_string());
    // provider search chokes on some symbols (HUNTER×HUNTER), so fall back to plainer queries
    let plain = |t: &String| {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut queries: Vec<String> = match &saved {
        Some((q, _)) => vec![q.clone()],
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
    let mut found = Err("no title to search for".to_string());
    for q in queries {
        found = search(&q).map(|r| (q, r));
        if found.is_ok() {
            break;
        }
    }
    let (query, (hits, eps)) = found?;
    if let Some(eps) = eps {
        return Ok(Source {
            query,
            index: 1,
            title: hits[0].clone(),
            episodes: eps,
        });
    }
    let candidates = match saved.and_then(|(_, t)| hits.iter().position(|h| *h == t)) {
        Some(pos) => vec![pos],
        None => best_matches(m, &hits),
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
    Ok(Source {
        query,
        index: i + 1,
        title: hits[i].clone(),
        episodes,
    })
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
    fs::read(audio_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
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
    fs::create_dir_all(config_dir())?;
    fs::write(audio_path(), serde_json::to_vec_pretty(&all)?)
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
    // an mpv flag, other players ani-cli knows (vlc, syncplay) don't start with it
    let mpv = std::env::var("ANI_CLI_PLAYER").map_or(true, |p| p.contains("mpv"));
    if mpv && !subs(audio) {
        flags.push_str(" --sub-visibility=no");
    }
    let child = ani_cli()
        .env("ANI_CLI_MODE", mode(dub(audio)))
        .env("ANI_CLI_PLAYER_FLAGS", flags)
        .args(skip_on().then_some("--skip"))
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
    fs::read(index_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
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
    // written aside and renamed, so a full disk or quitting mid-write can't break it
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(&all)?)?;
    fs::rename(&tmp, &path)
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

/// Downloads one episode, calling `progress` with yt-dlp's status lines.
pub fn download(
    m: &Media,
    src: &Source,
    ep: &str,
    dub: bool,
    mut progress: impl FnMut(&str),
) -> Result<(), String> {
    let dir = folder(m);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    edit_index(|all| all.entry(dir.clone()).or_default().media = m.clone())
        .map_err(|e| e.to_string())?;
    if local_episodes(&dir).iter().any(|(e, _)| e == ep) {
        return Ok(()); // yt-dlp wouldn't replace it, so it stays the audio it was
    }
    let child = {
        let mut dl = DOWNLOAD.lock().unwrap();
        if dl.0 {
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
        dl.1 = Some(child.id());
        child
    };
    let (_, stderr) = follow(child, |l| {
        if let Some(rest) = l.strip_prefix("[download]") {
            progress(rest.trim());
        }
    });
    DOWNLOAD.lock().unwrap().1 = None;
    // ani-cli exits 0 even if yt-dlp failed, so the file is the only reliable signal
    match local_episodes(&dir).into_iter().find(|(e, _)| e == ep) {
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

/// Stops the running download for good; yt-dlp resumes the .part file next time.
/// (Closing the terminal needs nothing: the hangup reaches our whole process group.)
pub fn stop_download() {
    let mut dl = DOWNLOAD.lock().unwrap();
    dl.0 = true;
    if let Some(pid) = dl.1.take().map(|p| p.to_string()) {
        // The ani-cli shell first: it holds the signal until yt-dlp is gone and then exits.
        // Signalled after yt-dlp, it falls back to ffmpeg, which writes a partial .mp4 that
        // looks finished and keeps downloading after we quit.
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let _ = Command::new("pkill").args(["-TERM", "-P", &pid]).status();
    }
}

/// Whether downloads were stopped by quitting, so a failed one was cut short, not broken.
pub fn stopped() -> bool {
    DOWNLOAD.lock().unwrap().0
}

/// Whether `pid` is a running ani-hop (pids get reused, so the name has to match too).
pub fn running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim_end() == "ani-hop")
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
    fn strips_ani_cli_colors() {
        assert_eq!(
            last_error(b"info\n\x1b[2K\r\x1b[1;31mEpisode not released!\x1b[0m\n"),
            "Episode not released!"
        );
    }
}
