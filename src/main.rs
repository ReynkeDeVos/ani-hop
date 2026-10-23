#![allow(clippy::large_enum_variant)] // a handful of screens, size is irrelevant

mod ani;
mod anilist;
mod kitty;
mod menu;
mod ui;

use ani::Source;
use anilist::{Client, Entry, EpInfo, Media, PER_PAGE};
use menu::{Act, Item, Menu};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::Size;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry::Vacant;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
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
    Download,
    Delete,
}

pub enum View {
    Main,
    /// first start or missing tools: package check + login suggestion
    Setup,
    /// switches and the AniList login
    Settings,
    Shows(Vec<Media>, Feed),
    /// audio picker for a show, or the default (None)
    Audio(Option<Media>),
    Show(Media),
    Episodes(Media, EpMode),
    Status(Media),
    Sources(Media, String, Vec<String>),
    Bulk(Vec<Media>),
    Confirm(Pending),
    Input(Input),
}

/// Where a list of shows comes from.
pub enum Feed {
    /// the download folder, rescanned on refresh
    Local,
    /// an AniList list or search, all at once
    Whole,
    /// an AniList ranking (MediaSort) in pages: the next page if there is one, and whether
    /// it's on its way (the first page may refresh on its own, without "More…")
    Pages(String, Option<u32>, bool),
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
    Progress(Media, String),
    Quit,
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    pub media: Media,
    #[serde(skip)]
    source: Option<Source>,
    /// None = everything after the AniList progress
    pub eps: Option<Vec<String>>,
    /// audio when queued: yt-dlp resumes a half-done episode in whatever audio it gets
    /// next, so it must not change in between (None in queues of older versions)
    #[serde(default)]
    dub: Option<bool>,
}

enum Msg {
    Shows(u64, Result<Vec<Media>, String>),
    /// the next page of a ranking
    More(u64, Result<Vec<Media>, String>),
    Hits(u64, Result<Vec<String>, String>),
    Viewer(Result<(u64, String), String>),
    /// prepared poster: pixel file, pixel size, size in cells
    Cover(u64, CoverKey, Result<(PathBuf, u32, u32, Size), String>),
    EpInfo(u64, HashMap<String, EpInfo>),
    Resolved(Media, Result<Source, String>),
    Saved(Media, Result<Option<Entry>, String>),
    /// (media, episode, what it waits on now; None = the player is up)
    PlayStep(u64, String, Option<&'static str>),
    Played(Media, String, Result<(), String>),
    DlAdd(isize),
    DlProgress(String),
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
    /// poster size in cells, set by the last layout
    pub poster: Size,
    pub sources: HashMap<u64, Source>,
    /// sources re-resolved this session (the rest come from the disk cache)
    fresh: HashSet<u64>,
    /// downloaded episode count per show, rescanned on refresh
    pub local: HashMap<u64, usize>,
    resolving: HashMap<u64, Option<Want>>,
    dl_tx: Sender<Job>,
    pub dl_pending: isize,
    pub dl_line: String,
    pub playing: Option<Playing>,
    pub toast: Option<(String, bool, Instant)>,
    pub started: Instant,
    /// media whose title glint plays, and since when
    pub glint: (u64, Instant),
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
    // every way out stops the download, panics too: a left-behind ani-cli would finish the
    // episode with ffmpeg, as a file that looks complete
    struct StopDownload;
    impl Drop for StopDownload {
        fn drop(&mut self) {
            ani::stop_download();
        }
    }
    let _stop = StopDownload;
    kitty::init();
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty graphics only (Kitty and Ghostty); the query is just for font size and o=z support.

    let mut app = App::new();
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

/// Bumped on every list change, so refreshes that started before it don't count as fresh.
static LIST_CHANGES: AtomicU64 = AtomicU64::new(0);

fn token_path() -> PathBuf {
    ani::config_dir().join("token")
}

/// Last known (id, name, english titles) of the viewer, so lists show from the first frame.
fn user_path() -> PathBuf {
    ani::config_dir().join("user.json")
}

fn list_cache(key: &str) -> PathBuf {
    ani::cache_dir().join("lists").join(format!("{key}.json"))
}

/// A cached list: its shows and, for a ranking, the page "More…" fetches next.
type Cached = (Vec<Media>, Option<u32>);

/// What a cached list shows when it opens, and whether that's fresh enough to skip AniList.
/// A ranking hardly moves in an hour and keeps every page loaded; a list of yours changes as
/// you watch, so 2 minutes. Past that only the first page shows, which the refresh replaces.
fn from_cache(
    cached: Option<Cached>,
    age: Option<Duration>,
    feed: &mut Feed,
) -> (Vec<Media>, bool) {
    let ttl = match feed {
        Feed::Pages(..) => 60 * 60,
        _ => 120,
    };
    let fresh = cached.is_some() && age.is_some_and(|a| a < Duration::from_secs(ttl));
    let (mut list, next) = cached.unwrap_or_default();
    if let Feed::Pages(_, n, _) = feed {
        match fresh {
            true => *n = next,
            false => list.truncate(PER_PAGE),
        }
    }
    (list, fresh)
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
        Some(eps) if age.as_secs() < days * 86_400 => eps,
        cached => match anilist::episodes(m.id) {
            Ok(eps) => {
                let _ = ani::write_json(&path, &eps);
                eps
            }
            Err(_) => cached.unwrap_or_default(),
        },
    }
}

/// Downloads an ani-hop still has to do, kept on disk so the next start can resume them
/// after quitting early, a crash or a closed terminal. One file per process, as several
/// can run at once.
fn queue_file(pid: &str) -> PathBuf {
    ani::cache_dir().join("queue").join(format!("{pid}.json"))
}

fn read_queue(path: &Path) -> Vec<Job> {
    ani::read_json(path)
}

/// Changes this process's saved queue; jobs with nothing left fall out.
fn edit_queue(f: impl FnOnce(&mut Vec<Job>)) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = queue_file(&std::process::id().to_string());
    let mut q = read_queue(&path);
    f(&mut q);
    q.retain(|j| j.eps.as_ref().is_none_or(|e| !e.is_empty()));
    if q.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let _ = ani::write_json(&path, &q);
}

