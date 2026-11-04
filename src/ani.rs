//! Everything that touches ani-cli, mpv and the download folder.
//!
//! ani-cli has no machine-readable output, so we read its menus instead: with stdin not
//! being a tty it switches to rofi, and we put a stand-in `rofi` first in its PATH that
//! writes the menu to a file. That way the provider scraping stays ani-cli's business and
//! keeps working after `ani-cli -U`.

use crate::anilist::{Entry, Media};
use crate::skip;
use crate::source::{Source, by_hand};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const FAKE_ROFI: &str = r#"#!/bin/sh
# ani-hop: captures ani-cli's menu instead of showing one
while [ $# -gt 0 ]; do [ "$1" = -p ] && p="$2"; shift; done
{ printf '%s\n' "$p"; cat; } > "${ANIHOP_MENU_OUT:-/dev/null}"
exit 1
"#;

const FAKE_FFMPEG: &str = r#"#!/bin/sh
# ani-hop: ani-cli downloads a show whose audio is a separate track with ffmpeg, which writes
# the .mp4 as it goes, so one cut short would look finished. This writes .part, like yt-dlp,
# renames it once ffmpeg got all of it and reports how far it got, and of how long, on stdout.
PATH=${PATH#"${0%/*}:"}
for a; do case $a in *.mp4) out=$a ;; esac; done
[ "$1" = -extension_picky ] && [ -n "$out" ] || exec ffmpeg "$@"
ref= src= prev=
for a; do
	shift
	[ "$a" = "$out" ] && set -- "$@" -y -nostdin -nostats -loglevel level+warning -progress pipe:1 -f mp4 && a=$out.part
	case $prev in -referer) ref=${ref:-$a} ;; -i) src=${src:-$a} ;; esac
	prev=$a
	set -- "$@" "$a"
done
log=$out.part.log
# ani-hop stops a download by signalling ani-cli's children, and this is one. By parent, not
# saved pids: ffprobe's is free again once it's done, maybe someone else's by then.
trap 'pkill -P $$ 2>/dev/null; rm -f "$log"; exit 1' INT HUP TERM
# the episode's length, from its playlist, alongside the download
ffprobe -v error -extension_picky 0 -referer "$ref" -show_entries format=duration -of default=nw=1 "$src" 2>/dev/null &
ffmpeg "$@" 2>"$log" &
wait $!
ok=$?
# still asking after all, it would hold ani-hop's end of stdout open
pkill -P $$ 2>/dev/null
cat "$log" >&2
# ffmpeg exits 0 after skipping a segment it can't get, which only the input side warns about,
# and after errors in what it got. shortcut: a segment that arrives short or empty with no
# error at all still passes; checking each segment means a downloader of our own.
grep -Eq '^\[(in#|hls|https?|tcp|tls|mpegts)|\[(error|fatal|panic)\]' "$log" && ok=1 && echo "parts of it failed to download" >&2
rm -f "$log"
[ $ok = 0 ] && mv -f "$out.part" "$out"
"#;

const FAKE_MPV: &str = r#"#!/bin/sh
# ani-hop: ani-cli's way to mpv. With no subtitles from ani-cli, ani-hop-subs looks for them.
case " $* " in
*" --sub-file="*) ;;
*) [ -n "$ANIHOP_SUBS_PAGE" ] && ani-hop-subs "$ANIHOP_SUBS_PAGE" "$ANIHOP_SUBS_FILE" && set -- --sub-file="$ANIHOP_SUBS_FILE" "$@" ;;
esac
# ani-skip's stand-in named the times, this is what jumps over them. Here as it can't say a path
# with spaces: ani-cli splits what it says on those
case " $* " in
*" --script-opts=skip-"*) set -- --script="${0%/*}/ani-hop-skip.lua" "$@" ;;
esac
# fetched ahead: what mpv would start with goes in a file, for when the episode is wanted
if [ -n "$ANIHOP_HOLD" ]; then
	printf '%s\0' "$@" >"$ANIHOP_HOLD.$$" && mv -f "$ANIHOP_HOLD.$$" "$ANIHOP_HOLD"
	exit
fi
PATH=${PATH#"${0%/*}:"}
echo ani-hop-player-starts
exec mpv "$@"
"#;

const FAKE_SKIP: &str = r#"#!/bin/sh
# ani-hop: ani-cli runs this as ani-skip, with -i <MyAnimeList id> -e <episode>, and hands what it
# prints to mpv. ani-hop answers it: no answer just means no skipping.
while [ $# -gt 1 ]; do
	case $1 in -i) id=$2 ;; -e) ep=$2 ;; esac
	shift
done
# an update can take away the ani-hop that started this, and the one on PATH is the new one
[ -x "$ANIHOP_EXE" ] || ANIHOP_EXE=ani-hop
exec "$ANIHOP_EXE" --skip-flags "$id" "$ep"
"#;

/// The mpv script that jumps over what skip::flags names, once each, so seeking back shows it.
/// It reads the options ani-skip's own script reads: one a user keeps loaded does the same.
const SKIP_SCRIPT: &str = r#"-- ani-hop: jumps over an episode's opening and ending
local times = { op_start = 0, op_end = 0, ed_start = 0, ed_end = 0 }
require("mp.options").read_options(times, "skip")

local done = {}
local function jump(part, now, from, to)
    if not done[part] and now >= from and now < to then
        done[part] = true
        mp.set_property_number("time-pos", to)
    end
end

mp.observe_property("time-pos", "number", function(_, now)
    if now then
        jump("op", now, times.op_start, times.op_end)
        jump("ed", now, times.ed_start, times.ed_end)
    end
end)
"#;

const FAKE_SUBS: &str = r#"#!/bin/sh
# ani-hop: the emergency fallback for an episode ani-cli finds no subtitles for. ani-cli reads
# one of the provider's servers, whose copy can lack them (The Apothecary Diaries 1 has no
# English track); the other server has the same release with them. Writes its English ones from
# page $1 to file $2, or fails and leaves no file: the episode then plays as ani-cli found it.
# shortcut: knows that server's page and API, so a change there ends the fallback; drop this
# once ani-cli reads a second server (its issue 1932).
host=https://megaplay.buzz/
id=$(curl -sfL --max-time 5 -e "$host" "$1" | sed -nE 's/.*data-id="([0-9]+)".*/\1/p' | head -n 1)
[ -n "$id" ] || exit 1
url=$(curl -sfL --max-time 5 -e "$host" "${host}stream/getSources?id=$id" | tr '{' '\n' |
	grep -m 1 '"label":"English' | sed -nE 's|.*"file":"([^"]*)".*|\1|p' | sed 's|\\/|/|g')
# from the network, and curl would take one starting with - for an option
case $url in https://*) ;; *) exit 1 ;; esac
tmp=$2.$$
trap 'rm -f "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
curl -sfL --proto =https --max-time 5 -e "$host" -o "$tmp" "$url" &&
	head -n 1 "$tmp" | grep -q WEBVTT && mv -f "$tmp" "$2"
"#;

/// A test outside `in_child`: it gets a home of its own, so no test run touches the user's
/// config, cache or downloads.
fn testing() -> bool {
    cfg!(test) && std::env::var_os("ANIHOP_CHILD").is_none()
}

pub fn home() -> PathBuf {
    if testing() {
        return std::env::temp_dir().join(format!("ani-hop-test-{}", std::process::id()));
    }
    std::env::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// `$var` if it's an absolute path, else `~/fallback` (the XDG base-dir rule).
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && !testing())
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

pub fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}

/// Written aside and renamed, so a full disk or quitting mid-write can't leave half a file.
/// The file aside is this writer's own: another ani-hop, or another thread, may be writing
/// the same file right then.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(path.parent().unwrap())?;
    // through a symlink (dotfiles), as a plain write would
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
    let res = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, &path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
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
const DEPS: [(&str, &str, &str); 6] = [
    ("ani-cli", "ani-cli", "streams and downloads"),
    ("mpv", "mpv", "plays episodes"),
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

/// install.sh for what's missing, on Linux unless Homebrew put ani-hop here. It leaves a mise
/// ani-hop alone: a second copy in ~/.local/bin could come first on PATH, and mise would
/// update one that doesn't run.
fn install_sh(exe: &Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        None
    } else if from_mise(exe) {
        Some(INSTALL_SH.replace("| sh", "| ANI_HOP_KEEP=1 sh"))
    } else {
        (!from_brew(exe)).then(|| INSTALL_SH.into())
    }
}

fn aur_helper() -> Option<&'static str> {
    ["yay", "paru"].into_iter().find(|h| have(h))
}

/// One command for everything missing (Arch package names, as in DEPS). ani-cli lives in the
/// AUR, so plain pacman only qualifies when it isn't needed; other distros get install.sh, which
/// fetches it from upstream. macOS, and Linux when ani-hop came from Homebrew, get brew, which
/// has no xdg-utils.
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
    if let Some(sh) = install_sh(exe()) {
        return Some(sh);
    }
    let brew: Vec<&str> = pkgs
        .iter()
        .filter_map(|p| match *p {
            "ani-cli" => Some("pystardust/ani-cli/ani-cli"),
            "xdg-utils" => None,
            p => Some(p),
        })
        .collect();
    let tap = match pkgs.contains(&"ani-cli") {
        true => "brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git && ",
        false => "",
    };
    (have("brew") && !brew.is_empty()).then(|| format!("{tap}brew install {}", brew.join(" ")))
}

/// The installed ani-cli's version.
pub fn ani_cli_version() -> Option<String> {
    let out = Command::new("ani-cli")
        .arg("-V")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?; // it exits early when curl or a player is missing
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
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

/// Installed by `mise use github:ReynkeDeVos/ani-hop`, wherever MISE_DATA_DIR puts it.
fn from_mise(p: &Path) -> bool {
    p.components()
        .any(|c| c.as_os_str() == "github-reynke-de-vos-ani-hop")
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
/// Where ani-hop is, as of when it started: an update replaces the file, and asked after that,
/// Linux says "(deleted)" while the same path holds the new one.
fn exe() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        std::env::current_exe()
            .and_then(fs::canonicalize)
            .unwrap_or_default()
    })
}

/// How to update this ani-hop, judged by where it lives.
pub fn update_command() -> String {
    update_command_for(exe())
}

fn update_command_for(exe: &Path) -> String {
    // first: MISE_DATA_DIR can put it anywhere, even under the paths below
    if from_mise(exe) {
        // the full name: `mise up ani-hop` matches nothing and says all is up to date
        "mise up github:ReynkeDeVos/ani-hop".into()
    } else if exe.starts_with(home().join(".local/bin")) {
        INSTALL_SH.into()
    } else if from_brew(exe) {
        "brew upgrade ani-hop".into()
    } else if exe.starts_with("/nix/store") {
        nix_update(exe)
    } else if exe.starts_with("/usr") {
        system_update()
    } else {
        "cargo install --locked --force --git https://github.com/ReynkeDeVos/ani-hop".into()
    }
}

/// From the flake: `nix profile upgrade` when it was added to a profile, the flake input the
/// README's NixOS lines name when the system or Home Manager has it, else `nix run`.
fn nix_update(exe: &Path) -> String {
    // /nix/store/<hash>-ani-hop-<version>, as the profile's manifest lists it
    let pkg: PathBuf = exe.components().take(4).collect();
    // nix run starts it without putting it on PATH
    if !located("ani-hop").is_some_and(|p| p.starts_with(&pkg)) {
        return "nix run --refresh github:ReynkeDeVos/ani-hop".into();
    }
    let profiles = [
        home().join(".nix-profile"),
        xdg("XDG_STATE_HOME", ".local/state").join("nix/profile"),
    ];
    profiles
        .iter()
        .find_map(|p| profile_element(&fs::read_to_string(p.join("manifest.json")).ok()?, &pkg))
        .map_or_else(
            || "nix flake update ani-hop, then rebuild".into(),
            |name| format!("nix profile upgrade {name}"),
        )
}

