#![allow(clippy::large_enum_variant)] // a handful of screens, size is irrelevant

mod ani;
mod anilist;
mod downloads;
mod entries;
mod kitty;
mod lists;
mod menu;
mod playback;
mod preload;
mod preview;
mod progress;
mod related;
mod show_downloads;
mod skip;
mod source;
mod ui;

use anilist::{Client, EpInfo, Media};
use downloads::{AniCli, Job, Queue};
use entries::{Change, Entries, Stamp, Work};
use lists::{Feed, List};
use menu::{Act, Item, Menu};
use playback::{Playback, Start};
use preview::Preview;
use progress::{Mark, Parts, Progress};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use related::Series;
use show_downloads::{Action, ShowDownloads, which};
use source::{Focus, Lookup, Source, Sources};
use std::collections::HashMap;
use std::collections::hash_map::Entry::Vacant;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub const PIN_REDIRECT: &str = "https://anilist.co/api/v2/oauth/pin";
/// How long enter on the Continue row waits to hear which episode is next, before it plays what
/// it knows: AniList takes half a second for a list, and much longer when it's down or busy.
const CONTINUE_LIMIT: Duration = Duration::from_secs(3);
/// The row of "Watched up to…" that goes back to none: not an episode a provider could list.
const NO_EPISODES: &str = "none";
const LISTS: [(&str, &str); 6] = [
    ("CURRENT", "Watching"),
    ("PLANNING", "Planning"),
    ("COMPLETED", "Completed"),
    ("PAUSED", "Paused"),
    ("DROPPED", "Dropped"),
    ("REPEATING", "Rewatching"),
];

#[derive(Clone, Copy, PartialEq)]
pub enum EpMode {
    Play,
    /// every episode with its download state; enter does what that state allows
    Download,
    /// enter saves the AniList progress up to an episode, up or down
    Watched,
}

pub enum View {
    Main,
    /// first start or missing tools: package check + login suggestion
    Setup,
    /// switches and the AniList login
    Settings,
    Shows(List),
    /// audio picker for a show, or the default (None)
    Audio(Option<Media>),
    Show(Media),
    Episodes(Media, EpMode),
    Status(Media),
    Sources(Media, String, Vec<String>),
    Bulk(Vec<Media>),
    /// marked episodes in different download states: which of them to act on
    DlChoice(Media, Vec<String>),
    Confirm(Pending),
    Input(Input),
}

#[derive(Clone)]
pub enum Input {
    Search,
    ClientId,
    Token,
    SourceQuery(Media),
    DownloadDir,
}

pub enum Pending {
    Delete(Vec<PathBuf>),
    /// marking an episode watched, or none of them: what Yes saves, as the question says it,
    /// and the show and episode it asks about, if it's one
    Progress(Vec<Mark>, Option<(u64, String)>),
    /// taking a show off the list, which loses its progress and score
    Remove(Media),
    Quit,
    /// queued or downloading episodes to take out of the queue
    Stop(Media, Vec<String>),
    /// downloads, while ffmpeg has this episode of the show, which then starts over
    Pause(Media, String),
    /// unfinished downloads of ani-hops that are gone, and their queue files
    Resume(Vec<Job>, Vec<PathBuf>),
}

pub struct Screen {
    pub id: u64,
    pub title: String,
    pub view: View,
    pub menu: Menu,
    /// waits on a background result other than a list's (Pick source's search)
    waiting: bool,
}

impl Screen {
    /// Something it shows is on its way.
    pub fn loading(&self) -> bool {
        self.waiting || matches!(&self.view, View::Shows(list) if list.loading())
    }
}

enum Want {
    Episodes(EpMode),
    Play(String),
}

/// What AniList links a show to, as far as this session knows.
enum Related {
    /// nothing, or it hasn't said yet: a page offers nothing to go to
    Nothing,
    /// another show at least
    Links,
    /// its series is being walked
    Walking,
    /// the show's whole series, walked once for all of its shows
    Series(Arc<Series>),
}

enum Msg {
    /// a list's refreshed shows
    /// a list's shows; for a search, also what found them, as a typo was spelled again
    Shows(
        u64,
        Stamp,
        Result<Vec<Media>, String>,
        lists::Ask,
        Option<String>,
    ),
    /// the downloaded shows as AniList has them now
    Local(Stamp, Result<Vec<Media>, String>),
    /// Watching as AniList has it now, for the Continue row: whose, when asked, the answer
    Watching(u64, Stamp, Result<Vec<Media>, String>, lists::Ask),
    /// the next page of a ranking
    More(u64, Stamp, Result<Vec<Media>, String>),
    Hits(u64, Result<Vec<String>, String>),
    /// who's logged in, asked with this token
    Viewer(Option<String>, Result<(u64, String, bool), String>),
    EpInfo(u64, HashMap<String, EpInfo>),
    /// sequels of a show, looked up for its episode counts (App::parts_key)
    Parts(u64, (u32, u32), Stamp, Result<Parts, String>),
    /// whether AniList links a show to another
    Linked(u64, Result<bool, String>),
    /// a show's series as AniList has it
    Series(u64, Stamp, Result<Series, String>),
    /// a lookup's answer, with its ticket (Sources::ask)
    Resolved(Media, u64, Result<Source, String>),
    /// a change to the user's list, answered: a show's parts with it, the show first
    Entry(Vec<entries::Done>),
    /// the Continue row has waited long enough for this page
    ContinueLimit(u64),
    /// (Playback attempt, when it started, its show, what it waits on now; None = the player
    /// is up)
    PlayStep(u64, Stamp, Media, Option<&'static str>),
    /// (Playback attempt, when it started, show, episode, how it ended)
    Played(u64, Stamp, Media, String, Result<(), String>),
    /// what a rescan of the download folder found: the shows and their episodes
    Scanned(Vec<(Media, OnDisk)>),
    /// an episode downloaded or failed: ("Show E3", result)
    DlDone(String, Result<(), String>),
    /// (version, update command) of a newer ani-hop
    Release(Option<(String, String)>),
    /// (the ani-cli version it's about, and a newer one that's out)
    AniCliRelease(Option<String>, Option<String>),
}

const PAUSED: &str = "Paused downloads · enter on a queued episode resumes them";

pub struct App {
    pub stack: Vec<Screen>,
    next_id: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Client,
    pub user: Option<(u64, String)>,
    /// the show or episode in the right-hand pane: its poster, and how far it's read
    pub preview: Preview,
    /// the user's Entries, newest known, and the changes to them
    pub entries: Entries,
    /// episode titles and summaries per show (empty while loading or when there are none)
    pub ep_info: HashMap<u64, HashMap<String, EpInfo>>,
    /// AniList sequels per show that its provider entry runs on into, each with the episodes
    /// before it (none yet while AniList hasn't answered), and the episode counts they were
    /// looked up for
    parts: HashMap<u64, ((u32, u32), Option<Parts>)>,
    /// what AniList links each show opened this session to
    related: HashMap<u64, Related>,
    pub sources: Sources,
    /// downloaded episodes per show, as the last rescan found them
    pub local: HashMap<u64, OnDisk>,
    /// the downloaded shows, as the last rescan found them (none before the first)
    downloaded: Option<Vec<Media>>,
    /// a rescan of the download folder is on its way, and (true) another was asked for since
    scanning: Option<bool>,
    /// Downloads opened: AniList is asked about its shows once the rescan has them
    ask_local: bool,
    /// episodes per show other running ani-hops have queued, rescanned on refresh
    elsewhere: HashMap<u64, Vec<String>>,
    resolving: HashMap<u64, Option<Want>>,
    pub downloads: Queue,
    pub playback: Playback,
    pub toast: Option<(String, bool, Instant)>,
    pub started: Instant,
    /// a key came since the start
    pub pressed: bool,
    pub missing: Vec<(&'static str, &'static str, &'static str)>,
    /// the installed ani-cli's version
    pub ani_cli: Option<String>,
    /// (version, update command) of a newer ani-hop release
    pub release: Option<(String, String)>,
    /// the newer ani-cli release that's out
    pub ani_cli_newer: Option<String>,
    /// shell command to run in the real terminal (TUI paused), e.g. installing packages
    external: Option<String>,
    /// asked for on a show's page while ani-cli searched, waiting to get back to that page
    parked: Option<(Media, Want)>,
    /// an episode that was played, whose question waits for AniList to say which part it's in
    marking: Option<(Media, String)>,
    /// the show played last on this login, which the Continue row goes on with
    last_played: Option<Media>,
    /// Watching as last read, which it goes on with otherwise (none while logged out)
    watching: Option<List>,
    /// Watching's answer is on its way
    reading_watching: bool,
    /// the page of a show Continue opened, whose next episode plays once that's known
    continuing: Option<u64>,
    /// the show on screen, for work done ahead of time
    focus: Focus,
    /// the next episode's stream is fetched ahead (Settings)
    preloading: bool,
    /// whether background work runs: a test delivers the answers itself, in the order it's about
    background: bool,
    quit: bool,
}

/// What an App runs on: the real things, or stand-ins in tests.
struct Seams {
    client: Client,
    anilist: Arc<dyn entries::AniList>,
    player: Arc<dyn playback::Player>,
    /// where the download queues are, and this ani-hop's queue's name
    queue: (PathBuf, String),
    /// whether this terminal can draw posters
    posters: bool,
    background: bool,
}

fn main() -> std::io::Result<()> {
    // no options beyond these, everything else happens in the UI
    let usage =
        "Usage: ani-hop [--version | --help]\nStarts the terminal UI, everything happens in there.";
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("-V" | "--version") => {
            println!("ani-hop {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help") => {
            println!("{usage}");
            return Ok(());
        }
        // what ani-cli's stand-in for ani-skip runs, not one for people
        Some("--skip-flags") => {
            let arg = |n| std::env::args().nth(n).unwrap_or_default();
            println!("{}", skip::flags_for_ani_cli(&arg(2), &arg(3)));
            return Ok(());
        }
        Some(arg) => {
            eprintln!("ani-hop: unknown option {arg}\n{usage}");
            std::process::exit(2);
        }
    }
    if let Err(e) = ani::setup() {
        eprintln!(
            "ani-hop: can't write to {}: {e}",
            ani::tilde(&ani::cache_dir())
        );
        std::process::exit(1);
    }
    kitty::init();
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty graphics only (Kitty and Ghostty); the query is just for font size and o=z support.

    let mut app = App::new();
    // every way out stops the download, panics too: a left-behind ani-cli would finish the
    // episode with ffmpeg, as a file that looks complete. And the player, as nothing would be
    // left to ask about saving the episode
    struct StopAll(Queue);
    impl Drop for StopAll {
        fn drop(&mut self) {
            self.0.quit();
            ani::close_players();
        }
    }
    let _stop = StopAll(app.downloads.clone());
    let res = (|| -> std::io::Result<()> {
        let mut dirty = true;
        while !app.quit {
            let (fx, glint) = (ui::effects_running(&app), app.preview.glint());
            if dirty || app.animating() {
                terminal.draw(|f| ui::render(&mut app, f))?;
            }
            // one more frame after an effect ends (or starts in a slow frame), so it never
            // freezes mid-sweep
            dirty = fx || app.preview.glint() != glint;
            // ~60 fps while an effect plays, otherwise just often enough for spinners
            let fps = if ui::effects_running(&app) { 16 } else { 80 };
            let mut wait = Duration::from_millis(fps);
            while event::poll(wait)? {
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Paste(s) => app.top().menu.paste(&s),
                    _ => {}
                }
                (wait, dirty) = (Duration::ZERO, true);
            }
            while let Ok(msg) = app.rx.try_recv() {
                app.on_msg(msg);
                dirty = true;
            }
            app.focus.on_screen(app.show_on_screen());
            let (shown, off) = app.preview.tick(Instant::now());
            if off {
                app.toast(
                    "This terminal didn't take the posters, showing colors instead",
                    true,
                );
            }
            dirty |= shown || off;
            if let Some(cmd) = app.external.take() {
                execute!(std::io::stdout(), DisableBracketedPaste)?;
                ratatui::restore();
                run_in_terminal(&cmd);
                terminal = ratatui::init();
                execute!(std::io::stdout(), EnableBracketedPaste)?;
                terminal.clear()?;
                app.after_install();
            }
        }
        Ok(())
    })();
    app.preview.forget();
    kitty::shutdown();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
}

/// The newest release's version of GitHub's `repo`, if it's newer than `installed`. An answer
/// is kept a day in `file`; without one the next start asks again. Through the redirect of
/// /releases/latest (no API, so no rate limit).
fn newer_release(repo: &str, file: &str, installed: &str) -> Option<String> {
    let seen = ani::cache_dir().join(file);
    let day = Duration::from_secs(24 * 60 * 60);
    let fresh = fs::metadata(&seen)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < day));
    let tag = match fresh {
        true => fs::read_to_string(&seen).ok()?,
        false => {
            let to = anilist::location(&format!("https://github.com/{repo}/releases/latest"));
            // .../releases/tag/v0.6.0; before the first release just .../releases
            let tag = to.ok()?.rsplit('/').next()?.to_string();
            let _ = fs::write(&seen, &tag);
            tag
        }
    };
    let v = tag.trim().trim_start_matches('v');
    ani::older(installed, v).then(|| v.to_string())
}