fn queue_files() -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(ani::cache_dir().join("queue")) else {
        return Vec::new();
    };
    dir.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect()
}

/// Saved queues of ani-hops that are gone. Our own pid can only be a leftover too.
fn orphaned_queues() -> (Vec<Job>, Vec<PathBuf>) {
    let me = std::process::id().to_string();
    let files: Vec<PathBuf> = queue_files()
        .into_iter()
        .filter(|p| {
            let pid = p.file_stem().unwrap_or_default().to_string_lossy();
            pid == me || !ani::running(&pid)
        })
        .collect();
    (files.iter().flat_map(|p| read_queue(p)).collect(), files)
}

/// Takes over saved queues. Another ani-hop may have asked too: whoever deletes a file
/// gets its jobs.
fn claim(files: &[PathBuf]) -> Vec<Job> {
    files
        .iter()
        .flat_map(|f| {
            let jobs = read_queue(f);
            fs::remove_file(f).map_or(Vec::new(), |()| jobs)
        })
        .collect()
}

fn spawn_downloader(tx: Sender<Msg>) -> Sender<Job> {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        for job in job_rx {
            let m = job.media;
            let dub = job.dub.unwrap_or_default();
            // the saved copy of the job follows what's left of it
            let mut saved = job.eps.clone();
            let mut left = |eps: &[String]| {
                edit_queue(|q| {
                    if let Some(j) = q.iter_mut().find(|j| j.media.id == m.id && j.eps == saved) {
                        j.eps = Some(eps.to_vec());
                    }
                });
                saved = Some(eps.to_vec());
            };
            let src = match job.source.map_or_else(|| ani::resolve(&m), Ok) {
                Ok(s) => s,
                Err(e) => {
                    left(&[]);
                    // counted as its episodes, it ends as one
                    let n = job.eps.as_ref().map_or(1, Vec::len) as isize;
                    let _ = tx.send(Msg::DlAdd(1 - n));
                    let _ = tx.send(Msg::DlDone(m.name().to_string(), Err(e)));
                    continue;
                }
            };
            let eps = job.eps.unwrap_or_else(|| {
                let eps: Vec<String> = src
                    .episodes
                    .iter()
                    .filter(|e| ani::ep_num(e) > m.progress() as f64)
                    .cloned()
                    .collect();
                let _ = tx.send(Msg::DlAdd(eps.len() as isize - 1)); // the job itself was counted as one
                left(&eps);
                eps
            });
            for (i, ep) in eps.iter().enumerate() {
                let label = format!("{} E{ep}", m.name());
                let _ = tx.send(Msg::DlProgress(format!("{label} · starting")));
                let res = ani::download(&m, &src, ep, dub, |p| {
                    _ = tx.send(Msg::DlProgress(format!("{label} · {p}")))
                });
                // cut short by quitting: stays saved for the next start
                if res.is_ok() || !ani::stopped() {
                    left(&eps[i + 1..]);
                }
                let _ = tx.send(Msg::DlDone(label, res));
            }
        }
    });
    job_tx
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
        let mut app = App {
            stack: Vec::new(),
            next_id: 0,
            dl_tx: spawn_downloader(tx.clone()),
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
            poster: Size::new(0, 0),
            sources: ani::load_resolved(),
            fresh: HashSet::new(),
            local: HashMap::new(),
            resolving: HashMap::new(),
            dl_pending: 0,
            dl_line: String::new(),
            playing: None,
            toast: None,
            started: Instant::now(),
            glint: (0, Instant::now()),
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
        match orphaned_queues() {
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
    }

    /// Back to the main menu; an unanswered resume question stays.
    fn back_to_main(&mut self) {
        self.stack
            .retain(|s| matches!(s.view, View::Main | View::Confirm(Pending::Resume(..))));
    }

    pub fn top(&mut self) -> &mut Screen {
        self.stack.last_mut().unwrap()
    }

    /// Queues a download, saved first so quitting early doesn't lose it.
    fn download(&mut self, mut job: Job) {
        job.dub
            .get_or_insert_with(|| ani::dub(ani::audio(Some(job.media.id))));
        self.dl_pending += job.eps.as_ref().map_or(1, Vec::len) as isize;
        edit_queue(|q| q.push(job.clone()));
        let _ = self.dl_tx.send(job);
    }

    fn spawn(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        thread::spawn(move || _ = tx.send(f()));
    }

    fn toast(&mut self, text: impl Into<String>, err: bool) {
        self.toast = Some((text.into(), err, Instant::now()));
    }

    fn push(&mut self, title: impl Into<String>, view: View) -> u64 {
        let multi = matches!(
            view,
            View::Shows(..) | View::Episodes(_, EpMode::Download | EpMode::Delete)
        );
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
        mut feed: Feed,
        cache: Option<String>,
        f: impl FnOnce() -> Result<Vec<Media>, String> + Send + 'static,
    ) {
        let path = cache.map(|k| list_cache(&k));
        let cached: Option<Cached> = path.as_ref().and_then(|p| ani::read_json(p));
        let age = path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok()?.modified().ok()?.elapsed().ok());
        let (list, fresh) = from_cache(cached, age, &mut feed);
        let paged = matches!(feed, Feed::Pages(..));
        let id = self.push(title, View::Shows(list, feed));
        if fresh {
            return;
        }
        self.top().loading = true;
        let changes = LIST_CHANGES.load(Ordering::Relaxed);
        self.spawn(move || {
            let res = f();
            if let (Ok(list), Some(p)) = (&res, path) {
                let next = (paged && list.len() >= PER_PAGE).then_some(2);
                let _ = ani::write_json(&p, &(list, next));
                // a save landed while this was in flight: keep it, but not as fresh
                if LIST_CHANGES.load(Ordering::Relaxed) != changes {
                    let file = fs::File::options().write(true).open(&p);
                    let _ = file.map(|f| f.set_modified(std::time::UNIX_EPOCH));
                }
            }
            Msg::Shows(id, res)
        });
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
        if let View::Shows(list, Feed::Local) = &mut self.top().view {
            *list = ani::local_shows().into_iter().map(|(m, _)| m).collect();
        }
        let items = self.items(&self.stack.last().unwrap().view);
        self.top().menu.set_items(items);
    }

    fn scan_local(&mut self) {
        self.local = ani::local_shows()
            .into_iter()
            .map(|(m, p)| (m.id, ani::local_episodes(&p).len()))
            .collect();
    }

    /// Something on screen changes without input (spinner, progress, fading toast).
    fn animating(&self) -> bool {
        ui::effects_running(self)
            || self.stack.last().unwrap().loading
            || self.dl_pending > 0
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

    /// Episode titles and summaries, once a session per show.
    pub fn want_episodes(&mut self, m: &Media) {
        // ani.zip knows shows by AniList id only
        if m.on_anilist()
            && let Vacant(e) = self.ep_info.entry(m.id)
        {
            e.insert(HashMap::new());
            let m = m.clone();
            self.spawn(move || Msg::EpInfo(m.id, load_episodes(&m)));
        }
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
                v.push(Item::new(
                    "downloads",
                    "Downloads",
                    plural(self.local.len(), "show"),
                ));
                v.push(Item::new(
                    "settings",
                    "Settings & AniList…",
                    self.user.as_ref().map_or("not logged in", |u| u.1.as_str()),
                ));
                v.push(Item::new("quit", "Quit", ""));
                v
            }
            View::Settings => vec![
                Item::new(
                    "skip",
                    "Skip openings and endings",
                    if ani::skip_on() { "on" } else { "off" },
                ),
                Item::new("audio", "Audio…", ani::audio_label(ani::audio(None))),
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
            View::Shows(list, feed) => {
                let mut v: Vec<Item> = list
                    .iter()
                    .map(|m| {
                        let mut hint = Vec::new();
                        if let Some(n) = self.local.get(&m.id).filter(|n| **n > 0) {
                            hint.push(format!("↓{n}"));
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
                if let Feed::Pages(_, Some(page), _) = feed
                    && !list.is_empty()
                {
                    let mut more = Item::new("more", "More…", format!("page {page}"));
                    more.action = true;
                    v.push(more);
                }
                v
            }
            View::Show(m) => {
                let local = ani::local_episodes(&ani::folder(m));
                let src = self.sources.get(&m.id);
                let next = m.progress() + 1;
                let mut v = Vec::new();
                if m.released().is_none_or(|r| next <= r) {
                    let verb = if m.progress() > 0 {
                        "Continue with"
                    } else {
                        "Play"
                    };
                    let on_disk = local.iter().any(|(e, _)| ani::ep_num(e) == next as f64);
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
                v.push(Item::new("download", "↓  Download episodes…", ""));
                if !local.is_empty() {
                    v.push(Item::new(
                        "delete",
                        "✗  Delete downloads…",
                        format!("{} on disk", local.len()),
                    ));
                }
                if self.user.is_some() && m.on_anilist() {
                    let status = m
                        .media_list_entry
                        .as_ref()
                        .map(|e| list_label(&e.status))
                        .unwrap_or("not on list");
                    v.push(Item::new("status", "●  List status…", status));
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
                let local = ani::local_episodes(&ani::folder(m));
                let info = self.ep_info.get(&m.id);
                // what a watched episode is called and about; nothing that spoils the rest
                let item = |key: &str, ep: &str, hint: String| {
                    let mut it = Item::new(key, format!("Episode {ep}"), hint);
                    it.done = m.watched(ep);
                    if let Some(i) = info.and_then(|i| i.get(ep)).filter(|_| it.done) {
                        if !i.title.is_empty() {
                            it.label = format!("{} · {}", it.label, i.title);
                        }
                        it.about = i.about.clone();
                    }
                    it
                };
                if *mode == EpMode::Delete {
                    return local
                        .into_iter()
                        .map(|(ep, p)| {
                            let mb = fs::metadata(&p).map_or(0, |md| md.len() >> 20);
                            item(&p.to_string_lossy(), &ep, format!("{mb} MB"))
                        })
                        .collect();
                }
                let mut eps: Vec<String> = self
                    .sources
                    .get(&m.id)
                    .map(|s| s.episodes.clone())
                    .unwrap_or_default();
                if *mode == EpMode::Play {
                    eps.extend(
                        local
                            .iter()
                            .map(|(e, _)| e.clone())
                            .filter(|e| !eps.contains(e))
                            .collect::<Vec<_>>(),
                    );
                    eps.sort_by(|a, b| ani::ep_num(a).total_cmp(&ani::ep_num(b)));
                }
                eps.into_iter()
                    .map(|ep| {
                        let on_disk = local.iter().any(|(e, _)| *e == ep);
                        item(&ep, &ep, if on_disk { "on disk" } else { "" }.into())
                    })
                    .collect()
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
                    .filter(|m| self.local.get(&m.id).is_some_and(|n| *n > 0))
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
            View::Confirm(Pending::Progress(m, ep)) => {
                // marking it watched can also put the show on another list
                let to = progress_status(m, ani::ep_num(ep) as u32);
                let hint = match m.media_list_entry.as_ref().map(|e| e.status.as_str()) {
                    Some(s) if s == to => String::new(),
                    Some(_) => format!("moves it to {}", list_label(to)),
                    None => format!("adds it to {}", list_label(to)),
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
        match self.top().menu.key(k) {
            Some(Act::Accept) => self.accept(),
            Some(Act::Back) => self.pop(1),
            None => {}
        }
    }

    fn request_quit(&mut self) {
        let busy = self.dl_pending > 0 || self.saves_pending > 0;
        if busy && !matches!(self.top().view, View::Confirm(Pending::Quit)) {
            let mut left = Vec::new();
            if self.dl_pending > 0 {
                left.push(plural(self.dl_pending as usize, "download"));
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
            View::Shows(_, Feed::Pages(..))
                if screen.menu.current().is_some_and(|i| i.key == "more") =>
            {
                self.more()
            }
            View::Shows(list, _) => {
                let picked: Vec<Media> = keys
                    .iter()
                    .filter_map(|k| list.iter().find(|m| m.id.to_string() == *k).cloned())
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
                    EpMode::Download => {
                        let n = keys.len();
                        // a cached source gets re-checked by the worker anyway, a fresh one is ready
                        let source = self
                            .fresh
                            .contains(&m.id)
                            .then(|| self.sources.get(&m.id).cloned())
                            .flatten();
                        self.download(Job {
                            media: m,
                            source,
                            eps: Some(keys),
                            dub: None,
                        });
                        self.toast(format!("Queued {}", plural(n, "episode")), false);
                        self.pop(1);
                    }
                    EpMode::Delete => {
                        let paths: Vec<PathBuf> = keys.iter().map(PathBuf::from).collect();
                        self.push(
                            format!("Delete {}?", plural(paths.len(), "episode")),
                            View::Confirm(Pending::Delete(paths)),
                        );
                    }
                }
            }
            View::Audio(m) => {
                let id = m.as_ref().map(|m| m.id);
                let name = m
                    .as_ref()
                    .map_or("Default audio".into(), |m| m.name().to_string());
                if let Err(e) = ani::set_audio(id, &key) {
                    return self.toast(e.to_string(), true);
                }
                self.toast(format!("{name}: {}", ani::audio_label(&key)), false);
                self.pop(1);
            }
            View::Status(m) => {
                let (m, c) = (m.clone(), self.client.clone());
                self.toast(format!("Saving {}…", m.name()), false);
                match (key.as_str(), m.media_list_entry.as_ref().map(|e| e.id)) {
                    ("remove", Some(entry)) => {
                        self.saves_pending += 1;
                        self.spawn(move || Msg::Saved(m, c.remove(entry).map(|_| None)))
                    }
                    (status, _) => {
                        let status = status.to_string();
                        self.saves_pending += 1;
                        self.spawn(move || {
                            Msg::Saved(m.clone(), c.save(m.id, Some(&status), None).map(Some))
                        })
                    }
                }
            }
            View::Sources(m, q, hits) => {
                let i: usize = key.parse().unwrap();
                if let Err(e) = ani::save_override(m.id, q, &hits[i]) {
                    return self.toast(e.to_string(), true);
                }
                let m = m.clone();
                self.sources.remove(&m.id);
                self.fresh.remove(&m.id);
                self.toast(format!("Source set to {}", hits[i]), false);
                self.pop(2);
                self.resolve(&m, None);
            }
            View::Bulk(list) => {
                let list = list.clone();
                if key == "download" {
                    let shows = plural(list.len(), "show");
                    for m in list {
                        self.download(Job {
                            media: m,
                            source: None,
                            eps: None,
                            dub: None,
                        });
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
                        let jobs = claim(files);
                        // a show another running ani-hop downloads keeps its files
                        let busy: Vec<u64> = queue_files()
                            .iter()
                            .flat_map(|f| read_queue(f))
                            .map(|j| j.media.id)
                            .collect();
                        for j in jobs.iter().filter(|j| !busy.contains(&j.media.id)) {
                            let eps = j.eps.as_deref().unwrap_or_default();
                            ani::discard(&ani::folder(&j.media), eps);
                        }
                        self.toast(
                            match jobs.is_empty() {
                                true => "Another ani-hop took these over already",
                                false => "Dropped the unfinished downloads",
                            },
                            false,
                        );
                    }
                    return self.pop(1);
                }
                match p {
                    Pending::Quit => self.quit = true,
                    Pending::Resume(_, files) => {
                        let jobs = claim(files);
                        self.toast(
                            match jobs.is_empty() {
                                true => "Another ani-hop took these over already",
                                false => "Resuming the unfinished downloads",
                            },
                            false,
                        );
                        for job in jobs {
                            self.download(job);
                        }
                        self.pop(1);
                    }
                    Pending::Delete(paths) => {
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
                        self.toast(format!("Deleted {} of {n}", n - failed), failed > 0);
                        self.pop(2);
                        self.top().menu.marked.clear();
                    }
                    Pending::Progress(m, ep) => {
                        let (m, ep, c) = (m.clone(), ani::ep_num(ep) as u32, self.client.clone());
                        let status = progress_status(&m, ep);
                        self.saves_pending += 1;
                        self.spawn(move || {
                            Msg::Saved(m.clone(), c.save(m.id, Some(status), Some(ep)).map(Some))
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
        let screen = self.stack.last().unwrap();
        let View::Shows(_, Feed::Pages(sort, next, fetching)) = &screen.view else {
            return;
        };
        let Some(page) = *next else {
            return self.toast("No more shows", false);
        };
        // one page at a time; the first one's refresh would drop what comes after it
        match (screen.loading, *fetching) {
            (true, true) => return,
            (true, false) => return self.toast("Still refreshing the first page…", false),
            _ => {}
        }
        let (sort, id, c) = (sort.clone(), screen.id, self.client.clone());
        self.top().loading = true;
        if let View::Shows(_, Feed::Pages(_, _, f)) = &mut self.top().view {
            *f = true;
        }
        self.spawn(move || Msg::More(id, c.browse(&sort, None, page)));
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
                self.push("Downloads", View::Shows(list, Feed::Local));
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
                    ani::or_ani_cli(found, &text, ani::search)
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
                self.spawn(move || Msg::Hits(id, ani::search(&text).map(|r| r.0)));
            }
        }
    }

    fn open_show(&mut self, m: Media) {
        self.resolve(&m, None); // warm up so "Episodes…" opens instantly
        self.push(m.name().to_string(), View::Show(m));
    }

    fn show_action(&mut self, m: Media, key: &str) {
        match key {
            "next" => self.play(m.clone(), (m.progress() + 1).to_string()),
            "play" => self.resolve(&m, Some(Want::Episodes(EpMode::Play))),
            "download" => self.resolve(&m, Some(Want::Episodes(EpMode::Download))),
            "delete" => _ = self.push("Delete", View::Episodes(m, EpMode::Delete)),
            "status" => _ = self.push("List status", View::Status(m)),
            "source" => {
                let q = self
                    .sources
                    .get(&m.id)
                    .map_or_else(|| ani::default_query(&m), |s| s.query.clone());
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
            Some(w) if self.sources.contains_key(&m.id) => {
                self.fulfill(m.clone(), w);
                None
            }
            w => w,
        };
        if self.fresh.contains(&m.id) {
            return;
        }
        if matches!(want, Some(Want::Episodes(_))) {
            self.toast(format!("Searching ani-cli for {}…", m.name()), false);
        }
        if let Some(slot) = self.resolving.get_mut(&m.id) {
            if want.is_some() {
                *slot = want;
            }
            return;
        }
        self.resolving.insert(m.id, want);
        let m = m.clone();
        self.spawn(move || {
            let res = ani::resolve(&m);
            Msg::Resolved(m, res)
        });
    }

    fn fulfill(&mut self, m: Media, want: Want) {
        match want {
            Want::Play(ep) => self.play(m, ep),
            Want::Episodes(mode) => {
                let next = (m.progress() + 1).to_string();
                let title = if mode == EpMode::Play {
                    "Episodes"
                } else {
                    "Download"
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
        let src = self.sources.get(&m.id).cloned();
        let cached = !self.fresh.contains(&m.id);
        let step = match (&local, &src) {
            (Some(_), _) => "opening the player",
            (None, None) => SEARCHING,
            (_, Some(_)) if cached => "checking the source",
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
        if local.is_none() && src.is_none() {
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
            let res = match (&local, src) {
                (Some((_, file)), _) => ani::play_file(&m, &ep, file, || step(None)),
                (_, Some(src)) if cached => ani::locate(&m, &src).and_then(|s| {
                    step(Some("fetching the stream"));
                    stream(&s)
                }),
                (_, Some(src)) => stream(&src),
                _ => unreachable!(),
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

    /// Caches a ranking with every page loaded. Its first page's age stays its freshness, so
    /// loading more never keeps that page longer than an hour.
    fn save_pages(&self, id: u64) {
        let screen = self.stack.iter().find(|s| s.id == id);
        let Some(View::Shows(list, Feed::Pages(sort, next, _))) = screen.map(|s| &s.view) else {
            return;
        };
        let path = list_cache(&self.ranking_key(sort));
        let fetched = fs::metadata(&path).and_then(|m| m.modified());
        let _ = ani::write_json(&path, &(list, next));
        if let Ok(t) = fetched {
            let _ = fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(t));
        }
    }

    /// Marks this user's cached lists as old, so the next visit shows them and refreshes.
    fn outdate_lists(&self) {
        LIST_CHANGES.fetch_add(1, Ordering::Relaxed);
        let Ok(dir) = fs::read_dir(ani::cache_dir().join("lists")) else {
            return;
        };
        for e in dir.flatten() {
            if let Ok(f) = fs::File::options().write(true).open(e.path()) {
                let _ = f.set_modified(std::time::UNIX_EPOCH);
            }
        }
    }

    fn update_media(&mut self, id: u64, entry: Option<Entry>) {
        for s in &mut self.stack {
            let media: Vec<&mut Media> = match &mut s.view {
                View::Show(m)
                | View::Episodes(m, _)
                | View::Status(m)
                | View::Sources(m, ..)
                | View::Audio(Some(m))
                | View::Input(Input::SourceQuery(m)) => vec![m],
                View::Shows(l, _) | View::Bulk(l) => l.iter_mut().collect(),
                _ => Vec::new(),
            };
            for m in media {
                if m.id == id {
                    m.media_list_entry = entry.clone();
                }
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Shows(id, res) => {
                let fallback = res
                    .as_ref()
                    .is_ok_and(|l| l.iter().any(|m| !m.on_anilist()))
                    && self.stack.iter().any(|s| s.id == id);
                self.fill(id, res, |v, list| {
                    if let View::Shows(l, feed) = v {
                        if let Feed::Pages(_, next, _) = feed {
                            *next = (list.len() >= PER_PAGE).then_some(2);
                        }
                        *l = list;
                    }
                });
                if fallback {
                    self.toast("AniList search failed, showing ani-cli's matches", true);
                }
            }
            Msg::More(id, res) => {
                let screen = self.stack.iter_mut().find(|s| s.id == id);
                if let Some(View::Shows(_, Feed::Pages(_, _, fetching))) =
                    screen.map(|s| &mut s.view)
                {
                    *fetching = false;
                }
                let mut first = None; // Some(None): the page brought nothing new
                self.fill(id, res, |v, more| {
                    if let View::Shows(l, Feed::Pages(_, next, _)) = v {
                        *next = next.filter(|_| more.len() >= PER_PAGE).map(|n| n + 1);
                        // rankings shift between requests, a show may come round again
                        let more: Vec<Media> = more
                            .into_iter()
                            .filter(|m| l.iter().all(|o| o.id != m.id))
                            .collect();
                        let start = l.len();
                        l.extend(more);
                        first = Some(l.get(start).map(|m| m.id.to_string()));
                    }
                });
                self.save_pages(id);
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
                if matches!(&self.top().view, View::Episodes(m, _) if m.id == id) {
                    self.refresh();
                }
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
            Msg::Resolved(m, res) => {
                let want = self.resolving.remove(&m.id).flatten();
                // the wait is over either way; playing it starts anew below
                let waited = self
                    .playing
                    .take_if(|p| p.id == m.id && p.step == Some(SEARCHING));
                let here = matches!(&self.top().view, View::Show(s) | View::Episodes(s, _) if s.id == m.id);
                match res {
                    Ok(src) => {
                        self.sources.insert(m.id, src);
                        self.fresh.insert(m.id);
                        ani::save_resolved(&self.sources);
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
                        let tip = match e.starts_with("No results") {
                            true => " · try Change source…",
                            false => "",
                        };
                        self.toast(format!("ani-cli: {e}{tip}"), true)
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
                        let text = match entry {
                            Some(_) => format!("Saved {}", m.name()),
                            None => format!("Removed {} from your list", m.name()),
                        };
                        self.toast(text, false);
                        self.update_media(m.id, entry);
                        self.outdate_lists();
                        if matches!(self.top().view, View::Status(_)) {
                            self.stack.pop();
                        }
                        self.refresh();
                    }
                    Err(e) => self.toast(format!("{} not saved · {e}", m.name()), true),
                }
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
                match res {
                    Err(e) => self.toast(format!("Couldn't play episode {ep} · {e}"), true),
                    Ok(())
                        if self.user.is_some()
                            && m.on_anilist()
                            && ani::ep_num(&ep) > m.progress() as f64 =>
                    {
                        let title = format!("Mark episode {ep} of {} as watched?", m.name());
                        self.push(title, View::Confirm(Pending::Progress(m, ep)));
                        self.top().menu.select_key("yes");
                    }
                    Ok(()) => {}
                }
            }
            Msg::DlAdd(n) => self.dl_pending += n,
            Msg::DlProgress(line) => self.dl_line = line,
            Msg::DlDone(label, res) => {
                self.dl_pending -= 1;
                self.dl_line.clear();
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

/// The list a show is on once episode `ep` is marked watched.
fn progress_status(m: &Media, ep: u32) -> &'static str {
    match m.media_list_entry.as_ref().map(|e| e.status.as_str()) {
        _ if m.episodes == Some(ep) => "COMPLETED",
        Some("REPEATING") => "REPEATING",
        _ => "CURRENT",
    }
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
    fn rankings_keep_their_pages_for_an_hour() {
        use super::{Feed, Media, from_cache};
        use std::time::Duration;
        let min = |m: u64| Some(Duration::from_secs(m * 60));
        let pages = || Some((vec![Media::default(); 120], Some(4)));
        let mut feed = Feed::Pages("TRENDING_DESC".into(), Some(2), false);
        let (list, fresh) = from_cache(pages(), min(59), &mut feed);
        assert!(fresh && list.len() == 120);
        assert!(
            matches!(feed, Feed::Pages(_, Some(4), _)),
            "More… goes on with page 4"
        );
        let mut feed = Feed::Pages("TRENDING_DESC".into(), Some(2), false);
        let (list, fresh) = from_cache(pages(), min(61), &mut feed);
        assert!(
            !fresh && list.len() == 50,
            "past the hour: page 1, refreshed"
        );
        assert!(matches!(feed, Feed::Pages(_, Some(2), _)));
        let (list, fresh) = from_cache(pages(), min(3), &mut Feed::Whole);
        assert!(
            !fresh && list.len() == 120,
            "your own lists: 2 minutes, all shown"
        );
        let (list, fresh) = from_cache(None, None, &mut Feed::Whole);
        assert!(!fresh && list.is_empty());
    }

    #[test]
    fn watching_an_episode_picks_the_list() {
        let mut m = super::Media {
            episodes: Some(12),
            ..Default::default()
        };
        assert_eq!(
            super::progress_status(&m, 3),
            "CURRENT",
            "not on a list yet"
        );
        assert_eq!(super::progress_status(&m, 12), "COMPLETED");
        let entry = |status: &str| super::Entry {
            id: 1,
            status: status.into(),
            progress: 2,
        };
        m.media_list_entry = Some(entry("REPEATING"));
        assert_eq!(super::progress_status(&m, 3), "REPEATING");
        m.media_list_entry = Some(entry("PAUSED"));
        assert_eq!(super::progress_status(&m, 3), "CURRENT");
    }
}
