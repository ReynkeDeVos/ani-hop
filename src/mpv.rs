//! Concrete playback adapter: runs ani-cli, mpv, yt-dlp (via mpv) and catt, tracks open
//! player processes so quitting closes them, and detects whether an episode played to the end.

use crate::ani::{self, ANI_CLI_LIMIT, NO_NETWORK, ani_cli, follow, last_error, plain};
use crate::anilist::Media;
use crate::playback::{OPENING, Player};
use crate::prefs;
use crate::skip;
use crate::source::{Lookup, Source};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// What FAKE_MPV says right before it starts mpv.
const PLAYER_STARTS: &str = "ani-hop-player-starts";

/// Has mpv's status line say how many seconds are left, for `End`.
pub(crate) const SAY_LEFT: &str = "--term-status-msg=ani-hop-left ${=time-remaining}";

/// Flags prepended to mpv so `VO:`, `ani-hop-left …` and `Exiting... (End of file)` still print
/// when stdout is piped and the user's `mpv.conf` sets `quiet`, `really-quiet`, `terminal=no`
/// or `msg-level=all=no`. Placed first so user CLI flags still win.
pub(crate) const LOUD_MPV: [&str; 5] = [
    "--quiet=no",
    "--really-quiet=no",
    "--terminal=yes",
    "--msg-level=cplayer=info,statusline=status",
    SAY_LEFT,
];

/// Why `cast` didn't: the episode isn't downloaded.
pub const NOT_DOWNLOADED: &str = "not downloaded";

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
    all.into_iter().for_each(ani::end);
}

/// ani-cli for streams, mpv for files and trailers, catt for casting.
pub struct Mpv;