/// Runs `cmd` in the plain terminal (sudo prompts work) and waits for enter.
fn run_in_terminal(cmd: &str) {
    use std::io::Write;
    println!("\n\x1b[1;35m❯\x1b[0m {cmd}\n");
    let _ = Command::new("sh").args(["-c", cmd]).status();
    print!("\nPress enter to return to ani-hop ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn token_path() -> PathBuf {
    ani::config_dir().join("token")
}

/// Last known (id, name, english titles) of the viewer, so lists show from the first frame.
fn user_path() -> PathBuf {
    ani::config_dir().join("user.json")
}

/// The show a login played last. A cache: without it the Continue row goes by Watching.
fn last_played_path(user: u64) -> PathBuf {
    lists::cache(&format!("{user}-last-played"))
}

fn client_id() -> Option<String> {
    fs::read_to_string(ani::config_dir().join("client_id"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Readable only by the user, and written aside then renamed: a crash mid-write keeps the
/// login that was there.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    fs::create_dir_all(ani::config_dir())?;
    use std::io::Write;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let res = (fs::OpenOptions::new())
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Accepts the bare token or the whole redirect URL with `#access_token=...`.
fn parse_token(input: &str) -> String {
    let s = input.trim();
    let s = s.split_once("access_token=").map_or(s, |(_, t)| t);
    s.split('&').next().unwrap_or("").to_string()
}

fn open_url(url: &str) {
    let open = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(open)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Episode titles and summaries from the disk cache, or ani.zip once it is a day old (a
/// month for shows that are out completely).
fn load_episodes(m: &Media) -> HashMap<String, EpInfo> {
    let path = ani::cache_dir()
        .join("episodes")
        .join(format!("{}.json", m.id));
    let days = if m.status.as_deref() == Some("FINISHED") {
        30
    } else {
        1
    };
    let age = fs::metadata(&path).and_then(|md| md.modified()).ok();
    let age = age.and_then(|t| t.elapsed().ok()).unwrap_or(Duration::MAX);
    let cached: Option<HashMap<String, EpInfo>> = ani::read_json(&path);
    match cached {
        // one from before air dates or seasons fetches again, but still stands in when that fails
        Some(eps)
            if age.as_secs() < days * 86_400
                && eps
                    .values()
                    .all(|e| e.aired.is_some() && e.season.is_some()) =>
        {
            eps
        }
        cached => match anilist::episodes(m.id) {
            Ok(eps) => {
                let _ = ani::write_json(&path, &eps);
                eps
            }
            Err(_) => cached.unwrap_or_default(),
        },
    }
}

/// Each episode's TheTVDB season, for episodes in order. One without (0: a recap, a special,
/// one not sorted in yet) is in the season before it, or at the start the one after.
fn seasons(each: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut last = 0;
    let mut all: Vec<u32> = (each.into_iter())
        .map(|s| {
            last = if s > 0 { s } else { last };
            last
        })
        .collect();
    let first = all.iter().copied().find(|&s| s > 0).unwrap_or(0);
    all.iter_mut()
        .take_while(|s| **s == 0)
        .for_each(|s| *s = first);
    all
}

impl App {
    /// An App with nothing on its screen yet, running on `seams`.
    fn with(seams: Seams, fetch: impl downloads::Fetch) -> Self {
        let (tx, rx) = mpsc::channel();
        let done = tx.clone();
        let downloads = Queue::start(seams.queue.0, seams.queue.1, fetch, move |label, res| {
            _ = done.send(Msg::DlDone(label, res))
        });
        App {
            stack: Vec::new(),
            next_id: 0,
            downloads,
            tx,
            rx,
            client: seams.client,
            user: None,
            preview: Preview::new(kitty::Posters::new(kitty::Tty, seams.posters)),
            entries: Entries::new(seams.anilist),
            ep_info: HashMap::new(),
            parts: HashMap::new(),
            related: HashMap::new(),
            sources: Sources::load(),
            local: HashMap::new(),
            downloaded: None,
            scanning: None,
            ask_local: false,
            elsewhere: HashMap::new(),
            resolving: HashMap::new(),
            playback: Playback::new(seams.player),
            toast: None,
            started: Instant::now(),
            pressed: false,
            missing: Vec::new(),
            ani_cli: None,
            release: None,
            ani_cli_newer: None,
            external: None,
            parked: None,
            marking: None,
            last_played: None,
            watching: None,
            reading_watching: false,
            continuing: None,
            focus: Focus::default(),
            preloading: false,
            background: seams.background,
            quit: false,
        }
    }

    fn new() -> Self {
        let token = fs::read_to_string(token_path())
            .ok()
            .map(|t| t.trim().to_string());
        let user: Option<(u64, String, bool)> = ani::read_json(&user_path());
        let user = user.filter(|_| token.is_some());
        if let Some((_, _, english)) = &user {
            anilist::ENGLISH_TITLES.store(*english, Ordering::Relaxed);
        }
        let client = Client { token };
        let seams = Seams {
            anilist: Arc::new(client.clone()),
            client,
            player: Arc::new(playback::Mpv),
            queue: (
                ani::cache_dir().join("queue"),
                std::process::id().to_string(),
            ),
            posters: kitty::supported(),
            background: true,
        };
        let mut app = App::with(seams, AniCli);
        app.user = user.map(|(id, name, _)| (id, name));
        app.missing = ani::missing_deps();
        app.ani_cli = ani::ani_cli_version();
        app.preloading = ani::preload_on();
        app.read_continue();
        app.push("ani-hop", View::Main);
        let welcomed = ani::config_dir().join("welcomed");
        let first = !welcomed.exists();
        if first || !app.missing.is_empty() {
            let _ = fs::create_dir_all(ani::config_dir());
            let _ = fs::write(&welcomed, "");
            app.push(if first { "Welcome" } else { "Setup" }, View::Setup);
        }
        // on top, so it's answered before anything can log in and reset the screens
        match app.downloads.leftovers() {
            (jobs, files) if jobs.is_empty() => files.iter().for_each(|f| _ = fs::remove_file(f)),
            (jobs, files) => {
                app.push(
                    "Resume the downloads that didn't finish last time?",
                    View::Confirm(Pending::Resume(jobs, files)),
                );
                app.top().menu.select_key("yes");
            }
        }
        if let Some(file) = app.downloads.unreadable().first() {
            let file = ani::tilde(file);
            app.toast(format!("Can't read a download queue · kept {file}"), true);
        }
        if app.client.token.is_some() {
            let c = app.client.clone();
            app.spawn(move || Msg::Viewer(c.token.clone(), c.viewer()));
        }
        app.spawn(|| {
            let new = newer_release(
                "ReynkeDeVos/ani-hop",
                "latest-release",
                env!("CARGO_PKG_VERSION"),
            );
            Msg::Release(new.map(|v| (v, ani::update_command())))
        });
        app.check_ani_cli();
        // so the first episode doesn't wait for it. Not without mpv: its answer lasts the run, and
        // the setup screen may still install mpv
        if ani::have("mpv") {
            app.run(|| _ = ani::hwdec_unset());
        }
        app
    }

    /// Asks whether ani-cli has an update. Its tags, not its master: that's what packages ship,
    /// so a newer master would ask for an update they can't give.
    fn check_ani_cli(&self) {
        let installed = self.ani_cli.clone();
        self.spawn(move || {
            let newer = (installed.as_deref())
                .and_then(|v| newer_release("pystardust/ani-cli", "latest-ani-cli", v));
            Msg::AniCliRelease(installed, newer)
        });
    }

    fn after_install(&mut self) {
        self.missing = ani::missing_deps();
        self.ani_cli = ani::ani_cli_version();
        // that was about the ani-cli before this, and GitHub can take a while
        self.ani_cli_newer = None;
        self.check_ani_cli();
        let names: Vec<&str> = self.missing.iter().map(|d| d.0).collect();
        match names.is_empty() {
            false => self.toast(format!("Still missing: {}", names.join(", ")), true),
            true => self.toast("All set: everything ani-hop needs is installed", false),
        }
        self.refresh();
    }

    /// Logs out, once what's saved of the login is gone: a token left behind would log back in
    /// at the next start. What couldn't go, if anything.
    fn log_out(&mut self) -> Result<(), PathBuf> {
        let gone = |path: PathBuf| match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(path),
            _ => Ok(()),
        };
        gone(token_path())?;
        gone(user_path())?;
        self.forget_login();
        Ok(())
    }

    fn forget_login(&mut self) {
        (self.client.token, self.user) = (None, None);
        self.entries.login(Arc::new(self.client.clone()));
        // they carry this login's progress
        self.parts.clear();
        self.related.clear();
        self.marking = None;
        self.forget_continue();
    }

    /// Nothing of the last login's for the Continue row: the next login's comes from its files.
    fn forget_continue(&mut self) {
        (self.last_played, self.watching, self.continuing) = (None, None, None);
        self.reading_watching = false;
    }

    /// Reads what the Continue row goes on with for this login, as of its last run, so the row
    /// is there from the first frame, and asks AniList how things stand now: also right after
    /// a read, which may have been a list's, whose answer the show played last doesn't have.
    fn read_continue(&mut self) {
        let Some(user) = self.user.as_ref().map(|u| u.0) else {
            return;
        };
        self.last_played = ani::read_json(&last_played_path(user));
        let cache = lists::cache(&format!("{user}-CURRENT"));
        let (list, _) = List::open(Feed::Whole, Some(cache), std::time::SystemTime::now());
        self.watching = Some(list);
        self.reading_watching = true;
        let (c, stamp, ask) = (self.client.clone(), self.entries.stamp(), lists::Ask::now());
        self.spawn(move || Msg::Watching(user, stamp, c.list(user, "CURRENT"), ask));
        // it may go on into one, which is how it still has an episode to go on to
        if let Some(m) = self.last_played.clone() {
            self.want_parts(&m);
        }
    }

    /// `m` played: the Continue row goes on with it from now on, also at the next start. Only
    /// a show AniList keeps progress of: others would say episode 1 whatever was watched.
    fn played(&mut self, mut m: Media) {
        let Some(user) = self.user.as_ref().filter(|_| m.on_anilist()).map(|u| u.0) else {
            return;
        };
        m.media_list_entry = self.entries.entry(&m).cloned();
        // without it the next start goes by Watching
        let _ = ani::edit_json(&last_played_path(user), |saved| *saved = Some(m.clone()));
        self.last_played = Some(m);
        self.rebuild();
    }

    /// Changes this login's file of the show played last, while that's still `id`: what another
    /// ani-hop played since is what counts at the next start then.
    fn edit_last_played(&self, id: u64, to: Option<&Media>) {
        if let Some(user) = self.user.as_ref().map(|u| u.0) {
            let _ = ani::edit_json(&last_played_path(user), |saved: &mut Option<Media>| {
                if saved.as_ref().is_some_and(|m| m.id == id) {
                    *saved = to.cloned();
                }
            });
        }
    }

    /// Newer data of the show played last, which doesn't make it played again.
    fn update_last_played(&mut self, mut m: Media) {
        m.media_list_entry = self.entries.entry(&m).cloned();
        self.edit_last_played(m.id, Some(&m));
        self.last_played = Some(m);
    }

    /// The show played last isn't one to go on with anymore.
    fn forget_last_played(&mut self) {
        if let Some(m) = self.last_played.take() {
            self.edit_last_played(m.id, None);
        }
    }

    /// Watching's answer, asked at `stamp`, doesn't have `m`, which was on it as far as anyone
    /// knew: another device moved it on, to where is for AniList to say.
    fn left_watching(&self, m: &Media, stamp: Stamp) -> bool {
        let shows = self.watching.iter().flat_map(|l| &l.shows);
        !(shows.into_iter()).any(|s| s.id == m.id)
            && !self.entries.newer_than(m, stamp)
            && self.entries.entry(m).is_some_and(|e| e.status == "CURRENT")
    }

    /// What the Continue row goes on with, and the episode: the show played last unless it was
    /// put aside, else the first on Watching, as long as it has an episode out to go on to.
    fn to_continue(&self) -> Option<(&Media, u32)> {
        let status = |m: &Media| self.entries.entry(m).map(|e| e.status.clone());
        let last = (self.last_played.iter())
            .filter(|m| !matches!(status(m).as_deref(), Some("PAUSED" | "DROPPED")));
        let watching = (self.watching.iter().flat_map(|l| &l.shows))
            .filter(|m| status(m).as_deref() == Some("CURRENT"));
        (last.chain(watching)).find_map(|m| self.progress(m).next_out().map(|ep| (m, ep)))
    }

    /// The show on the Continue row with this `key`, whatever would go there by now.
    pub fn on_continue_row(&self, key: &str) -> Option<&Media> {
        let id = key.strip_prefix("continue:")?;
        let watching = self.watching.iter().flat_map(|l| &l.shows);
        (self.last_played.iter().chain(watching)).find(|m| m.id.to_string() == id)
    }

    /// Enter on the Continue row: `m`'s page, and its next episode as enter there plays it.
    fn go_on_with(&mut self, m: Media) {
        self.open_show(m);
        self.continuing = Some(self.top().id);
        self.continue_play();
        self.limit_continue();
    }

    /// Ends the Continue row's wait at CONTINUE_LIMIT, if it's waiting.
    fn limit_continue(&self) {
        if let Some(id) = self.continuing {
            self.spawn(move || {
                thread::sleep(CONTINUE_LIMIT);
                Msg::ContinueLimit(id)
            });
        }
    }

    /// The page the Continue row is waiting on. Not if a key came meanwhile, or something's on
    /// top of it: what's on screen is theirs then.
    fn continuing_page(&mut self) -> Option<Media> {
        let id = self.continuing?;
        let top = self.stack.last().unwrap();
        match &top.view {
            View::Show(m) if top.id == id => Some(m.clone()),
            _ => {
                self.continuing = None;
                None
            }
        }
    }

    /// Plays what the Continue row asked for, once it's known which episode that is.
    fn continue_play(&mut self) {
        let Some(m) = self.continuing_page() else {
            return;
        };
        // what's next may change with these answers
        if !self.reading_watching
            && !matches!(self.parts.get(&m.id), Some((_, None)))
            && !self.saving_progress(&m)
        {
            self.continue_now(m);
        }
    }

    /// Plays what the Continue row asked for, as enter on `m`'s page would by now.
    fn continue_now(&mut self, m: Media) {
        self.continuing = None;
        // Playback only stops a second player while the first is starting
        if let Some(p) = self.playback.now() {
            return self.toast(format!("Already playing {}", p.title), false);
        }
        match self.progress(&m).next_out() {
            Some(_) => self.show_action(m, "next"),
            None => self.toast(format!("You're caught up on {}", m.name()), false),
        }
    }

    /// A save of `m`'s or its parts' progress is on its way: until AniList has answered, its
    /// next episode is still the one just marked.
    fn saving_progress(&self, m: &Media) -> bool {
        self.entries.saving(m) || (self.parts(m).iter()).any(|(_, p)| self.entries.saving(p))
    }

    /// Back to the main menu; an unanswered resume question stays.
    fn back_to_main(&mut self) {
        self.stack
            .retain(|s| matches!(s.view, View::Main | View::Confirm(Pending::Resume(..))));
    }

    pub fn top(&mut self) -> &mut Screen {
        self.stack.last_mut().unwrap()
    }

    /// A download of `m`'s `eps` (None: every episode not watched), with the newest Entries
    /// known: they pick the episodes not watched, and a first download's Show folder starts
    /// with them.
    fn job(&self, mut m: Media, eps: Option<Vec<String>>) -> Job {
        self.entries.remember(&m);
        m.media_list_entry = self.entries.entry(&m).cloned();
        let mut parts = self.parts(&m).to_vec();
        for (_, p) in &mut parts {
            p.media_list_entry = self.entries.entry(p).cloned();
        }
        let lookup = self.sources.lookup(m.id);
        let mut job = Job::new(m, eps);
        (job.parts, job.lookup) = (parts, lookup);
        job
    }

    /// Queues a download, saved first so quitting early doesn't lose it; false (and says
    /// why) when that can't be saved.
    fn download(&mut self, job: Job) -> bool {
        let res = self.downloads.enqueue(job);
        if let Err(e) = &res {
            self.toast(format!("Not queued · {e}"), true);
        }
        res.is_ok()
    }

    /// Takes episodes of `m` out of the queue. The one downloading stops, and the worker
    /// deletes what it got of it.
    fn stop(&mut self, m: &Media, eps: &[String]) {
        let on_disk = self.local.get(&m.id).cloned().unwrap_or_default();
        let src = self.sources.get(m.id);
        if let Err(e) = self.downloads.stop(m, eps, src, &on_disk) {
            self.toast(e, true);
        }
    }

    /// Enter on episodes in Downloads: what their state allows (download, stop, delete).
    /// Marked ones in different states, and the one downloading (pause or stop), ask which
    /// first; `chosen` is that answer.
    fn dl_act(&mut self, m: Media, eps: Vec<String>, chosen: Option<&str>) {
        let dl = self.show_downloads(&m);
        let action = dl.action(&eps, chosen);
        // asks when that deletes what they got, so no stray enter or → can
        let stop_asks = matches!(&action, Action::Stop(stop) if !dl.deletes(stop).is_empty());
        match &action {
            Action::Nothing(why) if !why.is_empty() => return self.toast(*why, false),
            Action::Choose => {
                let title = match eps.as_slice() {
                    [one] => format!("Episode {one}"),
                    _ => plural(eps.len(), "episode"),
                };
                self.push(title, View::DlChoice(m, eps));
                return;
            }
            Action::Delete(files) => {
                let names: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
                let paths = files.iter().map(|f| f.1.clone()).collect();
                let title = format!("Delete {}?", which(&names));
                self.push(title, View::Confirm(Pending::Delete(paths)));
                return;
            }
            // ffmpeg can't go on from what it got, so pausing loses that, and asks like stopping
            Action::Pause(ep, Some(which)) => {
                let title = format!("Pause downloads? {which} starts over when resumed");
                self.push(title, View::Confirm(Pending::Pause(m, ep.clone())));
                return;
            }
            Action::Stop(stop) if stop_asks => {
                let title = format!("Stop downloading {}?", which(stop));
                self.push(title, View::Confirm(Pending::Stop(m, stop.clone())));
                return;
            }
            _ => {}
        }
        if chosen.is_some() {
            self.pop(1); // back on the list, which shows what changed
        }
        match action {
            Action::Download(get) => {
                // paused with something held, not just a pause left over from an emptied queue
                let s = self.downloads.snapshot();
                let going = match s.paused && s.pending > 0 {
                    true => " · resuming downloads",
                    false => "",
                };
                let queued = format!("Queued {}{going}", which(&get));
                if self.download(self.job(m, Some(get))) {
                    self.toast(queued, false);
                }
            }
            Action::Stop(stop) => {
                self.toast(format!("Stopped downloading {}", which(&stop)), false);
                self.stop(&m, &stop);
            }
            // the one it was offered for; one that started since may be ffmpeg's, whose pause
            // asks first
            Action::Pause(ep, None) => {
                if self.downloads.pause(m.id, &ep) {
                    self.toast(PAUSED, false);
                }
            }
            Action::Resume => {
                self.downloads.resume();
                self.toast("Resuming downloads", false);
            }
            _ => {}
        }
        self.top().menu.marked.clear();
        self.refresh();
    }

    /// What enter does on the screen on top, for the footer: none when nothing.
    pub fn enter_verb(&self) -> Option<&'static str> {
        let screen = self.stack.last().unwrap();
        let m = match &screen.view {
            View::Input(Input::Search | Input::SourceQuery(_)) => return Some("search"),
            View::Input(_) => return Some("save"),
            // as accept: no row under the cursor and none marked
            _ if screen.menu.selection().is_empty() => return None,
            View::Episodes(m, EpMode::Download) => m,
            View::Episodes(_, EpMode::Play) => return Some("play"),
            View::Show(_) if screen.menu.current().is_some_and(|i| i.key == "next") => {
                return Some("play");
            }
            View::Main
                if (screen.menu.current()).is_some_and(|i| i.key.starts_with("continue:")) =>
            {
                return Some("play");
            }
            _ => return Some("select"),
        };
        let eps: Vec<String> = (screen.menu.selection().iter())
            .map(|&i| screen.menu.items[i].key.clone())
            .collect();
        self.show_downloads(m).verb(&eps)
    }

    /// Runs `f` in the background.
    fn run(&self, f: impl FnOnce() + Send + 'static) {
        if self.background {
            thread::spawn(f);
        }
    }

    /// Runs `f` in the background; its answer comes to `on_msg`.
    fn spawn(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        self.run(move || _ = tx.send(f()));
    }

    /// The show the screens on top are about, for work done ahead of time.
    fn show_on_screen(&self) -> u64 {
        (self.stack.iter().rev())
            .find_map(|s| match &s.view {
                View::Show(m) | View::Episodes(m, _) => Some(m.id),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn toast(&mut self, text: impl Into<String>, err: bool) {
        // what went wrong stays until a key takes it away, so this didn't come from a key
        if !err && self.toast.as_ref().is_some_and(|t| t.1) {
            return;
        }
        self.toast = Some((text.into(), err, Instant::now()));
    }

    fn push(&mut self, title: impl Into<String>, view: View) -> u64 {
        // episode titles load while the show is open, so its episode list has them right away
        if let View::Show(m) | View::Episodes(m, _) = &view {
            self.want_episodes(m);
        }
        let multi = matches!(view, View::Shows(..) | View::Episodes(_, EpMode::Download));
        self.scan_local();
        let menu = Menu::new(self.items(&view), multi);
        self.next_id += 1;
        self.stack.push(Screen {
            id: self.next_id,
            title: title.into(),
            view,
            menu,
            waiting: false,
        });
        self.next_id
    }

    /// Pushes a list screen, instantly from the disk cache if there is one, and refreshes
    /// it from AniList in the background.
    fn push_loading(
        &mut self,
        title: &str,
        feed: Feed,
        cache: Option<String>,
        f: impl FnOnce() -> Result<Vec<Media>, String> + Send + 'static,
    ) {
        let cache = cache.map(|k| lists::cache(&k));
        let (list, ask) = List::open(feed, cache, std::time::SystemTime::now());
        let id = self.push(title, View::Shows(list));
        if let Some(ask) = ask {
            let stamp = self.entries.stamp();
            self.spawn(move || Msg::Shows(id, stamp, f(), ask, None));
        }
    }

    fn pop(&mut self, n: usize) {
        self.stack
            .truncate(self.stack.len().saturating_sub(n).max(1));
        if let Some((m, w)) = self.parked.take() {
            let page = |s: &Screen| matches!(&s.view, View::Show(x) if x.id == m.id);
            match (
                page(self.stack.last().unwrap()),
                self.stack.iter().any(page),
            ) {
                (true, _) => self.fulfill(m, w),
                (false, true) => self.parked = Some((m, w)),
                _ => {} // the show was left
            }
        }
        self.refresh();
        self.preload_next();
    }

    /// Rescans what's downloaded and rebuilds the visible menu (keeps query and cursor).
    fn refresh(&mut self) {
        self.scan_local();
        self.rebuild();
    }

    /// Rebuilds the visible menu from current state (keeps query and cursor).
    fn rebuild(&mut self) {
        let downloaded = self.downloaded.clone();
        if let View::Shows(list) = &mut self.top().view
            && list.is_local()
            && let Some(shows) = downloaded
        {
            list.scanned(shows);
        }
        let items = self.items(&self.stack.last().unwrap().view);
        // a chooser whose action under the cursor went (the episode finished, say), or a pause
        // question about an episode no longer downloading, is stale: back to the list, which
        // shows what changed, rather than another action there
        let top = self.stack.last().unwrap();
        let stale = match &top.view {
            View::DlChoice(..) => {
                (top.menu.current()).is_some_and(|c| !items.iter().any(|i| i.key == c.key))
            }
            View::Confirm(Pending::Pause(m, ep)) => !self.show_downloads(m).downloading(ep),
            _ => false,
        };
        if stale {
            return self.pop(1);
        }
        self.top().menu.set_items(items);
    }

    /// Rescans what's downloaded in the background, as the download folder may be on a slow
    /// disk (a NAS, a USB disk asleep), which screens must never wait for. Other ani-hops'
    /// queues are in the cache, read at once.
    fn scan_local(&mut self) {
        self.elsewhere = self.downloads.elsewhere();
        match &mut self.scanning {
            Some(again) => *again = true,
            None => {
                self.scanning = Some(false);
                self.spawn(|| {
                    let shows = ani::local_shows().into_iter();
                    Msg::Scanned(shows.map(|(m, p)| (m, ani::local_episodes(&p))).collect())
                });
            }
        }
    }

    /// Where `m`'s episodes stand with downloads, as of the last rescan and the queue now.
    pub fn show_downloads<'a>(&self, m: &'a Media) -> ShowDownloads<'a> {
        let on_disk = self.local.get(&m.id).cloned().unwrap_or_default();
        let queued = (self.downloads).queued(m, self.sources.get(m.id), &on_disk);
        let elsewhere = self.elsewhere.get(&m.id).cloned().unwrap_or_default();
        let queue = self.downloads.snapshot();
        ShowDownloads::of(m, on_disk, queued, elsewhere, queue)
    }

    /// Something on screen changes without input (spinner, progress, fading toast).
    fn animating(&self) -> bool {
        ui::effects_running(self)
            || self.stack.last().unwrap().loading()
            || self.downloads.snapshot().pending > 0 && !self.downloads.snapshot().paused
            || self
                .toast
                .as_ref()
                .is_some_and(|t| t.2.elapsed().as_secs() < 5)
    }

    /// Episode titles and summaries, once a session per show, and the sequels the provider
    /// runs on into once it lists more episodes than AniList.
    fn want_episodes(&mut self, m: &Media) {
        // ani.zip and AniList know shows by AniList id only
        if !m.on_anilist() {
            return;
        }
        if let Vacant(e) = self.ep_info.entry(m.id) {
            e.insert(HashMap::new());
            let m = m.clone();
            self.spawn(move || Msg::EpInfo(m.id, load_episodes(&m)));
        }
        self.want_parts(m);
    }

    /// Looks up the sequels the provider runs on into when it lists more episodes than
    /// AniList, again once it has another, which may change what adds up.
    fn want_parts(&mut self, m: &Media) {
        let Some(key) = self.parts_key(m) else {
            self.parts.remove(&m.id);
            return;
        };
        if self.parts.get(&m.id).is_none_or(|p| p.0 != key) {
            self.parts.insert(m.id, (key, None));
            let (m, c, stamp) = (m.clone(), self.client.clone(), self.entries.stamp());
            self.spawn(move || {
                let parts = c.parts(&m, key.0);
                Msg::Parts(m.id, key, stamp, parts)
            });
        }
    }

    /// The provider's last whole episode and AniList's count, when the provider has more.
    fn parts_key(&self, m: &Media) -> Option<(u32, u32)> {
        let src = self.sources.get(m.id)?;
        let last = src.episodes.iter().filter_map(|e| e.parse().ok()).max()?;
        m.episodes.filter(|own| last > *own).map(|own| (last, own))
    }

    /// The show's sequels, while it has the episode counts they were looked up for.
    pub fn parts(&self, m: &Media) -> &[(u32, Media)] {
        match self.parts.get(&m.id) {
            Some((key, Some(parts))) if Some(*key) == self.parts_key(m) => parts,
            _ => &[],
        }
    }

    /// Whether `parts` is all of `m`'s: there are none to look for, or AniList has answered.
    fn parts_known(&self, m: &Media) -> bool {
        let answered = |key| matches!(self.parts.get(&m.id), Some((k, Some(_))) if *k == key);
        self.parts_key(m).is_none_or(answered)
    }

    /// `m`'s episodes as AniList counts them, with the parts known so far.
    pub fn progress<'a>(&'a self, m: &'a Media) -> Progress<'a> {
        self.entries.progress(m, self.parts(m))
    }

    /// AniList says none of `m` is out, and ani-cli has no source for it, known, picked by
    /// hand or being looked up: a search could only fail, or find another show.
    pub fn upcoming(&self, m: &Media) -> bool {
        m.released() == Some(0)
            && self.sources.get(m.id).is_none()
            && !self.sources.picked(m.id)
            && !self.resolving.contains_key(&m.id)
    }

    /// What episode `ep` of `m` is called and about.
    pub fn ep_about(&self, m: &Media, ep: &str) -> Option<&EpInfo> {
        let (p, n) = self.progress(m).entry(ep);
        self.ep_info.get(&p.id)?.get(&n)
    }

    fn items(&self, view: &View) -> Vec<Item> {
        match view {
            View::Main => {
                let grouped = |mut it: Item, group: &str| {
                    it.group = group.to_string();
                    it
                };
                // found by "my list" too
                let list = |group: &'static str| {
                    move |(s, l): &(&str, &str)| {
                        let mut it = Item::new(format!("list:{s}"), *l, "");
                        it.about = "my list".into();
                        grouped(it, group)
                    }
                };
                let mut v = Vec::new();
                if let Some((m, ep)) = self.to_continue() {
                    let key = format!("continue:{}", m.id);
                    let hint = format!("continue episode {ep}");
                    let mut it = Item::new(key, format!("▶  {}", m.name()), hint);
                    // all a row has room for beside a title at 80 columns, so no word in the label
                    it.short = format!("ep {ep}");
                    it.about = "continue".into();
                    // a rule of its own under it: it's what this menu is for
                    it.group = "continue".into();
                    v.push(it);
                }
                // what you're watching goes first; then finding something new
                if self.user.is_some() {
                    v.extend(LISTS[..2].iter().map(list("")));
                }
                v.push(Item::new("search", "Search…", ""));
                v.extend([
                    grouped(
                        Item::new("browse:TRENDING_DESC", "Trending now", ""),
                        "Browse",
                    ),
                    grouped(Item::new("browse:SCORE_DESC", "Top rated", ""), "Browse"),
                    grouped(
                        Item::new("browse:POPULARITY_DESC", "All-time popular", ""),
                        "Browse",
                    ),
                ]);
                if self.user.is_some() {
                    v.extend(LISTS[2..].iter().map(list("Other lists")));
                }
                let local = self.downloaded.as_ref().map_or(0, Vec::len);
                let shows = if local > 0 {
                    plural(local, "show")
                } else {
                    String::new()
                };
                v.push(Item::new("downloads", "Downloads", shows));
                // who's logged in is in the header; logging in starts here
                let login = if self.user.is_none() { "log in" } else { "" };
                v.push(Item::new("settings", "Settings & AniList…", login));
                v.push(Item::new("quit", "Quit", ""));
                v
            }
            View::Settings => vec![
                Item::new(
                    "skip",
                    // short enough for its on/off at 80 columns
                    "Skip openings/endings",
                    if ani::skip_on() { "on" } else { "off" },
                ),
                Item::new(
                    "preload",
                    "Preload next episode",
                    if self.preloading { "on" } else { "off" },
                ),
                audio_item("Default audio…", ani::audio(None)),
                Item::new(
                    "folder",
                    "Download folder…",
                    ani::tilde(&ani::download_root()),
                ),
                match (&self.client.token, &self.user) {
                    (Some(_), user) => Item::new(
                        "logout",
                        "Log out of AniList",
                        user.as_ref().map_or("", |u| u.1.as_str()),
                    ),
                    (None, _) => Item::new("login", "Log in to AniList…", ""),
                },
            ],
            View::Setup => {
                let mut v = Vec::new();
                let pkgs: Vec<&str> = self.missing.iter().map(|d| d.1).collect();
                if let Some(cmd) = ani::install_command(&pkgs).filter(|_| !pkgs.is_empty()) {
                    v.push(Item::new("install", "Install missing packages", cmd));
                }
                if self.user.is_none() {
                    v.push(Item::new("login", "Log in to AniList…", ""));
                }
                v.push(Item::new("continue", "Continue", ""));
                v
            }
            View::Shows(list) => {
                let related = list.related_to();
                let mut v: Vec<Item> = (list.shows.iter().enumerate())
                    .map(|(i, m)| {
                        let mut hint = Vec::new();
                        // the one it opened from; on a series, movies and specials say so
                        if related == Some(m.id) {
                            hint.push("this show".to_string());
                        }
                        if related.is_some() {
                            let what = m.format.as_deref().filter(|f| *f != "TV");
                            hint.extend(what.map(preview::pretty));
                        }
                        if let Some(n) = self.local.get(&m.id).map(Vec::len).filter(|n| *n > 0) {
                            hint.push(format!("↓ {n}"));
                        }
                        let mut seen = String::new();
                        if let Some(e) = self.entries.entry(m) {
                            let total = m.episodes.map_or("?".into(), |e| e.to_string());
                            seen = format!("{}/{total}", e.progress);
                            hint.push(seen.clone());
                        } else if let Some(e) = m.episodes
                            // a movie's one episode goes without saying, when it says it's one
                            && !(related.is_some() && m.format.as_deref() == Some("MOVIE") && e == 1)
                        {
                            hint.push(format!("{e} ep"));
                        }
                        let year = m.season_year.or(m.start_date.year);
                        hint.extend(year.map(|y| y.to_string()));
                        let mut it = Item::new(m.id.to_string(), m.name(), hint.join(" · "));
                        // found by its other titles too: "attack on titan", "aot"
                        it.about = m.titles().cloned().collect::<Vec<_>>().join(" · ");
                        it.group = list.group(i).to_string();
                        it.short = seen;
                        it
                    })
                    .collect();
                if let Some(page) = list.more_page()
                    && !list.shows.is_empty()
                {
                    let mut more = Item::new("more", "More…", format!("page {page}"));
                    more.action = true;
                    v.push(more);
                }
                v
            }
            View::Show(m) => {
                let dl = self.show_downloads(m);
                let src = self.sources.get(m.id);
                let mut v = Vec::new();
                if let Some(next) = self.progress(m).next_out() {
                    let verb = if next > 1 { "Continue with" } else { "Play" };
                    let on_disk = (dl.on_disk().iter()).any(|(e, _)| ani::ep_num(e) == next as f64);
                    v.push(Item::new(
                        "next",
                        format!("▶  {verb} episode {next}"),
                        if on_disk { "on disk" } else { "" },
                    ));
                }
                let avail = match src {
                    Some(s) => format!("{} available", s.episodes.len()),
                    None if self.resolving.contains_key(&m.id) => "searching…".into(),
                    None if self.upcoming(m) => "not out yet".into(),
                    None => String::new(),
                };
                v.push(Item::new("play", "≡  Episodes…", avail));
                v.push(Item::new("download", "↓  Downloads…", dl.summary()));
                if self.user.is_some() && m.on_anilist() {
                    let entry = self.entries.entry(m);
                    let status = entry.map_or("not on list", |e| list_label(&e.status));
                    v.push(Item::new("status", "●  List status…", status));
                    let total = m.episodes.map_or("?".into(), |t| t.to_string());
                    let progress = format!("{}/{total}", entry.map_or(0, |e| e.progress));
                    v.push(Item::new("watched", "✓  Watched up to…", progress));
                }
                v.push(audio_item("♪  Audio…", ani::audio(Some(m.id))));
                v.push(Item::new(
                    "source",
                    "⇄  Change source…",
                    src.map(|s| s.title.clone()).unwrap_or_default(),
                ));
                if m.on_anilist() {
                    v.push(Item::new("web", "↗  Open on AniList", ""));
                }
                // last, as it comes once AniList says so: no other row moves, and ↑ gets here
                if matches!(
                    self.related.get(&m.id),
                    Some(Related::Links | Related::Walking | Related::Series(_))
                ) {
                    v.push(Item::new("related", "⇅  Related shows…", ""));
                }
                v
            }
            View::Episodes(m, mode) => {
                let dl = self.show_downloads(m);
                let run = self.progress(m);
                let listed = self.sources.get(m.id).map_or(&[][..], |s| &s.episodes);
                let eps = dl.episodes(listed);
                let seasons = seasons(
                    eps.iter()
                        .map(|ep| (self.ep_about(m, ep).and_then(|i| i.season)).unwrap_or(0)),
                );
                // what a watched episode is called and about; nothing that spoils the rest
                let item = |key: &str, ep: &str, hint: String, season: u32| {
                    let mut it = Item::new(key, format!("Episode {ep}"), hint);
                    it.done = run.watched(ep);
                    if let Some((title, about)) = self.ep_about(m, ep).map(|i| i.told(it.done)) {
                        if !title.is_empty() {
                            it.label = format!("{} · {title}", it.label);
                        }
                        it.about = about.to_string();
                    }
                    // a part AniList counts on its own, or TheTVDB's season (a long show
                    // like One Piece has 23, where AniList has one entry)
                    it.group = match run.group(ep) {
                        "" if season > 0 => format!("Season {season}"),
                        part => part.to_string(),
                    };
                    it
                };
                let sizes = *mode == EpMode::Download;
                let mut rows: Vec<Item> = (eps.iter().zip(seasons))
                    .map(|(ep, season)| item(ep, ep, dl.hint(ep, sizes), season))
                    .collect();
                // AniList counts up to an episode, so none is before the first
                if *mode == EpMode::Watched && !run.none().is_empty() {
                    let mut none = Item::new(NO_EPISODES, "No episodes", "");
                    none.action = true;
                    // no rule between it and the first group, which never names itself
                    none.group = rows.first().map_or(String::new(), |r| r.group.clone());
                    rows.insert(0, none);
                }
                rows
            }
            View::DlChoice(m, eps) => self.show_downloads(m).choices(eps),
            View::Status(m) => {
                let cur = self.entries.entry(m).map(|e| e.status.as_str());
                let mut v: Vec<Item> = LISTS
                    .iter()
                    .map(|(s, l)| Item::new(*s, *l, if cur == Some(s) { "current" } else { "" }))
                    .collect();
                if cur.is_some() {
                    v.push(Item::new(
                        "remove",
                        "Remove from list",
                        "loses progress and score",
                    ));
                }
                v
            }
            View::Audio(m) => {
                let id = m.as_ref().map(|m| m.id);
                let (cur, default) = (ani::audio(id), ani::audio(None));
                ani::AUDIO
                    .iter()
                    .map(|(k, label, _)| {
                        let mut hint = Vec::new();
                        if *k == cur {
                            hint.push("current");
                        }
                        if id.is_some() && *k == default {
                            hint.push("default");
                        }
                        Item::new(*k, *label, hint.join(" · "))
                    })
                    .collect()
            }
            View::Sources(_, _, hits) => hits
                .iter()
                .enumerate()
                .map(|(i, h)| Item::new(i.to_string(), h.clone(), format!("#{}", i + 1)))
                .collect(),
            View::Bulk(list) => {
                let local = list
                    .iter()
                    .filter(|m| self.local.get(&m.id).is_some_and(|eps| !eps.is_empty()))
                    .count();
                // only what's out downloads; the preview says which isn't
                let out: Vec<&Media> = list.iter().filter(|m| !self.upcoming(m)).collect();
                // as the downloader decides: episodes past each show's progress
                let started = |m: &&Media| self.entries.entry(m).is_some_and(|e| e.progress > 0);
                let which = if out.iter().any(started) {
                    "unwatched"
                } else {
                    "all"
                };
                let mut v = Vec::new();
                if !out.is_empty() {
                    let label = format!(
                        "↓  Download {which} episodes of {}",
                        plural(out.len(), "show")
                    );
                    v.push(Item::new("download", label, ""));
                }
                if local > 0 {
                    v.push(Item::new(
                        "delete",
                        format!("✗  Delete downloads of {}", plural(local, "show")),
                        "",
                    ));
                }
                v
            }
            // the safe answer first, both named by what they do, with what that changes
            View::Confirm(p) => {
                let (no, yes, no_hint, yes_hint) = match p {
                    Pending::Delete(_) => ("Keep", "Delete", "", String::new()),
                    Pending::Stop(m, eps) => {
                        let parts = self.show_downloads(m).deletes(eps);
                        let hint = show_downloads::deletes_hint(&parts);
                        ("Keep downloading", "Stop", "", hint)
                    }
                    Pending::Pause(..) => ("Keep going", "Pause", "", String::new()),
                    Pending::Remove(_) => ("Keep on list", "Remove", "", String::new()),
                    // what it saves, as that can be the entry's last (the second half of a
                    // special) or a part's
                    Pending::Progress(marks, _) => {
                        let hints: Vec<String> = marks.iter().map(Mark::hint).collect();
                        ("Don't save", "Save", "", hints.join(" · "))
                    }
                    Pending::Quit => {
                        // when it asks about downloads, which it doesn't while they're paused
                        let s = self.downloads.snapshot();
                        let mut hint = Vec::new();
                        if self.playing() {
                            hint.push("closes the player");
                        }
                        if s.pending > 0 && !s.paused {
                            hint.push("downloads kept for next start");
                        }
                        ("Stay", "Quit", "", hint.join(" · "))
                    }
                    // dropping them deletes what they got; esc asks again next start
                    Pending::Resume(..) => (
                        "Drop them",
                        "Resume",
                        "deletes partial files",
                        String::new(),
                    ),
                };
                vec![
                    Item::new("no", no, no_hint),
                    Item::new("yes", yes, yes_hint),
                ]
            }
            View::Input(_) => Vec::new(),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.pressed = true;
        self.continuing = None;
        // read by now; what this key does may have its own to say
        self.toast.take_if(|t| t.1);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c' | 'q')) {
            return self.request_quit();
        }
        // the next page of a ranking from anywhere in it, not just on "More…" at its end
        if ctrl && k.code == KeyCode::Char('f') {
            return self.more();
        }
        // fzf's preview keys page through a preview that doesn't fit; Menu::key would take
        // them for plain ↑↓
        if k.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(k.code, KeyCode::Up | KeyCode::Down)
        {
            return self.preview.scroll(k.code == KeyCode::Down);
        }
        // a question has no prompt, so any filter it has is out of sight: these keep going back
        let asked = matches!(self.top().view, View::Confirm(_));
        if asked && ctrl && matches!(k.code, KeyCode::Char('h') | KeyCode::Left) {
            return self.pop(1);
        }
        // typing is the text itself on an input: ← doesn't throw it away, esc clears it first
        let typed = matches!(self.top().view, View::Input(_)) && !self.top().menu.query.is_empty();
        match self.top().menu.key(k) {
            Some(Act::Accept) => self.accept(),
            Some(Act::Back) if typed => {}
            Some(Act::Back) => self.pop(1),
            None => {}
        }
    }

    /// Whether quitting would close a player, also one starting.
    fn playing(&self) -> bool {
        self.playback.now().is_some() || ani::player_open()
    }

    fn request_quit(&mut self) {
        // paused, nothing runs, and the queue keeps them for the next start
        let s = self.downloads.snapshot();
        let downloads = if s.paused { 0 } else { s.pending };
        let saves = self.entries.pending();
        let playing = self.playing();
        let busy = downloads > 0 || saves > 0 || playing;
        if busy && !matches!(self.top().view, View::Confirm(Pending::Quit)) {
            let mut left = Vec::new();
            if playing {
                left.push("the player".to_string());
            }
            if downloads > 0 {
                left.push(plural(downloads, "download"));
            }
            if saves > 0 {
                left.push(plural(saves, "AniList save"));
            }
            self.push(
                format!("{} still running, quit anyway?", left.join(" and ")),
                View::Confirm(Pending::Quit),
            );
        } else {
            self.quit = true;
        }
    }

    fn accept(&mut self) {
        let screen = self.stack.last().unwrap();
        let query = screen.menu.query.trim().to_string();
        let keys: Vec<String> = screen
            .menu
            .selection()
            .iter()
            .map(|&i| screen.menu.items[i].key.clone())
            .collect();
        let Some(key) = keys
            .first()
            .cloned()
            .or_else(|| matches!(screen.view, View::Input(_)).then(String::new))
        else {
            return;
        };
        let label = screen
            .menu
            .current()
            .map_or(String::new(), |i| i.label.clone());
        match &screen.view {
            View::Main | View::Settings => self.main_action(&key, &label),
            View::Setup => match key.as_str() {
                "install" => self.external = screen.menu.current().map(|i| i.hint.clone()),
                "login" => self.main_action("login", ""),
                _ => self.pop(1),
            },
            // marked shows don't count here, "More…" is what the cursor is on
            View::Shows(list)
                if list.paged() && screen.menu.current().is_some_and(|i| i.key == "more") =>
            {
                self.more()
            }
            View::Shows(list) => {
                let picked: Vec<Media> = keys
                    .iter()
                    .filter_map(|k| {
                        (list.shows.iter())
                            .find(|m| m.id.to_string() == *k)
                            .cloned()
                    })
                    .collect();
                // marks stay until the bulk action runs, so esc from it keeps them
                if picked.len() > 1 {
                    self.push(format!("{} shows", picked.len()), View::Bulk(picked));
                } else if let Some(m) = picked.into_iter().next() {
                    // back on the page it came from, not another one of it
                    let here = list.related_to() == Some(m.id);
                    self.top().menu.marked.clear();
                    match here {
                        true => self.pop(1),
                        false => self.open_show(m),
                    }
                }
            }
            View::Show(m) => {
                let m = m.clone();
                self.show_action(m, &key);
            }
            View::Episodes(m, mode) => {
                let (m, mode) = (m.clone(), *mode);
                match mode {
                    EpMode::Play => self.play(m, key),
                    EpMode::Download => self.dl_act(m, keys, None),
                    EpMode::Watched if key == NO_EPISODES => self.ask_none(&m),
                    EpMode::Watched if !self.counted(&m, &key) => self.still_checking(&m),
                    EpMode::Watched => {
                        let title = format!("Watched up to episode {key} of {}?", m.name());
                        let mark = self.progress(&m).mark(&key);
                        self.ask_progress(&m, &key, mark, title);
                    }
                }
            }
            View::DlChoice(m, eps) => {
                let (m, eps) = (m.clone(), eps.clone());
                self.dl_act(m, eps, Some(&key));
            }
            View::Audio(m) => {
                let id = m.as_ref().map(|m| m.id);
                let name = m.as_ref().map_or("Default audio".into(), |m| {
                    format!("Audio for {}", m.name())
                });
                if let Err(e) = ani::set_audio(id, &key) {
                    return self.toast(e.to_string(), true);
                }
                self.toast(format!("{name}: {}", ani::audio_label(&key)), false);
                self.pop(1);
            }
            View::Status(m) => {
                let m = m.clone();
                // AniList can't undo it, so a stray enter or → mustn't
                if key == "remove" {
                    let title = format!(
                        "Remove {} from your list? Its progress and score go too.",
                        m.name()
                    );
                    self.push(title, View::Confirm(Pending::Remove(m)));
                    return self.top().menu.select_key("no");
                }
                self.toast(format!("Saving {}…", m.name()), false);
                let work = self.entries.begin(Change::Status(m, key));
                self.spawn(move || Msg::Entry(vec![work.run()]));
            }
            View::Sources(m, q, hits) => {
                let i: usize = key.parse().unwrap();
                let (m, q, hits) = (m.clone(), q.clone(), hits.clone());
                if let Err(e) = self.sources.choose(m.id, &q, &hits, i) {
                    return self.toast(e.to_string(), true);
                }
                self.parts.remove(&m.id);
                self.toast(format!("Source set to {}", hits[i]), false);
                self.pop(2);
                self.resolve(&m, None);
            }
            View::Bulk(list) => {
                let list = list.clone();
                if key == "download" {
                    let list: Vec<Media> = list.into_iter().filter(|m| !self.upcoming(m)).collect();
                    let shows = plural(list.len(), "show");
                    for m in list {
                        if !self.download(self.job(m, None)) {
                            return;
                        }
                    }
                    self.toast(format!("Queued {shows}, looking up their episodes…"), false);
                    self.pop(1);
                    self.top().menu.marked.clear();
                } else {
                    let dirs: Vec<PathBuf> = list
                        .iter()
                        .map(ani::folder)
                        .filter(|d| ani::ours(d))
                        .collect();
                    self.push(
                        format!("Delete all downloads of {}?", plural(dirs.len(), "show")),
                        View::Confirm(Pending::Delete(dirs)),
                    );
                }
            }
            View::Confirm(p) => {
                if key != "yes" {
                    if let Pending::Resume(_, files) = p {
                        let dropped = self.downloads.drop_leftovers(files);
                        self.toast(
                            match dropped {
                                0 => "Another ani-hop took these over already",
                                _ => "Dropped the unfinished downloads",
                            },
                            false,
                        );
                    }
                    return self.pop(1);
                }
                match p {
                    Pending::Quit => self.quit = true,
                    Pending::Resume(_, files) => {
                        match self.downloads.adopt(files) {
                            Ok(0) => self.toast("Another ani-hop took these over already", false),
                            Ok(_) => self.toast("Resuming the unfinished downloads", false),
                            Err(e) => self.toast(format!("Not resumed · {e}"), true),
                        }
                        self.pop(1);
                    }
                    Pending::Delete(paths) => {
                        let paths = paths.clone();
                        let what = if paths.iter().any(|p| p.is_dir()) {
                            "show"
                        } else {
                            "episode"
                        };
                        let (mut failed, mut busy) = (0, 0);
                        for p in &paths {
                            let res = if p.is_dir() {
                                ani::delete_show(p)
                            } else {
                                ani::delete_episode(p)
                            };
                            match res {
                                Ok(()) => {
                                    // the rescan will say so too, in a moment
                                    (self.local.values_mut())
                                        .for_each(|eps| eps.retain(|(_, f)| !f.starts_with(p)));
                                }
                                Err(e) => {
                                    failed += 1;
                                    busy += (e.kind() == std::io::ErrorKind::ResourceBusy) as usize;
                                }
                            }
                        }
                        let n = paths.len();
                        let done = match (failed, busy) {
                            (0, _) => format!("Deleted {}", plural(n, what)),
                            (_, 0) => format!("Deleted {} of {}", n - failed, plural(n, what)),
                            (_, busy) => format!(
                                "Deleted {} of {} · {busy} still downloading",
                                n - failed,
                                plural(n, what)
                            ),
                        };
                        self.toast(done, failed > 0);
                        self.pop(1);
                        // back on the list they were picked from
                        if matches!(self.top().view, View::Bulk(_) | View::DlChoice(..)) {
                            self.pop(1);
                        }
                        self.top().menu.marked.clear();
                    }
                    Pending::Pause(m, ep) => {
                        // only the episode it warned about; one that started since would
                        // start over unasked. Checked where it runs: its messages may not be
                        // here yet.
                        if self.downloads.pause(m.id, ep) {
                            self.toast(PAUSED, false);
                        }
                        self.pop(1);
                        if matches!(self.top().view, View::DlChoice(..)) {
                            self.pop(1);
                        }
                    }
                    Pending::Stop(m, eps) => {
                        // what finished or was stopped meanwhile stays as it is
                        let (m, stop) = (m.clone(), self.show_downloads(m).still_queued(eps));
                        if !stop.is_empty() {
                            self.toast(format!("Stopped downloading {}", which(&stop)), false);
                            self.stop(&m, &stop);
                        }
                        self.pop(1);
                        if matches!(self.top().view, View::DlChoice(..)) {
                            self.pop(1);
                        }
                        self.top().menu.marked.clear();
                    }
                    Pending::Remove(m) => {
                        let m = m.clone();
                        self.pop(1);
                        let Some(entry) = self.entries.entry(&m).map(|e| e.id) else {
                            return;
                        };
                        self.toast(format!("Removing {}…", m.name()), false);
                        let work = self.entries.begin(Change::Remove(m, entry));
                        self.spawn(move || Msg::Entry(vec![work.run()]));
                    }
                    Pending::Progress(marks, asked) => {
                        // saved only while AniList still has the progress the question showed
                        let saves = (marks.iter().cloned()).map(Change::Progress);
                        let works: Vec<Work> = saves.map(|c| self.entries.begin(c)).collect();
                        self.spawn(move || Msg::Entry(works.into_iter().map(Work::run).collect()));
                        let asked = asked.clone();
                        self.pop(1);
                        self.next_after(asked);
                    }
                }
            }
            View::Input(input) => self.submit(input.clone(), query),
        }
    }

    /// Back on an episode list after marking the episode under its cursor: the next one is
    /// what's wanted now, so enter plays it.
    fn next_after(&mut self, asked: Option<(u64, String)>) {
        let top = self.top();
        if let (View::Episodes(m, EpMode::Play), Some((id, ep))) = (&top.view, asked)
            && m.id == id
            && top.menu.current().is_some_and(|i| i.key == ep)
        {
            top.menu.down();
        }
    }

    /// Fetches the next page of the ranking or search on top, as "More…" and ^f do.
    fn more(&mut self) {
        let screen = self.top();
        let id = screen.id;
        let View::Shows(list) = &mut screen.view else {
            return;
        };
        match list.more() {
            Ok(Some((sort, search, page))) => {
                let (c, stamp) = (self.client.clone(), self.entries.stamp());
                self.spawn(move || {
                    let more = match search {
                        Some(q) => c.search(&q, page),
                        None => c.browse(&sort, None, page),
                    };
                    Msg::More(id, stamp, more)
                });
            }
            Ok(None) => {}
            Err(why) => self.toast(why, false),
        }
    }

    /// Cache key of a ranking, per user: its entries carry that user's list status and progress.
    fn ranking_key(&self, sort: &str) -> String {
        format!("{}-{sort}", self.user.as_ref().map_or(0, |u| u.0))
    }

    fn main_action(&mut self, key: &str, label: &str) {
        let c = self.client.clone();
        if key.starts_with("continue:") {
            if let Some(m) = self.on_continue_row(key).cloned() {
                self.go_on_with(m);
            }
            return;
        }
        if let Some(sort) = key.strip_prefix("browse:") {
            let sort = sort.to_string();
            let cache = self.ranking_key(&sort);
            let feed = Feed::Pages(sort.clone(), None, Some(2), false);
            return self.push_loading(label, feed, Some(cache), move || c.browse(&sort, None, 1));
        }
        match key {
            "search" => _ = self.push("Search", View::Input(Input::Search)),
            "settings" => _ = self.push("Settings & AniList", View::Settings),
            "folder" => {
                self.push("Download folder", View::Input(Input::DownloadDir));
                self.top().menu.paste(&ani::tilde(&ani::download_root()));
            }
            "downloads" => {
                // as the last rescan found them; the one opening it starts fills it in
                let list = List::local(self.downloaded.clone());
                self.push("Downloads", View::Shows(list));
                self.ask_local = true;
            }
            "login" => match client_id() {
                Some(id) => self.start_auth(&id),
                None => self.ask_client_id(),
            },
            "skip" => {
                let on = !ani::skip_on();
                if let Err(e) = ani::set_skip(on) {
                    return self.toast(e.to_string(), true);
                }
                let state = if on { "on" } else { "off" };
                self.toast(format!("Skipping openings and endings: {state}"), false);
                self.refresh();
            }
            "audio" => {
                self.push("Default audio", View::Audio(None));
                self.top().menu.select_key(ani::audio(None));
            }
            "logout" => {
                match self.log_out() {
                    Ok(()) => self.toast("Logged out", false),
                    Err(file) => {
                        let file = ani::tilde(&file);
                        self.toast(format!("Not logged out · can't delete {file}"), true);
                    }
                }
                self.refresh();
            }
            "preload" => {
                let on = !self.preloading;
                if let Err(e) = ani::set_preload(on) {
                    return self.toast(e.to_string(), true);
                }
                self.preloading = on;
                let text = match on {
                    true => "Preload next episode: on · it starts sooner, using a little more data",
                    false => "Preload next episode: off · saves a little data",
                };
                if !on {
                    self.playback.preload.clear();
                }
                self.toast(text, false);
                self.refresh();
            }
            "quit" => self.request_quit(),
            list => {
                let status = list.trim_start_matches("list:").to_string();
                let user = self.user.as_ref().map_or(0, |u| u.0);
                let cache = format!("{user}-{status}");
                self.push_loading(list_label(&status), Feed::Whole, Some(cache), move || {
                    c.list(user, &status)
                });
            }
        }
    }

    /// The one-time setup of an AniList API client of the user's own; a saved client ID stays
    /// until another one is entered.
    fn ask_client_id(&mut self) {
        open_url("https://anilist.co/settings/developer");
        self.push("AniList client ID", View::Input(Input::ClientId));
    }

    fn start_auth(&mut self, client_id: &str) {
        open_url(&format!(
            "https://anilist.co/api/v2/oauth/authorize?client_id={client_id}&response_type=token"
        ));
        self.push("AniList token", View::Input(Input::Token));
    }

    fn submit(&mut self, input: Input, text: String) {
        if text.is_empty() && !matches!(input, Input::DownloadDir | Input::Token) {
            return;
        }
        match input {
            // a mistyped client ID would fail every login: enter it again
            Input::Token if text.is_empty() => {
                self.pop(1);
                self.ask_client_id();
            }
            Input::Search => {
                let c = self.client.clone();
                let feed = Feed::Pages("SEARCH_MATCH".into(), Some(text.clone()), None, false);
                let (list, ask) = List::open(feed, None, std::time::SystemTime::now());
                let (id, stamp) = (self.push(&text, View::Shows(list)), self.entries.stamp());
                self.spawn(move || {
                    // the last AniList search is what found them: More… goes on with it
                    let mut searched = text.clone();
                    let found = |q: &str| {
                        searched = q.to_string();
                        c.search(q, 1)
                    };
                    let res = source::or_ani_cli(found, &text, source::search);
                    // a list without a cache always asks
                    Msg::Shows(id, stamp, res, ask.unwrap(), Some(searched))
                });
            }
            Input::ClientId => {
                // AniList shows a secret right next to it, and a saved wrong one blocks login
                if !text.chars().all(|c| c.is_ascii_digit()) {
                    return self.toast(
                        "The client ID is the number AniList shows, not the secret",
                        true,
                    );
                }
                if let Err(e) = write_private(&ani::config_dir().join("client_id"), &text) {
                    return self.toast(e.to_string(), true);
                }
                self.pop(1);
                self.start_auth(&text);
            }
            Input::Token => {
                let token = parse_token(&text);
                if let Err(e) = write_private(&token_path(), &token) {
                    return self.toast(e.to_string(), true);
                }
                self.client.token = Some(token);
                self.entries.login(Arc::new(self.client.clone()));
                self.parts.clear();
                self.related.clear();
                self.forget_continue();
                let c = self.client.clone();
                self.spawn(move || Msg::Viewer(c.token.clone(), c.viewer()));
                self.toast("Checking token…", false);
                self.pop(1);
            }
            Input::DownloadDir => match ani::set_download_root(&text) {
                Ok(dir) => {
                    self.toast(format!("Downloads go to {}", ani::tilde(&dir)), false);
                    self.pop(1);
                }
                Err(e) => self.toast(e, true),
            },
            Input::SourceQuery(m) => {
                let text = source::source_query(&m, &text);
                let id = self.push("Pick source", View::Sources(m, text.clone(), Vec::new()));
                self.top().waiting = true;
                self.spawn(move || Msg::Hits(id, source::search(&text).map(|r| r.0)));
            }
        }
    }

    fn open_show(&mut self, m: Media) {
        if !self.upcoming(&m) {
            self.resolve(&m, None); // warm up so "Episodes…" opens instantly
        }
        self.want_related(&m);
        self.push(m.name().to_string(), View::Show(m));
        self.preload_next();
    }

    /// Fetches the stream of the episode next up ahead of time, while Settings asks for that,
    /// on its show's page or episode list. Not while one's starting or playing: "next" is that
    /// one until it's marked. Not with a save on its way either, whose answer changes "next".
    fn preload_next(&mut self) {
        let (View::Show(m) | View::Episodes(m, EpMode::Play)) = &self.stack.last().unwrap().view
        else {
            return;
        };
        if !self.preloading
            || !ani::can_hold()
            || self.saving_progress(m)
            || self.playback.now().is_some()
        {
            return;
        }
        let Lookup::Ready(src) = self.sources.lookup(m.id) else {
            return;
        };
        let next = self.progress(m).next().to_string();
        let on_disk = (self.local.get(&m.id)).is_some_and(|eps| eps.iter().any(|e| e.0 == next));
        // not out at the provider, or it plays from disk
        if on_disk || !src.episodes.contains(&next) {
            return;
        }
        self.focus.on_screen(m.id);
        let audio = ani::audio(Some(m.id));
        let ahead = (self.playback).ahead(m, &src, &next, audio, self.focus.clone());
        if let Some(job) = ahead {
            self.run(move || job.run());
        }
    }

    /// Asks AniList once a session whether `m` links to another show, which puts "Related
    /// shows…" on its page. Its series is only walked if that's opened: that takes up to
    /// MOST_REQUESTS of a limit that everything else shares.
    fn want_related(&mut self, m: &Media) {
        if !m.on_anilist() {
            return;
        }
        if let Vacant(e) = self.related.entry(m.id) {
            e.insert(Related::Nothing);
            let (id, c) = (m.id, self.client.clone());
            self.spawn(move || Msg::Linked(id, c.has_related(id)));
        }
    }

    /// The shows related to `m`: at once when this session knows its series, whose other shows
    /// that goes for too, so hopping along its seasons asks AniList nothing. One walk at a time,
    /// however often it's opened meanwhile.
    fn open_related(&mut self, m: &Media) {
        let seed = m.id;
        let series = match self.related.get(&seed) {
            Some(Related::Series(s)) => Some(s.clone()),
            _ => None,
        };
        let list = List::related(seed, series.as_ref().map(|s| s.list(seed)));
        let id = self.push("Related shows", View::Shows(list));
        if let Some(s) = series {
            return self.series_shown(id, seed, &s);
        }
        if !matches!(self.related.get(&seed), Some(Related::Walking)) {
            self.related.insert(seed, Related::Walking);
            let (c, stamp) = (self.client.clone(), self.entries.stamp());
            self.spawn(move || Msg::Series(seed, stamp, c.series(seed)));
        }
    }

    /// `seed`'s series on screen `id`: the cursor starts on it, a key from the season before
    /// or after, and it's said if that isn't all of them.
    fn series_shown(&mut self, id: u64, seed: u64, series: &Series) {
        if let Some(screen) = self.stack.iter_mut().find(|s| s.id == id) {
            screen.menu.select_key(&seed.to_string());
            if series.cut {
                let n = series.shows.len();
                self.toast(format!("Stopped at {n} shows · AniList links more"), false);
            }
        }
    }

    fn show_action(&mut self, m: Media, key: &str) {
        match key {
            // their lists open at once and say why they're empty, without asking ani-cli
            "play" | "download" | "watched" if self.upcoming(&m) => {
                let mode = match key {
                    "play" => EpMode::Play,
                    "download" => EpMode::Download,
                    _ => EpMode::Watched,
                };
                self.fulfill(m, Want::Episodes(mode));
            }
            "next" if self.saving_progress(&m) => self.toast("Saving your progress…", false),
            "next" => self.play(m.clone(), self.progress(&m).next().to_string()),
            "play" => self.resolve(&m, Some(Want::Episodes(EpMode::Play))),
            // what's on disk can be deleted before ani-cli answers, or without it
            "download"
                if self.sources.get(m.id).is_none()
                    && !self.show_downloads(&m).on_disk().is_empty() =>
            {
                self.resolve(&m, None);
                self.fulfill(m, Want::Episodes(EpMode::Download));
            }
            "download" => self.resolve(&m, Some(Want::Episodes(EpMode::Download))),
            "status" => _ = self.push("List status", View::Status(m)),
            "related" => self.open_related(&m),
            "watched" => self.resolve(&m, Some(Want::Episodes(EpMode::Watched))),
            "source" => {
                let q = self
                    .sources
                    .get(m.id)
                    .map_or_else(|| source::default_query(&m), |s| s.query.clone());
                self.push("Change source", View::Input(Input::SourceQuery(m)));
                self.top().menu.paste(&q);
            }
            "web" => open_url(&format!("https://anilist.co/anime/{}", m.id)),
            "audio" => {
                let cur = ani::audio(Some(m.id));
                self.push("Audio", View::Audio(Some(m)));
                self.top().menu.select_key(cur);
            }
            _ => {}
        }
    }

    /// Opens a list on the known (maybe cached) source right away and re-resolves it in the
    /// background once per session; without one, `want` waits for ani-cli, as a play does for
    /// the lookup on its way.
    fn resolve(&mut self, m: &Media, want: Option<Want>) {
        let want = match want {
            Some(w @ Want::Episodes(_)) if self.sources.get(m.id).is_some() => {
                self.fulfill(m.clone(), w);
                None
            }
            w => w,
        };
        if matches!(self.sources.lookup(m.id), Lookup::Ready(_)) {
            return;
        }
        if matches!(want, Some(Want::Episodes(_))) {
            self.toast(format!("Searching ani-cli for {}…", m.name()), false);
        }
        let slot = self.resolving.entry(m.id).or_default();
        if want.is_some() {
            *slot = want;
            self.focus.waited(m.id);
        }
        // none while one runs already
        let Some(ticket) = self.sources.ask(m.id) else {
            return;
        };
        let (m, focus) = (m.clone(), self.focus.clone());
        self.spawn(move || {
            let res = source::resolve_ahead(&m, &focus);
            Msg::Resolved(m, ticket, res)
        });
    }

    /// Whether AniList's count for episode `ep` of `m` is known: past the show's own episodes
    /// it counts toward a part, if there is one, which takes AniList's answer.
    fn counted(&self, m: &Media, ep: &str) -> bool {
        self.parts_known(m) || m.episodes.is_some_and(|own| ani::ep_num(ep) <= own as f64)
    }

    /// Says a question has to wait for `m`'s parts, and looks them up if nothing does.
    fn still_checking(&mut self, m: &Media) {
        self.want_parts(m);
        let text = format!(
            "Still checking {}'s parts · try again in a moment",
            m.name()
        );
        self.toast(text, false);
    }

    /// Asks to mark episode `ep` of `m` watched after playing it, if that adds anything: once
    /// it's known what it counts toward, as saving it to the wrong entry would finish that.
    fn ask_watched(&mut self, m: Media, ep: String) {
        if !self.counted(&m, &ep) {
            self.want_parts(&m);
            self.marking = Some((m, ep));
            return;
        }
        let mark = self.progress(&m).unwatched(&ep);
        if mark.is_some() {
            let title = format!("Mark episode {ep} of {} as watched?", m.name());
            self.ask_progress(&m, &ep, mark, title);
        }
    }

    /// Asks before saving the AniList progress up to `ep`, as `mark` would. AniList keeps a
    /// count, not single episodes, so Yes goes first only when that changes no other episode.
    fn ask_progress(&mut self, m: &Media, ep: &str, mark: Option<Mark>, title: String) {
        let Some(mark) = mark else {
            return self.toast(
                format!("Episode {ep} isn't a number AniList can count"),
                true,
            );
        };
        // what else Yes changes goes in the question, which wraps; a hint would be cut
        let title = format!("{title} {}", mark.also()).trim_end().to_string();
        let first = if mark.only_this() { "yes" } else { "no" };
        let asked = Some((m.id, ep.to_string()));
        self.push(title, View::Confirm(Pending::Progress(vec![mark], asked)));
        self.top().menu.select_key(first);
    }

    /// Asks before taking `m` back to no episodes watched. It unmarks at least one that was, so
    /// Don't save goes first. Not before its parts are known, or theirs would stay marked.
    fn ask_none(&mut self, m: &Media) {
        if !self.parts_known(m) {
            return self.still_checking(m);
        }
        let marks = self.progress(m).none();
        if marks.is_empty() {
            return; // saved in the meantime
        }
        let also = self.progress(m).none_also();
        let title = format!("Watched no episodes of {}? {also}", m.name());
        self.push(title, View::Confirm(Pending::Progress(marks, None)));
        self.top().menu.select_key("no");
    }

    fn fulfill(&mut self, m: Media, want: Want) {
        match want {
            Want::Play(ep) => self.play(m, ep),
            Want::Episodes(mode) => {
                let next = self.progress(&m).next().to_string();
                let title = match mode {
                    EpMode::Play => "Episodes",
                    EpMode::Download => "Downloads",
                    EpMode::Watched => "Watched up to",
                };
                self.push(title, View::Episodes(m, mode));
                self.top().menu.select_key(&next);
                // all of them watched, so no next: not on a row that unmarks them
                let menu = &mut self.top().menu;
                if menu.current().is_some_and(|i| i.key == NO_EPISODES)
                    && let Some(last) = menu.items.last().map(|i| i.key.clone())
                {
                    menu.select_key(&last);
                }
            }
        }
    }

    fn play(&mut self, m: Media, ep: String) {
        // as the last scan found it: the attempt looks again, as that may take a sleeping disk
        let on_disk = (self.local.get(&m.id)).is_some_and(|eps| eps.iter().any(|e| e.0 == ep));
        let (lookup, audio) = (self.sources.to_play(m.id), ani::audio(Some(m.id)));
        match (self.playback).start(&m, &ep, on_disk, lookup, audio) {
            Start::Busy(title) => self.toast(format!("Still starting {title}…"), false),
            Start::Lookup => self.resolve(&m, Some(Want::Play(ep))),
            Start::Run(run) => {
                let (tx, attempt) = (self.tx.clone(), run.attempt);
                let stamp = self.entries.stamp();
                self.spawn(move || {
                    let step = |step| _ = tx.send(Msg::PlayStep(attempt, stamp, m.clone(), step));
                    let res = run.run(&step);
                    Msg::Played(attempt, stamp, m, ep, res)
                });
            }
        }
    }

    /// Shows AniList sent for a read asked at `stamp`, carrying the newest Entries known. An
    /// earlier login's answer comes back as an error without words: nothing to show or say.
    fn observed(
        &mut self,
        stamp: Stamp,
        mut res: Result<Vec<Media>, String>,
    ) -> Result<Vec<Media>, String> {
        match self.entries.observe(stamp, res.iter_mut().flatten()) {
            true => res,
            false => Err(String::new()),
        }
    }

    /// Hands a background result to the screen that waited on it, through `set`. On failure
    /// the screen goes away, unless it already shows something (cached entries stay usable
    /// offline), and says why.
    fn fill(&mut self, id: u64, set: impl FnOnce(&mut View) -> Result<(), String>) {
        let Some(pos) = self.stack.iter().position(|s| s.id == id) else {
            return;
        };
        self.stack[pos].waiting = false;
        match set(&mut self.stack[pos].view) {
            Ok(()) => {
                let items = self.items(&self.stack[pos].view);
                self.stack[pos].menu.set_items(items);
            }
            Err(e) => {
                if self.stack[pos].menu.items.is_empty() {
                    self.stack.truncate(pos);
                }
                // none for an earlier login's answer
                if !e.is_empty() {
                    self.toast(e, true);
                }
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Shows(id, stamp, res, ask, searched) => {
                let res = self.observed(stamp, res);
                let fallback = res
                    .as_ref()
                    .is_ok_and(|l| l.iter().any(|m| !m.on_anilist()))
                    && self.stack.iter().any(|s| s.id == id);
                self.fill(id, |v| match v {
                    View::Shows(list) => {
                        list.searched(searched);
                        list.refreshed(ask, res)
                    }
                    _ => Ok(()),
                });
                // a show's page on top shows their Entries too, the newest now
                self.rebuild();
                if fallback {
                    self.toast("AniList search failed · showing ani-cli's shows", true);
                }
            }
            Msg::Local(stamp, res) => {
                // offline is when downloads are watched: no error for that
                if let Ok(fresh) = self.observed(stamp, res) {
                    ani::refresh_shows(&fresh);
                    self.refresh();
                }
            }
            Msg::More(id, stamp, res) => {
                let res = self.observed(stamp, res);
                let mut first = None; // Some(None): the page brought nothing new
                self.fill(id, |v| {
                    if let View::Shows(list) = v {
                        first = Some(list.more_came(res)?);
                    }
                    Ok(())
                });
                match (first, self.stack.iter_mut().find(|s| s.id == id)) {
                    (Some(Some(key)), Some(s)) => s.menu.select_key(&key),
                    (Some(None), Some(_)) => self.toast("No more shows", false),
                    _ => {}
                }
                self.rebuild();
            }
            Msg::Watching(user, stamp, res, ask) => {
                let res = self.observed(stamp, res);
                // asked for someone who isn't logged in anymore; another read is on its way
                if self.user.as_ref().map(|u| u.0) != Some(user)
                    || res.as_ref().err().is_some_and(String::is_empty)
                {
                    return;
                }
                self.reading_watching = false;
                // a failed one keeps what's known: the row still works, and it's nothing to say
                let read = (self.watching.as_mut()).is_some_and(|l| l.refreshed(ask, res).is_ok());
                if read && let Some(m) = self.last_played.clone() {
                    let shows = self.watching.iter().flat_map(|l| &l.shows);
                    match shows.into_iter().find(|s| s.id == m.id).cloned() {
                        Some(fresh) => self.update_last_played(fresh),
                        None if self.left_watching(&m, stamp) => self.forget_last_played(),
                        None => {}
                    }
                }
                // what enter on the row took for next isn't known anymore
                if read
                    && self.continuing.is_some()
                    && let View::Show(m) = &self.stack.last().unwrap().view
                    && self.left_watching(m, stamp)
                {
                    let text = format!("{} isn't on Watching anymore", m.name());
                    self.continuing = None;
                    self.toast(text, false);
                }
                self.rebuild();
                self.continue_play();
            }
            Msg::Hits(id, res) => self.fill(id, |v| {
                if let View::Sources(_, _, h) = v {
                    // back on the search, which says so in its own words
                    *h = res.map_err(|e| match e.starts_with("No results") {
                        true => "ani-cli: no shows found · try other words".into(),
                        false => format!("ani-cli: {}", e.trim_start_matches("ani-cli: ")),
                    })?;
                }
                Ok(())
            }),
            // asked before logging out or in again: about another login
            Msg::Viewer(token, _) if token != self.client.token => {}
            Msg::Viewer(_, Ok((id, name, english))) => {
                anilist::ENGLISH_TITLES.store(english, Ordering::Relaxed);
                let _ = write_private(
                    &user_path(),
                    &serde_json::json!([id, name, english]).to_string(),
                );
                let login = self.user.is_none();
                let known = self.user.as_ref().is_some_and(|u| u.0 == id);
                self.user = Some((id, name));
                // from the start, it went by who was logged in last time
                if !known {
                    self.read_continue();
                }
                if login {
                    self.toast(
                        format!("Logged in as {}", self.user.as_ref().unwrap().1),
                        false,
                    );
                    self.back_to_main();
                    self.top().menu.edit(String::clear);
                }
                self.refresh();
                if login {
                    self.top().menu.select_key("list:CURRENT");
                }
            }
            Msg::Viewer(_, Err(e)) => {
                // keep the token on network trouble, drop it when AniList rejects it
                if e.contains("Invalid token") || e.contains("Unauthorized") {
                    // no good anymore, whether or not it could be deleted
                    if self.log_out().is_err() {
                        self.forget_login();
                    }
                    self.back_to_main();
                    self.toast("AniList rejected the login, log in again", true);
                } else {
                    self.toast(e, true);
                }
                self.refresh();
            }
            Msg::EpInfo(id, eps) => {
                self.ep_info.insert(id, eps);
                // the show's or a sequel's it runs on into
                if matches!(&self.top().view, View::Episodes(..)) {
                    self.refresh();
                }
            }
            // without them, the provider's episodes stay the show's own
            Msg::Parts(id, key, stamp, parts) => {
                // looked up for other episode counts, or an earlier login
                if !self.parts.get(&id).is_some_and(|p| p.0 == key) {
                    return;
                }
                // asked again when the show or its episodes open
                let Ok(mut parts) = parts else {
                    self.parts.remove(&id);
                    let waiting = self.marking.take_if(|(m, _)| m.id == id);
                    if let Some((_, ep)) = waiting {
                        let text = format!(
                            "Not saved · AniList didn't say what episode {ep} counts toward, \
                             Watched up to… can save it"
                        );
                        self.toast(text, true);
                    }
                    return self.continue_play();
                };
                if !(self.entries).observe(stamp, parts.iter_mut().map(|p| &mut p.1)) {
                    return;
                }
                for (_, p) in &parts {
                    self.want_episodes(p);
                }
                self.parts.insert(id, (key, Some(parts)));
                self.refresh();
                if let Some((m, ep)) = self.marking.take_if(|(m, _)| m.id == id) {
                    self.ask_watched(m, ep);
                }
                self.continue_play();
                self.preload_next();
            }
            Msg::Linked(id, res) => {
                // none of it after another login's clean slate; a failed one's asked again
                let slot = self.related.get_mut(&id);
                match (slot, res) {
                    (Some(r @ Related::Nothing), Ok(true)) => {
                        *r = Related::Links;
                        self.rebuild();
                    }
                    (Some(Related::Nothing), Err(_)) => _ = self.related.remove(&id),
                    _ => {}
                }
            }
            Msg::Series(seed, stamp, res) => {
                let res = res.and_then(|mut s| match self.entries.observe(stamp, &mut s.shows) {
                    true => Ok(Arc::new(s)),
                    false => Err(String::new()),
                });
                // an earlier login's says nothing, and this login's may be out
                if res.as_ref().err().is_some_and(String::is_empty) {
                    return;
                }
                match &res {
                    Ok(s) => {
                        for id in s.listed_for(seed) {
                            self.related.insert(id, Related::Series(s.clone()));
                        }
                    }
                    Err(_) => _ = self.related.insert(seed, Related::Links),
                }
                let waiting: Vec<u64> = (self.stack.iter())
                    .filter(|s| {
                        let list = |l: &List| l.related_to() == Some(seed) && l.loading();
                        matches!(&s.view, View::Shows(l) if list(l))
                    })
                    .map(|s| s.id)
                    .collect();
                for id in waiting {
                    let filled = res.clone();
                    self.fill(id, |v| {
                        if let View::Shows(list) = v {
                            *list = List::related(seed, Some(filled?.list(seed)));
                        }
                        Ok(())
                    });
                    if let Ok(s) = &res {
                        self.series_shown(id, seed, s);
                    }
                }
                // a failed one's screen went: what waited for the page below it goes on
                self.pop(0);
            }
            Msg::Resolved(m, ticket, res) => {
                // a lookup from before the source was changed by hand
                if !self.sources.answer(m.id, ticket, &res) {
                    return;
                }
                let (id, want) = (m.id, self.resolving.remove(&m.id).flatten());
                let here = matches!(&self.top().view, View::Show(s) | View::Episodes(s, _) if s.id == m.id);
                // it gave up as nobody waited: if someone's back by now, it's looked up after all
                if res.as_ref().is_err_and(|e| e == source::ABANDONED) {
                    if here || want.is_some() {
                        self.resolve(&m, want);
                    }
                    return;
                }
                match res {
                    Ok(_) => {
                        // before the menu is rebuilt, so it can't offer what went
                        self.want_parts(&m);
                        let open = (self.stack.iter())
                            .any(|s| matches!(&s.view, View::Show(x) if x.id == m.id));
                        match (want, here) {
                            (Some(w), true) => self.fulfill(m, w),
                            // e.g. the audio picker is open: once back on the show's page
                            (Some(w), false) if open => self.parked = Some((m, w)),
                            _ => {}
                        }
                    }
                    // the source known from before may still work: what a play does when no
                    // lookup is on its way
                    Err(_)
                        if here
                            && matches!(want, Some(Want::Play(_)))
                            && self.sources.get(id).is_some() =>
                    {
                        self.fulfill(m, want.unwrap());
                    }
                    Err(e) if want.is_some() || here => {
                        // the provider may call it something else: picking it by hand fixes that
                        let e = match e.starts_with("No results") {
                            true => no_match(&m).into(),
                            false => e,
                        };
                        self.toast(format!("ani-cli: {e}"), true)
                    }
                    Err(_) => {}
                }
                // a play that waited on it and didn't start again (failed, or the page was left)
                self.playback.give_up(id);
                self.refresh();
                self.preload_next();
            }
            Msg::Entry(done) => {
                let answers = done.into_iter().filter_map(|d| self.entries.finish(d));
                let notices: Vec<_> = answers.collect();
                // what failed, if anything did, else the show's: it comes first
                let Some(n) = notices.iter().find(|n| n.error).or(notices.first()) else {
                    return; // an earlier login's
                };
                self.toast(n.text.clone(), n.error);
                // its list status question is answered, not another show's
                if matches!(&self.top().view, View::Status(s) if Some(s.id) == n.closes) {
                    self.stack.pop();
                }
                // what went wrong comes first; it's still next
                if notices.iter().any(|n| n.error) {
                    self.continuing = None;
                }
                // as saved, for a next start without AniList; taken off the list, it's done with
                if let Some(m) = self.last_played.clone() {
                    match m.media_list_entry.is_some() && self.entries.entry(&m).is_none() {
                        true => self.forget_last_played(),
                        false => self.update_last_played(m),
                    }
                }
                self.refresh();
                self.continue_play();
                self.preload_next();
            }
            Msg::ContinueLimit(id) if self.continuing != Some(id) => {}
            Msg::ContinueLimit(_) => match self.continuing_page() {
                // not over a save: until that's answered, next is the episode just marked
                Some(m) if self.saving_progress(&m) => self.limit_continue(),
                Some(m) => self.continue_now(m),
                None => {}
            },
            Msg::PlayStep(attempt, stamp, m, step) => {
                self.playback.step(attempt, step);
                // the episode's on screen: now it counts as played, whatever comes next
                if step.is_none() && self.entries.same_login(stamp) {
                    self.played(m);
                }
            }
            Msg::Played(attempt, stamp, m, ep, res) => {
                // a player other than mpv doesn't say when it's up; one that did, counted then
                let unsaid = self.playback.starting(attempt);
                self.playback.ended(attempt);
                // started before logging out or in again: no business of this login's
                let ours = self.entries.same_login(stamp) && self.user.is_some();
                match res {
                    Err(e) => self.toast(format!("Couldn't play episode {ep} · {e}"), true),
                    Ok(()) if ours && m.on_anilist() => {
                        if unsaid {
                            self.played(m.clone());
                        }
                        self.ask_watched(m, ep);
                    }
                    Ok(()) => {}
                }
            }
            Msg::Scanned(found) => {
                self.entries.catch_up();
                let again = self.scanning.take() == Some(true);
                let shows: Vec<Media>;
                (shows, self.local) = (found.into_iter())
                    .map(|(m, eps)| (m.clone(), (m.id, eps)))
                    .unzip();
                // saved when downloaded, maybe weeks ago: their progress is AniList's now
                let ids: Vec<u64> = (shows.iter().filter(|m| m.on_anilist()))
                    .map(|m| m.id)
                    .collect();
                if self.ask_local && !ids.is_empty() {
                    self.ask_local = false;
                    let (c, stamp) = (self.client.clone(), self.entries.stamp());
                    self.spawn(move || Msg::Local(stamp, c.shows(&ids)));
                }
                self.downloaded = Some(shows);
                self.rebuild();
                // and the one asked for meanwhile, for what changed since this one started
                if again {
                    self.scan_local();
                }
                self.preload_next();
            }
            Msg::DlDone(label, res) => {
                match res {
                    Ok(()) => self.toast(format!("Downloaded {label}"), false),
                    // the queue paused and kept it
                    Err(e) if ani::unreachable(&e) => self.toast(
                        format!("Paused downloads · {e} · enter on a queued episode resumes them"),
                        true,
                    ),
                    Err(e) => self.toast(format!("{label}: {e}"), true),
                }
                self.refresh();
            }
            Msg::Release(v) => self.release = v,
            // an answer from before an update isn't about this ani-cli
            Msg::AniCliRelease(asked, newer) if asked == self.ani_cli => self.ani_cli_newer = newer,
            Msg::AniCliRelease(..) => {}
        }
    }
}

/// The row for choosing audio, saying the one now: shorter where the row is narrow.
fn audio_item(label: &str, audio: &str) -> Item {
    let mut it = Item::new("audio", label, ani::audio_label(audio));
    it.short = ani::audio_short(audio).into();
    it
}

/// A show's downloaded episodes, as (episode, file).
type OnDisk = Vec<(String, PathBuf)>;

/// What ani-cli finding nothing for `m` says: with only episode 1 out, the provider may not
/// list it yet.
fn no_match(m: &Media) -> &'static str {
    match m.next_airing_episode.is_some() && m.released() == Some(1) {
        true => "no match yet · new shows can take a day, or try Change source…",
        false => "no match for this show · try Change source…",
    }
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn list_label(status: &str) -> &'static str {
    LISTS
        .iter()
        .find(|l| l.0 == status)
        .map_or("on list", |l| l.1)
}

/// Runs test `name` again in a child process with `env` set (temp XDG dirs, a stub first on
/// PATH), for a test that needs a process environment of its own; the child sees
/// ANIHOP_CHILD and does the work. Whether it passed, as that one test: a name that no
/// longer matches would run none and pass.
#[cfg(test)]
pub fn in_child(name: &str, env: &[(&str, std::ffi::OsString)]) -> bool {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1"])
        .env("ANIHOP_CHILD", "1")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output();
    out.is_ok_and(|o| {
        o.status.success()
            && String::from_utf8_lossy(&o.stdout).contains("test result: ok. 1 passed")
    })
}

#[cfg(test)]
mod tests {
    use super::anilist::Entry;
    use super::downloads::Fetch;
    use super::entries::AniList;
    use super::playback::{OPENING, Player, SEARCHING};
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::process::Child;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    /// Background work doesn't run in these, so this never plays anything.
    struct NoPlayer;

    impl Player for NoPlayer {
        fn stream(
            &self,
            _: &Media,
            _: &Source,
            _: &str,
            _: &str,
            _: &dyn Fn(Option<&'static str>),
        ) -> Result<(), String> {
            unreachable!()
        }
        fn hold(&self, _: &Media, _: &Source, _: &str, _: &str) -> Result<ani::Held, String> {
            unreachable!()
        }
        fn replay(&self, _: &ani::Held, _: &dyn Fn(Option<&'static str>)) -> ani::Replayed {
            unreachable!()
        }
        fn on_disk(&self, _: &Media, _: &str) -> Option<PathBuf> {
            unreachable!()
        }
        fn file(&self, _: &Media, _: &str, _: &Path, _: &mut dyn FnMut()) -> Result<(), String> {
            unreachable!()
        }
        fn locate(&self, _: &Media, _: Lookup) -> Result<Source, String> {
            unreachable!()
        }
    }

    /// Nothing gets queued in these either.
    struct NoFetch;

    impl Fetch for NoFetch {
        fn folder(&self, _: &Media) -> PathBuf {
            PathBuf::from("/nowhere")
        }
        fn prepare(&self, _: &Media) -> Result<(PathBuf, ani::Hold), String> {
            Err("no".into())
        }
        fn resolve(&self, _: &Media, _: Lookup, _: bool) -> Result<Source, String> {
            Err("no".into())
        }
        fn start(&self, _: &Source, _: &str, _: bool, _: &Path) -> Result<Child, String> {
            Err("no".into())
        }
    }

    /// AniList as a test wants it: saves go through, and it has what the test put there.
    #[derive(Default)]
    struct Saves(Mutex<HashMap<u64, Entry>>);

    impl AniList for Saves {
        fn entry(&self, media: u64) -> Result<Option<Entry>, String> {
            Ok(self.0.lock().unwrap().get(&media).cloned())
        }
        fn save(
            &self,
            media: u64,
            status: Option<&str>,
            progress: Option<u32>,
        ) -> Result<Entry, String> {
            let saved = Entry {
                id: media,
                status: status.unwrap_or("CURRENT").into(),
                progress: progress.unwrap_or(0),
            };
            self.0.lock().unwrap().insert(media, saved.clone());
            Ok(saved)
        }
        fn remove(&self, _: u64) -> Result<(), String> {
            Ok(())
        }
    }

    /// An App logged in as a tester, on stand-ins, with no background work running: a test
    /// delivers each answer itself, in the order it's about.
    fn app_with(anilist: Arc<Saves>) -> App {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let seams = Seams {
            client: Client { token: None },
            anilist,
            player: Arc::new(NoPlayer),
            queue: (ani::cache_dir().join(format!("app-queue-{n}")), "t".into()),
            posters: false,
            background: false,
        };
        let mut app = App::with(seams, NoFetch);
        app.user = Some((1, "Tester".into()));
        app.push("ani-hop", View::Main);
        app
    }

    fn app() -> App {
        app_with(Arc::default())
    }

    /// A show whose id no other test uses: the tests share one set of files.
    fn show(id: u64, episodes: u32, progress: Option<u32>) -> Media {
        let mut m = Media {
            id,
            episodes: Some(episodes),
            media_list_entry: progress.map(|progress| Entry {
                id,
                status: "CURRENT".into(),
                progress,
            }),
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        m
    }

    fn source(episodes: u32) -> Source {
        Source {
            query: "show".into(),
            index: 1,
            title: "Show".into(),
            nth: 1,
            episodes: (1..=episodes).map(|e| e.to_string()).collect(),
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn enter(app: &mut App) {
        press(app, KeyCode::Enter);
    }

    fn step(app: &App) -> Option<&'static str> {
        app.playback.now().and_then(|p| p.step)
    }

    fn cursor(app: &mut App) -> String {
        app.top().menu.current().unwrap().key.clone()
    }

    fn toast(app: &App) -> Option<(&str, bool)> {
        app.toast.as_ref().map(|t| (t.0.as_str(), t.1))
    }

    /// The screen as text, at the size it's drawn at.
    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ui::render(app, f)).unwrap();
        // the start-up sweep is over for anyone looking
        app.pressed = true;
        term.draw(|f| ui::render(app, f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_play_right_after_opening_waits_for_the_lookup_on_its_way() {
        let mut app = app();
        let m = show(50_001, 12, None);
        let known = HashMap::from([(m.id, source(12))]);
        app.sources = Sources::knowing(known);
        app.open_show(m.clone());
        enter(&mut app);
        assert_eq!(step(&app), Some(SEARCHING), "no search of its own");
        let since = app.playback.now().unwrap().since;
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(source(12))));
        assert_eq!(step(&app), Some("fetching the stream"));
        assert_eq!(app.playback.now().unwrap().since, since, "the same pop-up");
        assert!(toast(&app).is_none());
    }

    #[test]
    fn a_lookup_that_answered_first_plays_right_away() {
        let mut app = app();
        let m = show(50_002, 12, None);
        app.open_show(m.clone());
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(source(12))));
        assert!(app.playback.now().is_none(), "nobody asked to play yet");
        enter(&mut app);
        assert_eq!(step(&app), Some("fetching the stream"));
    }

    #[test]
    fn a_failed_lookup_leaves_the_known_source_to_play_without_a_word() {
        let mut app = app();
        let m = show(50_003, 12, None);
        app.sources = Sources::knowing(HashMap::from([(m.id, source(12))]));
        app.open_show(m.clone());
        enter(&mut app);
        let failed = Err("No results found!".to_string());
        app.on_msg(Msg::Resolved(m, 1, failed));
        assert_eq!(step(&app), Some("checking the source"));
        assert!(toast(&app).is_none());
    }

    #[test]
    fn a_failed_lookup_with_nothing_known_says_so_and_ends_the_wait() {
        let mut app = app();
        let m = show(50_004, 12, None);
        app.open_show(m.clone());
        enter(&mut app);
        assert_eq!(step(&app), Some(SEARCHING));
        let failed = Err("No results found!".to_string());
        app.on_msg(Msg::Resolved(m, 1, failed));
        assert!(app.playback.now().is_none());
        assert!(toast(&app).unwrap().0.starts_with("ani-cli: no match"));
    }

    #[test]
    fn a_lookup_that_gave_up_asks_again_only_if_someone_is_back() {
        let mut app = app();
        let m = show(50_005, 12, None);
        app.open_show(m.clone());
        let gave_up = || Err(source::ABANDONED.to_string());
        // back on the page while it was giving up: it's asked again
        app.on_msg(Msg::Resolved(m.clone(), 1, gave_up()));
        assert_eq!(app.sources.ask(m.id), None, "it runs again");
        assert!(toast(&app).is_none());
        // the page is left: nothing runs, and nothing's said
        press(&mut app, KeyCode::Esc);
        app.on_msg(Msg::Resolved(m.clone(), 2, gave_up()));
        assert_eq!(app.sources.ask(m.id), Some(3), "none ran");
        assert!(toast(&app).is_none());
    }

    /// A show of 12 on AniList that the provider runs on to 22, on its own page, 11 watched.
    fn split_season(id: u64) -> (App, Media, Media, entries::Stamp) {
        let mut app = app();
        let m = show(id, 12, Some(11));
        let part = show(id + 1, 10, Some(0));
        app.sources = Sources::knowing(HashMap::from([(m.id, source(22))]));
        let stamp = app.entries.stamp();
        app.open_show(m.clone());
        (app, m, part, stamp)
    }

    #[test]
    fn an_episode_past_the_shows_own_waits_to_know_which_part_its_in() {
        let (mut app, m, part, stamp) = split_season(50_010);
        app.on_msg(Msg::Played(
            1,
            app.entries.stamp(),
            m.clone(),
            "13".into(),
            Ok(()),
        ));
        assert!(
            !matches!(app.top().view, View::Confirm(_)),
            "it would save the show's own 12/12"
        );
        let parts = vec![(12, part.clone())];
        app.on_msg(Msg::Parts(m.id, (22, 12), stamp, Ok(parts)));
        let View::Confirm(Pending::Progress(marks, _)) = &app.top().view else {
            panic!("asks now");
        };
        assert_eq!((marks[0].entry.id, marks[0].to), (part.id, 1));
    }

    #[test]
    fn a_failed_parts_lookup_saves_nothing_and_says_so() {
        let (mut app, m, _, stamp) = split_season(50_020);
        app.on_msg(Msg::Played(
            1,
            app.entries.stamp(),
            m.clone(),
            "13".into(),
            Ok(()),
        ));
        let failed = Err("Can't reach AniList".to_string());
        app.on_msg(Msg::Parts(m.id, (22, 12), stamp, failed));
        assert!(!matches!(app.top().view, View::Confirm(_)));
        let (text, error) = toast(&app).unwrap();
        assert!(error && text.contains("episode 13"), "{text}");
    }

    #[test]
    fn an_episode_of_the_shows_own_asks_at_once_while_parts_are_looked_up() {
        let (mut app, m, _, _) = split_season(50_030);
        app.on_msg(Msg::Played(
            1,
            app.entries.stamp(),
            m.clone(),
            "12".into(),
            Ok(()),
        ));
        let View::Confirm(Pending::Progress(marks, _)) = &app.top().view else {
            panic!("asks");
        };
        assert_eq!((marks[0].entry.id, marks[0].to), (m.id, 12));
    }

    #[test]
    fn watched_up_to_a_later_episode_waits_for_the_parts_too() {
        let (mut app, m, _, _) = split_season(50_040);
        app.push("Watched up to", View::Episodes(m, EpMode::Watched));
        app.top().menu.select_key("13");
        enter(&mut app);
        assert!(!matches!(app.top().view, View::Confirm(_)));
        assert!(toast(&app).unwrap().0.starts_with("Still checking"));
    }

    /// An episode list of a show of 12, on episode `on`, with `played` just played.
    fn played(id: u64, on: &str, played: &str) -> App {
        let mut app = app();
        let m = show(id, 12, Some(1));
        app.sources = Sources::knowing(HashMap::from([(m.id, source(12))]));
        app.push("Episodes", View::Episodes(m.clone(), EpMode::Play));
        app.top().menu.select_key(on);
        app.on_msg(Msg::Played(
            1,
            app.entries.stamp(),
            m,
            played.into(),
            Ok(()),
        ));
        app
    }

    #[test]
    fn saving_an_episode_puts_the_cursor_on_the_next() {
        let mut app = played(50_050, "2", "2");
        assert_eq!(cursor(&mut app), "yes", "saving goes first");
        enter(&mut app);
        assert_eq!(cursor(&mut app), "3");
        // don't save: it stays
        let mut app = played(50_051, "2", "2");
        press(&mut app, KeyCode::Up);
        enter(&mut app);
        assert_eq!(cursor(&mut app), "2");
        // moved on while it played: it stays where it was put
        let mut app = played(50_052, "5", "2");
        enter(&mut app);
        assert_eq!(cursor(&mut app), "5");
    }

    #[test]
    fn continue_waits_only_for_a_save_of_its_own_show() {
        let mut app = app();
        let (mine, other) = (show(50_060, 12, Some(1)), show(50_061, 12, Some(1)));
        app.sources = Sources::knowing(HashMap::from([
            (mine.id, source(12)),
            (other.id, source(12)),
        ]));
        let mark = app.progress(&mine).mark("2").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        // another show plays: not its save
        app.open_show(other.clone());
        enter(&mut app);
        assert_eq!(app.playback.now().unwrap().id, other.id);
        app.on_msg(Msg::Played(
            app.playback.now().unwrap().id,
            app.entries.stamp(),
            other,
            "2".into(),
            Ok(()),
        ));
        app.playback = Playback::new(Arc::new(NoPlayer));
        app.back_to_main();
        // this show's next is still the episode just marked: it mustn't play that
        app.open_show(mine.clone());
        enter(&mut app);
        assert!(app.playback.now().is_none());
        assert_eq!(toast(&app), Some(("Saving your progress…", false)));
        drop(work);
    }

    #[test]
    fn what_went_wrong_stays_until_a_key_and_a_key_can_say_something_new() {
        let mut app = app();
        app.toast("It broke", true);
        app.toast("Downloaded Show E1", false);
        assert_eq!(toast(&app), Some(("It broke", true)), "not unread");
        app.toast("It broke again", true);
        assert_eq!(toast(&app), Some(("It broke again", true)));
        press(&mut app, KeyCode::Down);
        assert_eq!(toast(&app), None);
        app.toast("Downloaded Show E1", false);
        assert_eq!(toast(&app), Some(("Downloaded Show E1", false)));
    }

    #[test]
    fn the_main_menu_starts_on_watching_with_the_rest_in_groups() {
        let mut app = app();
        assert_eq!(cursor(&mut app), "list:CURRENT");
        let screen = screen(&mut app, 80, 24);
        for line in ["Watching", "Search…", "── Browse", "── Other lists", "Quit"] {
            assert!(screen.contains(line), "{line}\n{screen}");
        }
        assert!(!screen.contains("My list"));
        // typing it still finds them
        app.top().menu.edit(|q| *q = "my list".into());
        assert_eq!(app.top().menu.matches.len(), 6);
    }

    #[test]
    fn a_list_keeps_progress_beside_a_long_title_at_80_columns() {
        let mut app = app();
        let mut m = show(50_070, 28, Some(1));
        m.title.romaji = Some("Attack on Titan Final Season Part 2".into());
        let list = List::local(Some(vec![m]));
        app.push("List", View::Shows(list));
        let screen = screen(&mut app, 80, 24);
        assert!(screen.contains("Part 2 1/28"), "{screen}");
        // and at less room the title comes first
        let narrow = self::screen(&mut app, 60, 18);
        let row = narrow.lines().find(|l| l.contains("Part 2")).unwrap();
        assert!(!row.contains("1/28"), "{narrow}");
    }

    #[test]
    fn a_long_toast_gets_the_whole_row_rather_than_lose_its_end() {
        let mut app = app();
        app.push("Search", View::Settings);
        app.toast = Some(("x".repeat(51), true, Instant::now()));
        assert!(screen(&mut app, 80, 24).contains(&"x".repeat(51)));
    }

    #[test]
    fn a_rescan_goes_on_in_the_background_and_one_asked_for_meanwhile_follows() {
        let mut app = app();
        let m = show(50_080, 12, None);
        assert_eq!(app.scanning, Some(false), "the main menu asked for one");
        app.push("Show", View::Show(m.clone()));
        assert_eq!(app.scanning, Some(true));
        assert!(app.local.is_empty(), "the screen didn't read the folder");
        let file = PathBuf::from("/d/Show Episode 1.mp4");
        let found = vec![(m.clone(), vec![("1".to_string(), file)])];
        app.on_msg(Msg::Scanned(found));
        assert_eq!(app.local[&m.id].len(), 1);
        assert_eq!(app.scanning, Some(false), "the one asked for meanwhile");
        // the page says so, and enter plays it without a lookup
        assert_eq!(app.items(&app.stack[1].view)[0].hint, "on disk");
        enter(&mut app);
        assert_eq!(step(&app), Some(OPENING));
        // gone by the next one
        app.on_msg(Msg::Scanned(Vec::new()));
        assert!(app.local.is_empty());
        assert_eq!(app.downloaded.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn quitting_while_the_player_is_open_asks_and_says_it_closes_it() {
        let mut app = app();
        let m = show(50_090, 12, None);
        app.push("Show", View::Show(m));
        enter(&mut app);
        assert!(app.playback.now().is_some());
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        assert!(app.top().title.starts_with("the player still running"));
        let items = app.items(&app.stack.last().unwrap().view);
        assert_eq!(items[1].hint, "closes the player");
        // asked already: this one goes
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    /// What's being fetched ahead, if it's for `ep` of `m`.
    fn fetching(app: &App, m: &Media, ep: &str, src: &Source) -> bool {
        let key = preload::Key::new(m.id, ep, ani::audio(Some(m.id)), src);
        app.playback.preload.fetching(&key)
    }

    #[test]
    fn with_preloading_on_the_next_episode_is_fetched_on_its_page() {
        let mut app = app();
        let m = show(50_100, 12, Some(3));
        let src = source(12);
        // off by default: nothing's fetched
        app.open_show(m.clone());
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(src.clone())));
        assert!(!fetching(&app, &m, "4", &src));
        app.back_to_main();
        // on: once the source has been checked
        app.preloading = true;
        app.open_show(m.clone());
        assert!(fetching(&app, &m, "4", &src));
        // left for another page that isn't about it, it's still the one on its way
        press(&mut app, KeyCode::Esc);
        assert!(fetching(&app, &m, "4", &src));
    }

    #[test]
    fn preloading_goes_for_the_next_one_once_a_save_is_answered() {
        let anilist = Arc::new(Saves::default());
        let mut app = app_with(anilist.clone());
        app.preloading = true;
        let m = show(50_110, 12, Some(3));
        let src = source(12);
        anilist.0.lock().unwrap().insert(
            m.id,
            Entry {
                id: m.id,
                status: "CURRENT".into(),
                progress: 3,
            },
        );
        app.open_show(m.clone());
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(src.clone())));
        assert!(fetching(&app, &m, "4", &src));
        let mark = app.progress(&m).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.preload_next();
        assert!(fetching(&app, &m, "4", &src), "next isn't known yet");
        app.on_msg(Msg::Entry(vec![work.run()]));
        assert!(fetching(&app, &m, "5", &src));
    }

    #[test]
    fn preloading_skips_what_it_cant_use() {
        let mut app = app();
        app.preloading = true;
        // not out at the provider yet
        let m = show(50_120, 12, Some(5));
        let src = source(5);
        app.open_show(m.clone());
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(src.clone())));
        assert!(!fetching(&app, &m, "6", &src));
        // on disk already
        let m = show(50_121, 12, Some(5));
        let src = source(12);
        let file = PathBuf::from("/d/Show Episode 6.mp4");
        app.on_msg(Msg::Scanned(vec![(
            m.clone(),
            vec![("6".to_string(), file)],
        )]));
        app.back_to_main();
        app.open_show(m.clone());
        app.on_msg(Msg::Resolved(m.clone(), 2, Ok(src.clone())));
        assert!(!fetching(&app, &m, "6", &src));
    }

    #[test]
    fn preloading_shows_in_settings_and_switches_off_at_once() {
        let mut app = app();
        app.push("Settings", View::Settings);
        let row = |app: &App| {
            let items = app.items(&app.stack.last().unwrap().view);
            let row = items.iter().find(|i| i.key == "preload").unwrap();
            (row.label.clone(), row.hint.clone())
        };
        assert_eq!(row(&app), ("Preload next episode".into(), "off".into()));
        app.main_action("preload", "");
        assert!(app.preloading);
        assert_eq!(row(&app).1, "on");
        assert!(toast(&app).unwrap().0.contains("sooner"));
        let m = show(50_130, 12, Some(1));
        let src = source(12);
        let ticket = app.sources.ask(m.id).unwrap();
        app.on_msg(Msg::Resolved(m.clone(), ticket, Ok(src.clone())));
        app.push("Show", View::Show(m.clone()));
        app.preload_next();
        assert!(fetching(&app, &m, "2", &src));
        app.main_action("preload", "");
        assert!(!app.preloading && !fetching(&app, &m, "2", &src));
        assert!(!ani::preload_on());
    }

    #[test]
    fn logging_out_says_so_when_the_login_cant_be_deleted() {
        if std::env::var_os("ANIHOP_CHILD").is_some() {
            // a folder where the file goes can't be removed as one
            fs::create_dir_all(token_path().join("x")).unwrap();
            let mut app = app();
            app.client.token = Some("t".into());
            app.main_action("logout", "");
            assert!(app.user.is_some() && app.client.token.is_some(), "still in");
            let (text, error) = toast(&app).unwrap();
            assert!(error && text.starts_with("Not logged out"), "{text}");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ani-hop-logout-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let env = [
            ("XDG_CONFIG_HOME", dir.clone().into_os_string()),
            ("XDG_CACHE_HOME", dir.clone().into_os_string()),
        ];
        assert!(super::in_child(
            "tests::logging_out_says_so_when_the_login_cant_be_deleted",
            &env
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_episode_without_a_season_is_in_the_one_around_it() {
        // a special before season 1, a recap in it, and new episodes TheTVDB hasn't sorted in
        assert_eq!(super::seasons([0, 1, 0, 1, 2, 0, 0]), [1, 1, 1, 1, 2, 2, 2]);
        assert_eq!(super::seasons([0, 0]), [0, 0], "none at all");
        assert!(super::seasons([]).is_empty());
    }

    #[test]
    fn token_from_url_or_bare() {
        assert_eq!(
            super::parse_token(
                "https://anilist.co/api/v2/oauth/pin#access_token=abc.def&token_type=Bearer"
            ),
            "abc.def"
        );
        assert_eq!(super::parse_token("  abc.def \n"), "abc.def");
    }

    #[test]
    fn a_show_with_only_its_first_episode_out_may_not_be_on_ani_cli_yet() {
        let now = super::anilist::now();
        let show = |status: &str, episodes, next: Option<(u32, i64)>| super::Media {
            status: Some(status.into()),
            episodes,
            next_airing_episode: next
                .map(|(episode, airing_at)| super::anilist::Airing { episode, airing_at }),
            ..Default::default()
        };
        let just = "no match yet · new shows can take a day, or try Change source…";
        let other = "no match for this show · try Change source…";
        assert_eq!(
            super::no_match(&show("RELEASING", Some(12), Some((2, now + 600_000)))),
            just
        );
        let cached = show("NOT_YET_RELEASED", Some(12), Some((1, now - 3600)));
        assert_eq!(
            super::no_match(&cached),
            just,
            "aired since the list was cached"
        );
        assert_eq!(super::no_match(&show("FINISHED", Some(12), None)), other);
        assert_eq!(
            super::no_match(&show("RELEASING", Some(12), Some((9, now + 600_000)))),
            other
        );
        assert_eq!(
            super::no_match(&show("RELEASING", Some(12), Some((1, now + 90)))),
            other
        );
        assert!(!just.contains('—') && !other.contains('—'));
    }

    /// An App whose Continue row goes by `last` and `watching`, with the cursor on its first row.
    fn app_going_on(last: Option<Media>, watching: Vec<Media>) -> App {
        let mut app = app();
        app.last_played = last;
        app.watching = Some(List::local(Some(watching)));
        app.rebuild();
        press(&mut app, KeyCode::Home);
        app
    }

    fn first_row(app: &mut App) -> (String, String, String) {
        let it = &app.top().menu.items[0];
        (it.key.clone(), it.label.clone(), it.hint.clone())
    }

    /// Watching's answer, as asked for right before it comes.
    fn watching_came(app: &mut App, res: Result<Vec<Media>, String>) {
        let (user, stamp) = (app.user.as_ref().unwrap().0, app.entries.stamp());
        app.on_msg(Msg::Watching(user, stamp, res, lists::Ask::now()));
    }

    /// `m`'s player shows its episode, for an attempt on this login.
    fn shows_episode(app: &mut App, attempt: u64, m: &Media) {
        let stamp = app.entries.stamp();
        app.on_msg(Msg::PlayStep(attempt, stamp, m.clone(), None));
    }

    /// The show played last, as the file has it.
    fn on_file(user: u64) -> Option<Media> {
        ani::read_json(&last_played_path(user))
    }

    #[test]
    fn the_continue_row_goes_on_with_the_show_played_last_else_the_first_on_watching() {
        let last = show(50_200, 12, Some(3));
        let first = show(50_201, 12, Some(1));
        let mut app = app_going_on(Some(last), vec![first.clone(), show(50_202, 12, Some(5))]);
        let row = (
            "continue:50200".into(),
            "▶  Show".into(),
            "continue episode 4".into(),
        );
        assert_eq!(first_row(&mut app), row);
        assert_eq!(cursor(&mut app), "continue:50200", "enter is all it takes");
        assert_eq!(app.enter_verb(), Some("play"));

        let mut app = app_going_on(None, vec![first]);
        assert_eq!(first_row(&mut app).0, "continue:50201");
        assert_eq!(first_row(&mut app).2, "continue episode 2");
    }

    #[test]
    fn the_continue_row_skips_what_has_nothing_to_go_on_to_or_was_put_aside() {
        let caught_up = show(50_210, 12, Some(12));
        let mut paused = show(50_211, 12, Some(3));
        paused.media_list_entry.as_mut().unwrap().status = "PAUSED".into();
        let mut not_out = show(50_212, 12, Some(3));
        not_out.next_airing_episode = Some(anilist::Airing {
            episode: 4,
            airing_at: anilist::now() + 3600,
        });
        let next = show(50_213, 12, Some(3));
        for last in [caught_up.clone(), paused.clone(), not_out.clone()] {
            let watching = vec![caught_up.clone(), not_out.clone(), next.clone()];
            let mut app = app_going_on(Some(last), watching);
            assert_eq!(first_row(&mut app).0, "continue:50213");
        }
        // nothing to offer: the menu as it was
        let mut app = app_going_on(None, vec![caught_up]);
        assert_eq!(cursor(&mut app), "list:CURRENT");
    }

    #[test]
    fn one_enter_plays_the_next_episode_on_top_of_its_page() {
        let m = show(50_220, 12, Some(3));
        let mut app = app_going_on(Some(m.clone()), Vec::new());
        enter(&mut app);
        assert!(matches!(&app.top().view, View::Show(s) if s.id == m.id));
        assert_eq!(step(&app), Some(SEARCHING));
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(source(12))));
        assert_eq!(step(&app), Some("fetching the stream"));
        assert_eq!(app.playback.now().unwrap().ep, "4");
        assert_eq!(app.stack.len(), 2, "its page, and the main menu under it");
        assert_eq!(app.continuing, None, "once");
    }

    #[test]
    fn a_continue_row_that_comes_late_leaves_the_cursor_on_its_row() {
        let mut app = app_going_on(None, Vec::new());
        press(&mut app, KeyCode::Down);
        assert_eq!(cursor(&mut app), "list:PLANNING");
        watching_came(&mut app, Ok(vec![show(50_230, 12, Some(1))]));
        assert_eq!(first_row(&mut app).0, "continue:50230");
        assert_eq!(cursor(&mut app), "list:PLANNING");
        // and it's not what a filter finds first without being asked
        app.top().menu.edit(|q| *q = "plan".into());
        watching_came(&mut app, Ok(vec![show(50_231, 12, Some(1))]));
        assert_eq!(cursor(&mut app), "list:PLANNING");
    }

    #[test]
    fn watching_read_again_brings_what_another_device_did() {
        let mut stale = show(50_240, 12, Some(3));
        let mut app = app_going_on(Some(stale.clone()), vec![stale.clone()]);
        assert_eq!(first_row(&mut app).2, "continue episode 4");
        stale.media_list_entry.as_mut().unwrap().progress = 6;
        watching_came(&mut app, Ok(vec![stale.clone()]));
        assert_eq!(first_row(&mut app).2, "continue episode 7");
        // finished or moved on from Watching there
        let other = show(50_241, 12, Some(1));
        watching_came(&mut app, Ok(vec![other]));
        assert_eq!(first_row(&mut app).0, "continue:50241");
        assert_eq!(app.last_played.as_ref().map(|m| m.id), None);
    }

    #[test]
    fn a_failed_read_of_watching_keeps_the_row() {
        let m = show(50_250, 12, Some(3));
        let mut app = app_going_on(Some(m.clone()), vec![m]);
        watching_came(&mut app, Err("AniList is down".into()));
        assert_eq!(first_row(&mut app).0, "continue:50250");
        assert!(toast(&app).is_none(), "the row still works");
    }

    #[test]
    fn watching_from_an_earlier_login_says_nothing_about_this_one() {
        let m = show(50_260, 12, Some(3));
        let mut app = app_going_on(None, vec![m.clone()]);
        let stamp = app.entries.stamp();
        app.entries.login(Arc::new(Saves::default()));
        app.reading_watching = true;
        let res = Ok(vec![show(50_261, 12, Some(1))]);
        app.on_msg(Msg::Watching(1, stamp, res, lists::Ask::now()));
        assert!(app.reading_watching, "this login's answer is still to come");
        assert!(
            app.watching
                .as_ref()
                .unwrap()
                .shows
                .iter()
                .all(|s| s.id == m.id)
        );
    }

    /// `m` is known at ani-cli with more episodes than AniList counts, so the page asks for its
    /// parts and Continue waits for the answer.
    fn app_on_parts(m: &Media, provider: u32) -> App {
        let mut app = app_going_on(Some(m.clone()), Vec::new());
        app.sources = Sources::knowing(HashMap::from([(m.id, source(provider))]));
        app
    }

    #[test]
    fn continue_waits_for_the_parts_that_say_which_episode_is_next() {
        let m = show(50_270, 12, Some(12));
        let mut app = app_on_parts(&m, 22);
        app.go_on_with(m.clone());
        assert!(app.playback.now().is_none(), "13 may have been watched");
        let mut part = show(50_271, 10, Some(3));
        part.status = Some("FINISHED".into());
        let stamp = app.entries.stamp();
        app.on_msg(Msg::Parts(m.id, (22, 12), stamp, Ok(vec![(12, part)])));
        assert_eq!(app.playback.now().unwrap().ep, "16");
    }

    #[test]
    fn continue_plays_on_when_the_parts_stay_unknown() {
        let m = show(50_272, 12, Some(5));
        let mut app = app_on_parts(&m, 22);
        enter(&mut app);
        assert!(app.playback.now().is_none());
        let stamp = app.entries.stamp();
        app.on_msg(Msg::Parts(
            m.id,
            (22, 12),
            stamp,
            Err("AniList is down".into()),
        ));
        assert_eq!(app.playback.now().unwrap().ep, "6");
    }

    #[test]
    fn a_key_while_continue_waits_takes_over() {
        let m = show(50_273, 12, Some(5));
        let mut app = app_on_parts(&m, 22);
        enter(&mut app);
        press(&mut app, KeyCode::Down);
        let stamp = app.entries.stamp();
        app.on_msg(Msg::Parts(m.id, (22, 12), stamp, Ok(Vec::new())));
        assert!(app.playback.now().is_none(), "the cursor went elsewhere");
        assert!(matches!(app.top().view, View::Show(_)));
    }

    #[test]
    fn something_on_top_of_the_page_stops_continue_too() {
        let m = show(50_274, 12, Some(5));
        let mut app = app_on_parts(&m, 22);
        enter(&mut app);
        app.push("Audio", View::Audio(Some(m.clone())));
        let stamp = app.entries.stamp();
        app.on_msg(Msg::Parts(m.id, (22, 12), stamp, Ok(Vec::new())));
        assert!(app.playback.now().is_none());
        assert_eq!(app.continuing, None);
    }

    #[test]
    fn continue_waits_for_a_save_so_it_doesnt_replay_what_was_marked() {
        let anilist = Arc::new(Saves::default());
        let m = show(50_280, 12, Some(3));
        let on_list = m.media_list_entry.clone().unwrap();
        anilist.0.lock().unwrap().insert(m.id, on_list);
        let mut app = app_with(anilist);
        app.user = Some((70_010, "Tester".into()));
        shows_episode(&mut app, 1, &m);
        let mark = app.progress(&m).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.rebuild();
        press(&mut app, KeyCode::Home);
        assert_eq!(first_row(&mut app).2, "continue episode 4");
        enter(&mut app);
        assert!(app.playback.now().is_none());
        app.on_msg(Msg::Entry(vec![work.run()]));
        assert_eq!(app.playback.now().unwrap().ep, "5");
        let kept = on_file(70_010).unwrap().media_list_entry.unwrap();
        assert_eq!(kept.progress, 4, "for a next start without AniList");
    }

    #[test]
    fn a_save_that_failed_comes_before_playing_on() {
        struct Refuses;
        impl AniList for Refuses {
            fn entry(&self, _: u64) -> Result<Option<Entry>, String> {
                Err("AniList is down".into())
            }
            fn save(&self, _: u64, _: Option<&str>, _: Option<u32>) -> Result<Entry, String> {
                Err("AniList is down".into())
            }
            fn remove(&self, _: u64) -> Result<(), String> {
                Err("AniList is down".into())
            }
        }
        let mut app = app();
        app.entries.login(Arc::new(Refuses));
        let m = show(50_281, 12, Some(3));
        app.last_played = Some(m.clone());
        let mark = app.progress(&m).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.rebuild();
        press(&mut app, KeyCode::Home);
        enter(&mut app);
        app.on_msg(Msg::Entry(vec![work.run()]));
        assert!(app.playback.now().is_none(), "it would replay episode 4");
        assert!(toast(&app).unwrap().1);
    }

    #[test]
    fn continue_with_a_player_up_says_so_rather_than_open_another() {
        let m = show(50_290, 12, Some(3));
        let mut app = app_going_on(Some(m.clone()), Vec::new());
        app.sources = Sources::knowing(HashMap::from([(m.id, source(12))]));
        app.on_msg(Msg::Resolved(m.clone(), 1, Ok(source(12))));
        app.play(m.clone(), "4".into());
        shows_episode(&mut app, 1, &m);
        assert!(step(&app).is_none(), "up");
        press(&mut app, KeyCode::Home);
        enter(&mut app);
        assert!(matches!(app.top().view, View::Show(_)));
        assert!(toast(&app).unwrap().0.starts_with("Already playing"));
        assert_eq!(app.playback.now().unwrap().ep, "4");
    }

    #[test]
    fn continue_says_when_nothing_is_left_by_the_time_it_gets_there() {
        let m = show(50_291, 12, Some(12));
        let mut app = app_going_on(None, Vec::new());
        app.go_on_with(m);
        assert!(app.playback.now().is_none());
        assert_eq!(toast(&app).unwrap().0, "You're caught up on Show");
    }

    #[test]
    fn a_show_counts_as_played_once_its_episode_is_on_screen() {
        let m = show(50_300, 12, Some(3));
        let mut app = app();
        app.user = Some((70_001, "Tester".into()));
        let stamp = app.entries.stamp();
        app.on_msg(Msg::PlayStep(1, stamp, m.clone(), Some(OPENING)));
        assert!(app.last_played.is_none(), "still starting, or failing");
        shows_episode(&mut app, 1, &m);
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(m.id));
        assert_eq!(cursor(&mut app), "list:CURRENT", "the cursor stayed");
        assert_eq!(first_row(&mut app).0, "continue:50300");

        // the next start has it from the first frame
        app.last_played = None;
        app.read_continue();
        assert!(app.reading_watching, "and asks how things stand now");
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(m.id));
        assert_eq!(
            app.last_played.unwrap().media_list_entry.unwrap().progress,
            3
        );
    }

    #[test]
    fn a_player_that_never_says_its_up_still_counts_once_it_closes() {
        let m = show(50_301, 12, Some(3));
        let mut app = app();
        app.user = Some((70_002, "Tester".into()));
        let stamp = app.entries.stamp();
        app.play(m.clone(), "4".into());
        app.on_msg(Msg::Played(1, stamp, m.clone(), "4".into(), Ok(())));
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(m.id));
        // one that failed doesn't
        let other = show(50_302, 12, Some(3));
        app.play(other.clone(), "4".into());
        let failed = Err("no stream".into());
        app.on_msg(Msg::Played(2, stamp, other, "4".into(), failed));
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(m.id));
    }

    #[test]
    fn only_a_show_with_progress_to_go_on_from_counts_as_played() {
        let mut app = app();
        // ani-cli's own: AniList keeps no progress of it
        shows_episode(&mut app, 1, &Media::from_ani_cli("Show", source(12)));
        assert!(app.last_played.is_none());
        // logged out: no progress at all
        let m = show(50_303, 12, None);
        app.user = None;
        shows_episode(&mut app, 1, &m);
        assert!(app.last_played.is_none());
        assert!(app.to_continue().is_none());
    }

    #[test]
    fn logging_out_or_in_as_someone_else_leaves_the_row_behind() {
        let m = show(50_310, 12, Some(3));
        let mut app = app();
        app.user = Some((70_003, "Tester".into()));
        shows_episode(&mut app, 1, &m);
        app.forget_login();
        app.refresh();
        assert!(app.last_played.is_none() && app.watching.is_none());
        assert!(!first_row(&mut app).0.starts_with("continue:"));
        // someone else's file, or none
        app.user = Some((70_004, "Other".into()));
        app.read_continue();
        assert!(app.last_played.is_none());
        // back as the first: theirs again
        app.user = Some((70_003, "Tester".into()));
        app.read_continue();
        assert_eq!(app.last_played.map(|s| s.id), Some(m.id));
    }

    #[test]
    fn a_show_taken_off_the_list_is_not_offered_anymore() {
        let anilist = Arc::new(Saves::default());
        let mut app = app_with(anilist);
        app.user = Some((70_005, "Tester".into()));
        let m = show(50_320, 12, Some(3));
        shows_episode(&mut app, 1, &m);
        assert_eq!(first_row(&mut app).0, "continue:50320");
        let work = app.entries.begin(entries::Change::Remove(m.clone(), m.id));
        app.on_msg(Msg::Entry(vec![work.run()]));
        assert!(app.last_played.is_none());
        assert!(on_file(70_005).is_none());
        assert!(!first_row(&mut app).0.starts_with("continue:"));
    }

    #[test]
    fn enter_before_watchings_answer_plays_what_that_says_is_next() {
        let mut m = show(50_340, 12, Some(3));
        let mut app = app_going_on(Some(m.clone()), vec![m.clone()]);
        app.reading_watching = true;
        enter(&mut app);
        assert!(
            app.playback.now().is_none(),
            "another device may have watched 4"
        );
        m.media_list_entry.as_mut().unwrap().progress = 6;
        watching_came(&mut app, Ok(vec![m]));
        assert_eq!(app.playback.now().unwrap().ep, "7");
    }

    #[test]
    fn enter_before_a_failed_read_of_watching_plays_what_was_known() {
        let m = show(50_341, 12, Some(3));
        let mut app = app_going_on(Some(m), Vec::new());
        app.reading_watching = true;
        enter(&mut app);
        watching_came(&mut app, Err("AniList is down".into()));
        assert_eq!(app.playback.now().unwrap().ep, "4");
    }

    #[test]
    fn enter_before_watching_says_a_show_moved_on_stays_on_its_page() {
        let m = show(50_342, 12, Some(3));
        let mut app = app_going_on(None, vec![m.clone()]);
        app.reading_watching = true;
        enter(&mut app);
        watching_came(&mut app, Ok(Vec::new()));
        assert!(app.playback.now().is_none(), "it may be finished by now");
        assert!(matches!(&app.top().view, View::Show(s) if s.id == m.id));
        assert_eq!(toast(&app).unwrap().0, "Show isn't on Watching anymore");
    }

    #[test]
    fn continue_waits_for_anilist_only_so_long() {
        let m = show(50_347, 12, Some(3));
        let mut app = app_going_on(Some(m), Vec::new());
        app.reading_watching = true;
        enter(&mut app);
        assert!(app.playback.now().is_none());
        let page = app.top().id;
        // from an earlier enter, gone by now
        app.on_msg(Msg::ContinueLimit(page - 1));
        assert!(app.playback.now().is_none());
        app.on_msg(Msg::ContinueLimit(page));
        assert_eq!(app.playback.now().unwrap().ep, "4");
        assert_eq!(app.continuing, None);
        // the answer that comes after all doesn't start another
        watching_came(&mut app, Ok(Vec::new()));
        assert!(!app.reading_watching);
        assert_eq!(app.playback.now().unwrap().ep, "4");
        // and it's nothing to a page left in the meantime
        press(&mut app, KeyCode::Esc);
        app.playback = Playback::new(Arc::new(NoPlayer));
        app.on_msg(Msg::ContinueLimit(page));
        assert!(app.playback.now().is_none());
    }

    #[test]
    fn the_limit_on_continue_waits_out_a_save_and_it_still_plays() {
        let anilist = Arc::new(Saves::default());
        let m = show(50_348, 12, Some(3));
        let on_list = m.media_list_entry.clone().unwrap();
        anilist.0.lock().unwrap().insert(m.id, on_list);
        let mut app = app_with(anilist);
        app.last_played = Some(m.clone());
        app.reading_watching = true;
        let mark = app.progress(&m).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.rebuild();
        press(&mut app, KeyCode::Home);
        enter(&mut app);
        let page = app.top().id;
        app.on_msg(Msg::ContinueLimit(page));
        assert!(app.playback.now().is_none(), "it would replay episode 4");
        assert_eq!(app.continuing, Some(page), "still on");
        assert!(toast(&app).is_none());
        // the next limit gets past Watching, which still hasn't answered
        app.on_msg(Msg::Entry(vec![work.run()]));
        assert!(app.playback.now().is_none());
        app.on_msg(Msg::ContinueLimit(page));
        assert_eq!(app.playback.now().unwrap().ep, "5");
    }

    #[test]
    fn enter_on_the_page_goes_ahead_while_watching_keeps_continue_waiting() {
        let m = show(50_343, 12, Some(3));
        let mut app = app_going_on(Some(m), Vec::new());
        app.reading_watching = true;
        enter(&mut app);
        assert!(app.playback.now().is_none());
        enter(&mut app);
        assert_eq!(app.playback.now().unwrap().ep, "4");
    }

    #[test]
    fn watching_of_someone_no_longer_logged_in_is_dropped() {
        let m = show(50_344, 12, Some(3));
        let mut app = app_going_on(None, vec![m.clone()]);
        app.reading_watching = true;
        let stamp = app.entries.stamp();
        let res = Ok(vec![show(50_345, 12, Some(1))]);
        app.on_msg(Msg::Watching(2, stamp, res, lists::Ask::now()));
        assert!(app.reading_watching, "this login's is on its way");
        assert_eq!(first_row(&mut app).0, "continue:50344");
    }

    #[test]
    fn a_start_right_after_the_last_still_asks_how_things_stand() {
        let mut app = app();
        app.user = Some((70_011, "Tester".into()));
        let cached = (vec![show(50_346, 12, Some(6))], None::<u32>);
        ani::write_json(&lists::cache("70011-CURRENT"), &cached).unwrap();
        app.read_continue();
        assert!(
            app.reading_watching,
            "a fresh list says nothing of the show played last"
        );
        assert_eq!(
            app.to_continue().map(|c| c.1),
            Some(7),
            "from the first frame"
        );
    }

    #[test]
    fn a_player_from_an_earlier_login_leaves_this_ones_row_and_file_alone() {
        let m = show(50_350, 12, Some(3));
        let mut app = app();
        app.user = Some((70_012, "Tester".into()));
        let stamp = app.entries.stamp();
        app.forget_login();
        app.user = Some((70_013, "Other".into()));
        app.on_msg(Msg::PlayStep(1, stamp, m.clone(), None));
        assert!(app.last_played.is_none());
        app.on_msg(Msg::Played(1, stamp, m, "4".into(), Ok(())));
        assert!(app.last_played.is_none());
        assert!(on_file(70_013).is_none());
        assert!(matches!(app.top().view, View::Main), "no question about it");
    }

    #[test]
    fn closing_an_older_player_leaves_the_show_played_since() {
        let (a, b) = (show(50_351, 12, Some(3)), show(50_352, 12, Some(3)));
        let mut app = app();
        app.user = Some((70_014, "Tester".into()));
        let stamp = app.entries.stamp();
        app.play(a.clone(), "4".into());
        shows_episode(&mut app, 1, &a);
        app.play(b.clone(), "4".into());
        shows_episode(&mut app, 2, &b);
        app.on_msg(Msg::Played(1, stamp, a, "4".into(), Ok(())));
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(b.id));
        assert_eq!(on_file(70_014).map(|s| s.id), Some(b.id));
    }

    #[test]
    fn what_another_ani_hop_played_since_stays_on_file() {
        let anilist = Arc::new(Saves::default());
        let (x, y) = (show(50_353, 12, Some(3)), show(50_354, 12, Some(1)));
        let on_list = x.media_list_entry.clone().unwrap();
        anilist.0.lock().unwrap().insert(x.id, on_list);
        let mut app = app_with(anilist);
        app.user = Some((70_015, "Tester".into()));
        shows_episode(&mut app, 1, &x);
        ani::write_json(&last_played_path(70_015), &y).unwrap();
        // a save, a read and a removal here don't make x played again
        let mark = app.progress(&x).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.on_msg(Msg::Entry(vec![work.run()]));
        watching_came(&mut app, Ok(vec![x.clone()]));
        assert_eq!(on_file(70_015).map(|s| s.id), Some(y.id));
        app.forget_last_played();
        assert_eq!(on_file(70_015).map(|s| s.id), Some(y.id));
    }

    #[test]
    fn a_late_answer_of_watching_keeps_a_show_saved_onto_it_since() {
        let anilist = Arc::new(Saves::default());
        let m = show(50_355, 12, Some(3));
        let on_list = m.media_list_entry.clone().unwrap();
        anilist.0.lock().unwrap().insert(m.id, on_list);
        let mut app = app_with(anilist);
        app.user = Some((70_016, "Tester".into()));
        let stamp = app.entries.stamp();
        shows_episode(&mut app, 1, &m);
        app.watching = Some(List::local(Some(Vec::new())));
        let mark = app.progress(&m).mark("4").unwrap();
        let work = app.entries.begin(entries::Change::Progress(mark));
        app.on_msg(Msg::Entry(vec![work.run()]));
        app.on_msg(Msg::Watching(
            70_016,
            stamp,
            Ok(Vec::new()),
            lists::Ask::now(),
        ));
        assert_eq!(app.last_played.as_ref().map(|s| s.id), Some(m.id));
        // read after that save, it's moved on
        watching_came(&mut app, Ok(Vec::new()));
        assert!(app.last_played.is_none());
    }

    #[test]
    fn the_continue_row_keeps_its_episode_beside_a_long_title() {
        let mut m = show(50_330, 28, Some(12));
        m.title.romaji = Some("Attack on Titan Final Season Part 2 and a Long Subtitle".into());
        let mut app = app_going_on(Some(m), Vec::new());
        let at_80 = screen(&mut app, 80, 24);
        let row = at_80.lines().find(|l| l.contains('▶')).unwrap();
        assert!(row.contains("ep 13"), "{at_80}");
        assert!(row.contains('…'), "{at_80}");
        // what it plays beside it
        assert!(at_80.contains("▶ Next: episode 13"), "{at_80}");
        // and with room it says what it does
        let short = show(50_331, 28, Some(12));
        let mut roomy = app_going_on(Some(short), Vec::new());
        let wide = self::screen(&mut roomy, 200, 24);
        assert!(wide.contains("continue episode 13"), "{wide}");
        app.top().menu.edit(|q| *q = "continue".into());
        assert_eq!(cursor(&mut app), "continue:50330");
    }

    #[test]
    fn a_question_has_no_prompt_so_its_back_keys_still_go_back() {
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        for code in [KeyCode::Char('h'), KeyCode::Left] {
            let mut app = app();
            app.push("Quit?", View::Confirm(Pending::Quit));
            // typing still filters its answers, out of sight
            press(&mut app, KeyCode::Char('q'));
            assert_eq!(app.top().menu.query, "q");
            app.on_key(ctrl(code));
            assert_eq!(app.stack.len(), 1, "{code:?}");
        }
        // where the text is on screen they delete a word of it instead
        let mut app = app();
        app.push("Search", View::Input(Input::Search));
        app.top().menu.edit(|q| *q = "attack on".into());
        app.on_key(ctrl(KeyCode::Char('h')));
        assert_eq!(
            (app.stack.len(), app.top().menu.query.as_str()),
            (2, "attack ")
        );
    }

    #[test]
    fn the_screen_shares_its_edges() {
        let mut app = app();
        app.user = Some((1, "RenkeB".into()));
        app.ani_cli = Some("5.1.6".into());
        app.top().menu.edit(|q| *q = "wat".into());
        let drawn = screen(&mut app, 120, 30);
        let lines: Vec<&str> = drawn.lines().collect();
        let (top, foot) = (lines[0], lines[29]);
        // the column `text` starts at, and the one it ends at
        let start = |line: &str, text: &str| line.find(text).map(|at| line[..at].chars().count());
        let end = |line: &str, text: &str| start(line, text).unwrap() + text.chars().count() - 1;

        // the left edge's text
        let edge = Some(2);
        assert_eq!(start(lines[2], "❯"), edge, "{}", lines[2]);
        assert_eq!(start(lines[3], "2/13"), edge, "{}", lines[3]);
        assert_eq!(start(foot, "ani-hop"), edge, "{foot}");
        // what's typed starts where the rows' labels do
        assert_eq!(start(lines[2], "wat"), Some(5), "{}", lines[2]);
        assert_eq!(start(lines[4], "Watching"), Some(5), "{}", lines[4]);
        // the right edge's text
        assert_eq!(end(top, "RenkeB"), 117, "{top}");
        assert_eq!(end(foot, "quit"), 117, "{foot}");

        // the panels share their inset, and the logo gets a blank row more than text
        let right = start(lines[1], "╮╭").unwrap() + 1;
        assert_eq!(start(lines[3], "▄▀█"), Some(right + 2), "{}", lines[3]);
        // and the key table starts under the logo, at its widest key
        let keys = lines
            .iter()
            .find(|l| l.contains("filter the list"))
            .unwrap();
        assert_eq!(start(keys, "type"), Some(right + 2), "{keys}");

        // a message too long beside the keys ends on the right edge too, and says it's cut
        app.toast("too long ".repeat(20), true);
        let drawn = screen(&mut app, 120, 30);
        let foot = drawn.lines().last().unwrap().trim_end();
        assert_eq!(
            (foot.chars().count(), foot.chars().last()),
            (118, Some('…'))
        );

        // the tag is on every screen: its fill starts on the border's line, in the middle of the
        // column, and its name on the left edge
        assert!(top.starts_with('▐'), "{top}");
        assert_eq!(start(top, "ani-hop"), edge, "{top}");
        // every prompt is alike: the top line and the help say what it asks for
        app.push("Download folder", View::Input(Input::DownloadDir));
        app.top().menu.edit(|q| *q = "~/x".into());
        let drawn = screen(&mut app, 120, 30);
        let lines: Vec<&str> = drawn.lines().collect();
        assert!(lines[0].starts_with('▐'), "{}", lines[0]);
        assert!(lines[1].starts_with('╭'), "{}", lines[1]);
        assert_eq!(start(lines[0], "ani-hop"), edge, "{}", lines[0]);
        assert_eq!(start(lines[2], "❯"), edge, "{}", lines[2]);
        assert_eq!(start(lines[2], "~/x"), Some(5), "{}", lines[2]);
        assert_eq!(start(lines[3], "─"), edge, "{}", lines[3]);
    }

    #[test]
    fn the_main_menu_shows_what_its_continue_row_plays() {
        let mut watched = show(50_400, 12, Some(3));
        watched.description = Some("What the show is about.".into());
        let mut app = app_going_on(Some(watched), Vec::new());
        // the poster's show and next episode, instead of the keys
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("▶ Next: episode 4"), "{drawn}");
        assert!(drawn.contains("What the show is about."), "{drawn}");
        assert!(!drawn.contains("filter the list"), "{drawn}");
        // a new version only says how on the start panel; the bottom line says it's out on both
        app.release = Some(("99.0.0".into(), "brew upgrade ani-hop".into()));
        let drawn = screen(&mut app, 120, 30);
        assert!(!drawn.contains("is out: brew"), "{drawn}");
        assert!(drawn.contains("↑ (99.0.0 is out)"), "{drawn}");
        press(&mut app, KeyCode::Down);
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("↑ ani-hop 99.0.0 is out: brew"), "{drawn}");
        assert!(drawn.contains("↑ (99.0.0 is out)"), "{drawn}");
        press(&mut app, KeyCode::Up);
        app.release = None;

        // it stands out, and Quit steps back
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| ui::render(&mut app, f)).unwrap();
        let buf = term.backend().buffer().clone();
        let line = |y: u16| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>();
        let cell = |row: &str, glyph: &str| {
            let y = (0..30).find(|&y| line(y).contains(row)).unwrap();
            let x = (0..120).find(|&x| buf[(x, y)].symbol() == glyph).unwrap();
            buf[(x, y)].clone()
        };
        assert_eq!(cell("▶  Show", "▶").fg, ui::ACCENT);
        assert_eq!(cell("continue episode 4", "4").fg, ui::ACCENT);
        assert_eq!(Some(cell("Quit", "Q").fg), ui::SOFT.fg);
        // under a rule of its own, which doesn't cost a screen of 24 rows any row
        let drawn = screen(&mut app, 80, 24);
        let rows: Vec<&str> = drawn.lines().collect();
        assert!(rows[5].contains("───"), "{drawn}");
        assert!(rows[6].contains("Watching"), "{drawn}");
        assert!(drawn.contains("Quit"), "{drawn}");

        // any other row has the keys again, and the show's gone
        press(&mut app, KeyCode::Down);
        let drawn = screen(&mut app, 120, 30);
        assert!(drawn.contains("filter the list"), "{drawn}");
        assert!(!drawn.contains("Next: episode 4"), "{drawn}");
        // and its marker no longer passes for the cursor
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| ui::render(&mut app, f)).unwrap();
        let buf = term.backend().buffer();
        let play = (0..120).find(|&x| buf[(x, 4)].symbol() == "▶").unwrap();
        assert_ne!(buf[(play, 4)].fg, ui::ACCENT);
    }

    #[test]
    fn help_on_a_prompt_keeps_its_addresses_on_one_row_at_80_columns() {
        let mut app = app();
        app.push("AniList client ID", View::Input(Input::ClientId));
        let drawn = screen(&mut app, 80, 24);
        assert!(drawn.contains(PIN_REDIRECT), "{drawn}");
        assert!(drawn.contains("anilist.co/settings/developer"), "{drawn}");
        // the left panel keeps the prompt and its rule alone
        let left: String = (drawn.lines().skip(4).take(18))
            .map(|l| l.chars().take(34).collect::<String>())
            .collect();
        assert!(!left.contains(|c: char| c.is_alphanumeric()), "{drawn}");
        // except beside the show a source is searched for, which has the other panel
        app.push(
            "Change source",
            View::Input(Input::SourceQuery(show(50_410, 12, None))),
        );
        let drawn = screen(&mut app, 80, 24);
        let row = drawn
            .lines()
            .find(|l| l.contains("Search ani-cli"))
            .unwrap();
        assert!(
            row.find("Search ani-cli").unwrap() < row.find("││").unwrap(),
            "{drawn}"
        );
    }

    #[test]
    fn text_never_takes_the_color_meant_for_lines() {
        let m = show(50_411, 12, Some(3));
        let mut app = app();
        app.ani_cli = Some("5.1.6".into());
        let screens: Vec<(&str, View)> = vec![
            ("Settings", View::Settings),
            ("Welcome", View::Setup),
            ("Search", View::Input(Input::Search)),
            ("Episodes", View::Episodes(m.clone(), EpMode::Play)),
            ("Bulk", View::Bulk(vec![m.clone()])),
            ("Show", View::Show(m.clone())),
        ];
        let check = |app: &mut App, name: &str| {
            let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
            term.draw(|f| ui::render(app, f)).unwrap();
            app.pressed = true;
            term.draw(|f| ui::render(app, f)).unwrap();
            let buf = term.backend().buffer();
            for pos in buf.area.positions() {
                let cell = &buf[pos];
                assert!(
                    cell.fg != ui::LINE || "─│╭╮╰╯━ ›·\u{a0}".contains(cell.symbol()),
                    "{name}: {:?} at {pos:?}",
                    cell.symbol()
                );
            }
        };
        check(&mut app, "main menu");
        for (name, view) in screens {
            app.push(name, view);
            check(&mut app, name);
            app.stack.pop();
        }
        // and a setting's value reads like the setting
        app.push("Settings", View::Settings);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| ui::render(&mut app, f)).unwrap();
        let buf = term.backend().buffer();
        let row = (0..40).find(|&y| {
            let line: String = (0..50).map(|x| buf[(x, y)].symbol()).collect();
            line.contains("Download folder…")
        });
        let y = row.unwrap();
        let end = (0..48)
            .rev()
            .find(|&x| buf[(x, y)].symbol() != " ")
            .unwrap();
        assert_eq!(buf[(end, y)].fg, ratatui::style::Color::Reset);
    }

    #[test]
    fn a_description_keeps_one_left_edge() {
        // the column each row of it starts at, in the preview
        let columns = |drawn: &str| -> Vec<usize> {
            let mut at: Vec<usize> = (drawn.lines())
                .filter_map(|l| l.find("word").map(|at| l[..at].chars().count()))
                .collect();
            at.dedup();
            at
        };
        let shown = |words: usize, w, h| {
            let mut m = show(50_412, 12, None);
            m.description = Some("word ".repeat(words));
            let mut app = app();
            app.open_show(m);
            screen(&mut app, w, h)
        };
        // too long for the column beside the poster: all of it under the poster, at the inset
        let long = shown(200, 80, 24);
        assert_eq!(columns(&long), [36], "{long}");
        // short enough: all of it beside the poster, also when it runs on past it
        let short = shown(40, 188, 48);
        let at = columns(&short);
        assert!(at.len() == 1 && at[0] > 80, "{short}");
        // a column too narrow for a wide character doesn't lose it: it goes under the poster
        let mut m = show(50_414, 12, None);
        m.description = Some("ビデ".into());
        let mut app = app();
        app.open_show(m);
        let drawn = screen(&mut app, 18, 40);
        assert!(drawn.contains('ビ') && drawn.contains('デ'), "{drawn}");
    }

    #[test]
    fn an_empty_prompt_says_what_typing_does_on_its_screen() {
        // the prompt's row, as drawn
        let prompt = |app: &mut App, w| screen(app, w, 30).lines().nth(2).unwrap().to_string();
        let hint_at = |row: &str, text: &str| row.find(text).map(|at| row[..at].chars().count());
        let filter = "type to filter";

        // on every row of a list, starting where what's typed would, and gone with the first key
        let mut app = app_going_on(Some(show(50_413, 12, Some(3))), Vec::new());
        let row = prompt(&mut app, 120);
        assert_eq!(hint_at(&row, filter), Some(5), "{row}");
        press(&mut app, KeyCode::Down);
        assert_eq!(hint_at(&prompt(&mut app, 120), filter), Some(5));
        app.top().menu.edit(|q| *q = "s".into());
        assert!(!prompt(&mut app, 120).contains(filter));
        app.top().menu.edit(String::clear);
        assert!(prompt(&mut app, 120).contains(filter));
        // whole or not at all, and the cursor stays on its first letter
        assert!(!prompt(&mut app, 20).contains("type"), "too narrow");
        // and on the lists below it
        app.push("Settings", View::Settings);
        assert!(prompt(&mut app, 120).contains(filter));
        app.push("Empty", View::Bulk(Vec::new()));
        assert!(!prompt(&mut app, 120).contains(filter), "nothing to filter");
        app.stack.pop();
        app.stack.pop();

        // where typing does something else it says that
        let m = show(50_415, 12, None);
        let says = [
            (Input::Search, "type to search AniList"),
            (Input::ClientId, "paste your client ID"),
            (Input::Token, "paste the token"),
            (Input::DownloadDir, ""),
            (Input::SourceQuery(m), ""),
        ];
        for (input, text) in says {
            app.push("Prompt", View::Input(input));
            app.top().menu.edit(String::clear);
            let row = prompt(&mut app, 120);
            let left: String = row.chars().take(40).collect();
            assert!(!left.contains(filter), "{row}");
            match text {
                "" => assert!(left.trim_end_matches([' ', '│']).ends_with('❯'), "{row}"),
                text => assert_eq!(hint_at(&row, text), Some(5), "{row}"),
            }
            app.stack.pop();
        }
    }

    #[test]
    fn on_a_wide_screen_the_list_stops_growing_and_a_page_stays_readable() {
        let mut m = show(50_401, 12, None);
        m.description = Some("word ".repeat(400));
        let mut app = app();
        app.open_show(m);
        let drawn = screen(&mut app, 260, 40);
        let top = drawn.lines().nth(1).unwrap();
        let left = top.find("╮╭").map(|at| top[..at].chars().count() + 1);
        assert_eq!(left, Some(64), "{top}");
        // nothing of the page past its width, in a preview far wider
        let past = 64 + 2 + 110;
        for line in drawn.lines().skip(2).take(36) {
            let text: String = line.chars().skip(past).take(260 - past - 2).collect();
            assert_eq!(text.trim(), "", "{line}");
        }
        // and narrower screens keep their 42%
        let mut width = |w: u16| {
            let drawn = screen(&mut app, w, 24);
            let top = drawn.lines().nth(1).unwrap().to_string();
            top.find("╮╭").map(|at| top[..at].chars().count() + 1)
        };
        assert_eq!(width(80), Some(34));
        assert_eq!(width(120), Some(50));
    }

    #[test]
    fn the_main_menu_says_the_versions_unless_anything_else_is_there() {
        // what it says, past the column every line of it keeps free
        let foot = |app: &mut App, w| screen(app, w, 24).lines().last().unwrap()[1..].to_string();
        let me = env!("CARGO_PKG_VERSION");
        let with_versions = || {
            let mut app = app();
            app.ani_cli = Some("5.1.6".into());
            app
        };
        let mut app = with_versions();
        let all = format!(" ani-hop {me} · ani-cli 5.1.6");
        assert!(foot(&mut app, 80).starts_with(&all));
        // nothing up top, where the name is whole or not there
        let top = |app: &mut App, w| screen(app, w, 24).lines().next().unwrap().to_string();
        assert!(!top(&mut app, 80).contains(me));
        assert!(top(&mut app, 22).starts_with("▐ ani-hop ▌"));
        assert!(!top(&mut app, 21).contains("ani"));

        // less room: only ani-hop, then only its number, then nothing
        let narrow = foot(&mut app, 50);
        assert!(narrow.starts_with(&format!(" ani-hop {me} ")), "{narrow}");
        assert!(!narrow.contains("ani-cli"), "{narrow}");
        let narrower = foot(&mut app, 35);
        assert!(narrower.starts_with(&format!(" {me} ")), "{narrower}");
        let narrowest = foot(&mut app, 28);
        assert!(!narrowest.contains(me), "{narrowest}");

        // moving along the main menu leaves them; other screens don't have them, till it's back
        press(&mut app, KeyCode::Down);
        assert!(foot(&mut app, 80).starts_with(&all));
        app.push("Search", View::Settings);
        assert!(!foot(&mut app, 80).contains(me));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.stack.len(), 1);
        assert!(foot(&mut app, 80).starts_with(&all));

        // anything else goes over them, until it's faded
        app.toast("Saved", false);
        assert!(foot(&mut app, 80).starts_with(" Saved"));
        app.toast.as_mut().unwrap().2 -= Duration::from_secs(5);
        assert!(foot(&mut app, 80).starts_with(&all));

        // only the arrows stand out, followed by what's out. Only ani-hop's says how, as ani-cli
        // has too many ways to update
        let mut app = with_versions();
        app.release = Some(("99.0.0".into(), "brew upgrade ani-hop".into()));
        app.ani_cli_newer = Some("5.2".into());
        let wide = foot(&mut app, 90);
        let all_said = format!(" ani-hop {me} ↑ (99.0.0 is out) · ani-cli 5.1.6 ↑ (5.2 is out)");
        assert!(wide.starts_with(&all_said), "{wide}");
        // both at 80 columns: the words go before ani-cli does
        let drawn = screen(&mut app, 80, 24);
        let marked = format!(" ani-hop {me} ↑ (99.0.0) · ani-cli 5.1.6 ↑ (5.2)");
        assert!(
            drawn.lines().last().unwrap()[1..].starts_with(&marked),
            "{drawn}"
        );
        assert!(drawn.contains("↑ ani-hop 99.0.0 is out"), "{drawn}");
        assert_eq!(drawn.matches("is out").count(), 1, "{drawn}");
        // with less room what's out goes before the number does
        assert!(foot(&mut app, 60).starts_with(&format!(" ani-hop {me} ↑ (99.0.0 is out) ")));
        // just enough room for it, however long this version is
        let room = 44 + me.len() as u16;
        assert!(foot(&mut app, room).starts_with(&format!(" {me} ↑ (99.0.0 is out) ")));
        let number_only = foot(&mut app, 35);
        assert!(number_only.starts_with(&format!(" {me} ")), "{number_only}");
        assert!(!number_only.contains('↑'), "{number_only}");
        assert!(!foot(&mut app, 28).contains(me));
        // one update fits at 80 columns with its words
        app.release = None;
        assert!(foot(&mut app, 80).contains(" · ani-cli 5.1.6 ↑ (5.2 is out)"));
        app.release = Some(("99.0.0".into(), "brew upgrade ani-hop".into()));
        screen(&mut app, 80, 24);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| ui::render(&mut app, f)).unwrap();
        let buf = term.backend().buffer();
        let bright: String = (0..marked.chars().count() as u16)
            .map(|x| &buf[(x + 1, 23)])
            .filter(|cell| cell.fg == ui::ACCENT)
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(bright, " ↑ ↑");

        // an old ani-cli gets no arrow without knowing of a release
        let mut app = with_versions();
        app.ani_cli = Some("5.0.2".into());
        assert!(!foot(&mut app, 80).contains("↑"));

        // an answer about the ani-cli from before an update doesn't bring the arrow back
        let newer = || Some("5.3".to_string());
        app.ani_cli = Some("5.2".into());
        app.on_msg(Msg::AniCliRelease(Some("5.0.2".into()), newer()));
        assert_eq!(app.ani_cli_newer, None);
        app.on_msg(Msg::AniCliRelease(Some("5.2".into()), newer()));
        assert_eq!(app.ani_cli_newer, newer());
        // and an update takes it along at once, with the new answer still on its way
        app.after_install();
        assert_eq!(app.ani_cli_newer, None);
    }
}
