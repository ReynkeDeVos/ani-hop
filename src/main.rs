#![allow(clippy::large_enum_variant)] // a handful of screens, size is irrelevant

mod ani;
mod anilist;
mod downloads;
mod kitty;
mod lists;
mod menu;
mod progress;
mod source;
mod ui;

use anilist::{Client, Entry, EpInfo, Media};
use downloads::{AniCli, Job, Queue};
use lists::{Feed, List};
use menu::{Act, Item, Menu};
use progress::{Mark, Parts, Progress};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::Size;
use source::{Lookup, Source, Sources};
use std::collections::HashMap;
use std::collections::hash_map::Entry::Vacant;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub const PIN_REDIRECT: &str = "https://anilist.co/api/v2/oauth/pin";
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
    /// marking an episode watched: what Yes saves, as the question says it
    Progress(Mark),
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
    pub loading: bool,
}

/// A poster is made for a size in cells and the pixels per cell (font zoom changes those).
pub type CoverKey = (Size, (u16, u16));

pub enum Cover {
    Loading(CoverKey),
    Ready(CoverKey, kitty::Poster),
    Failed,
}

enum Want {
    Episodes(EpMode),
    Play(String),
}

enum Msg {
    /// a list's refreshed shows
    Shows(u64, Result<Vec<Media>, String>, lists::Ask),
    /// the next page of a ranking
    More(u64, Result<Vec<Media>, String>),
    Hits(u64, Result<Vec<String>, String>),
    Viewer(Result<(u64, String), String>),
    /// prepared poster: pixel file, pixel size, size in cells
    Cover(u64, CoverKey, Result<(PathBuf, u32, u32, Size), String>),
    EpInfo(u64, HashMap<String, EpInfo>),
    /// sequels of a show, looked up for its episode counts (App::parts_key), login and
    /// LIST_CHANGES
    Parts(u64, (u32, u32), Option<String>, u64, Result<Parts, String>),
    /// a lookup's answer, with its ticket (Sources::ask)
    Resolved(Media, u64, Result<Source, String>),
    Saved(Media, Result<Option<Entry>, String>),
    /// progress not saved, as AniList's entry moved since the question was asked: (the
    /// entry's media, the entry now)
    Moved(Media, Option<Entry>),
    /// (media, episode, what it waits on now; None = the player is up)
    PlayStep(u64, String, Option<&'static str>),
    Played(Media, String, Result<(), String>),
    /// an episode downloaded or failed: ("Show E3", result)
    DlDone(String, Result<(), String>),
    /// (version, update command) of a newer ani-hop
    Release(Option<(String, String)>),
}

/// The episode being started or watched.
pub struct Playing {
    pub id: u64,
    pub ep: String,
    pub title: String,
    /// what it waits on, None once mpv shows it
    pub step: Option<&'static str>,
    /// when enter was pressed, for the pop-up
    pub since: Instant,
    pub color: Option<String>,
}

/// Where an episode stands with downloads.
#[derive(PartialEq)]
enum Dl {
    Missing,
    /// in this ani-hop's queue, maybe downloading right now
    Queued,
    OnDisk(PathBuf),
    /// queued by another running ani-hop
    Elsewhere,
}

/// One show's episodes that aren't missing.
struct Downloads {
    on_disk: Vec<(String, PathBuf)>,
    queued: Vec<String>,
    elsewhere: Vec<String>,
}

impl Downloads {
    fn state(&self, ep: &str) -> Dl {
        if let Some((_, p)) = self.on_disk.iter().find(|(e, _)| e == ep) {
            Dl::OnDisk(p.clone())
        } else if self.queued.iter().any(|e| e == ep) {
            Dl::Queued
        } else if self.elsewhere.iter().any(|e| e == ep) {
            Dl::Elsewhere
        } else {
            Dl::Missing
        }
    }

    /// `eps` sorted by what enter can do with them: download, stop, delete (with the file).
    fn split(&self, eps: &[String]) -> (Vec<String>, Vec<String>, Vec<(String, PathBuf)>) {
        let (mut get, mut stop, mut delete) = (Vec::new(), Vec::new(), Vec::new());
        for ep in eps {
            match self.state(ep) {
                Dl::Missing => get.push(ep.clone()),
                Dl::Queued => stop.push(ep.clone()),
                Dl::OnDisk(p) => delete.push((ep.clone(), p)),
                Dl::Elsewhere => {}
            }
        }
        (get, stop, delete)
    }
}

const SEARCHING: &str = "searching ani-cli";

pub struct App {
    pub stack: Vec<Screen>,
    next_id: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Client,
    pub user: Option<(u64, String)>,
    /// pixels per cell, read from the terminal every frame (font zoom changes it)
    pub cell: (u16, u16),
    next_image: u32,
    /// posters handed over as files: (file, when, media id); still there later = unreadable
    sent: Vec<(PathBuf, Instant, u64)>,
    /// the terminal didn't read a poster file, so posters are off for this session
    no_graphics: bool,
    /// AniList list changes not answered yet
    saves_pending: usize,
    pub covers: HashMap<u64, Cover>,
    /// episode titles and summaries per show (empty while loading or when there are none)
    pub ep_info: HashMap<u64, HashMap<String, EpInfo>>,
    /// AniList sequels per show that its provider entry runs on into, each with the episodes
    /// before it (empty while loading or when there are none), and the episode counts they
    /// were looked up for
    parts: HashMap<u64, ((u32, u32), Parts)>,
    /// poster size in cells, set by the last layout
    pub poster: Size,
    pub sources: Sources,
    /// downloaded episodes per show, rescanned on refresh
    pub local: HashMap<u64, Vec<(String, PathBuf)>>,
    /// episodes per show other running ani-hops have queued, rescanned on refresh
    elsewhere: HashMap<u64, Vec<String>>,
    resolving: HashMap<u64, Option<Want>>,
    pub downloads: Queue,
    pub playing: Option<Playing>,
    pub toast: Option<(String, bool, Instant)>,
    pub started: Instant,
    /// media whose title glint plays, and since when
    pub glint: (u64, Instant),
    /// the show and episode in the preview, and how many pages down it's read; another
    /// starts at its top
    pub scroll: (Option<(u64, Option<String>)>, u16),
    pub missing: Vec<(&'static str, &'static str, &'static str)>,
    /// the installed ani-cli's version, when it's older than ani::TESTED_ANI_CLI
    pub old_ani_cli: Option<String>,
    /// (version, update command) of a newer ani-hop release
    pub release: Option<(String, String)>,
    /// shell command to run in the real terminal (TUI paused), e.g. installing packages
    external: Option<String>,
    /// asked for on a show's page while ani-cli searched, waiting to get back to that page
    parked: Option<(Media, Want)>,
    quit: bool,
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
        Some(arg) => {
            eprintln!("ani-hop: unknown option {arg}\n{usage}");
            std::process::exit(2);
        }
    }
    ani::setup()?;
    kitty::init();
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty graphics only (Kitty and Ghostty); the query is just for font size and o=z support.