impl Player for Mpv {
    fn stream(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        skip: bool,
        step: &dyn Fn(Option<&'static str>),
    ) -> Result<bool, String> {
        stream(m, src, ep, audio, skip, step)
    }

    fn hold(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        skip: bool,
    ) -> Result<Held, String> {
        hold_stream(m, src, ep, audio, skip)
    }

    fn replay(&self, held: &Held, step: &dyn Fn(Option<&'static str>)) -> Replayed {
        play_held(held, || step(None))
    }

    fn on_disk(&self, m: &Media, ep: &str) -> Option<PathBuf> {
        let eps = ani::local_episodes(&ani::folder(m));
        eps.into_iter().find(|(e, _)| e == ep).map(|f| f.1)
    }

    fn file(
        &self,
        m: &Media,
        ep: &str,
        file: &Path,
        audio: &str,
        skip: bool,
        up: &mut dyn FnMut(),
    ) -> Result<bool, String> {
        play_file(m, ep, file, audio, skip, up)
    }

    fn trailer(&self, m: &Media, url: &str, up: &mut dyn FnMut()) -> Result<(), String> {
        play_trailer(m, url, up)
    }

    fn cast(
        &self,
        m: &Media,
        ep: &str,
        audio: &str,
        wanted: &dyn Fn() -> bool,
        asked: &dyn Fn(String),
    ) -> Result<(), String> {
        cast(m, ep, audio, wanted, asked)
    }

    fn locate(&self, m: &Media, lookup: Lookup) -> Result<Source, String> {
        lookup.run(m)
    }
}

/// Whether mpv played an episode to its end, from what it prints. It says "End of file" also
/// when a stream broke off, so only with about nothing left by its status line (SAY_LEFT).
#[derive(Default)]
struct End {
    left: Option<f64>,
    eof: bool,
}

impl End {
    /// After a prefix, as a config can have mpv name the module ("cplayer: ") or the time.
    fn hear(&mut self, line: &str) {
        let left = line
            .split_once("ani-hop-left ")
            .map(|(_, l)| l.trim().parse());
        if let Some(Ok(left)) = left {
            self.left = Some(left);
        }
        self.eof |= line.ends_with("Exiting... (End of file)");
    }

    /// Closing it says "(Quit)"; without a status line it can't tell.
    fn played(&self) -> bool {
        self.eof && self.left.is_some_and(|left| left < 10.0)
    }
}

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
        PLAYER_STARTS => step(Some(OPENING)),
        line => player_up(line, &mut || step(None)),
    }
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
        let out = ani::wait_within(child, Duration::from_secs(5))?;
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

/// ani-cli set up to take episode `ep` of `src` to the player, and the file
/// the player will read subtitles from, if ani-cli finds none.
fn stream_command(
    m: &Media,
    src: &Source,
    ep: &str,
    audio: &str,
    skip: bool,
) -> (Command, Option<PathBuf>) {
    let mut flags = std::env::var("ANI_CLI_PLAYER_FLAGS").unwrap_or_default();
    let player = own_player().unwrap_or_else(|| "mpv".into());
    // an mpv flag, other players ani-cli knows (vlc, syncplay) don't start with it
    if player.contains("mpv") && !prefs::subs(audio) {
        flags.push_str(" --sub-visibility=no");
    }
    // first, as the last flag wins and the user's own come after it. Another player has another
    // config
    if player == "mpv" && hwdec_unset() {
        flags = format!("{HWDEC} {flags}");
    }
    let mut cli = ani_cli();
    cli.env("ANIHOP_EXE", ani::exe());
    if skip::last_episode(m, &src.episodes, ep) {
        cli.env(skip::LAST_EPISODE, "1");
    }
    cli.env(skip::JUMP, if skip { "yes" } else { "no" });
    let subs = ani::subs_page(m, src, ep, prefs::dub(audio)).map(|page| {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = format!("subs-{}-{n}.vtt", std::process::id());
        let file = ani::cache_dir().join("probe").join(name);
        cli.env("ANIHOP_SUBS_FILE", &file)
            .env("ANIHOP_SUBS_PAGE", page);
        file
    });
    cli.env("ANI_CLI_PLAYER", player)
        .env("ANI_CLI_MODE", ani::mode(prefs::dub(audio)))
        .env("ANI_CLI_PLAYER_FLAGS", flags)
        .args([
            // asks ani-hop's stand-in for ani-skip, whose answer has the chapters even with
            // skipping off
            "--skip",
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

/// Ends a child process if it hasn't reached the player within `limit`.
struct Watchdog {
    done: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
}

impl Watchdog {
    fn start(pid: u32, limit: Duration, armed: bool) -> Self {
        let done = Arc::new(AtomicBool::new(!armed));
        let stalled = Arc::new(AtomicBool::new(false));
        if armed {
            let (done, stalled) = (done.clone(), stalled.clone());
            std::thread::spawn(move || {
                let since = std::time::Instant::now();
                while !done.load(Ordering::Relaxed) {
                    if since.elapsed() >= limit {
                        if !done.swap(true, Ordering::Relaxed) {
                            stalled.store(true, Ordering::Relaxed);
                            ani::end(pid);
                        }
                        return;
                    }
                    std::thread::sleep(limit.min(Duration::from_millis(200)));
                }
            });
        }
        Watchdog { done, stalled }
    }

    fn disarm(&self) {
        self.done.store(true, Ordering::Relaxed);
    }

    fn stalled(&self) -> bool {
        self.stalled.load(Ordering::Relaxed)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// Streams through ani-cli; `step` hears when mpv starts, and None once it
/// shows the episode. Ok(true): mpv played it to the end.
fn stream(
    m: &Media,
    src: &Source,
    ep: &str,
    audio: &str,
    skip: bool,
    step: &dyn Fn(Option<&'static str>),
) -> Result<bool, String> {
    let (mut cli, subs) = stream_command(m, src, ep, audio, skip);
    let child = cli.spawn().map_err(|e| format!("ani-cli: {e}"))?;
    let _player = Listed::on(&PLAYERS, &child);
    // up to mpv only: another player may never say it's there, and one playing mustn't be ended
    let watch = Watchdog::start(child.id(), ANI_CLI_LIMIT, own_player().is_none());
    let mut end = End::default();
    let played = follow(child, |l| {
        if l == PLAYER_STARTS || l.starts_with("VO:") {
            watch.disarm();
        }
        end.hear(l);
        stream_step(l, step)
    });
    watch.disarm();
    // gone once the player is, which read it at the start
    if let Some(file) = subs {
        let _ = fs::remove_file(file);
    }
    match played {
        (true, _) => Ok(end.played()),
        (false, _) if watch.stalled() => Err("ani-cli didn't answer for a minute".into()),
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
    /// Ok(true): played to the end
    Ended(Result<bool, String>),
    /// mpv never showed it and failed: its links are too old, nothing was watched
    TooOld,
}

/// Whether a stream can be fetched ahead: another player may take other arguments, or none of
/// these.
pub fn can_hold() -> bool {
    own_player().is_none()
}

/// Runs ani-cli up to where it would start mpv, and keeps what it would start it with.
fn hold_stream(m: &Media, src: &Source, ep: &str, audio: &str, skip: bool) -> Result<Held, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let (mut cli, subs) = stream_command(m, src, ep, audio, skip);
    let name = format!(
        "held-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let file = ani::cache_dir().join("probe").join(name);
    cli.env("ANIHOP_HOLD", &file)
        // nothing's watched yet
        .env("ANI_CLI_HIST_DIR", ani::cache_dir().join("probe"));
    // from here on the subtitles go with it, also when it fails
    let mut held = Held {
        args: Vec::new(),
        subs,
    };
    let child = cli.spawn().map_err(|e| format!("ani-cli: {e}"))?;
    let _listed = Listed::on(&AHEAD, &child);
    let out = ani::wait_within(child, ANI_CLI_LIMIT).ok_or("ani-cli didn't answer for a minute")?;
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
fn play_held(held: &Held, mut up: impl FnMut()) -> Replayed {
    let child = (Command::new("mpv").args(&held.args))
        // for the skip script, which asks ani-hop for the times
        .env("ANIHOP_EXE", ani::exe())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => return Replayed::Ended(Err(format!("mpv: {e}"))),
    };
    let _player = Listed::on(&PLAYERS, &child);
    let (mut shown, mut end) = (false, End::default());
    let played = follow(child, |l| {
        end.hear(l);
        player_up(l, &mut || {
            shown = true;
            up()
        })
    });
    match played {
        (true, _) => Replayed::Ended(Ok(end.played())),
        (false, _) if !shown => Replayed::TooOld,
        (false, stderr) => Replayed::Ended(Err(last_error(&stderr))),
    }
}

/// Ok(true): mpv played it to the end.
fn play_file(
    m: &Media,
    ep: &str,
    file: &Path,
    audio: &str,
    skip: bool,
    mut up: impl FnMut(),
) -> Result<bool, String> {
    let mut mpv = Command::new("mpv");
    mpv.args(LOUD_MPV)
        .arg(format!("--force-media-title={} Episode {ep}", m.name()))
        .args(hwdec_unset().then_some(HWDEC));
    let vtt = file.with_extension("vtt");
    if vtt.exists() {
        mpv.arg(format!("--sub-file={}", vtt.display()));
        if prefs::hide_subs(audio, ani::is_dub(file)) {
            mpv.arg("--sub-visibility=no");
        }
    }
    if let Some(mal) = m.id_mal {
        let last = skip::last_episode(m, &[], ep);
        mpv.arg(format!("--script={}", ani::skip_script().display()))
            .args(skip::ask_later(mal, ep, ani::is_dub(file), last, skip))
            .env("ANIHOP_EXE", ani::exe());
    }
    let child = mpv
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("mpv: {e}"))?;
    let _player = Listed::on(&PLAYERS, &child);
    let mut end = End::default();
    let played = follow(child, |l| {
        end.hear(l);
        player_up(l, &mut up)
    });
    match played {
        (true, _) => Ok(end.played()),
        (false, stderr) => Err(last_error(&stderr)),
    }
}

fn trailer_command(m: &Media, url: &str) -> Command {
    let mut mpv = Command::new("mpv");
    mpv.arg(format!("--force-media-title={} · Trailer", m.name()))
        .args([
            "--resume-playback=no",
            "--save-position-on-quit=no",
            // stdout is piped: ensure VO: and ytdl_hook errors still print under a quiet mpv.conf
            "--quiet=no",
            "--really-quiet=no",
            "--terminal=yes",
            "--msg-level=cplayer=info,ytdl_hook=error",
        ])
        .args(hwdec_unset().then_some(HWDEC))
        .arg("--")
        .arg(url);
    mpv
}

/// Plays a show's trailer URL directly in mpv (which resolves YouTube/Dailymotion via yt-dlp).
fn play_trailer(m: &Media, url: &str, up: impl FnMut()) -> Result<(), String> {
    run_trailer(trailer_command(m, url), url, ANI_CLI_LIMIT, up)
}

fn run_trailer(
    mut mpv: Command,
    url: &str,
    limit: Duration,
    mut up: impl FnMut(),
) -> Result<(), String> {
    let child = mpv
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("mpv: {e}"))?;
    let _player = Listed::on(&PLAYERS, &child);
    let watch = Watchdog::start(child.id(), limit, true);
    let mut out = Vec::new();
    let played = follow(child, |l| {
        if l.contains("[ytdl_hook]") {
            out.push(l.to_string());
        }
        player_up(l, &mut || {
            watch.disarm();
            up();
        });
    });
    watch.disarm();
    match played {
        (true, _) => Ok(()),
        (false, _) if watch.stalled() => Err(unreachable_host(url)),
        (false, stderr) => Err(trailer_error(url, &out, &stderr)),
    }
}

fn unreachable_host(url: &str) -> String {
    let host = ani::url_host(url).map(|h| h.strip_prefix("www.").unwrap_or(h));
    format!("can't reach {}", host.unwrap_or("its site"))
}

/// Turns mpv's ytdl_hook stdout (where yt-dlp errors land) or stderr into a short user message.
fn trailer_error(url: &str, stdout: &[String], stderr: &[u8]) -> String {
    let err_text = String::from_utf8_lossy(stderr);
    let offline = |s: &str| {
        NO_NETWORK.iter().any(|n| s.contains(n))
            || s.contains("Failed to resolve")
            || s.contains("Name or service not known")
            || s.contains("timed out")
            || s.contains("ConnectTimeoutError")
    };
    if stdout.iter().any(|l| offline(l)) || offline(&err_text) {
        return unreachable_host(url);
    }
    if let Some(msg) = stdout
        .iter()
        .find_map(|l| l.split_once("[ytdl_hook] ERROR:").map(|(_, r)| r.trim()))
    {
        // strip "[youtube] <id>: " when present, and drop yt-dlp's CLI/cookie advice
        let msg = match msg.strip_prefix('[').and_then(|s| s.split_once("] ")) {
            Some((_, rest)) => match rest.split_once(": ") {
                Some((id, reason)) if !id.contains(' ') => reason,
                _ => rest,
            },
            None => msg,
        };
        let msg = msg
            .split(" Use --")
            .next()
            .unwrap_or(msg)
            .split(" See http")
            .next()
            .unwrap_or(msg)
            .trim()
            .trim_end_matches('.');
        if !msg.is_empty() {
            return msg.to_string();
        }
    }
    if stdout
        .iter()
        .any(|l| l.contains("youtube-dl failed:") && l.contains("not found"))
    {
        return "yt-dlp not found".into();
    }
    if !err_text.trim().is_empty() {
        return last_error(stderr);
    }
    "mpv couldn't play it".into()
}

/// Casts a downloaded episode with catt, to its default Chromecast or the one it finds, with
/// the subtitles mpv would show. The file's looked for as playing does, so an ani-cli result
/// finds the AniList show's folder it shares. `wanted` says right before catt starts whether
/// it's still the episode asked for, and `asked` hears the TV's name once catt asks it to play.
/// catt serves the file for as long as the TV plays it, in a process group of its own so
/// quitting ani-hop doesn't end it, and this waits for it: Ok once the TV is done, else why
/// not, NOT_DOWNLOADED, or nothing when it wasn't wanted anymore.
fn cast(
    m: &Media,
    ep: &str,
    audio: &str,
    wanted: &dyn Fn() -> bool,
    asked: &dyn Fn(String),
) -> Result<(), String> {
    let on_disk = ani::local_episodes(&ani::folder(m))
        .into_iter()
        .find(|e| e.0 == ep);
    let Some((_, file)) = on_disk else {
        return Err(NOT_DOWNLOADED.into());
    };
    let mut catt = Command::new("catt");
    catt.arg("cast")
        .arg(format!("--title={} Episode {ep}", m.name()));
    let vtt = file.with_extension("vtt");
    match vtt.exists() && !prefs::hide_subs(audio, ani::is_dub(&file)) {
        true => catt.arg("--subtitles").arg(vtt),
        false => catt.arg("--no-subs"),
    };
    // a file, not a pipe: catt goes on writing while the TV plays, maybe after ani-hop quit
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let log = ani::cache_dir()
        .join("probe")
        .join(format!("cast-{}-{n}", std::process::id()));
    let said = fs::create_dir_all(log.parent().unwrap()).and_then(|()| fs::File::create(&log));
    let said = said.map_err(|e| e.to_string())?;
    // last, as what comes before it may wait on a sleeping disk
    if !wanted() {
        let _ = fs::remove_file(&log);
        return Err(String::new());
    }
    let mut child = (catt.arg(&file))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(said.try_clone().map_err(|e| e.to_string())?)
        .stderr(said)
        .spawn()
        .map_err(|e| format!("catt: {e}"))?;
    let read = || fs::read_to_string(&log).unwrap_or_default();
    // asking the TV comes first; whether it plays, a while later, only as how catt ends
    let ended = loop {
        if let Some(tv) = read().lines().find_map(casting_to) {
            asked(tv);
            break child.wait();
        }
        if let Some(ended) = child.try_wait().transpose() {
            break ended;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let said = read();
    let _ = fs::remove_file(&log);
    match ended {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(last_error(said.as_bytes())),
        Err(e) => Err(e.to_string()),
    }
}

/// The TV in what catt says right before it serves the file: Playing "<title>" on "<TV>"...
fn casting_to(line: &str) -> Option<String> {
    let (_, tv) = line.strip_prefix("Playing ")?.rsplit_once(" on \"")?;
    Some(tv.strip_suffix("\"...")?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closing the players goes for all of them, so these take turns.
    static CLOSING: Mutex<()> = Mutex::new(());

    /// Ended whatever way the test goes.
    struct Ends(Child);

    impl Drop for Ends {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

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

    #[test]
    fn mpv_says_when_it_played_to_the_end_and_not_when_closed() {
        let end = |lines: &[&str]| {
            let mut end = End::default();
            lines.iter().for_each(|l| end.hear(l));
            end.played()
        };
        assert!(end(&["ani-hop-left 0.07", "Exiting... (End of file)"]));
        let prefixed = [
            "statusline: ani-hop-left 0.0",
            "cplayer: Exiting... (End of file)",
        ];
        assert!(end(&prefixed));
        // a stream that broke off
        assert!(!end(&["ani-hop-left 28.06", "Exiting... (End of file)"]));
        assert!(!end(&["ani-hop-left 0.0", "Exiting... (Quit)"]));
        assert!(
            !end(&["Exiting... (End of file)"]),
            "no status line: can't tell"
        );
        // CI has no mpv
        if !ani::have("mpv") {
            return;
        }
        let ends = |before: &[&str], after: &[&str]| {
            let child = Command::new("mpv")
                .args(["--no-config", "--vo=null", "--ao=null"])
                .args(before)
                .args(LOUD_MPV)
                .args(after)
                .arg("av://lavfi:testsrc=d=0.2")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut end = End::default();
            let mut up = false;
            let (ok, _) = follow(child, |l| {
                end.hear(l);
                player_up(l, &mut || up = true);
            });
            assert!(ok, "mpv played it");
            (end.played(), up)
        };
        assert_eq!(ends(&[], &[]), (true, true));
        assert_eq!(ends(&[], &["--msg-module", "--msg-time"]), (true, false));
        // quiet mpv.conf settings before LOUD_MPV must not hide VO: or end-of-file status
        assert_eq!(ends(&["--quiet"], &[]), (true, true));
        assert_eq!(
            ends(
                &["--really-quiet", "--terminal=no", "--msg-level=all=no"],
                &[]
            ),
            (true, true)
        );
        assert!(!ends(&[], &["--input-commands=quit"]).0);
    }

    #[test]
    fn mpv_itself_says_what_its_config_sets_and_nothing_in_it_starts() {
        // mpv reads this config as well, and CI has no mpv
        if !ani::have("mpv") || Path::new("/etc/mpv/mpv.conf").exists() {
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
    fn casting_hears_the_tv_asked_then_how_catt_ended() {
        let dir = std::env::temp_dir().join(format!("ani-hop-cast-{}", std::process::id()));
        if std::env::var_os("ANIHOP_CHILD").is_none() {
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("bin")).unwrap();
            let catt = "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"${0%/*}/args\"\n\
                echo 'Playing \"Show Episode 1\" on \"Wohnzimmer\"...'\n\
                sleep 0.3\necho 'Error: Playback of local file has failed' >&2\nexit 1\n";
            ani::write_helper(&dir.join("bin/catt"), catt).unwrap();
            let path = format!(
                "{}:{}",
                dir.join("bin").display(),
                std::env::var("PATH").unwrap()
            );
            let env = [
                ("PATH", path.into()),
                ("HOME", dir.clone().into_os_string()),
                ("XDG_CONFIG_HOME", dir.join("config").into_os_string()),
                ("XDG_CACHE_HOME", dir.join("cache").into_os_string()),
                ("ANI_CLI_DOWNLOAD_DIR", dir.join("dl").into_os_string()),
            ];
            let name = "mpv::tests::casting_hears_the_tv_asked_then_how_catt_ended";
            assert!(crate::in_child(name, &env));
            let args = fs::read_to_string(dir.join("bin/args")).unwrap();
            assert!(
                args.contains("--subtitles\n") && args.contains("Show Episode 1.vtt"),
                "{args}"
            );
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let mut m = Media {
            id: 1,
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        let tvs = std::sync::Mutex::new(Vec::new());
        let asked = |tv| tvs.lock().unwrap().push(tv);
        assert_eq!(
            cast(&m, "1", "sub", &|| true, &asked),
            Err(NOT_DOWNLOADED.into())
        );
        let folder = ani::folder(&m);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("Show Episode 1.mp4"), "").unwrap();
        fs::write(folder.join("Show Episode 1.vtt"), "").unwrap();
        assert_eq!(
            cast(&m, "1", "sub", &|| false, &asked),
            Err(String::new()),
            "not wanted anymore"
        );
        assert!(tvs.lock().unwrap().is_empty());
        let res = cast(&m, "1", "sub", &|| true, &asked);
        assert_eq!(*tvs.lock().unwrap(), ["Wohnzimmer"]);
        assert_eq!(res, Err("Error: Playback of local file has failed".into()));
    }

    #[test]
    fn casting_says_the_tv_catt_plays_on() {
        let said = r#"Playing "Frieren "on" TV Episode 1" on "Wohnzimmer"..."#;
        assert_eq!(casting_to(said).as_deref(), Some("Wohnzimmer"));
        assert_eq!(casting_to("Casting local file /a.mp4..."), None);
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
    }

    #[test]
    fn trailer_errors_say_what_failed_in_plain_words() {
        let yt = "https://www.youtube.com/watch?v=00000000000";
        let err = |lines: &[&str], stderr: &[u8]| {
            let out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            trailer_error(yt, &out, stderr)
        };
        assert_eq!(
            err(
                &[
                    "[ytdl_hook] ERROR: [youtube] 00000000000: This video is unavailable",
                    "[ytdl_hook] youtube-dl failed: unexpected error occurred",
                ],
                b""
            ),
            "This video is unavailable"
        );
        assert_eq!(
            err(
                &[
                    "[ytdl_hook] ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies-from-browser or --cookies for the authentication. See https://github.com/yt-dlp/yt-dlp/wiki/FAQ",
                ],
                b""
            ),
            "Sign in to confirm you're not a bot"
        );
        assert_eq!(
            err(
                &[
                    "[ytdl_hook] ERROR: [youtube] 00000000000: Unable to download webpage: Failed to resolve 'www.youtube.com' ([Errno -3] Temporary failure in name resolution)",
                ],
                b""
            ),
            "can't reach youtube.com"
        );
        assert_eq!(
            err(
                &[
                    "[ytdl_hook] ERROR: [youtube] 00000000000: Unable to download API page: ('Unable to connect to proxy', ConnectTimeoutError('Connection to www.youtube.com timed out.'))",
                ],
                b""
            ),
            "can't reach youtube.com"
        );
        assert_eq!(
            err(
                &["[ytdl_hook] youtube-dl failed: not found or not enough permissions"],
                b""
            ),
            "yt-dlp not found"
        );
        assert_eq!(err(&[], b""), "mpv couldn't play it");
    }

    #[test]
    fn a_stalled_trailer_stops_at_its_limit_and_a_playing_one_does_not() {
        let _turn = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = Media::default();
        m.title.romaji = Some("Show".into());
        let url = "https://www.youtube.com/watch?v=4MYo8FfiXMA";
        let cmd = trailer_command(&m, url);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        for flag in [
            "--resume-playback=no",
            "--save-position-on-quit=no",
            "--quiet=no",
            "--really-quiet=no",
            "--terminal=yes",
            "--msg-level=cplayer=info,ytdl_hook=error",
        ] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}: {args:?}");
        }
        assert!(args.ends_with(&["--".into(), url.into()]));

        // never opens a window: stopped at the limit
        let mut hung = Command::new("sh");
        hung.args(["-c", "sleep 5"]);
        let since = std::time::Instant::now();
        let mut shown = false;
        let res = run_trailer(hung, url, Duration::from_millis(150), || shown = true);
        assert_eq!(res, Err("can't reach youtube.com".into()));
        assert!(!shown && since.elapsed() < Duration::from_secs(3));

        // once VO: arrives, the watchdog disarms and it plays past the limit
        let mut playing = Command::new("sh");
        playing.args(["-c", "echo 'VO: [gpu] 1920x1080'; sleep 0.3"]);
        let mut shown = false;
        let res = run_trailer(playing, url, Duration::from_millis(100), || shown = true);
        assert!(res.is_ok() && shown);
    }
}