/// The name of the element that holds `pkg` in a `nix profile` manifest.
fn profile_element(manifest: &str, pkg: &Path) -> Option<String> {
    let m: serde_json::Value = serde_json::from_str(manifest).ok()?;
    m["elements"].as_object()?.iter().find_map(|(name, e)| {
        let paths = e["storePaths"].as_array()?;
        paths
            .iter()
            .any(|p| p.as_str() == pkg.to_str())
            .then(|| name.clone())
    })
}

/// Our lock in run/, held until we exit (or die): other ani-hops see we're alive by failing
/// to take it. Works where there's no /proc (macOS) and isn't fooled by reused pids.
static ALIVE: OnceLock<fs::File> = OnceLock::new();

fn run_dir() -> PathBuf {
    cache_dir().join("run")
}

pub fn setup() -> std::io::Result<()> {
    exe();
    let bin = cache_dir().join("bin");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(cache_dir().join("probe"))?;
    for (name, script) in [
        ("rofi", FAKE_ROFI),
        ("ffmpeg", FAKE_FFMPEG),
        ("mpv", FAKE_MPV),
        ("ani-skip", FAKE_SKIP),
        ("ani-hop-skip.lua", SKIP_SCRIPT),
        ("ani-hop-subs", FAKE_SUBS),
    ] {
        write_helper(&bin.join(name), script)?;
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
    clear_leftovers(&cache_dir().join("probe"));
    for p in fs::read_dir(&run)?.flatten().map(|e| e.path()) {
        let pid = p.file_stem().unwrap_or_default().to_string_lossy();
        if p.extension().is_some_and(|x| x == "lock") && !running(&pid) {
            let _ = fs::remove_file(&p);
        }
    }
    Ok(())
}

/// Removes what ani-hops that are gone left in the folder ani-cli works in: only the files
/// with their pid in the name, as ani-cli keeps its history there.
fn clear_leftovers(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for p in rd.flatten().map(|e| e.path()) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        let pid = (["menu-", "held-", "subs-"].iter())
            .find_map(|kind| name.strip_prefix(kind))
            .and_then(|rest| rest.split('-').next());
        if pid.is_some_and(|pid| !running(pid)) {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Puts a helper script in place, unless it's there already: another ani-hop may be running it
/// right now, and one rewritten in place would be busy or empty for that moment. An empty
/// helper exits as if all went well.
fn write_helper(path: &Path, script: &str) -> std::io::Result<()> {
    let runs = |p: &Path| fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 == 0o111);
    if runs(path) && fs::read(path).is_ok_and(|now| now == script.as_bytes()) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let res = fs::write(&tmp, script)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)))
        .and_then(|()| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// The players running, ani-cli under them too: quitting closes them.
static PLAYERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// ani-cli fetching a stream ahead and mpv asked about its config, which quitting ends too.
static AHEAD: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn pids(list: &'static Mutex<Vec<u32>>) -> std::sync::MutexGuard<'static, Vec<u32>> {
    list.lock().unwrap_or_else(|e| e.into_inner())
}

/// A process on one of those lists until this is dropped.
struct Listed(&'static Mutex<Vec<u32>>, u32);

impl Listed {
    fn on(list: &'static Mutex<Vec<u32>>, child: &Child) -> Listed {
        pids(list).push(child.id());
        Listed(list, child.id())
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        pids(self.0).retain(|p| *p != self.1);
    }
}

/// Whether a player is open: an earlier one can be, after a later one closed.
pub fn player_open() -> bool {
    !pids(&PLAYERS).is_empty()
}

/// Closes the players, as ani-hop quits: nothing would be left to ask about saving the episode.
pub fn close_players() {
    let all: Vec<u32> = [&PLAYERS, &AHEAD]
        .into_iter()
        .flat_map(|list| pids(list).drain(..).collect::<Vec<_>>())
        .collect();
    all.into_iter().for_each(end);
}

/// Ends a process and everything under it, as it ends a stalled ani-cli or a download. The
/// process first: ani-cli holds the signal until yt-dlp is gone and then exits, while signalled
/// after yt-dlp, it falls back to ffmpeg and keeps downloading.
pub fn end(pid: u32) {
    // collected first: once their parent is gone they can't be found through it
    fn under(pid: &str, all: &mut Vec<String>) {
        let found = Command::new("pgrep")
            .args(["-P", pid])
            .stderr(Stdio::null())
            .output();
        let found = found.map_or(String::new(), |o| {
            String::from_utf8_lossy(&o.stdout).into_owned()
        });
        for child in found.split_whitespace() {
            all.push(child.to_string());
            under(child, all);
        }
    }
    let pid = pid.to_string();
    let mut all = Vec::new();
    under(&pid, &mut all);
    // without stderr: one that just ended would print "No such process" over the screen
    let term = |pid: &str| {
        let _ = (Command::new("kill").args(["-TERM", pid]))
            .stderr(Stdio::null())
            .status();
    };
    term(&pid);
    all.iter().for_each(|p| term(p));
}

/// What `cmd` wrote and how it ended, within `limit`: a helper that stalls is ended, and mustn't
/// hold up what waits for it. None past that, or when it can't start.
pub fn output_within(cmd: &mut Command, limit: Duration) -> Option<Output> {
    let child = (cmd.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .spawn()
        .ok()?;
    wait_within(child, limit)
}

fn wait_within(child: Child, limit: Duration) -> Option<Output> {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || _ = tx.send(child.wait_with_output()));
    let out = rx.recv_timeout(limit);
    if out.is_err() {
        end(pid);
    }
    out.ok()?.ok()
}

/// How long ani-cli gets to reach the player or a menu: every one of its requests has a limit,
/// but something stalled would leave the pop-up up for good.
const ANI_CLI_LIMIT: Duration = Duration::from_secs(60);

pub fn ani_cli() -> Command {
    let path = format!(
        "{}:{}",
        cache_dir().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new("ani-cli");
    // ANI_CLI_EXACT compares titles with the query as `plain` encodes it; ani-hop matches its own
    c.env("PATH", path)
        .env_remove("ANI_CLI_EXACT")
        .stdin(Stdio::null());
    c
}

/// A query as ani-cli takes it: one starting with "-" would be one of its options (-D clears
/// its history, -U updates it), and ani-cli puts the rest in the provider's URL unencoded,
/// where "×", "é", "’" or Japanese get an HTTP 400, "&" cuts it short and "[" trips curl.
/// Spaces stay, ani-cli makes them "+".
/// shortcut: assumes ani-cli 5.1 doesn't encode; drop the encoding if it starts to.
pub fn plain(query: &str) -> String {
    let mut out = String::new();
    for b in query.trim_start_matches('-').bytes() {
        match b {
            b' ' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ if b.is_ascii_alphanumeric() => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What the system says, through ffmpeg or yt-dlp, when there's no connection: ani-cli got the
/// episode's links, then the network went.
const NO_NETWORK: [&str; 4] = [
    "Network is unreachable",
    "No route to host",
    "Temporary failure in name resolution",
    "nodename nor servname provided", // macOS
];

/// ani-cli's error, without the colors and line clears it prints it with. A connection error
/// wins over what failed after it: ani-cli goes on past a server page it couldn't fetch and
/// ends with "No sources found for dub!", and ani-hop's ffmpeg with "parts of it failed".
pub fn last_error(stderr: &[u8]) -> String {
    let plain = |line: &str| {
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
    };
    let s = String::from_utf8_lossy(stderr);
    let lines: Vec<String> = s.lines().map(plain).filter(|l| !l.is_empty()).collect();
    // "Connection error: could not fetch https://hianime.at/search?keyword=… (no HTTP res…",
    // "[tcp @ 0x55d0] Connection to tcp://vault-12.owocdn.top:443 failed: Network is unreac…"
    let offline =
        |l: &&String| l.contains("could not fetch ") || NO_NETWORK.iter().any(|n| l.contains(n));
    match lines.iter().find(offline) {
        Some(l) => {
            let host = l
                .split_once("://")
                .and_then(|(_, u)| u.split(['/', ':', ' ']).next());
            format!("can't reach {}", host.unwrap_or("its site"))
        }
        None => {
            let last = lines.last().map_or("ani-cli failed", String::as_str);
            // ani-cli says so for a server it can't read too: it reads just one kind
            let mode =
                (last.strip_prefix("No sources found for ")).and_then(|m| m.strip_suffix('!'));
            match mode {
                // what's after "Couldn't play episode 2 · " fits 80 columns
                Some(mode) => format!("no {mode} stream ani-cli can play"),
                None => last.into(),
            }
        }
    }
}

/// Whether `last_error` says ani-cli's site couldn't be reached: offline, worth another try.
pub fn unreachable(error: &str) -> bool {
    error.starts_with("can't reach ")
}

/// Feeds the child's output lines to `line` (also the ones redrawn with \r) until it
/// exits; returns whether it succeeded and what it wrote to stderr.
pub fn follow(mut child: Child, mut line: impl FnMut(&str)) -> (bool, Vec<u8>) {
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

/// What FAKE_MPV says right before it starts mpv.
const PLAYER_STARTS: &str = "ani-hop-player-starts";

/// mpv prints "VO: [gpu-next] 1920x1080 ..." once its window opens.
fn player_up(line: &str, up: &mut impl FnMut()) {
    if line.starts_with("VO:") {
        up();
    }
}

/// What a stream waits on, from ani-cli's output: FAKE_MPV says when it starts mpv, which takes
/// a moment longer than everything before it to show the episode.
fn stream_step(line: &str, step: &dyn Fn(Option<&'static str>)) {
    match line {
        PLAYER_STARTS => step(Some(crate::playback::OPENING)),
        line => player_up(line, &mut || step(None)),
    }
}

/// Whether mpv jumps over openings and endings (switched in the main menu).
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

/// Whether the next episode's stream is fetched ahead (Settings): off saves the requests.
pub fn preload_on() -> bool {
    config_dir().join("preload").exists()
}

pub fn set_preload(on: bool) -> std::io::Result<()> {
    let flag = config_dir().join("preload");
    match on {
        true => fs::create_dir_all(config_dir()).and_then(|()| fs::write(flag, "")),
        false => fs::remove_file(flag),
    }
}

fn audio_path() -> PathBuf {
    config_dir().join("audio.json")
}

/// What a show plays with, as (key, label, the label for a narrow row). The provider has
/// Japanese and English audio, both with English subtitles, which mpv shows or keeps hidden
/// (v switches).
pub const AUDIO: [(&str, &str, &str); 4] = [
    ("raw", "Japanese · no subtitles", "no subs"),
    ("sub", "Japanese · English subtitles", "subtitled"),
    ("dub", "English dub", "dubbed"),
    ("dub+sub", "English dub · English subtitles", "dub + subs"),
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

/// The audio's label for a row too narrow for `audio_label`, which says the same.
pub fn audio_short(audio: &str) -> &'static str {
    AUDIO.iter().find(|a| a.0 == audio).map_or("", |a| a.2)
}

pub fn dub(audio: &str) -> bool {
    audio.starts_with("dub")
}

fn subs(audio: &str) -> bool {
    audio.ends_with("sub")
}

pub fn set_audio(id: Option<u64>, audio: &str) -> std::io::Result<()> {
    edit_json(&audio_path(), |all: &mut HashMap<String, String>| {
        choose(all, id, audio)
    })
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

/// Episode `ep`'s page on the provider's other server, for FAKE_SUBS. That server goes by the
/// show's MyAnimeList id, so there's none unless `src` numbers episodes as the show does: a
/// source chosen by hand can be another part, one of another length than a finished show (or
/// longer than one still airing) holds several parts or is another. Those would get another
/// episode's subtitles.
/// shortcut: a wrong source of the same length still passes, it plays the wrong show anyway;
/// telling them apart needs the id ani-cli only gives its ani-skip.
fn subs_page(m: &Media, src: &Source, ep: &str, dub: bool) -> Option<String> {
    let mal = m.id_mal?;
    let eps = src.episodes.len();
    let fits = match m.episodes {
        Some(n) if m.status.as_deref() == Some("FINISHED") => eps == n as usize,
        Some(n) => eps <= n as usize,
        None => true,
    };
    (fits && !by_hand(m.id)).then(|| {
        format!(
            "https://megaplay.buzz/stream/mal/{mal}/{}/{}",
            plain(ep),
            mode(dub)
        )
    })
}

/// The player the user picked, if not plain mpv: ani-cli would take IINA on macOS and Windows'
/// mpv.exe under WSL, so it's asked for by name.
fn own_player() -> Option<String> {
    let player = std::env::var("ANI_CLI_PLAYER").ok();
    player.filter(|p| !p.is_empty() && p != "mpv")
}

/// What mpv gets when its config files don't say: by itself it decodes on the CPU, and stutters.
const HWDEC: &str = "--hwdec=auto-safe";

/// Whether mpv's config files leave hwdec out; `hwdec=no` is the user's choice. Asked once a run,
/// in the background at the start or else when the first episode starts. No when mpv can't say.
pub fn hwdec_unset() -> bool {
    static UNSET: OnceLock<bool> = OnceLock::new();
    *UNSET.get_or_init(|| config_leaves_hwdec(&|| Command::new("mpv")))
}

/// Asks with options mpv answers right after reading its config files, and then quits. So nothing
/// in them starts: another mpv's control socket, a log or a stats file stay as they are, and the
/// user's scripts don't run. Each profile is asked after, as -v only says what one sets once it
/// applies, and the file played decides that. Only answers that show they're what was asked for
/// count, so a changed mpv costs the default, never the user's choice. A built-in profile the user
/// added to only counts once it's applied: by the config, which -v shows, or by a flag, which
/// comes after ours.
fn config_leaves_hwdec(mpv: &dyn Fn() -> Command) -> bool {
    let ask = |args: &[&str]| {
        let child = (mpv().args(args).stdin(Stdio::null()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        // one stuck on the config files mustn't outlive ani-hop
        let _listed = Listed::on(&AHEAD, &child);
        let out = wait_within(child, Duration::from_secs(5))?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(said) = ask(&["-v", "--profile=help"]) else {
        return false;
    };
    if !said.contains("Setting option 'profile' = 'help'") || config_may_set_hwdec(&said) {
        return false;
    }
    let Some(builtin) = ask(&["--no-config", "--profile=help"]) else {
        return false;
    };
    let builtin = profiles(&builtin);
    if builtin.is_empty() {
        return false;
    }
    let mut own = profiles(&said).into_iter().filter(|p| !builtin.contains(p));
    own.all(|p| {
        let shown = ask(&[&format!("--show-profile={p}")]);
        shown.is_some_and(|shown| shown.starts_with("Profile ") && !profile_may_set_hwdec(&shown))
    })
}

/// From `mpv -v`: hwdec itself, or a config next to each file, which only that file would show.
fn config_may_set_hwdec(said: &str) -> bool {
    (said.contains("Setting option 'hwdec' = "))
        || said.contains("Setting option 'use-filedir-conf' = ")
}

/// The names in `mpv --profile=help`: "\tname\tdescription", after -v's prefix if it's there.
fn profiles(list: &str) -> Vec<&str> {
    list.lines()
        .filter_map(|l| l.trim_start_matches("[cplayer] ").strip_prefix('\t'))
        .filter_map(|l| l.split('\t').next())
        .collect()
}

/// From `mpv --show-profile`, where `hwdec-codecs` is another option, and neither a file a profile
/// includes nor the one next to each file played is shown.
fn profile_may_set_hwdec(shown: &str) -> bool {
    shown.lines().any(|l| {
        let option = l.trim_start().split('=').next();
        matches!(
            option,
            Some("hwdec" | "no-hwdec" | "include" | "use-filedir-conf")
        )
    })
}

/// What jumps over openings and endings: setup writes it next to the helpers.
pub fn skip_script() -> PathBuf {
    cache_dir().join("bin").join("ani-hop-skip.lua")
}

/// ani-cli set up to take episode `ep` of `src` to the player, and the file
/// the player will read subtitles from, if ani-cli finds none.
fn stream_command(m: &Media, src: &Source, ep: &str, audio: &str) -> (Command, Option<PathBuf>) {
    let mut flags = std::env::var("ANI_CLI_PLAYER_FLAGS").unwrap_or_default();
    let player = own_player().unwrap_or_else(|| "mpv".into());
    // an mpv flag, other players ani-cli knows (vlc, syncplay) don't start with it
    if player.contains("mpv") && !subs(audio) {
        flags.push_str(" --sub-visibility=no");
    }
    // first, as the last flag wins and the user's own come after it. Another player has another
    // config
    if player == "mpv" && hwdec_unset() {
        flags = format!("{HWDEC} {flags}");
    }
    let mut cli = ani_cli();
    cli.env("ANIHOP_EXE", exe());
    if skip::last_episode(m, &src.episodes, ep) {
        cli.env(skip::LAST_EPISODE, "1");
    }
    let subs = subs_page(m, src, ep, dub(audio)).map(|page| {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = format!("subs-{}-{n}.vtt", std::process::id());
        let file = cache_dir().join("probe").join(name);
        cli.env("ANIHOP_SUBS_FILE", &file)
            .env("ANIHOP_SUBS_PAGE", page);
        file
    });
    cli.env("ANI_CLI_PLAYER", player)
        .env("ANI_CLI_MODE", mode(dub(audio)))
        .env("ANI_CLI_PLAYER_FLAGS", flags)
        .args(skip_on().then_some("--skip"))
        .args([
            "--exit-after-play",
            "-S",
            &src.index.to_string(),
            "-e",
            ep,
            &plain(&src.query),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    (cli, subs)
}

/// Streams through ani-cli; `step` hears when mpv starts, and None once it
/// shows the episode.
pub fn stream(
    m: &Media,
    src: &Source,
    ep: &str,
    audio: &str,
    step: &dyn Fn(Option<&'static str>),
) -> Result<(), String> {
    let (mut cli, subs) = stream_command(m, src, ep, audio);
    let child = cli.spawn().map_err(|e| format!("ani-cli: {e}"))?;
    let _player = Listed::on(&PLAYERS, &child);
    // up to mpv only: another player may never say it's there, and one playing mustn't be ended
    let started = Arc::new(AtomicBool::new(own_player().is_some()));
    let stalled = Arc::new(AtomicBool::new(false));
    let watch = (started.clone(), stalled.clone(), child.id());
    std::thread::spawn(move || {
        let (started, stalled, pid) = watch;
        let since = std::time::Instant::now();
        while !started.load(Ordering::Relaxed) {
            if since.elapsed() > ANI_CLI_LIMIT {
                stalled.store(true, Ordering::Relaxed);
                return end(pid);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let played = follow(child, |l| {
        if l == PLAYER_STARTS || l.starts_with("VO:") {
            started.store(true, Ordering::Relaxed);
        }
        stream_step(l, step)
    });
    started.store(true, Ordering::Relaxed);
    // gone once the player is, which read it at the start
    if let Some(file) = subs {
        let _ = fs::remove_file(file);
    }
    match played {
        (true, _) => Ok(()),
        (false, _) if stalled.load(Ordering::Relaxed) => {
            Err("ani-cli didn't answer for a minute".into())
        }
        (false, stderr) => Err(last_error(&stderr)),
    }
}

/// A stream's way into mpv, fetched ahead: what ani-cli would start it with, the links and the
/// skip times among them, as separate arguments, never through a shell.
pub struct Held {
    pub(crate) args: Vec<String>,
    /// the subtitles those name, if ani-hop-subs had to fetch them: gone with this
    pub(crate) subs: Option<PathBuf>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(file) = &self.subs {
            let _ = fs::remove_file(file);
        }
    }
}

/// How a stream fetched ahead went.
pub enum Replayed {
    Ended(Result<(), String>),
    /// mpv never showed it and failed: its links are too old, nothing was watched
    TooOld,
}

/// Whether a stream can be fetched ahead: another player may take other arguments, or none of
/// these.
pub fn can_hold() -> bool {
    own_player().is_none()
}

/// Runs ani-cli up to where it would start mpv, and keeps what it would start it with.
pub fn hold_stream(m: &Media, src: &Source, ep: &str, audio: &str) -> Result<Held, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let (mut cli, subs) = stream_command(m, src, ep, audio);
    let name = format!(
        "held-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let file = cache_dir().join("probe").join(name);
    cli.env("ANIHOP_HOLD", &file)
        // nothing's watched yet
        .env("ANI_CLI_HIST_DIR", cache_dir().join("probe"));
    // from here on the subtitles go with it, also when it fails
    let mut held = Held {
        args: Vec::new(),
        subs,
    };
    let child = cli.spawn().map_err(|e| format!("ani-cli: {e}"))?;
    let _listed = Listed::on(&AHEAD, &child);
    let out = wait_within(child, ANI_CLI_LIMIT).ok_or("ani-cli didn't answer for a minute")?;
    let args = fs::read(&file);
    let _ = fs::remove_file(&file);
    match args {
        Ok(args) if out.status.success() => {
            let args = args.split(|b| *b == 0).filter(|a| !a.is_empty());
            held.args = args
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            Ok(held)
        }
        _ => Err(last_error(&out.stderr)),
    }
}

/// Starts mpv with what `hold_stream` kept.
pub fn play_held(held: &Held, mut up: impl FnMut()) -> Replayed {
    let child = (Command::new("mpv").args(&held.args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => return Replayed::Ended(Err(format!("mpv: {e}"))),
    };
    let _player = Listed::on(&PLAYERS, &child);
    let mut shown = false;
    let played = follow(child, |l| {
        player_up(l, &mut || {
            shown = true;
            up()
        })
    });
    match played {
        (true, _) => Replayed::Ended(Ok(())),
        (false, _) if !shown => Replayed::TooOld,
        (false, stderr) => Replayed::Ended(Err(last_error(&stderr))),
    }
}

pub fn play_file(m: &Media, ep: &str, file: &Path, mut up: impl FnMut()) -> Result<(), String> {
    let mut mpv = Command::new("mpv");
    mpv.arg(format!("--force-media-title={} Episode {ep}", m.name()))
        .args(hwdec_unset().then_some(HWDEC));
    let vtt = file.with_extension("vtt");
    if vtt.exists() {
        mpv.arg(format!("--sub-file={}", vtt.display()));
        if hide_subs(audio(Some(m.id)), is_dub(file)) {
            mpv.arg("--sub-visibility=no");
        }
    }
    if let Some(mal) = m.id_mal.filter(|_| skip_on()) {
        let last = skip::last_episode(m, &[], ep);
        mpv.arg(format!("--script={}", skip_script().display()))
            .args(skip::flags(&skip::times(mal, ep, is_dub(file), last)));
    }
    let child = mpv
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("mpv: {e}"))?;
    let _player = Listed::on(&PLAYERS, &child);
    match follow(child, |l| player_up(l, &mut up)) {
        (true, _) => Ok(()),
        (false, stderr) => Err(last_error(&stderr)),
    }
}

/// What ani-hop knows about a show folder it downloaded into. It's kept in the config
/// folder, so the download folder holds only episodes and subtitles.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Downloaded {
    media: Media,
    /// episode files that are the English dub, ani-cli names both the same
    #[serde(default)]
    dub: Vec<String>,
}

/// By show folder.
type Index = std::collections::BTreeMap<PathBuf, Downloaded>;

fn index_path() -> PathBuf {
    config_dir().join("downloads.json")
}

/// One lock file per download folder ever downloaded into; they stay, as one deleted
/// could let a download and a removal both think they hold it.
fn holds_dir() -> PathBuf {
    cache_dir().join("folders")
}

/// A download folder held by a download into it, from making the folder until the download
/// is done: it isn't dropped meanwhile, also while still empty. Several may hold one.
pub struct Hold(fs::File);

impl Drop for Hold {
    /// Closing alone leaves the lock with any copy of the file: a process another thread is
    /// starting has one until it runs, which made a delete right after this say it's busy.
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// FNV-1a, for ids and lock names that are saved or shared with other ani-hops: std's hasher
/// may change between Rust releases.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    (bytes.iter()).fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn hold_file(dir: &Path) -> std::io::Result<fs::File> {
    let hash = fnv1a(dir.as_os_str().as_encoded_bytes());
    fs::create_dir_all(holds_dir())?;
    fs::File::create(holds_dir().join(format!("{hash:016x}.lock")))
}

/// Whether nothing holds `dir`, while the result is kept: removing it is safe then.
fn unheld(dir: &Path) -> Option<Hold> {
    let file = hold_file(dir).ok()?;
    file.try_lock().ok().map(|()| Hold(file))
}

fn index() -> Index {
    read_json(&index_path())
}

/// Changes a file of settings under a lock, as another ani-hop may be changing it too.
pub fn edit_json<T: Default + Serialize + DeserializeOwned>(
    path: &Path,
    f: impl FnOnce(&mut T),
) -> std::io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    // its own file, as the rename below swaps the file for a new one
    let lock = fs::File::create(path.with_extension("lock"))?;
    lock.lock()?;
    // a broken file stays as it is, rather than be overwritten with just this change
    let mut all = match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        r => serde_json::from_slice(&r?)?,
    };
    f(&mut all);
    write_json(path, &all)
}

fn edit_index(f: impl FnOnce(&mut Index)) -> std::io::Result<()> {
    edit_json(&index_path(), f)
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

/// Download folder of a show: the one it was downloaded into before, else a new one in the
/// download folder named after it.
pub fn folder(m: &Media) -> PathBuf {
    folder_in(&download_root(), index(), m)
}

/// Two AniList shows with one name (a remake) don't share a folder: the second one's name
/// gets its AniList id. One ani-cli found shares it with the AniList show of its name.
fn folder_in(root: &Path, all: Index, m: &Media) -> PathBuf {
    // only this show's folders are looked at: another's on a disk asleep mustn't hold it up
    let mine = (all.iter()).filter(|(_, d)| d.media.id == m.id);
    let mine: Index = mine.map(|(p, d)| (p.clone(), d.clone())).collect();
    if let Some((_, p)) = shows_in(root, mine).into_iter().next() {
        return p;
    }
    let name = m.name().replace('/', "∕");
    // the file system's own names would be the download folder, or the one above it
    let name = match name.trim() {
        "" | "." | ".." => format!("Show {}", m.id),
        _ => name,
    };
    let taken = |dir: &PathBuf| {
        let other = all.get(dir).map(|d| &d.media);
        other.is_some_and(|o| o.id != m.id && o.id != 0 && o.on_anilist() && m.on_anilist())
            && dir.is_dir()
    };
    let dir = root.join(&name);
    if !taken(&dir) {
        return dir;
    }
    // the one with its id can be another's too, whose name ends that way
    let with_id = format!("{name} ({})", m.id);
    let more = (2u32..).map(|n| root.join(format!("{with_id} {n}")));
    std::iter::once(root.join(&with_id))
        .chain(more)
        .find(|dir| !taken(dir))
        .unwrap_or(dir)
}

/// Puts the details AniList has now (progress, airing) on the downloaded shows it knows.
pub fn refresh_shows(fresh: &[Media]) {
    let _ = edit_index(|all| {
        for d in all.values_mut() {
            if let Some(m) = fresh.iter().find(|m| m.id == d.media.id) {
                d.media = m.clone();
            }
        }
    });
}

/// The user's AniList Entries as last read or saved, by show, each with when (`tick`): what
/// the Downloads list shows, also offline in a later session.
pub type Learnt = HashMap<u64, (Option<Entry>, u64)>;

fn learnt_path() -> PathBuf {
    cache_dir().join("entries.json")
}

/// The Entries kept (`learn`), by any ani-hop.
pub fn learnt() -> Learnt {
    read_json(&learnt_path())
}

/// Keeps `entries` as AniList had them at `at` (`tick`), for the shows downloaded, unless
/// newer ones are kept: a save, of this ani-hop or another, a list read before or after it,
/// in any order. Other shows aren't kept, so an old save of one can't outlast a fresher list
/// once it's downloaded. `queued`: a download of them was just queued, which keeps them too,
/// with what this session knew of them, so only over older ones.
pub fn learn(entries: impl IntoIterator<Item = (u64, Option<Entry>)>, at: u64, queued: bool) {
    let path = learnt_path();
    let edit = || -> std::io::Result<()> {
        fs::create_dir_all(cache_dir())?;
        let lock = fs::File::create(path.with_extension("lock"))?;
        lock.lock()?;
        let mut all: Learnt = read_json(&path);
        seen(&all);
        let downloaded: std::collections::HashSet<u64> =
            index().values().map(|d| d.media.id).collect();
        let mut changed = false;
        for (id, entry) in entries {
            let kept = all.get(&id).map(|k| k.1);
            let ours = queued || kept.is_some() || downloaded.contains(&id);
            let newer = kept.is_none_or(|t| t < at || t == at && !queued);
            if ours && newer {
                all.insert(id, (entry, at));
                changed = true;
            }
        }
        if changed {
            write_json(&path, &all)?;
        }
        Ok(())
    };
    let _ = edit();
}

/// The last `tick`.
static CLOCK: AtomicU64 = AtomicU64::new(0);

/// Now, as one number that orders AniList's Entries (`learn`): ms since 1970, but always past
/// every tick before it in this ani-hop and every one kept, by any ani-hop, also before this
/// one started, so a clock set back or two reads in one ms still come in order. A save's,
/// taken in its turn (`entries::turn`), comes after every save before it.
/// shortcut: reads another ani-hop still has out aren't seen, so with the clock set back while
/// two run, a read the other asked for before a save here can still be kept over the save;
/// a counter file every tick takes in turn if that ever shows up.
pub fn tick() -> u64 {
    seen(&learnt());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut last = CLOCK.load(Ordering::SeqCst);
    loop {
        let next = now.max(last.saturating_add(1));
        match CLOCK.compare_exchange_weak(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(now_last) => last = now_last,
        }
    }
}

/// Ticks from here on come after everything kept.
fn seen(all: &Learnt) {
    CLOCK.fetch_max(
        all.values().map(|k| k.1).max().unwrap_or(0),
        Ordering::SeqCst,
    );
}

/// Downloaded shows and their folders, by name: also those downloaded before the download
/// folder changed, which stay where they are. Each with its Entry as last learnt.
pub fn local_shows() -> Vec<(Media, PathBuf)> {
    let learnt = learnt();
    let mut shows = shows_in(&download_root(), index());
    for (m, _) in &mut shows {
        if let Some((entry, _)) = learnt.get(&m.id) {
            m.media_list_entry = entry.clone();
        }
    }
    shows
}

/// A show in two folders (downloaded again after the download folder changed, before 0.7)
/// once, from the one in `root`.
fn shows_in(root: &Path, all: Index) -> Vec<(Media, PathBuf)> {
    let mut shows: Vec<(Media, PathBuf)> = (all.into_iter())
        .filter(|(p, _)| p.is_dir())
        .map(|(p, d)| (d.media, p))
        .collect();
    shows.sort_by_key(|(_, p)| p.parent() != Some(root));
    let mut seen = std::collections::HashSet::new();
    shows.retain(|(m, _)| seen.insert(m.id));
    shows.sort_by(|a, b| a.0.name().cmp(b.0.name()));
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
/// While a download holds it, of this ani-hop or another, nothing goes: `ResourceBusy`.
pub fn delete_show(dir: &Path) -> std::io::Result<()> {
    fs::read_dir(dir)?;
    let mut res = Ok(());
    edit_index(|all| {
        let Some(_alone) = unheld(dir) else {
            res = Err(std::io::ErrorKind::ResourceBusy.into());
            return;
        };
        res = show_files(dir).iter().try_for_each(fs::remove_file);
        if res.is_ok() {
            all.remove(dir);
            let _ = fs::remove_dir(dir); // fails while something of the user's is left in it
        }
    })?;
    res
}

/// What deleting a show folder deletes: its episodes, subtitles and unfinished parts, by
/// name, sorted. The question before shows them, as a folder of the user's own by that
/// name may hold episode files of theirs.
pub fn show_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = (rd.flatten().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.contains(" Episode ")
                && (name.ends_with(".mp4") || name.ends_with(".vtt") || name.contains(".mp4."))
        })
        .collect();
    files.sort();
    files
}

/// Drops a show folder of ours once nothing is left in it and no download holds it (one
/// starting may not have written anything yet).
pub fn drop_if_empty(dir: &Path) -> std::io::Result<()> {
    edit_index(|all| {
        let empty = fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
        if all.contains_key(dir)
            && empty
            && let Some(_alone) = unheld(dir)
            && fs::remove_dir(dir).is_ok()
        {
            all.remove(dir);
        }
    })
}

pub fn discard_parts(dir: &Path, eps: &[String]) {
    for p in parts(dir, eps) {
        let _ = fs::remove_file(p);
    }
}

/// The pieces yt-dlp was writing when episode `ep` stopped (by a pause, quitting, a crash).
/// Going on from one that was complete but not renamed yet gets HTTP 416, and yt-dlp's retry
/// without a range still sends one (its Request keeps the headers it was given, in 2026.10.21),
/// forever. Deleted before it goes on, they're fetched again whole; the finished ones stay.
fn half_pieces(dir: &Path, ep: &str) -> Vec<PathBuf> {
    let piece = format!(" Episode {ep}.mp4.part-Frag");
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    (rd.flatten().map(|e| e.path()))
        .filter(|p| {
            // exactly yt-dlp's "….part-Frag<n>.part", nothing of anyone else's
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            let n = (name.rsplit_once(&piece)).and_then(|(_, n)| n.strip_suffix(".part"));
            n.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect()
}

/// What downloads of `eps` that didn't finish left in `dir`.
pub fn parts(dir: &Path, eps: &[String]) -> Vec<PathBuf> {
    let done: Vec<String> = local_episodes(dir).into_iter().map(|(e, _)| e).collect();
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    (rd.flatten().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            eps.iter().filter(|ep| !done.contains(ep)).any(|ep| {
                name.contains(&format!(" Episode {ep}.mp4."))
                    || name.ends_with(&format!(" Episode {ep}.vtt"))
            })
        })
        .collect()
}

/// Whether `m` may become the show saved for a download folder: one ani-cli found never
/// replaces one AniList knows, whose details and MAL id (for skip times) Downloads needs.
fn replaces(m: &Media, saved: &Media) -> bool {
    m.on_anilist() || saved.id == 0 // 0: a new entry
}

/// Makes `m`'s download folder, as `folder` picks it, and puts it in the index, which then
/// knows the show (one found through ani-cli never replaces one AniList knows). All under
/// the index's lock: two ani-hops starting on two shows with one name at once take a
/// folder each.
/// The folder comes held, see `Hold`.
pub fn prepare(m: &Media) -> Result<(PathBuf, Hold), String> {
    prepare_in(&download_root(), m)
}

pub fn prepare_in(root: &Path, m: &Media) -> Result<(PathBuf, Hold), String> {
    let mut made = Err("no download folder".to_string());
    edit_index(|all| {
        let dir = folder_in(root, all.clone(), m);
        // held before the index lock goes, so no removal finds it empty and unheld
        let held = fs::create_dir_all(&dir).and_then(|()| {
            let f = hold_file(&dir)?;
            f.lock_shared()?;
            Ok(Hold(f))
        });
        made = held.map_err(|e| e.to_string()).map(|hold| {
            let d = all.entry(dir.clone()).or_default();
            if replaces(m, &d.media) {
                // the show's Entry stays the index's: a queued job's is from when it was queued
                let kept = (d.media.id == m.id).then(|| d.media.media_list_entry.take());
                d.media = m.clone();
                if let Some(entry) = kept {
                    d.media.media_list_entry = entry;
                }
            }
            (dir, hold)
        });
    })
    .map_err(|e| e.to_string())?;
    made
}

/// Deletes the pieces yt-dlp was writing when episode `ep` stopped, see `half_pieces`.
pub fn drop_half_pieces(dir: &Path, ep: &str) {
    for p in half_pieces(dir, ep) {
        let _ = fs::remove_file(p);
    }
}

/// Starts ani-cli downloading episode `ep` of `src` into `dir`; it says how far it got on
/// stdout (see `Lines`).
pub fn start_download(src: &Source, ep: &str, dub: bool, dir: &Path) -> Result<Child, String> {
    ani_cli()
        .args([
            "-d",
            "-S",
            &src.index.to_string(),
            "-e",
            ep,
            &plain(&src.query),
        ])
        .env("ANI_CLI_DOWNLOAD_DIR", dir)
        .env("ANI_CLI_MODE", mode(dub))
        .env("ANI_CLI_HIST_DIR", cache_dir().join("probe")) // downloading isn't watching
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ani-cli: {e}"))
}

/// FAKE_SUBS for episode `ep` in `dir`, if it's there and ani-cli found no subtitles for
/// it.
pub fn subs_command(m: &Media, src: &Source, dir: &Path, ep: &str, dub: bool) -> Option<Command> {
    let (_, file) = local_episodes(dir).into_iter().find(|(e, _)| e == ep)?;
    let vtt = file.with_extension("vtt");
    let page = subs_page(m, src, ep, dub).filter(|_| !vtt.exists())?;
    let mut cmd = Command::new(cache_dir().join("bin/ani-hop-subs"));
    cmd.arg(page)
        .arg(vtt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Some(cmd)
}

/// Whether a download of episode `ep` into `dir` that ended left its file. ani-cli exits 0
/// even if yt-dlp failed, so the file is the only reliable signal. One without sound goes.
pub fn finished(dir: &Path, ep: &str, dub: bool) -> Result<bool, String> {
    match local_episodes(dir).into_iter().find(|(e, _)| e == ep) {
        Some((_, file)) if silent(&file) => {
            let _ = delete_episode(&file);
            Err("no sound, its audio track needs a newer ani-cli".into())
        }
        Some((_, file)) => {
            let _ = edit_index(|all| {
                if let Some(d) = all.get_mut(dir) {
                    d.dub.retain(|f| *f != name(&file));
                    if dub {
                        d.dub.push(name(&file));
                    }
                }
            });
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Reads what a download says on stdout: yt-dlp's status lines, or how far ffmpeg got
/// (through FAKE_FFMPEG) with the episode's length and its speed.
#[derive(Default)]
pub struct Lines {
    /// the last percentage, for while it retries
    pct: Option<u32>,
    length: Option<f64>,
    speed: Option<f64>,
}

impl Lines {
    /// What line `l` says to show, and whether that's kept after a pause (yt-dlp's is).
    pub fn read(&mut self, l: &str) -> Option<(String, bool)> {
        if let Some(line) = l.strip_prefix("[download]") {
            return ytdlp_progress(line.trim(), &mut self.pct).map(|t| (t, true));
        }
        if let Some(d) = l.strip_prefix("duration=") {
            self.length = d.parse().ok().filter(|d: &f64| *d >= 1.0);
        } else if let Some(x) = l.strip_prefix("speed=") {
            self.speed = times(x);
        } else if let Some(t) = l
            .strip_prefix("out_time=")
            .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        {
            return Some((so_far(t, self.length, self.speed), false));
        }
        None
    }
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

/// What a download line of yt-dlp's says that matters: how far along and how long it still
/// takes ("18% · 2m left"), or that it's retrying. Its size estimate, speed and piece count
/// are left out, and so are where it writes to and the pieces it has already.
fn ytdlp_progress(line: &str, pct: &mut Option<u32>) -> Option<String> {
    if line.starts_with("Got error") {
        return Some(pct.map_or("retrying".into(), |p| format!("{p}% · retrying")));
    }
    let p: f64 = line
        .split_whitespace()
        .next()?
        .strip_suffix('%')?
        .parse()
        .ok()?;
    *pct = Some(p as u32);
    let eta = line
        .split_once("ETA ")
        .and_then(|(_, e)| clock(e.split_whitespace().next()?));
    Some(match eta {
        Some(secs) if p < 100.0 => format!("{}% · {}", p as u32, left(secs)),
        _ => format!("{}%", p as u32),
    })
}

/// How far ffmpeg (through FAKE_FFMPEG) got, as a share of the episode's `length` in seconds,
/// with how long the rest takes at its `speed` (times real time). Without a length, how much
/// of the episode it has.
fn so_far(out_time: &str, length: Option<f64>, speed: Option<f64>) -> String {
    let t = out_time.split('.').next().unwrap_or(out_time);
    match (clock(t), length) {
        (Some(s), Some(l)) => {
            let pct = (s / l * 100.0).min(100.0) as u32;
            match speed {
                Some(x) if s < l => format!("{pct}% · {}", left((l - s) / x)),
                _ => format!("{pct}%"),
            }
        }
        _ => format!("{} so far", t.strip_prefix("00:").unwrap_or(t)),
    }
}

/// ffmpeg's speed in times real time, from "8.42x", "   8x" (padded) or "N/A"; None unless
/// it's moving.
fn times(speed: &str) -> Option<f64> {
    let x: f64 = speed.trim().strip_suffix('x')?.trim().parse().ok()?;
    (x > 0.0).then_some(x)
}

/// Seconds in "01:19" or "1:02:03"; None for "Unknown" and the like.
fn clock(t: &str) -> Option<f64> {
    (t.split(':'))
        .try_fold(0.0, |s, x| x.parse().map(|x: f64| s * 60.0 + x))
        .ok()
}

/// "40s left", "2m left", "1h 5m left": how long the rest of a download takes.
fn left(secs: f64) -> String {
    let s = secs.round() as u64;
    let m = (s + 30) / 60;
    match s {
        0..60 => format!("{s}s left"),
        60..3600 => format!("{m}m left"),
        _ => format!("{}h {}m left", m / 60, m % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_read_with_and_without_vs_prefix() {
        let plain = "Available profiles:\n\tfirst\t\n\tmy profile\tdescribed\n\nlast line\n";
        assert_eq!(profiles(plain), ["first", "my profile"]);
        let verbose = "[cplayer] Reading\n[cplayer] Available profiles:\n[cplayer] \tfirst\t\n";
        assert_eq!(profiles(verbose), ["first"]);
    }

    #[test]
    fn a_profile_setting_hwdec_counts_and_a_look_alike_does_not() {
        for shown in [
            "Profile a: \n hwdec=no\n",
            "Profile a: \n no-hwdec=\n",
            "Profile a: \n volume=9\n hwdec=auto\n",
            "Profile a: \n profile=b\n  hwdec=auto\n",
            "Profile a: \n include=/x.conf\n",
            "Profile a: \n use-filedir-conf=yes\n",
        ] {
            assert!(profile_may_set_hwdec(shown), "{shown}");
        }
        for shown in ["Profile a: \n", "Profile a: \n hwdec-codecs=all\n"] {
            assert!(!profile_may_set_hwdec(shown), "{shown}");
        }
    }

    /// Ended whatever way the test goes.
    struct Ends(Child);

    impl Drop for Ends {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn mpv_itself_says_what_its_config_sets_and_nothing_in_it_starts() {
        // mpv reads this config as well, and CI has no mpv
        if !have("mpv") || Path::new("/etc/mpv/mpv.conf").exists() {
            return;
        }
        // its questions are on AHEAD, which the tests of quitting end
        let _turn = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ani-hop-hwdec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("mpv.conf");
        let mpv = || {
            let mut mpv = Command::new("mpv");
            mpv.env("MPV_HOME", &dir);
            mpv
        };
        for (config, leaves) in [
            ("volume=80\n", true),
            ("# hwdec=auto\n", true),
            ("hwdec-codecs=all\n", true),
            ("[extension.mkv]\nhwdec-codecs=all\n", true),
            ("[fast]\nhwdec=no\n", true), // only once applied, and a flag comes after ours
            ("really-quiet=yes\n", true),
            ("msg-level=all=no\n", true),
            ("hwdec=auto\n", false),
            ("hwdec=no\n", false),
            ("no-hwdec\n", false),
            ("[extension.mkv]\nhwdec=no\n", false),
            ("[extension.mkv]\nno-hwdec\n", false),
            ("[protocol.https]\nhwdec=vaapi\n", false),
            ("[c]\nprofile-cond=width>100\nhwdec=auto-copy\n", false),
            ("[my profile]\nhwdec=auto\n", false),
            ("[a]\nprofile=b\n[b]\nhwdec=auto\n", false),
            ("really-quiet=yes\n[extension.mkv]\nhwdec=no\n", false),
            ("msg-level=all=no\n[extension.mkv]\nhwdec=no\n", false),
            ("[extension.mkv]\ninclude=/nowhere.conf\n", false),
            ("use-filedir-conf=yes\n", false),
            ("[extension.mkv]\nuse-filedir-conf=yes\n", false),
            ("profile=fast\n[fast]\nhwdec=no\n", false),
        ] {
            fs::write(&conf, config).unwrap();
            assert_eq!(config_leaves_hwdec(&mpv), leaves, "{config}");
        }
        // told to say less than -v, its answer doesn't show it's what was asked for
        fs::write(&conf, "hwdec=no\n").unwrap();
        let quiet = || {
            let mut mpv = mpv();
            mpv.env("MPV_VERBOSE", "-1");
            mpv
        };
        assert!(!config_leaves_hwdec(&quiet));
        // a profile mpv doesn't find, as a name with a tab in it would be
        let tabbed = "[a\tb]\nhwdec=no\n";
        fs::write(&conf, tabbed).unwrap();
        assert!(!config_leaves_hwdec(&mpv), "{tabbed}");

        // a config naming a control socket, a stats file and a script, with a profile that
        // applies right away: the mpv already on that socket keeps it, and nothing else starts
        let (socket, stats) = (dir.join("socket"), dir.join("stats"));
        let (script, ran) = (dir.join("user.lua"), dir.join("ran"));
        fs::write(
            &script,
            format!("io.open({:?}, 'w')\n", ran.display().to_string()),
        )
        .unwrap();
        fs::write(
            &conf,
            format!(
                "input-ipc-server={0}\ndump-stats={1}\nscript={2}\n[now]\nprofile-cond=true\ninput-ipc-server={0}\n",
                socket.display(),
                stats.display(),
                script.display(),
            ),
        )
        .unwrap();
        let player = (mpv().args(["--idle=yes", "--force-window=no", "--no-input-terminal"]))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _player = Ends(player);
        for _ in 0..100 {
            if socket.exists() && ran.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ran.exists(), "the script runs in a real player");
        fs::remove_file(&ran).unwrap();
        fs::write(&stats, "kept").unwrap();
        assert!(config_leaves_hwdec(&mpv));
        std::os::unix::net::UnixStream::connect(&socket).expect("the running mpv still answers");
        assert_eq!(fs::read_to_string(&stats).unwrap(), "kept");
        assert!(!ran.exists(), "the user's script didn't run");
    }

    #[test]
    fn a_mise_install_updates_with_mise_and_keeps_its_ani_hop() {
        let mise = "mise/installs/github-reynke-de-vos-ani-hop/0.9.0/ani-hop";
        let up = "mise up github:ReynkeDeVos/ani-hop";
        for dir in [
            home().join(".local/share"),
            home().join(".local/bin"),
            "/opt/Cellar".into(),
        ] {
            assert_eq!(update_command_for(&dir.join(mise)), up);
            #[cfg(target_os = "linux")]
            assert!(
                install_sh(&dir.join(mise))
                    .unwrap()
                    .ends_with("| ANI_HOP_KEEP=1 sh")
            );
        }
        let ours = home().join(".local/bin/ani-hop");
        assert_eq!(update_command_for(&ours), INSTALL_SH);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(install_sh(&ours).as_deref(), Some(INSTALL_SH));
            assert_eq!(
                install_sh(Path::new("/opt/Cellar/ani-hop/1/bin/ani-hop")),
                None
            );
        }
    }

    #[test]
    fn keeps_the_newest_entries_of_downloaded_or_queued_shows() {
        let at = |progress| {
            let status = "CURRENT".into();
            Some(Entry {
                id: 1,
                status,
                progress,
            })
        };
        let (queued, other) = (71_001, 71_002);
        learn([(other, at(6))], 10, false);
        learn([(queued, at(5))], 10, true);
        learn([(queued, at(4))], 9, false);
        let kept = || {
            let kept: Learnt = read_json(&learnt_path());
            (
                kept.contains_key(&other),
                kept[&queued].0.as_ref().map(|e| e.progress),
            )
        };
        assert_eq!(kept(), (false, Some(5)), "the newest, of shows queued");
        // queuing again keeps what this session knew only over older ones: another ani-hop may
        // have saved at the same moment since
        learn([(queued, at(6))], 11, false);
        learn([(queued, at(5))], 11, true);
        assert_eq!(kept().1, Some(6));
    }

    #[test]
    fn tests_never_touch_the_real_home() {
        let tmp = std::env::temp_dir();
        assert!(home().starts_with(&tmp) && config_dir().starts_with(&tmp));
        assert!(cache_dir().starts_with(&tmp) && index_path().starts_with(&tmp));
    }

    #[test]
    fn says_which_site_ani_cli_cant_reach() {
        let err = b"\x1b[2K\x1b[1;31mConnection error: could not fetch https://hianime.at/search?keyword=x (no HTTP response; curl exit 7)\x1b[0m\n";
        assert_eq!(last_error(err), "can't reach hianime.at");
        assert_eq!(last_error(b"No results found!\n"), "No results found!");
        // a server page it couldn't fetch, then no stream: offline, not a missing dub
        let later = b"\x1b[1;31mConnection error: could not fetch https://hianime.at/ajax/x (no HTTP response; curl exit 6)\x1b[0m\nNo sources found for dub!\n\x1b[0m\n";
        assert_eq!(last_error(later), "can't reach hianime.at");
        assert!(unreachable(&last_error(later)) && !unreachable("No results found!"));
        // the network went once ani-cli had the episode's links: ffmpeg's or yt-dlp's words
        let cut = b"[tcp @ 0x55d0] Connection to tcp://vault-12.owocdn.top:443 failed: Network is unreachable\n[in#0 @ 0x55d1] Error opening input: Network is unreachable\nparts of it failed to download\n";
        assert_eq!(last_error(cut), "can't reach vault-12.owocdn.top");
        let dns = b"ERROR: Unable to download webpage: <urlopen error [Errno -3] Temporary failure in name resolution>\n";
        assert_eq!(last_error(dns), "can't reach its site");
    }

    #[test]
    fn remakes_get_folders_of_their_own() {
        let root = std::env::temp_dir().join(format!("ani-hop-folders-{}", std::process::id()));
        let show = |id: u64, name: &str| {
            let mut m = Media {
                id,
                ..Default::default()
            };
            m.title.romaji = Some(name.into());
            m
        };
        let (old, remake) = (show(1, "Trigun"), show(2, "Trigun"));
        let mut all = Index::new();
        all.insert(
            root.join("Trigun"),
            Downloaded {
                media: old.clone(),
                dub: Vec::new(),
            },
        );
        fs::create_dir_all(root.join("Trigun")).unwrap();
        assert_eq!(folder_in(&root, all.clone(), &old), root.join("Trigun"));
        assert_eq!(
            folder_in(&root, all.clone(), &remake),
            root.join("Trigun (2)")
        );
        // one ani-cli found goes where the AniList show of its name is
        let at = Source::at("q", &["Trigun".into()], 1, Vec::new());
        let found = Media::from_ani_cli("Trigun", at);
        assert_eq!(folder_in(&root, all.clone(), &found), root.join("Trigun"));
        // a show downloaded before the download folder changed stays where it is
        let elsewhere = root.join("old/Frieren");
        fs::create_dir_all(&elsewhere).unwrap();
        let frieren = show(3, "Frieren");
        all.insert(
            elsewhere.clone(),
            Downloaded {
                media: frieren.clone(),
                dub: Vec::new(),
            },
        );
        let new = root.join("new");
        assert_eq!(folder_in(&new, all.clone(), &frieren), elsewhere);
        let listed: Vec<u64> = shows_in(&new, all.clone()).iter().map(|s| s.0.id).collect();
        assert_eq!(listed, [3, 1]);
        // downloaded again in the new one before 0.7: listed once, from there
        fs::create_dir_all(new.join("Frieren")).unwrap();
        all.insert(
            new.join("Frieren"),
            Downloaded {
                media: frieren.clone(),
                dub: Vec::new(),
            },
        );
        let shows = shows_in(&new, all.clone());
        assert_eq!(shows.len(), 2);
        assert_eq!(shows[0].1, new.join("Frieren"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_held_folder_stays_while_empty() {
        let root = std::env::temp_dir().join(format!("ani-hop-held-{}", std::process::id()));
        let mut m = Media {
            id: 31_337,
            ..Default::default()
        };
        m.title.romaji = Some("Held".into());
        let (dir, hold) = prepare_in(&root, &m).unwrap();
        // a download that hasn't written anything yet
        drop_if_empty(&dir).unwrap();
        assert!(dir.is_dir() && ours(&dir), "kept while held");
        assert!(delete_show(&dir).is_err());
        assert!(dir.is_dir() && ours(&dir));
        drop(hold);
        // a process another test forks right then holds the lock file too, until its exec
        let start = std::time::Instant::now();
        while dir.exists() && start.elapsed().as_secs() < 2 {
            drop_if_empty(&dir).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!dir.exists() && !ours(&dir), "dropped once let go");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleting_a_show_a_download_holds_deletes_nothing() {
        let root = std::env::temp_dir().join(format!("ani-hop-busy-{}", std::process::id()));
        let mut m = Media {
            id: 41_001,
            ..Default::default()
        };
        m.title.romaji = Some("Busy".into());
        let (dir, hold) = prepare_in(&root, &m).unwrap();
        let (done, part) = (
            dir.join("Busy Episode 1.mp4"),
            dir.join("Busy Episode 2.mp4.part"),
        );
        fs::write(&done, "x").unwrap();
        fs::write(&part, "x").unwrap();
        let held = delete_show(&dir).unwrap_err();
        assert_eq!(held.kind(), std::io::ErrorKind::ResourceBusy);
        assert!(done.exists() && part.exists() && ours(&dir));
        // a process started just now has a copy until it runs: letting go still counts
        let copy = hold.0.try_clone().unwrap();
        drop(hold);
        delete_show(&dir).unwrap();
        drop(copy);
        assert!(!done.exists() && !part.exists() && !dir.exists() && !ours(&dir));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_name_the_file_system_keeps_for_itself_stays_in_the_download_folder() {
        let root = std::env::temp_dir().join(format!("ani-hop-dots-{}", std::process::id()));
        for (i, title) in ["..", ".", "", " .. "].into_iter().enumerate() {
            let mut m = Media {
                id: 41_100 + i as u64,
                ..Default::default()
            };
            m.title.romaji = Some(title.into());
            let dir = folder_in(&root, Index::new(), &m);
            assert_eq!(dir.parent(), Some(root.as_path()), "{title:?}");
            let named = name(&dir);
            assert!(named.starts_with("Show ") || named == " .. ", "{named}");
        }
    }

    #[test]
    fn a_folder_with_a_shows_id_can_be_taken_too() {
        let root = std::env::temp_dir().join(format!("ani-hop-ids-{}", std::process::id()));
        let show = |id, title: &str| {
            let mut m = Media {
                id,
                ..Default::default()
            };
            m.title.romaji = Some(title.into());
            m
        };
        let (first, _) = prepare_in(&root, &show(880_003, "A")).unwrap();
        let (taken, _) = prepare_in(&root, &show(880_005, "A (880004)")).unwrap();
        let (mine, _) = prepare_in(&root, &show(880_004, "A")).unwrap();
        assert_eq!(name(&first), "A");
        assert_eq!(name(&taken), "A (880004)");
        assert_eq!(name(&mine), "A (880004) 2");
        assert_eq!(index()[&taken].media.id, 880_005, "it's still that show's");
        for d in [&first, &taken, &mine] {
            edit_index(|all| drop(all.remove(d))).unwrap();
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn helpers_are_left_alone_unless_they_changed_and_never_empty() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("ani-hop-helper-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("rofi");
        write_helper(&helper, "#!/bin/sh\necho one\n").unwrap();
        let first = fs::metadata(&helper).unwrap();
        assert_eq!(first.permissions().mode() & 0o777, 0o755);
        write_helper(&helper, "#!/bin/sh\necho one\n").unwrap();
        assert_eq!(
            fs::metadata(&helper).unwrap().ino(),
            first.ino(),
            "untouched"
        );
        // what's running keeps the old one: the new one comes as a new file, whole
        write_helper(&helper, "#!/bin/sh\necho two\n").unwrap();
        assert_ne!(fs::metadata(&helper).unwrap().ino(), first.ino());
        // through sh: a test forking right then holds a copy of the file open until its exec,
        // which running the file itself would call busy
        let out = Command::new("sh").arg(&helper).output().unwrap();
        assert_eq!(out.stdout, b"two\n");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).unwrap();
        write_helper(&helper, "#!/bin/sh\necho two\n").unwrap();
        assert_eq!(
            fs::metadata(&helper).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "nothing left aside");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_helper_that_stalls_is_ended_in_time() {
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 30 & wait"]).stdin(Stdio::null());
        let since = std::time::Instant::now();
        assert!(output_within(&mut slow, Duration::from_millis(200)).is_none());
        assert!(since.elapsed() < Duration::from_secs(5));
        let mut quick = Command::new("sh");
        quick.args(["-c", "echo hi"]).stdin(Stdio::null());
        let out = output_within(&mut quick, Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, b"hi\n");
    }

    /// Closing the players goes for all of them, so these take turns.
    static CLOSING: Mutex<()> = Mutex::new(());

    #[test]
    fn closing_the_players_ends_what_runs_under_them() {
        let _turn = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
        let child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let shell = child.id();
        let player = Listed::on(&PLAYERS, &child);
        assert!(pids(&PLAYERS).contains(&shell) && player_open());
        let (ok, _) = {
            // ended from elsewhere, as quitting does, while this waits for it
            let closing = std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                close_players();
            });
            let res = follow(child, |_| {});
            closing.join().unwrap();
            res
        };
        assert!(!ok);
        assert!(!pids(&PLAYERS).contains(&shell), "drained");
        drop(player);
    }

    #[test]
    fn closing_the_players_ends_what_fetches_ahead_but_that_is_no_player_open() {
        let _turn = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
        let child = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let listed = Listed::on(&AHEAD, &child);
        assert!(pids(&AHEAD).contains(&pid));
        let closing = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            close_players();
        });
        let (ok, _) = follow(child, |_| {});
        closing.join().unwrap();
        assert!(!ok && !pids(&AHEAD).contains(&pid));
        drop(listed);
    }

    #[test]
    fn only_what_gone_ani_hops_left_goes_from_the_working_folder() {
        let dir = std::env::temp_dir().join(format!("ani-hop-left-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // an ani-hop that's running, whatever the age of its files
        fs::create_dir_all(run_dir()).unwrap();
        let alive = fs::File::create(run_dir().join("777001.lock")).unwrap();
        alive.lock().unwrap();
        let (kept, gone) = (
            ["ani-hsts", "notes", "subs-777001-1.vtt", "held-777001-1"],
            [
                "subs-777002-1.vtt",
                "menu-777002-1",
                "held-777002-3",
                "held-777002-3.4242",
            ],
        );
        for name in kept.iter().chain(&gone) {
            fs::write(dir.join(name), "x").unwrap();
        }
        clear_leftovers(&dir);
        assert!(kept.iter().all(|n| dir.join(n).exists()));
        assert!(gone.iter().all(|n| !dir.join(n).exists()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fake_mpv_told_to_hold_writes_each_argument_and_starts_nothing() {
        let dir = std::env::temp_dir().join(format!("ani-hop-hold-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("mpv");
        write_helper(&fake, FAKE_MPV).unwrap();
        let held = dir.join("held");
        let out = Command::new("sh")
            .arg(&fake)
            .args([
                "--referrer=https://a b/",
                "--title=It's \"one\"",
                "https://x/y",
            ])
            .env("ANIHOP_HOLD", &held)
            .env_remove("ANIHOP_SUBS_PAGE")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "no player started");
        let args = fs::read(&held).unwrap();
        let args: Vec<&[u8]> = args.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
        assert_eq!(
            args,
            [
                b"--referrer=https://a b/".as_slice(),
                b"--title=It's \"one\"".as_slice(),
                b"https://x/y".as_slice(),
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remakes_started_at_once_take_a_folder_each() {
        let root = std::env::temp_dir().join(format!("ani-hop-remakes-{}", std::process::id()));
        let starts: Vec<_> = (0..8u64)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut m = Media {
                        id: 40_000 + i,
                        ..Default::default()
                    };
                    m.title.romaji = Some("Remade".into());
                    prepare_in(&root, &m).unwrap().0
                })
            })
            .collect();
        let dirs: std::collections::HashSet<PathBuf> =
            starts.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(dirs.len(), 8);
        for d in &dirs {
            edit_index(|all| drop(all.remove(d))).unwrap();
        }
        fs::remove_dir_all(&root).unwrap();
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
        assert!(older("5.0.2", "v5.1"));
        assert!(older("5.1.6", "v5.2"));
        assert!(!older("5.1.0", "v5.1"));
        // ahead of the latest tag, as from master: no update to show
        assert!(!older("5.1.6", "v5.1"));
        assert!(older("0.5.0", "v0.6.0"));
        assert!(!older("0.10.0", "v0.9.1"));
        // no release yet: /releases/latest points at /releases
        assert!(!older("0.6.0", "releases"));
    }

    #[test]
    fn finds_ani_hop_in_a_nix_profile() {
        // what `nix profile add path:/tmp/somedir` wrote (Nix 2.35)
        let manifest = r#"{"elements":{"somedir":{"active":true,"attrPath":"packages.x86_64-linux.default","originalUrl":"path:/tmp/somedir","outputs":null,"priority":5,"storePaths":["/nix/store/4q23alnfw6d88bcdcikb60bvjb0plv4j-ani-hop-0.6.12"],"url":"path:/tmp/somedir"}},"version":3}"#;
        let pkg = Path::new("/nix/store/4q23alnfw6d88bcdcikb60bvjb0plv4j-ani-hop-0.6.12");
        assert_eq!(profile_element(manifest, pkg).as_deref(), Some("somedir"));
        let other = Path::new("/nix/store/0000000000000000000000000000000-ani-hop-0.6.11");
        assert_eq!(profile_element(manifest, other), None);
        // before Nix 2.20 elements were a list without names
        assert_eq!(profile_element(r#"{"elements":[],"version":2}"#, pkg), None);
    }

    #[test]
    fn a_lock_is_held_until_its_file_closes() {
        let path = std::env::temp_dir().join(format!("ani-hop-lock-{}", std::process::id()));
        let file = fs::File::create(&path).unwrap();
        assert!(!held(&path));
        file.lock().unwrap();
        assert!(held(&path));
        drop(file);
        // a process another test forks right then holds the file too, until its exec closes it
        let start = std::time::Instant::now();
        while held(&path) && start.elapsed().as_secs() < 2 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
    fn goes_on_without_the_pieces_yt_dlp_was_writing() {
        let dir = std::env::temp_dir().join(format!("ani-hop-pieces-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let files = [
            "Show Episode 2.mp4.part-Frag321.part",
            "Show Episode 2.mp4.part-Frag322",
            "Show Episode 2.mp4.part",
            "Show Episode 2.mp4.ytdl",
            "Show Episode 12.mp4.part-Frag3.part",
            "Show Episode 2.mp4.part-Frag3.saved.part",
            "Show Episode 2.mp4.part-Frag.part",
        ];
        files
            .iter()
            .for_each(|f| fs::write(dir.join(f), "").unwrap());
        assert_eq!(half_pieces(&dir, "2"), [dir.join(files[0])]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What a stopped download's cleanup does: nothing in a folder that isn't ani-hop's.
    fn discard(dir: &Path, eps: &[String]) {
        if ours(dir) {
            discard_parts(dir, eps);
            drop_if_empty(dir).unwrap();
        }
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
    fn download_progress_says_how_far_along() {
        // yt-dlp, as ani-cli shows it
        let mut pct = None;
        let mut yt = |l: &str| ytdlp_progress(l, &mut pct);
        assert_eq!(yt("Destination: /x/Show Episode 4.mp4"), None);
        assert_eq!(
            yt("  1.0% of ~  12.00MiB at  Unknown B/s ETA Unknown (frag 0/342)").as_deref(),
            Some("1%")
        );
        let line = "18.0% of ~ 506.76MiB at    5.05MiB/s ETA 01:19 (frag 61/342)";
        assert_eq!(yt(line).as_deref(), Some("18% · 1m left"));
        assert_eq!(
            yt("/x/Show Episode 4.mp4.part-Frag7 has already been downloaded"),
            None
        );
        let error = "Got error: HTTP Error 403: Forbidden. Retrying fragment 62 (1/inf)...";
        assert_eq!(yt(error).as_deref(), Some("18% · retrying"));
        assert_eq!(
            yt("99.8% of ~ 506.76MiB at 5.05MiB/s ETA 00:00:01 (frag 341/342)").as_deref(),
            Some("99% · 1s left")
        );
        assert_eq!(
            yt("100% of  506.76MiB in 00:01:40 at 5.05MiB/s").as_deref(),
            Some("100%")
        );
        // ffmpeg, through FAKE_FFMPEG
        assert_eq!(times("   8x"), Some(8.0));
        assert_eq!(times(" 1.3x"), Some(1.3));
        assert_eq!(times("8.42x"), Some(8.42));
        assert_eq!(times("N/A"), None);
        assert_eq!(times("   0x"), None);
        assert_eq!(
            so_far("00:26:16.123456", Some(1693.1), Some(8.0)),
            "93% · 15s left"
        );
        assert_eq!(
            so_far("00:06:45.0", Some(1693.1), Some(4.0)),
            "23% · 5m left"
        );
        assert_eq!(
            so_far("00:10:00.0", Some(20000.0), Some(1.0)),
            "3% · 5h 23m left"
        );
        assert_eq!(so_far("00:26:16.0", Some(1693.1), None), "93%");
        assert_eq!(so_far("00:28:14.0", Some(1693.1), Some(8.0)), "100%");
        assert_eq!(so_far("00:06:45.000000", None, Some(8.0)), "06:45 so far");
    }

    #[test]
    fn subs_page_is_for_a_source_numbered_as_the_show() {
        let m = |id_mal, episodes, status: &str| Media {
            id_mal,
            episodes,
            status: Some(status.into()),
            ..Media::default()
        };
        let src = Source::at("q", &["Show".into()], 1, vec!["1".into(); 12]);
        let done = "FINISHED";
        assert_eq!(
            subs_page(&m(Some(54492), Some(12), done), &src, "1", false).unwrap(),
            "https://megaplay.buzz/stream/mal/54492/1/sub"
        );
        assert_eq!(
            subs_page(&m(Some(54492), Some(12), done), &src, "12.5", true).unwrap(),
            "https://megaplay.buzz/stream/mal/54492/12.5/dub"
        );
        // what the provider calls an episode stays one part of the url
        assert_eq!(
            subs_page(&m(Some(1), Some(12), done), &src, "../x", false).unwrap(),
            "https://megaplay.buzz/stream/mal/1/..%2Fx/sub"
        );
        // still airing: the source has what's out so far, AniList may not say how many there
        // will be
        let airing = "RELEASING";
        assert!(subs_page(&m(Some(1), Some(24), airing), &src, "1", false).is_some());
        assert!(subs_page(&m(Some(1), None, airing), &src, "1", false).is_some());
        // no id for that server
        assert_eq!(subs_page(&m(None, Some(12), done), &src, "1", false), None);
        // several parts in one entry, or only one of the show's
        assert_eq!(
            subs_page(&m(Some(1), Some(6), done), &src, "1", false),
            None
        );
        assert_eq!(
            subs_page(&m(Some(1), Some(6), airing), &src, "1", false),
            None
        );
        assert_eq!(
            subs_page(&m(Some(1), Some(24), done), &src, "1", false),
            None
        );
    }

    #[test]
    fn ani_clis_ani_skip_asks_ani_hop_with_the_id_and_episode_it_was_given() {
        // alone in a process, as in the test below
        if std::env::var_os("ANIHOP_CHILD").is_none() {
            let name =
                "ani::tests::ani_clis_ani_skip_asks_ani_hop_with_the_id_and_episode_it_was_given";
            assert!(crate::in_child(name, &[]));
            return;
        }
        let dir = std::env::temp_dir().join(format!("ani-hop-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (helper, exe) = (dir.join("ani-skip"), dir.join("ani-hop"));
        write_helper(&helper, FAKE_SKIP).unwrap();
        write_helper(&exe, "#!/bin/sh\nprintf '%s,' \"$@\"\n").unwrap();
        // as ani-cli runs it: `ani-skip -i "$mal_id" -e "$ep_no"`
        let said = |args: &[&str]| {
            let o = Command::new(&helper)
                .args(args)
                .env("ANIHOP_EXE", &exe)
                .output()
                .unwrap();
            String::from_utf8(o.stdout).unwrap()
        };
        assert_eq!(said(&["-i", "52991", "-e", "12"]), "--skip-flags,52991,12,");
        // ani-cli found no id: asked anyway, ani-hop answers with nothing
        assert_eq!(said(&["-i", "", "-e", "12"]), "--skip-flags,,12,");
        // an update took away the ani-hop that's running: the one on PATH answers
        let gone = Command::new(&helper)
            .args(["-i", "1", "-e", "2"])
            .env("ANIHOP_EXE", dir.join("gone"))
            .env(
                "PATH",
                format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert_eq!(gone.stdout, b"--skip-flags,1,2,");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fake_mpv_finds_subtitles_only_when_ani_cli_has_none() {
        // alone in a process: a script can't run while another test's fork still holds it open
        // from being written ("Text file busy")
        if std::env::var_os("ANIHOP_CHILD").is_none() {
            let name = "ani::tests::fake_mpv_finds_subtitles_only_when_ani_cli_has_none";
            assert!(crate::in_child(name, &[]));
            return;
        }
        let dir = std::env::temp_dir().join(format!("ani-hop-mpv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (fake, real) = (dir.join("fake"), dir.join("real"));
        let sys = std::env::var("PATH").unwrap_or_default();
        // stands in for the real one: logs its arguments, fails to show its exit code comes through
        let mpv = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LOG.mpv\"\nexit 7\n";
        // and the real curl: logs each call, answers with the server's page, API or file
        let curl = r#"#!/bin/sh
echo "$*" >> "$LOG.curl"
for a; do
	[ "$prev" = -o ] && out=$a
	prev=$a
done
case $* in
*getSources*) printf '%s' "$TRACKS" ;;
*stream/mal*) printf '%s' "$PAGE" ;;
*)
	printf '%s' "$VTT" > "$out"
	[ "$VTT" != partial ] || exit 22
	;;
esac
"#;
        for (bin, name, script) in [
            (&fake, "mpv", FAKE_MPV),
            (&fake, "ani-hop-subs", FAKE_SUBS),
            (&real, "mpv", mpv),
            (&real, "curl", curl),
        ] {
            fs::create_dir_all(bin).unwrap();
            fs::write(bin.join(name), script).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (log, file) = (dir.join("log"), dir.join("subs.vtt"));
        const PAGE: &str = r#"<div id="player" data-id="12763" data-realid="1"></div>"#;
        // as the server has them: a track without a label, then English among others
        const TRACKS: &str = r#"{"tracks":[{"file":"https:\/\/h\/thumbs.vtt","kind":"thumbnails"},{"file":"https:\/\/h\/ara.vtt","label":"Arabic","kind":"captions"},{"file":"https:\/\/h\/eng.vtt","label":"English","kind":"captions","default":true}]}"#;
        const VTT: &str = "WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n";
        // what mpv got, what curl was asked, with what's in the file afterwards
        let run = |args: &[&str], page: Option<&str>, vars: [(&str, &str); 3]| {
            let _ = fs::remove_file(log.with_extension("curl"));
            let mut mpv = Command::new(fake.join("mpv"));
            if let Some(page) = page {
                mpv.env("ANIHOP_SUBS_PAGE", page)
                    .env("ANIHOP_SUBS_FILE", &file);
            }
            let o = mpv
                .args(args)
                .envs(vars)
                .env(
                    "PATH",
                    format!("{}:{}:{sys}", fake.display(), real.display()),
                )
                .env("LOG", &log)
                .output()
                .unwrap();
            assert_eq!(o.status.code(), Some(7), "mpv's own exit code");
            (
                fs::read_to_string(log.with_extension("mpv")).unwrap(),
                fs::read_to_string(log.with_extension("curl")).unwrap_or_default(),
                fs::read_to_string(&file).ok(),
            )
        };
        let page = Some("https://megaplay.buzz/stream/mal/1/1/sub");
        let all = [("PAGE", PAGE), ("TRACKS", TRACKS), ("VTT", VTT)];
        let stream = ["--referrer=r", "--force-media-title=A Episode 1", "v.m3u8"];

        // ani-cli has some: nothing is asked and mpv gets what ani-cli passed
        let args = ["--sub-file=https://x/a.vtt", "v.m3u8"];
        let (mpv, curl, got) = run(&args, page, all);
        assert_eq!(mpv, "--sub-file=https://x/a.vtt\nv.m3u8\n");
        assert_eq!((curl.as_str(), got), ("", None));

        // a show without a MyAnimeList id has no page to ask
        let (mpv, curl, _) = run(&stream, None, all);
        assert!(!mpv.contains("--sub-file") && curl.is_empty());

        // the page doesn't have the episode: ends there, mpv plays without
        let (mpv, curl, got) = run(&stream, page, [("PAGE", "Error"), all[1], all[2]]);
        assert!(!mpv.contains("--sub-file") && got.is_none());
        assert_eq!(curl.lines().count(), 1);

        // no English track
        let french = r#"{"tracks":[{"file":"https:\/\/h\/f.vtt","label":"French"}]}"#;
        let (mpv, curl, got) = run(&stream, page, [all[0], ("TRACKS", french), all[2]]);
        assert!(!mpv.contains("--sub-file") && got.is_none());
        assert_eq!(curl.lines().count(), 2);

        // a url curl would take for an option is never given to it
        let evil = dir.join("overwritten");
        let tracks = format!(
            r#"{{"tracks":[{{"file":"--libcurl={}","label":"English"}}]}}"#,
            evil.display()
        );
        let (mpv, curl, got) = run(&stream, page, [all[0], ("TRACKS", &tracks), all[2]]);
        assert!(!mpv.contains("--sub-file") && got.is_none());
        assert_eq!(curl.lines().count(), 2);
        assert!(!evil.exists() && !curl.contains("libcurl"));

        // a block page where the file should be, or a transfer cut off: not taken for
        // subtitles, nothing left behind
        for vtt in ["<!DOCTYPE html>", "partial"] {
            let (mpv, curl, got) = run(&stream, page, [all[0], all[1], ("VTT", vtt)]);
            assert!(!mpv.contains("--sub-file") && got.is_none());
            assert_eq!(curl.lines().count(), 3);
            let left: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
            assert!(!left.iter().any(|e| {
                let name = e.file_name();
                name.to_string_lossy().starts_with("subs.vtt")
            }));
        }

        // no times, no script
        let (mpv, _, _) = run(&stream, None, all);
        assert!(!mpv.contains("--script"));
        // times from ani-cli: the script that acts on them, whatever its folder is called
        let timed = ["--script-opts=skip-op_start=1,skip-op_end=9", "v.m3u8"];
        let (mpv, _, _) = run(&timed, None, all);
        let script = format!("--script={}", fake.join("ani-hop-skip.lua").display());
        assert_eq!(mpv, format!("{script}\n{}\n", timed.join("\n")));

        // it's there: mpv gets the English file, with its other arguments as they were
        let (mpv, curl, got) = run(&stream, page, all);
        let sub_file = format!("--sub-file={}\n", file.display());
        assert_eq!(mpv, format!("{sub_file}{}\n", stream.join("\n")));
        assert_eq!(got.as_deref(), Some(VTT));
        assert!(curl.contains("getSources?id=12763") && curl.contains("https://h/eng.vtt"));
        assert!(
            curl.lines()
                .all(|l| l.contains("-e https://megaplay.buzz/"))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fake_ffmpeg_renames_only_finished_downloads() {
        let dir = std::env::temp_dir().join(format!("ani-hop-ffmpeg-{}", std::process::id()));
        let (fake, real) = (dir.join("fake"), dir.join("real"));
        // what the shim runs besides ffmpeg (pkill, grep, mv); no /usr/bin on NixOS or in Nix builds
        let sys = std::env::var("PATH").unwrap_or_default();
        // stands in for the real one: logs its arguments, writes its last one, warns $WARN
        // (with $SLOW it keeps going, its pid in $LOG.ffmpeg)
        let logger = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LOG\"\nfor a; do o=$a; done\n: > \"$o\"\n[ -z \"$SLOW\" ] || { echo $$ > \"$LOG.ffmpeg\"; exec sleep 60; }\n[ -z \"$WARN\" ] || echo \"$WARN\" >&2\nexit $CODE\n";
        // and the real ffprobe: says what it was asked as the length, or with $HANG never answers
        let probe = "#!/bin/sh\n[ -z \"$HANG\" ] || { echo $$ > \"$LOG.probe\"; exec sleep 60; }\necho \"duration=$*\"\n";
        for (bin, name, script) in [
            (&fake, "ffmpeg", FAKE_FFMPEG),
            (&real, "ffmpeg", logger),
            (&real, "ffprobe", probe),
        ] {
            fs::create_dir_all(bin).unwrap();
            fs::write(bin.join(name), script).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (log, out) = (dir.join("log"), dir.join("A Episode 1.mp4"));
        let part = dir.join("A Episode 1.mp4.part");
        let run = |code: &str, warn: &str, args: &[&str]| {
            let _ = fs::remove_file(&out);
            let path = format!("{}:{}:{sys}", fake.display(), real.display());
            // through sh: a test forking right then would make the file busy for an exec
            let o = Command::new("sh")
                .arg(fake.join("ffmpeg"))
                .args(args)
                .arg(&out)
                .env("PATH", path)
                .env("LOG", &log)
                .env("CODE", code)
                .env("WARN", warn)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            let length = String::from_utf8_lossy(&o.stdout).into_owned();
            (
                o.status.success(),
                fs::read_to_string(&log).unwrap(),
                err,
                length,
            )
        };
        // ani-cli's call for a show whose audio is a separate track
        #[rustfmt::skip]
        let ani_cli = ["-extension_picky", "0", "-referer", "r", "-i", "v.m3u8", "-extension_picky",
            "0", "-referer", "r2", "-i", "a.m3u8", "-c", "copy"];
        let (ok, args, _, length) = run("0", "", &ani_cli);
        assert!(ok && out.exists() && !part.exists());
        let asked = "-v error -extension_picky 0 -referer r -show_entries format=duration";
        assert_eq!(
            length,
            format!("duration={asked} -of default=nw=1 v.m3u8\n")
        );
        assert!(args.ends_with(&format!("-f\nmp4\n{}\n", part.display())));
        let (ok, _, _, _) = run("1", "", &ani_cli);
        assert!(
            !ok && !out.exists() && part.exists(),
            "a failed one stays .part"
        );
        // ffmpeg exits 0 after skipping a segment it couldn't get
        let skipped = "[in#0/hls @ 0x1] Segment 7 of playlist 0 failed too many times, skipping";
        let (ok, _, err, _) = run("0", skipped, &ani_cli);
        assert!(!ok && !out.exists() && part.exists());
        assert!(err.starts_with(skipped) && err.ends_with("parts of it failed to download\n"));
        let broken = "[NULL @ 0x1] [error] Invalid NAL unit size (1234 > 567).";
        let (ok, _, _, _) = run("0", broken, &ani_cli);
        assert!(
            !ok && !out.exists() && part.exists(),
            "errors in what it got"
        );
        // a probe that never answers doesn't keep the download from ending
        let start = std::time::Instant::now();
        let o = Command::new(fake.join("ffmpeg"))
            .args(ani_cli)
            .arg(&out)
            .env(
                "PATH",
                format!("{}:{}:{sys}", fake.display(), real.display()),
            )
            .env("LOG", &log)
            .env("CODE", "0")
            .env("HANG", "1")
            .output()
            .unwrap();
        assert!(o.status.success() && start.elapsed().as_secs() < 30);
        // stopped (pause, stop, quit): the .part stays for the next try to replace, ffmpeg and
        // ffprobe go
        let _ = fs::remove_file(&out);
        let mut shim = Command::new(fake.join("ffmpeg"))
            .args(ani_cli)
            .arg(&out)
            .env(
                "PATH",
                format!("{}:{}:{sys}", fake.display(), real.display()),
            )
            .env("LOG", &log)
            .env("CODE", "0")
            .env("SLOW", "1")
            .env("HANG", "1")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = |name: &str| fs::read_to_string(dir.join(format!("log.{name}"))).ok();
        let gone = |p: &str| {
            !Command::new("kill")
                .args(["-0", p.trim()])
                .status()
                .unwrap()
                .success()
        };
        let wait_for = |done: &dyn Fn() -> bool| {
            let start = std::time::Instant::now();
            while !done() && start.elapsed().as_secs() < 10 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            done()
        };
        assert!(wait_for(
            &|| pid("ffmpeg").is_some() && pid("probe").is_some()
        ));
        let id = shim.id().to_string();
        Command::new("kill").args(["-TERM", &id]).status().unwrap();
        assert!(!shim.wait().unwrap().success());
        assert!(part.exists() && !out.exists() && !dir.join("A Episode 1.mp4.part.log").exists());
        for name in ["ffmpeg", "probe"] {
            let p = pid(name).unwrap();
            assert!(wait_for(&|| gone(&p)), "{name} is gone");
        }
        let (ok, _, _, _) = run("0", "[mp4 @ 0x1] [warning] Non-monotonic DTS", &ani_cli);
        assert!(ok && out.exists(), "the muxer's warnings lose nothing");
        assert!(!dir.join("A Episode 1.mp4.part.log").exists());
        // yt-dlp's own calls (merging, fixups) go through untouched
        let (ok, args, _, _) = run("0", "", &["-y", "-i", "v.mp4"]);
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
        // playback's dub fallback looks for "dub" in it
        assert_eq!(
            last_error(b"\x1b[2K\x1b[1;31mNo sources found for dub!\x1b[0m\n"),
            "no dub stream ani-cli can play"
        );
    }

    #[test]
    fn a_stream_says_when_its_player_starts_and_when_it_shows_the_episode() {
        let steps = std::cell::RefCell::new(Vec::new());
        let step = |s: Option<&'static str>| steps.borrow_mut().push(s);
        for line in [
            "Checking dependencies",
            "ani-hop-player-starts",
            "AO: x",
            "VO: [gpu] 1920x1080",
        ] {
            stream_step(line, &step);
        }
        assert_eq!(*steps.borrow(), [Some("opening the player"), None]);
        assert!(FAKE_MPV.contains("echo ani-hop-player-starts"));
    }
}