    let mut app = App::new();
    // every way out stops the download, panics too: a left-behind ani-cli would finish the
    // episode with ffmpeg, as a file that looks complete
    struct StopDownload(Queue);
    impl Drop for StopDownload {
        fn drop(&mut self) {
            self.0.quit();
        }
    }
    let _stop = StopDownload(app.downloads.clone());
    let res = (|| -> std::io::Result<()> {
        let mut dirty = true;
        while !app.quit {
            let (fx, glint) = (ui::effects_running(&app), app.glint.1);
            if dirty || app.animating() {
                terminal.draw(|f| ui::render(&mut app, f))?;
            }
            // one more frame after an effect ends (or starts in a slow frame), so it never
            // freezes mid-sweep
            dirty = fx || app.glint.1 != glint;
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
            dirty |= app.check_sent();
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
    app.forget_posters();
    kitty::shutdown();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
}

/// The newest release's version, if it's newer than this ani-hop. GitHub is asked at most
/// once a day, through the redirect of /releases/latest (no API, so no rate limit).
fn newer_release() -> Option<String> {
    let seen = ani::cache_dir().join("latest-release");
    let day = Duration::from_secs(24 * 60 * 60);
    let fresh = fs::metadata(&seen)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < day));
    let tag = match fresh {
        true => fs::read_to_string(&seen).ok()?,
        false => {
            let to = anilist::location("https://github.com/ReynkeDeVos/ani-hop/releases/latest");
            // .../releases/tag/v0.6.0; before the first release just .../releases
            let tag = to.ok()?.rsplit('/').next()?.to_string();
            let _ = fs::write(&seen, &tag);
            tag
        }
    };
    let v = tag.trim().trim_start_matches('v');
    ani::older(env!("CARGO_PKG_VERSION"), v).then(|| v.to_string())
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

fn client_id() -> Option<String> {
    fs::read_to_string(ani::config_dir().join("client_id"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    fs::create_dir_all(ani::config_dir())?;
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(text.as_bytes())
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

/// Cover from the disk cache (or AniList), scaled and written out for the terminal.
fn load_cover(
    id: u64,
    url: &str,
    (size, cell): CoverKey,
) -> Result<(PathBuf, u32, u32, Size), String> {
    let path = ani::cache_dir().join("covers").join(format!("{id}.jpg"));
    let bytes = fs::read(&path).or_else(|_| {
        let bytes = anilist::fetch(url)?;
        let _ = fs::create_dir_all(path.parent().unwrap());
        let _ = fs::write(&path, &bytes);
        Ok::<_, String>(bytes)
    })?;
    kitty::prepare(&bytes, size, cell)
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
        // one from before air dates fetches again, but still stands in when that fails
        Some(eps) if age.as_secs() < days * 86_400 && eps.values().all(|e| e.aired.is_some()) => {
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

impl App {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let token = fs::read_to_string(token_path())
            .ok()
            .map(|t| t.trim().to_string());
        let user: Option<(u64, String, bool)> = ani::read_json(&user_path());
        let user = user.filter(|_| token.is_some());
        if let Some((_, _, english)) = &user {
            anilist::ENGLISH_TITLES.store(*english, Ordering::Relaxed);
        }
        let client = Client { token };
        let done = tx.clone();
        let downloads = Queue::start(
            ani::cache_dir().join("queue"),
            std::process::id().to_string(),
            AniCli,
            move |label, res| _ = done.send(Msg::DlDone(label, res)),
        );
        let mut app = App {
            stack: Vec::new(),
            next_id: 0,
            downloads,
            tx,
            rx,
            client,
            user: user.map(|(id, name, _)| (id, name)),
            cell: kitty::cell_px(),
            // instances sharing one terminal (tmux panes) must not reuse each other's ids
            next_image: std::process::id().wrapping_mul(2_654_435_761) & 0xFF_FFFF,
            sent: Vec::new(),
            no_graphics: false,
            saves_pending: 0,
            covers: HashMap::new(),
            ep_info: HashMap::new(),
            parts: HashMap::new(),
            poster: Size::new(0, 0),
            sources: Sources::load(),
            local: HashMap::new(),
            elsewhere: HashMap::new(),
            resolving: HashMap::new(),
            playing: None,
            toast: None,
            started: Instant::now(),
            glint: (0, Instant::now()),
            scroll: (None, 0),
            missing: ani::missing_deps(),
            old_ani_cli: ani::old_ani_cli(),
            release: None,
            external: None,
            parked: None,
            quit: false,
        };
        app.scan_local();
        app.push("ani-hop", View::Main);
        let welcomed = ani::config_dir().join("welcomed");
        let first = !welcomed.exists();
        // ani-skip is optional (and Homebrew has none), so it alone doesn't bring this up
        let needed = app.missing.iter().any(|d| d.0 != "ani-skip");
        if first || needed || app.old_ani_cli.is_some() {
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
        if app.client.token.is_some() {
            let c = app.client.clone();
            app.spawn(move || Msg::Viewer(c.viewer()));
        }
        app.spawn(|| Msg::Release(newer_release().map(|v| (v, ani::update_command()))));
        app
    }

    fn after_install(&mut self) {
        self.missing = ani::missing_deps();
        self.old_ani_cli = ani::old_ani_cli();
        // ani-skip is optional and Homebrew has none, so it's no reason for an error
        let names: Vec<&str> = self
            .missing
            .iter()
            .map(|d| d.0)
            .filter(|&n| n != "ani-skip")
            .collect();
        match (names.is_empty(), &self.old_ani_cli) {
            (false, _) => self.toast(format!("Still missing: {}", names.join(", ")), true),
            (true, Some(v)) => self.toast(
                format!(
                    "ani-cli is still {v}, ani-hop needs {}",
                    ani::TESTED_ANI_CLI
                ),
                true,
            ),
            (true, None) => self.toast("All set: everything ani-hop needs is installed", false),
        }
        self.refresh();
    }

    fn log_out(&mut self) {
        let _ = fs::remove_file(token_path());
        let _ = fs::remove_file(user_path());
        (self.client.token, self.user) = (None, None);
        self.parts.clear(); // they carry this login's progress
    }

    /// Back to the main menu; an unanswered resume question stays.
    fn back_to_main(&mut self) {
        self.stack
            .retain(|s| matches!(s.view, View::Main | View::Confirm(Pending::Resume(..))));
    }

    pub fn top(&mut self) -> &mut Screen {
        self.stack.last_mut().unwrap()
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

    /// The episode downloading, or paused, when ffmpeg has it, which can't go on from what it
    /// got: "Episode 3", or "Show E3" when `m`, the show on screen, isn't its show.
    fn restarts(&self, m: &Media) -> Option<String> {
        let n = self.downloads.snapshot().now.filter(|n| !n.resumes)?;
        Some(match n.id == m.id {
            true => format!("Episode {}", n.ep),
            false => n.label,
        })
    }

    /// Whether episode `ep` of `m` is the one downloading, not paused.
    fn downloading(&self, m: &Media, ep: &str) -> bool {
        let s = self.downloads.snapshot();
        !s.paused && s.now.is_some_and(|n| n.id == m.id && n.ep == ep)
    }

    /// Whether enter on `stop` (queued episodes of `m`) is about pausing or resuming them all:
    /// on the one downloading, or with downloads paused, on any queued one.
    fn holds(&self, m: &Media, stop: &[String]) -> bool {
        let s = self.downloads.snapshot();
        match s.paused {
            true => !stop.is_empty(),
            false => s.now.is_some_and(|n| n.id == m.id && stop.contains(&n.ep)),
        }
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
        let (get, stop, delete) = self.downloads(&m).split(&eps);
        let hold = self.holds(&m, &stop);
        let action = match (chosen, get.is_empty(), stop.is_empty(), delete.is_empty()) {
            (Some(a), ..) => a,
            (None, ..) if hold => {
                let title = match eps.as_slice() {
                    [one] => format!("Episode {one}"),
                    _ => plural(eps.len(), "episode"),
                };
                self.push(title, View::DlChoice(m, eps));
                return;
            }
            (None, false, true, true) => "download",
            (None, true, false, true) => "stop",
            (None, true, true, false) => "delete",
            (None, true, true, true) => {
                let it = if eps.len() == 1 { "it" } else { "them" };
                return self.toast(format!("Another ani-hop has {it} queued"), false);
            }
            (None, ..) => {
                self.push(plural(eps.len(), "episode"), View::DlChoice(m, eps));
                return;
            }
        };
        if action == "delete" && !delete.is_empty() {
            let (names, paths): (Vec<String>, Vec<PathBuf>) = delete.into_iter().unzip();
            let title = format!("Delete {}?", which(&names));
            self.push(title, View::Confirm(Pending::Delete(paths)));
            return;
        }
        // ffmpeg can't go on from what it got, so pausing loses that, and asks like stopping
        if action == "pause"
            && let Some(which) = self.restarts(&m)
            && let Some(ep) = self.downloads.snapshot().now.map(|n| n.ep)
        {
            let title = format!("Pause downloads? {which} starts over when resumed");
            self.push(title, View::Confirm(Pending::Pause(m, ep)));
            return;
        }
        // asks when that deletes what they got, so no stray enter or → can
        if action == "stop" && !ani::parts(&ani::folder(&m), &stop).is_empty() {
            let title = format!("Stop downloading {}?", which(&stop));
            self.push(title, View::Confirm(Pending::Stop(m, stop)));
            return;
        }
        if chosen.is_some() {
            self.pop(1); // back on the list, which shows what changed
        }
        // the question may be older than the states it was about
        if action == "download" && !get.is_empty() {
            let lookup = self.sources.lookup(m.id);
            // paused with something held, not just a pause left over from an emptied queue
            let s = self.downloads.snapshot();
            let going = match s.paused && s.pending > 0 {
                true => " · resuming downloads",
                false => "",
            };
            let queued = format!("Queued {}{going}", which(&get));
            let mut job = Job::new(m, Some(get));
            job.lookup = lookup;
            if self.download(job) {
                self.toast(queued, false);
            }
        } else if action == "stop" && !stop.is_empty() {
            self.toast(format!("Stopped downloading {}", which(&stop)), false);
            self.stop(&m, &stop);
        } else if action == "pause" && hold && !self.downloads.snapshot().paused {
            // the one it was offered for (holds: now); one that started since may be ffmpeg's,
            // whose pause asks first
            let ep = self.downloads.snapshot().now.map(|n| n.ep);
            if self
                .downloads
                .pause(m.id, ep.as_deref().unwrap_or_default())
            {
                self.toast(
                    "Paused downloads · enter on a queued episode resumes them",
                    false,
                );
            }
        } else if action == "resume" && self.downloads.snapshot().paused {
            self.downloads.resume();
            self.toast("Resuming downloads", false);
        }
        self.top().menu.marked.clear();
        self.refresh();
    }

    /// What enter does on the Downloads list, for the footer.
    pub fn dl_verb(&self) -> &'static str {
        let screen = self.stack.last().unwrap();
        let m = match &screen.view {
            View::Episodes(m, EpMode::Download) => m,
            View::Episodes(_, EpMode::Play) => return "play",
            _ => return "select",
        };
        let eps: Vec<String> = (screen.menu.selection().iter())
            .map(|&i| screen.menu.items[i].key.clone())
            .collect();
        let (get, stop, delete) = self.downloads(m).split(&eps);
        match (get.is_empty(), stop.is_empty(), delete.is_empty()) {
            _ if self.holds(m, &stop) && self.downloads.snapshot().paused => "resume…",
            _ if self.holds(m, &stop) => "pause…",
            (false, true, true) => "download",
            (true, false, true) => "stop",
            (true, true, false) => "delete",
            (true, true, true) => "select",
            _ => "choose",
        }
    }

    fn spawn(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        thread::spawn(move || _ = tx.send(f()));
    }

    fn toast(&mut self, text: impl Into<String>, err: bool) {
        self.toast = Some((text.into(), err, Instant::now()));
    }

    fn push(&mut self, title: impl Into<String>, view: View) -> u64 {
        let multi = matches!(view, View::Shows(..) | View::Episodes(_, EpMode::Download));
        self.scan_local();
        let menu = Menu::new(self.items(&view), multi);
        self.next_id += 1;
        self.stack.push(Screen {
            id: self.next_id,
            title: title.into(),
            view,
            menu,
            loading: false,
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
        let Some(ask) = ask else {
            return;
        };
        self.top().loading = true;
        self.spawn(move || Msg::Shows(id, f(), ask));
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
    }

    /// Rebuilds the visible menu from current state (keeps query and cursor).
    fn refresh(&mut self) {
        self.scan_local();
        if let View::Shows(list) = &mut self.top().view
            && list.is_local()
        {
            list.shows = ani::local_shows().into_iter().map(|(m, _)| m).collect();
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
            View::Confirm(Pending::Pause(m, ep)) => !self.downloading(m, ep),
            _ => false,
        };
        if stale {
            return self.pop(1);
        }
        self.top().menu.set_items(items);
    }

    fn scan_local(&mut self) {
        self.local = ani::local_shows()
            .into_iter()
            .map(|(m, p)| (m.id, ani::local_episodes(&p)))
            .collect();
        self.elsewhere = self.downloads.elsewhere();
    }

    /// Where `m`'s episodes stand, as of the last rescan and the queue now.
    fn downloads(&self, m: &Media) -> Downloads {
        let on_disk = self.local.get(&m.id).cloned().unwrap_or_default();
        let queued = (self.downloads).queued(m, self.sources.get(m.id), &on_disk);
        let elsewhere = self.elsewhere.get(&m.id).cloned().unwrap_or_default();
        Downloads {
            on_disk,
            queued,
            elsewhere,
        }
    }

    /// Something on screen changes without input (spinner, progress, fading toast).
    fn animating(&self) -> bool {
        ui::effects_running(self)
            || self.stack.last().unwrap().loading
            || self.downloads.snapshot().pending > 0 && !self.downloads.snapshot().paused
            || self
                .toast
                .as_ref()
                .is_some_and(|t| t.2.elapsed().as_secs() < 5)
    }

    /// Starts loading a poster at the current size. `urgent` (the one on screen) skips the
    /// queue; look-ahead ones wait while 4 are in flight and get asked for again next frame.
    pub fn want_cover(&mut self, m: &Media, urgent: bool) {
        let (size, key) = (self.poster, (self.poster, self.cell));
        let fresh = match self.covers.get(&m.id) {
            Some(Cover::Loading(k) | Cover::Ready(k, _)) => *k == key,
            Some(Cover::Failed) => true,
            None => false,
        };
        let busy = self
            .covers
            .values()
            .filter(|c| matches!(c, Cover::Loading(_)))
            .count();
        if fresh || size.width == 0 || (busy >= 4 && !urgent) {
            return;
        }
        // nobody would read (and delete) the pixel files; the cover color stands in
        if !kitty::supported() || self.no_graphics {
            self.covers.insert(m.id, Cover::Failed);
            return;
        }
        // ponytail: the terminal keeps ~3 MB per poster; past 60 they all go and reload from
        // the disk cache in ms
        if self.covers.len() > 60 {
            self.forget_posters();
        }
        if let Some(Cover::Ready(_, p)) = self.covers.get(&m.id) {
            kitty::delete(p.id); // resized: the old one is never drawn again
        }
        match m.cover_image.extra_large.clone() {
            Some(url) => {
                self.covers.insert(m.id, Cover::Loading(key));
                let id = m.id;
                self.spawn(move || Msg::Cover(id, key, load_cover(id, &url, key)));
            }
            None => _ = self.covers.insert(m.id, Cover::Failed),
        }
    }

    /// Episode titles and summaries, once a session per show, and the sequels the provider
    /// runs on into once it lists more episodes than AniList.
    pub fn want_episodes(&mut self, m: &Media) {
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
            self.parts.insert(m.id, (key, Vec::new()));
            let (m, c) = (m.clone(), self.client.clone());
            let asked = (c.token.clone(), lists::changes());
            self.spawn(move || {
                let parts = c.parts(&m, key.0);
                Msg::Parts(m.id, key, asked.0, asked.1, parts)
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
            Some((key, parts)) if Some(*key) == self.parts_key(m) => parts,
            _ => &[],
        }
    }

    /// `m`'s episodes as AniList counts them, with the parts known so far.
    pub fn progress<'a>(&'a self, m: &'a Media) -> Progress<'a> {
        Progress::of(m, self.parts(m))
    }

    /// What episode `ep` of `m` is called and about.
    pub fn ep_about(&self, m: &Media, ep: &str) -> Option<&EpInfo> {
        let (p, n) = self.progress(m).entry(ep);
        self.ep_info.get(&p.id)?.get(&n)
    }

    /// Frees every poster in the terminal.
    fn forget_posters(&mut self) {
        for c in self.covers.values() {
            if let Cover::Ready(_, p) = c {
                kitty::delete(p.id);
            }
        }
        self.covers.clear();
        self.sent.clear();
        kitty::cleanup(); // files of the in-flight ones come back unwanted and go too
    }

    /// The terminal deletes a poster file once it read it. One still around after 2 s means
    /// it never will (tmux without passthrough, ssh, ...): fall back to the cover colors.
    fn check_sent(&mut self) -> bool {
        let old = |s: &(PathBuf, Instant, u64)| s.1.elapsed() > Duration::from_secs(2);
        if !self.sent.iter().any(|s| old(s) && s.0.exists()) {
            self.sent.retain(|s| !old(s));
            return false;
        }
        self.no_graphics = true;
        self.forget_posters();
        self.toast(
            "This terminal didn't take the posters, showing colors instead",
            true,
        );
        true
    }

    fn items(&self, view: &View) -> Vec<Item> {
        match view {
            View::Main => {
                let list = |(s, l): &(&str, &str)| {
                    Item::new(format!("list:{s}"), format!("My list · {l}"), "")
                };
                let mut v = Vec::new();
                // what you're watching goes first, so enter-enter-enter continues it
                if self.user.is_some() {
                    v.extend(LISTS[..2].iter().map(list));
                }
                v.extend([
                    Item::new("browse:TRENDING_DESC", "Trending now", ""),
                    Item::new("browse:SCORE_DESC", "Top rated", ""),
                    Item::new("browse:POPULARITY_DESC", "All-time popular", ""),
                    Item::new("search", "Search…", ""),
                ]);
                if self.user.is_some() {
                    v.extend(LISTS[2..].iter().map(list));
                }
                let local = self.local.len();
                let shows = if local > 0 {
                    plural(local, "show")
                } else {
                    String::new()
                };
                v.push(Item::new("downloads", "Downloads", shows));
                // who's logged in is in the header; logging in starts here
                let login = if self.user.is_none() {
                    "not logged in"
                } else {
                    ""
                };
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
                    "audio",
                    "Default audio…",
                    ani::audio_label(ani::audio(None)),
                ),
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
                if let Some(cmd) = ani::ani_cli_update().filter(|_| self.old_ani_cli.is_some()) {
                    v.push(Item::new("update", "Update ani-cli", cmd));
                }
                if self.user.is_none() {
                    v.push(Item::new("login", "Log in to AniList…", ""));
                }
                v.push(Item::new("continue", "Continue", ""));
                v
            }
            View::Shows(list) => {
                let mut v: Vec<Item> = (list.shows.iter())
                    .map(|m| {
                        let mut hint = Vec::new();
                        if let Some(n) = self.local.get(&m.id).map(Vec::len).filter(|n| *n > 0) {
                            hint.push(format!("↓ {n}"));
                        }
                        if m.media_list_entry.is_some() {
                            hint.push(format!(
                                "{}/{}",
                                m.progress(),
                                m.episodes.map_or("?".into(), |e| e.to_string())
                            ));
                        } else if let Some(e) = m.episodes {
                            hint.push(format!("{e} ep"));
                        }
                        hint.extend(m.season_year.map(|y| y.to_string()));
                        Item::new(m.id.to_string(), m.name(), hint.join(" · "))
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
                let dl = self.downloads(m);
                let src = self.sources.get(m.id);
                let (next, released) = (self.progress(m).next(), self.progress(m).released());
                let mut v = Vec::new();
                if released.is_none_or(|r| next <= r) {
                    let verb = if next > 1 { "Continue with" } else { "Play" };
                    let on_disk = (dl.on_disk.iter()).any(|(e, _)| ani::ep_num(e) == next as f64);
                    v.push(Item::new(
                        "next",
                        format!("▶  {verb} episode {next}"),
                        if on_disk { "on disk" } else { "" },
                    ));
                }
                let avail = match src {
                    Some(s) => format!("{} available", s.episodes.len()),
                    None if self.resolving.contains_key(&m.id) => "searching…".into(),
                    None => String::new(),
                };
                v.push(Item::new("play", "≡  Episodes…", avail));
                let mut state = Vec::new();
                if !dl.queued.is_empty() {
                    let how = match self.downloads.snapshot().paused {
                        true => "paused",
                        false => "queued",
                    };
                    state.push(format!("{} {how}", dl.queued.len()));
                }
                if !dl.on_disk.is_empty() {
                    state.push(format!("{} on disk", dl.on_disk.len()));
                }
                v.push(Item::new("download", "↓  Downloads…", state.join(" · ")));
                if self.user.is_some() && m.on_anilist() {
                    let status = m
                        .media_list_entry
                        .as_ref()
                        .map(|e| list_label(&e.status))
                        .unwrap_or("not on list");
                    v.push(Item::new("status", "●  List status…", status));
                    let total = m.episodes.map_or("?".into(), |t| t.to_string());
                    let progress = format!("{}/{total}", m.progress());
                    v.push(Item::new("watched", "✓  Watched up to…", progress));
                }
                v.push(Item::new(
                    "audio",
                    "♪  Audio…",
                    ani::audio_label(ani::audio(Some(m.id))),
                ));
                v.push(Item::new(
                    "source",
                    "⇄  Change source…",
                    src.map(|s| s.title.clone()).unwrap_or_default(),
                ));
                if m.on_anilist() {
                    v.push(Item::new("web", "↗  Open on AniList", ""));
                }
                v
            }
            View::Episodes(m, mode) => {
                let dl = self.downloads(m);
                let run = self.progress(m);
                // what a watched episode is called and about; nothing that spoils the rest
                let item = |key: &str, ep: &str, hint: String| {
                    let mut it = Item::new(key, format!("Episode {ep}"), hint);
                    it.done = run.watched(ep);
                    if let Some(i) = self.ep_about(m, ep).filter(|_| it.done) {
                        if !i.title.is_empty() {
                            it.label = format!("{} · {}", it.label, i.title);
                        }
                        it.about = i.about.clone();
                    }
                    it.group = run.group(ep).to_string();
                    it
                };
                // also what the provider no longer lists, so it can still be played or deleted
                let mut eps: Vec<String> = self
                    .sources
                    .get(m.id)
                    .map(|s| s.episodes.clone())
                    .unwrap_or_default();
                let more = (dl.on_disk.iter().map(|(e, _)| e)).chain(&dl.queued);
                let more: Vec<String> = more.filter(|e| !eps.contains(e)).cloned().collect();
                eps.extend(more);
                eps.sort_by(|a, b| ani::ep_num(a).total_cmp(&ani::ep_num(b)));
                eps.dedup();
                // the one downloading shows its progress in place of "queued" (ui.rs)
                let hint = |ep: &str| match (dl.state(ep), mode) {
                    (Dl::OnDisk(p), EpMode::Download) => format!("on disk · {}", megabytes([&p])),
                    (Dl::OnDisk(_), _) => "on disk".into(),
                    (Dl::Queued, _) => "queued".into(),
                    (Dl::Elsewhere, _) => "queued in another ani-hop".into(),
                    (Dl::Missing, _) => String::new(),
                };
                eps.iter().map(|ep| item(ep, ep, hint(ep))).collect()
            }
            View::DlChoice(m, eps) => {
                let (get, stop, delete) = self.downloads(m).split(eps);
                let mut v = Vec::new();
                // under the cursor, so → twice only pauses, and → twice again resumes
                if self.holds(m, &stop) {
                    let over = self.restarts(m).map_or("", |_| "from the start");
                    v.push(match self.downloads.snapshot().paused {
                        true => Item::new("resume", "▶  Resume downloads", over),
                        false => Item::new("pause", "‖  Pause downloads", ""),
                    });
                }
                if !get.is_empty() {
                    let label = format!("↓  Download {}", which(&get));
                    v.push(Item::new("download", label, ""));
                }
                if !stop.is_empty() {
                    // the title says which when it's one
                    let which = if eps.len() == 1 {
                        String::new()
                    } else {
                        format!(" {}", which(&stop))
                    };
                    let label = format!("■  Stop downloading{which}");
                    v.push(Item::new(
                        "stop",
                        label,
                        deletes(&ani::parts(&ani::folder(m), &stop)),
                    ));
                }
                if !delete.is_empty() {
                    let size = megabytes(delete.iter().map(|(_, p)| p));
                    let names: Vec<String> = delete.into_iter().map(|(e, _)| e).collect();
                    v.push(Item::new(
                        "delete",
                        format!("✗  Delete {}", which(&names)),
                        size,
                    ));
                }
                v
            }
            View::Status(m) => {
                let cur = m.media_list_entry.as_ref().map(|e| e.status.as_str());
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
                    .map(|(k, label)| {
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
                // as the downloader decides: episodes past each show's progress
                let which = if list.iter().any(|m| m.progress() > 0) {
                    "unwatched"
                } else {
                    "all"
                };
                let mut v = vec![Item::new(
                    "download",
                    format!(
                        "↓  Download {which} episodes of {}",
                        plural(list.len(), "show")
                    ),
                    "",
                )];
                if local > 0 {
                    v.push(Item::new(
                        "delete",
                        format!("✗  Delete downloads of {}", plural(local, "show")),
                        "",
                    ));
                }
                v
            }
            // No discards what was downloaded so far; esc asks again next start
            View::Confirm(Pending::Resume(..)) => vec![
                Item::new("no", "No", "deletes partial files"),
                Item::new("yes", "Yes", ""),
            ],
            // what it saves, as that can be the entry's last (the second half of a special)
            // or a part's
            View::Confirm(Pending::Progress(mark)) => {
                vec![
                    Item::new("no", "No", ""),
                    Item::new("yes", "Yes", mark.hint()),
                ]
            }
            View::Confirm(Pending::Stop(m, eps)) => {
                let hint = deletes(&ani::parts(&ani::folder(m), eps));
                vec![Item::new("no", "No", ""), Item::new("yes", "Yes", hint)]
            }
            View::Confirm(Pending::Quit) => {
                // when it asks about downloads, which it doesn't while they're paused
                let s = self.downloads.snapshot();
                let hint = match s.pending > 0 && !s.paused {
                    true => "kept for next start",
                    false => "",
                };
                vec![Item::new("no", "No", ""), Item::new("yes", "Yes", hint)]
            }
            View::Confirm(_) => vec![Item::new("no", "No", ""), Item::new("yes", "Yes", "")],
            View::Input(_) => Vec::new(),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
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
            let n = &mut self.scroll.1;
            *n = match k.code {
                KeyCode::Up => n.saturating_sub(1),
                _ => n.saturating_add(1),
            };
            return;
        }
        match self.top().menu.key(k) {
            Some(Act::Accept) => self.accept(),
            Some(Act::Back) => self.pop(1),
            None => {}
        }
    }

    fn request_quit(&mut self) {
        // paused, nothing runs, and the queue keeps them for the next start
        let s = self.downloads.snapshot();
        let downloads = if s.paused { 0 } else { s.pending };
        let busy = downloads > 0 || self.saves_pending > 0;
        if busy && !matches!(self.top().view, View::Confirm(Pending::Quit)) {
            let mut left = Vec::new();
            if downloads > 0 {
                left.push(plural(downloads, "download"));
            }
            if self.saves_pending > 0 {
                left.push(plural(self.saves_pending, "AniList save"));
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
                "install" | "update" => {
                    self.external = screen.menu.current().map(|i| i.hint.clone())
                }
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
                    self.top().menu.marked.clear();
                    self.open_show(m);
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
                    EpMode::Watched => {
                        let title = format!("Watched up to episode {key} of {}?", m.name());
                        let mark = self.progress(&m).mark(&key);
                        self.ask_progress(&key, mark, title);
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
                let (m, c) = (m.clone(), self.client.clone());
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
                self.saves_pending += 1;
                self.spawn(move || Msg::Saved(m.clone(), c.save(m.id, Some(&key), None).map(Some)));
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
                    let shows = plural(list.len(), "show");
                    for m in list {
                        let mut job = Job::new(m, None);
                        job.parts = self.parts(&job.media).to_vec();
                        job.lookup = self.sources.lookup(job.media.id);
                        if !self.download(job) {
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
                        let what = if paths.iter().any(|p| p.is_dir()) {
                            "show"
                        } else {
                            "episode"
                        };
                        let mut failed = 0;
                        for p in paths {
                            let res = if p.is_dir() {
                                ani::delete_show(p)
                            } else {
                                ani::delete_episode(p)
                            };
                            failed += res.is_err() as usize;
                        }
                        let n = paths.len();
                        let done = match failed {
                            0 => format!("Deleted {}", plural(n, what)),
                            _ => format!("Deleted {} of {}", n - failed, plural(n, what)),
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
                            self.toast(
                                "Paused downloads · enter on a queued episode resumes them",
                                false,
                            );
                        }
                        self.pop(1);
                        if matches!(self.top().view, View::DlChoice(..)) {
                            self.pop(1);
                        }
                    }
                    Pending::Stop(m, eps) => {
                        // what finished or was stopped meanwhile stays as it is
                        let (m, (_, stop, _)) = (m.clone(), self.downloads(m).split(eps));
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
                        let (m, c) = (m.clone(), self.client.clone());
                        self.pop(1);
                        let Some(entry) = m.media_list_entry.as_ref().map(|e| e.id) else {
                            return;
                        };
                        self.toast(format!("Removing {}…", m.name()), false);
                        self.saves_pending += 1;
                        self.spawn(move || Msg::Saved(m, c.remove(entry).map(|_| None)));
                    }
                    Pending::Progress(k) => {
                        let (k, c) = (k.clone(), self.client.clone());
                        self.saves_pending += 1;
                        // the question showed `was`; Yes mustn't undo a save it didn't know of
                        // (an old copy: a cached list, the episode's start, a late lookup)
                        self.spawn(move || {
                            let m = k.entry;
                            match c.save_progress(m.id, k.status, k.was, k.to) {
                                Ok(Ok(entry)) => Msg::Saved(m, Ok(Some(entry))),
                                Ok(Err(now)) => Msg::Moved(m, now),
                                Err(e) => Msg::Saved(m, Err(e)),
                            }
                        });
                        self.pop(1);
                    }
                }
            }
            View::Input(input) => self.submit(input.clone(), query),
        }
    }

    /// Fetches the next page of the ranking on top, as "More…" and ^f do.
    fn more(&mut self) {
        let screen = self.top();
        let (id, refreshing) = (screen.id, screen.loading);
        let View::Shows(list) = &mut screen.view else {
            return;
        };
        match list.more(refreshing) {
            Ok(Some((sort, page))) => {
                self.top().loading = true;
                let c = self.client.clone();
                self.spawn(move || Msg::More(id, c.browse(&sort, None, page)));
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
        if let Some(sort) = key.strip_prefix("browse:") {
            let sort = sort.to_string();
            let cache = self.ranking_key(&sort);
            let feed = Feed::Pages(sort.clone(), Some(2), false);
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
                let list = ani::local_shows().into_iter().map(|(m, _)| m).collect();
                self.push("Downloads", View::Shows(List::local(list)));
            }
            "login" => match client_id() {
                Some(id) => self.start_auth(&id),
                None => {
                    open_url("https://anilist.co/settings/developer");
                    self.push("AniList setup", View::Input(Input::ClientId));
                }
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
                self.log_out();
                self.toast("Logged out", false);
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

    fn start_auth(&mut self, client_id: &str) {
        open_url(&format!(
            "https://anilist.co/api/v2/oauth/authorize?client_id={client_id}&response_type=token"
        ));
        self.push("AniList login", View::Input(Input::Token));
    }

    fn submit(&mut self, input: Input, text: String) {
        if text.is_empty() && !matches!(input, Input::DownloadDir) {
            return;
        }
        match input {
            Input::Search => {
                let c = self.client.clone();
                let title = text.clone();
                self.push_loading(&title, Feed::Whole, None, move || {
                    let found = c.browse("SEARCH_MATCH", Some(&text), 1);
                    source::or_ani_cli(found, &text, source::search)
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
                self.parts.clear();
                let c = self.client.clone();
                self.spawn(move || Msg::Viewer(c.viewer()));
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
                let id = self.push("Pick source", View::Sources(m, text.clone(), Vec::new()));
                self.top().loading = true;
                self.spawn(move || Msg::Hits(id, source::search(&text).map(|r| r.0)));
            }
        }
    }

    fn open_show(&mut self, m: Media) {
        self.resolve(&m, None); // warm up so "Episodes…" opens instantly
        self.push(m.name().to_string(), View::Show(m));
    }

    fn show_action(&mut self, m: Media, key: &str) {
        match key {
            "next" => self.play(m.clone(), self.progress(&m).next().to_string()),
            "play" => self.resolve(&m, Some(Want::Episodes(EpMode::Play))),
            // what's on disk can be deleted before ani-cli answers, or without it
            "download"
                if self.sources.get(m.id).is_none() && !self.downloads(&m).on_disk.is_empty() =>
            {
                self.resolve(&m, None);
                self.fulfill(m, Want::Episodes(EpMode::Download));
            }
            "download" => self.resolve(&m, Some(Want::Episodes(EpMode::Download))),
            "status" => _ = self.push("List status", View::Status(m)),
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

    /// Acts on the known (maybe cached) source right away and re-resolves it in the
    /// background once per session; without one, `want` waits for ani-cli.
    fn resolve(&mut self, m: &Media, want: Option<Want>) {
        let want = match want {
            Some(w) if self.sources.get(m.id).is_some() => {
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
        }
        // none while one runs already
        let Some(ticket) = self.sources.ask(m.id) else {
            return;
        };
        let m = m.clone();
        self.spawn(move || {
            let res = source::resolve(&m);
            Msg::Resolved(m, ticket, res)
        });
    }

    /// Asks before saving the AniList progress up to `ep`, as `mark` would. AniList keeps a
    /// count, not single episodes, so Yes goes first only when that changes no other episode.
    fn ask_progress(&mut self, ep: &str, mark: Option<Mark>, title: String) {
        let Some(mark) = mark else {
            return self.toast(
                format!("Episode {ep} isn't a number AniList can count"),
                true,
            );
        };
        // what else Yes changes goes in the question, which wraps; a hint would be cut
        let title = format!("{title} {}", mark.also()).trim_end().to_string();
        let first = if mark.only_this() { "yes" } else { "no" };
        self.push(title, View::Confirm(Pending::Progress(mark)));
        self.top().menu.select_key(first);
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
            }
        }
    }

    fn play(&mut self, m: Media, ep: String) {
        // a second enter would open a second player
        if let Some(p) = &self.playing
            && p.step.is_some()
        {
            return self.toast(format!("Still starting {}…", p.title), false);
        }
        let local = ani::local_episodes(&ani::folder(&m))
            .into_iter()
            .find(|(e, _)| *e == ep);
        let lookup = self.sources.lookup(m.id);
        let step = match (&local, &lookup) {
            (Some(_), _) => "opening the player",
            (None, Lookup::Resolve) => SEARCHING,
            (_, Lookup::Check(_)) => "checking the source",
            _ => "fetching the stream",
        };
        self.playing = Some(Playing {
            id: m.id,
            ep: ep.clone(),
            // the number first: a long title loses its end, not the episode
            title: format!("Episode {ep} · {}", m.name()),
            step: Some(step),
            since: Instant::now(),
            color: m.cover_image.color.clone(),
        });
        if local.is_none() && matches!(lookup, Lookup::Resolve) {
            return self.resolve(&m, Some(Want::Play(ep)));
        }
        let (tx, id, e) = (self.tx.clone(), m.id, ep.clone());
        let step = move |s| _ = tx.send(Msg::PlayStep(id, e.clone(), s));
        let audio = ani::audio(Some(m.id));
        self.spawn(move || {
            let stream = |s: &Source| match ani::stream(s, &ep, audio, || step(None)) {
                // dubs come later or not at all, the subtitled one is the next best
                Err(e) if ani::dub(audio) && e.contains("dub") => {
                    step(Some("no dub yet, fetching it with subtitles"));
                    ani::stream(s, &ep, "sub", || step(None))
                }
                res => res,
            };
            let res = match (&local, lookup) {
                (Some((_, file)), _) => ani::play_file(&m, &ep, file, || step(None)),
                (_, Lookup::Ready(src)) => stream(&src),
                (_, lookup) => lookup.run(&m).and_then(|s| {
                    step(Some("fetching the stream"));
                    stream(&s)
                }),
            };
            Msg::Played(m, ep, res)
        });
    }

    /// Fills a screen that waited on a background result. On failure it goes away, unless
    /// it already shows something (cached entries stay usable offline).
    fn fill<T>(&mut self, id: u64, res: Result<T, String>, set: impl FnOnce(&mut View, T)) {
        let Some(pos) = self.stack.iter().position(|s| s.id == id) else {
            return;
        };
        self.stack[pos].loading = false;
        match res {
            Ok(data) => {
                set(&mut self.stack[pos].view, data);
                let items = self.items(&self.stack[pos].view);
                self.stack[pos].menu.set_items(items);
            }
            Err(e) => {
                if self.stack[pos].menu.items.is_empty() {
                    self.stack.truncate(pos);
                }
                self.toast(e, true);
            }
        }
    }

    fn update_media(&mut self, id: u64, entry: Option<Entry>) {
        for s in &mut self.stack {
            let media: Vec<&mut Media> = match &mut s.view {
                View::Show(m)
                | View::Episodes(m, _)
                | View::DlChoice(m, _)
                | View::Status(m)
                | View::Sources(m, ..)
                | View::Audio(Some(m))
                | View::Input(Input::SourceQuery(m)) => vec![m],
                View::Shows(l) => l.shows.iter_mut().collect(),
                View::Bulk(l) => l.iter_mut().collect(),
                _ => Vec::new(),
            };
            for m in media {
                if m.id == id {
                    m.media_list_entry = entry.clone();
                }
            }
        }
        for (_, p) in self.parts.values_mut().flat_map(|p| &mut p.1) {
            if p.id == id {
                p.media_list_entry = entry.clone();
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Shows(id, res, ask) => {
                let fallback = res
                    .as_ref()
                    .is_ok_and(|l| l.iter().any(|m| !m.on_anilist()))
                    && self.stack.iter().any(|s| s.id == id);
                self.fill(id, res, |v, shows| {
                    if let View::Shows(list) = v {
                        list.refreshed(ask, shows);
                    }
                });
                if fallback {
                    self.toast("AniList search failed, showing ani-cli's matches", true);
                }
            }
            Msg::More(id, res) => {
                let screen = self.stack.iter_mut().find(|s| s.id == id);
                if let Some(View::Shows(list)) = screen.map(|s| &mut s.view) {
                    list.fetched();
                }
                let mut first = None; // Some(None): the page brought nothing new
                self.fill(id, res, |v, more| {
                    if let View::Shows(list) = v {
                        first = Some(list.more_came(more));
                    }
                });
                match (first, self.stack.iter_mut().find(|s| s.id == id)) {
                    (Some(Some(key)), Some(s)) => s.menu.select_key(&key),
                    (Some(None), Some(_)) => self.toast("No more shows", false),
                    _ => {}
                }
            }
            Msg::Hits(id, res) => self.fill(id, res, |v, hits| {
                if let View::Sources(_, _, h) = v {
                    *h = hits;
                }
            }),
            Msg::Viewer(Ok(user)) => {
                let english = anilist::ENGLISH_TITLES.load(Ordering::Relaxed);
                let _ = write_private(
                    &user_path(),
                    &serde_json::json!([user.0, user.1, english]).to_string(),
                );
                let login = self.user.is_none();
                self.user = Some(user);
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
            Msg::Viewer(Err(e)) => {
                // keep the token on network trouble, drop it when AniList rejects it
                if e.contains("Invalid token") || e.contains("Unauthorized") {
                    self.log_out();
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
            Msg::Parts(id, key, token, changes, parts) => {
                // looked up for other episode counts or login: their progress is stale
                let wanted = self.parts.get(&id).is_some_and(|p| p.0 == key);
                if !wanted || token != self.client.token {
                    return;
                }
                // and from before a save, which they may undo: asked again while it's shown
                if lists::changes() != changes {
                    self.parts.remove(&id);
                    return;
                }
                let parts = parts.unwrap_or_default();
                for (_, p) in &parts {
                    self.want_episodes(p);
                }
                self.parts.insert(id, (key, parts));
                self.refresh();
            }
            Msg::Cover(id, key, res) => {
                let wanted = matches!(self.covers.get(&id), Some(Cover::Loading(k)) if *k == key);
                match res {
                    // sent now, even for look-ahead posters: showing one later is free
                    Ok((file, w, h, cells)) if wanted => {
                        self.next_image = self.next_image % 0xFF_FFFF + 1; // 24-bit ids, never 0
                        kitty::transmit(self.next_image, &file, (w, h), cells);
                        self.sent.push((file, Instant::now(), id));
                        let poster = kitty::Poster {
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
            Msg::Resolved(m, ticket, res) => {
                // a lookup from before the source was changed by hand
                if !self.sources.answer(m.id, ticket, &res) {
                    return;
                }
                let want = self.resolving.remove(&m.id).flatten();
                // the wait is over either way; playing it starts anew below
                let waited = self
                    .playing
                    .take_if(|p| p.id == m.id && p.step == Some(SEARCHING));
                let here = matches!(&self.top().view, View::Show(s) | View::Episodes(s, _) if s.id == m.id);
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
                    Err(e) if want.is_some() || here => {
                        // the provider may call it something else: picking it by hand fixes that
                        let e = match e.starts_with("No results") {
                            true => "no match for this show · try Change source…".into(),
                            false => e,
                        };
                        self.toast(format!("ani-cli: {e}"), true)
                    }
                    Err(_) => {}
                }
                // same pop-up, it doesn't pop in a second time
                if let (Some(w), Some(p)) = (waited, &mut self.playing)
                    && p.id == w.id
                {
                    p.since = w.since;
                }
                self.refresh();
            }
            Msg::Saved(m, res) => {
                self.saves_pending -= 1;
                match res {
                    Ok(entry) => {
                        let text = match &entry {
                            Some(e) => {
                                let total = m.episodes.map_or("?".into(), |t| t.to_string());
                                let list = list_label(&e.status);
                                format!("Saved {} · {list} · {}/{total}", m.name(), e.progress)
                            }
                            None => format!("Removed {} from your list", m.name()),
                        };
                        self.toast(text, false);
                        self.update_media(m.id, entry);
                        lists::changed();
                        if matches!(self.top().view, View::Status(_)) {
                            self.stack.pop();
                        }
                        self.refresh();
                    }
                    Err(e) => self.toast(format!("{} not saved · {e}", m.name()), true),
                }
            }
            Msg::Moved(m, entry) => {
                self.saves_pending -= 1;
                let total = m.episodes.map_or("?".into(), |t| t.to_string());
                let now = entry.as_ref().map_or(0, |e| e.progress);
                let why = format!("AniList has {now}/{total} now, try again");
                self.toast(format!("{} not saved · {why}", m.name()), true);
                // what's on screen was old: AniList's entry, and lists that refresh
                self.update_media(m.id, entry);
                lists::changed();
                self.refresh();
            }
            Msg::PlayStep(id, ep, step) => {
                if let Some(p) = &mut self.playing
                    && p.id == id
                    && p.ep == ep
                {
                    p.step = step;
                }
            }
            Msg::Played(m, ep, res) => {
                if self
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.id == m.id && p.ep == ep)
                {
                    self.playing = None;
                }
                // not watched yet, as far as the show or the part it's in goes
                let mark = self.progress(&m).unwatched(&ep);
                match res {
                    Err(e) => self.toast(format!("Couldn't play episode {ep} · {e}"), true),
                    Ok(()) if mark.is_some() && self.user.is_some() && m.on_anilist() => {
                        let title = format!("Mark episode {ep} of {} as watched?", m.name());
                        self.ask_progress(&ep, mark, title);
                    }
                    Ok(()) => {}
                }
            }
            Msg::DlDone(label, res) => {
                match res {
                    Ok(()) => self.toast(format!("Downloaded {label}"), false),
                    Err(e) => self.toast(format!("{label}: {e}"), true),
                }
                self.refresh();
            }
            Msg::Release(v) => self.release = v,
        }
    }
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// "episode 3", or "4 episodes".
fn which(eps: &[String]) -> String {
    match eps {
        [one] => format!("episode {one}"),
        _ => plural(eps.len(), "episode"),
    }
}

/// What stopping deletes, as "deletes 230 MB", or nothing when nothing's there.
fn deletes(parts: &[PathBuf]) -> String {
    match parts {
        [] => String::new(),
        _ => format!("deletes {}", megabytes(parts)),
    }
}

/// How far a download got, from its last progress line ("45.3% of ~ 529MiB at …", "93% · 26:16
/// of 28:13"), for when it's paused and the rest of that line is stale.
pub fn got_so_far(text: &str) -> Option<String> {
    let pct: f64 = text
        .split_whitespace()
        .next()?
        .strip_suffix('%')?
        .parse()
        .ok()?;
    Some(format!("{}%", pct as u32))
}

/// Size of files on disk, as "910 MB".
fn megabytes<'a>(files: impl IntoIterator<Item = &'a PathBuf>) -> String {
    let bytes: u64 = (files.into_iter())
        .map(|p| fs::metadata(p).map_or(0, |m| m.len()))
        .sum();
    format!("{} MB", bytes >> 20)
}

pub fn list_label(status: &str) -> &'static str {
    LISTS
        .iter()
        .find(|l| l.0 == status)
        .map_or("on list", |l| l.1)
}

#[cfg(test)]
mod tests {
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
    fn a_paused_download_says_how_far_it_got() {
        assert_eq!(
            super::got_so_far("45.3% of ~ 529.62MiB at 4.9MiB/s ETA 01:23").as_deref(),
            Some("45%")
        );
        assert_eq!(
            super::got_so_far("93% · 26:16 of 28:13").as_deref(),
            Some("93%")
        );
        assert_eq!(super::got_so_far("26:16 so far"), None);
        assert_eq!(super::got_so_far("starting"), None);
    }

    #[test]
    fn episodes_get_what_their_download_state_allows() {
        let s = |v: &[&str]| v.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        let file = std::path::PathBuf::from("A Episode 1.mp4");
        let dl = super::Downloads {
            on_disk: vec![("1".into(), file.clone())],
            queued: s(&["1", "2"]),
            elsewhere: s(&["3"]),
        };
        // a finished file wins over a queue entry still naming it
        let (get, stop, delete) = dl.split(&s(&["1", "2", "3", "4"]));
        assert_eq!((get, stop), (s(&["4"]), s(&["2"])));
        assert_eq!(delete, [("1".to_string(), file.clone())]);
    }
}
