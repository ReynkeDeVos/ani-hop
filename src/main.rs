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
    Shows(Vec<Media>, Feed),
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
    /// marking an episode watched: the AniList entry that saves it (a sequel's, when the
    /// provider runs on into it), the progress there, and what to call it if it's a sequel
    Progress(Media, u32, String),
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    /// tells apart jobs of one show while the worker and stops edit the queue
    #[serde(skip)]
    id: u64,
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
    /// a list's shows, and where to cache them with LIST_CHANGES from when it was asked for
    Shows(u64, Result<Vec<Media>, String>, Option<(PathBuf, u64)>),
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
    Resolved(Media, Result<Source, String>),
    Saved(Media, Result<Option<Entry>, String>),
    /// progress not saved, as AniList's entry moved since the question was asked: (the
    /// entry's media, the entry now)
    Moved(Media, Option<Entry>),
    /// (media, episode, what it waits on now; None = the player is up)
    PlayStep(u64, String, Option<&'static str>),
    Played(Media, String, Result<(), String>),
    DlProgress(DlNow),
    DlDone(String, Result<(), String>),
    /// (version, update command) of a newer ani-hop
    Release(Option<(String, String)>),
}

/// AniList sequels a provider entry runs on into, each with the episodes before it.
type Parts = Vec<(u32, Media)>;

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

/// The episode being downloaded and how far it got.
pub struct DlNow {
    pub id: u64,
    pub ep: String,
    /// "Show E3", for the footer
    pub label: String,
    pub text: String,
    /// yt-dlp's, which goes on from what it got after a pause; ffmpeg starts over
    pub resumes: bool,
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
    pub sources: HashMap<u64, Source>,
    /// sources re-resolved this session (the rest come from the disk cache)
    fresh: HashSet<u64>,
    /// downloaded episodes per show, rescanned on refresh
    pub local: HashMap<u64, Vec<(String, PathBuf)>>,
    /// episodes per show other running ani-hops have queued, rescanned on refresh
    elsewhere: HashMap<u64, Vec<String>>,
    resolving: HashMap<u64, Option<Want>>,
    /// tells the download worker there's something in the queue
    dl_wake: Sender<()>,
    pub dl_now: Option<DlNow>,
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

/// Downloads an ani-hop still has to do, kept on disk so the next start can resume them
/// after quitting early, a crash or a closed terminal. One file per process, as several
/// can run at once.
fn queue_file(pid: &str) -> PathBuf {
    ani::cache_dir().join("queue").join(format!("{pid}.json"))
}

fn read_queue(path: &Path) -> Vec<Job> {
    ani::read_json(path)
}

/// This ani-hop's downloads, oldest first and the running one included: what the worker
/// does next, what the footer counts and what the next start resumes after quitting early.
static QUEUE: Mutex<Vec<Job>> = Mutex::new(Vec::new());

fn queue<R>(f: impl FnOnce(&[Job]) -> R) -> R {
    f(&QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Changes the queue and its saved copy; jobs with nothing left fall out.
fn edit_queue<R>(f: impl FnOnce(&mut Vec<Job>) -> R) -> R {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let r = f(&mut q);
    q.retain(|j| j.eps.as_ref().is_none_or(|e| !e.is_empty()));
    let path = queue_file(&std::process::id().to_string());
    match q.is_empty() {
        true => _ = fs::remove_file(&path),
        false => _ = ani::write_json(&path, &*q),
    }
    r
}

/// Episodes still to download; a show whose episodes the worker hasn't looked up counts as one.
pub fn dl_pending() -> usize {
    queue(|q| q.iter().map(|j| j.eps.as_ref().map_or(1, Vec::len)).sum())
}

fn is_queued(id: u64, ep: &str) -> bool {
    queue(|q| {
        q.iter()
            .filter(|j| j.media.id == id)
            .any(|j| j.eps.as_ref().is_some_and(|e| e.iter().any(|x| x == ep)))
    })
}

/// What a job without episodes downloads: everything after the AniList progress that
/// isn't on disk yet.
fn to_download(m: &Media, eps: &[String], on_disk: &[(String, PathBuf)]) -> Vec<String> {
    eps.iter()
        .filter(|e| ani::ep_num(e) > m.progress() as f64)
        .filter(|e| !on_disk.iter().any(|(d, _)| d == *e))
        .cloned()
        .collect()
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

/// Works through the queue until it's empty or paused, then waits to be woken.
fn spawn_downloader(tx: Sender<Msg>) -> Sender<()> {
    let (wake, woken) = mpsc::channel::<()>();
    thread::spawn(move || {
        for () in woken {
            while !ani::paused()
                && let Some(job) = queue(|q| q.first().cloned())
            {
                if !work(job, &tx) {
                    return; // quitting: what's left stays saved for the next start
                }
            }
        }
    });
    wake
}

/// Downloads a job's episodes, each looked up in the queue right before it starts, so
/// stopped ones are skipped. Returns at a pause, false once quitting stopped the downloads.
fn work(job: Job, tx: &Sender<Msg>) -> bool {
    let m = &job.media;
    let src = match job.source.map_or_else(|| ani::resolve(m), Ok) {
        Ok(s) => s,
        Err(e) => {
            edit_queue(|q| q.retain(|j| j.id != job.id));
            let _ = tx.send(Msg::DlDone(m.name().to_string(), Err(e)));
            return true;
        }
    };
    let all = (job.eps.is_none())
        .then(|| to_download(m, &src.episodes, &ani::local_episodes(&ani::folder(m))));
    edit_queue(|q| {
        if let Some(j) = q.iter_mut().find(|j| j.id == job.id) {
            // after a pause it goes on from here, without looking the show up again
            j.source.get_or_insert_with(|| src.clone());
            // stopping one of them may have listed them already
            if let Some(all) = all {
                j.eps.get_or_insert(all);
            }
        }
    });
    let dub = job.dub.unwrap_or_default();
    loop {
        let next = queue(|q| {
            q.iter()
                .find(|j| j.id == job.id)?
                .eps
                .as_ref()?
                .first()
                .cloned()
        });
        let Some(ep) = next else {
            return true;
        };
        let label = format!("{} E{ep}", m.name());
        let now = |text: &str, resumes| {
            let (id, ep, label) = (m.id, ep.clone(), label.clone());
            Msg::DlProgress(DlNow {
                id,
                ep,
                label,
                text: text.into(),
                resumes,
            })
        };
        let _ = tx.send(now("starting", true));
        // this job's, not just queued: stopped and queued again is another job, with its audio
        let mine = |q: &[Job]| {
            (q.iter()).any(|j| j.id == job.id && j.eps.as_ref().is_some_and(|e| e.contains(&ep)))
        };
        let res = ani::download(
            m,
            &src,
            &ep,
            dub,
            || queue(mine),
            |p, r| _ = tx.send(now(p, r)),
        );
        if res.is_err() && ani::stopped() {
            return false;
        }
        // paused, and maybe resumed already: it stays first, to go on from what it got
        if res.as_ref().is_err_and(|e| e == ani::STOPPED) && queue(mine) {
            match ani::paused() {
                true => return true,
                false => continue,
            }
        }
        // gone from the job already: stopped (and maybe queued again since, which stays)
        let stopped = !edit_queue(|q| {
            let had = mine(q);
            // done with it, also where another job of the show has it
            for j in q.iter_mut().filter(|j| had && j.media.id == m.id) {
                if let Some(e) = &mut j.eps {
                    e.retain(|x| *x != ep);
                }
            }
            had
        });
        match stopped {
            true => ani::discard(&ani::folder(m), std::slice::from_ref(&ep)),
            false => _ = tx.send(Msg::DlDone(label, res)),
        }
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
        let mut app = App {
            stack: Vec::new(),
            next_id: 0,
            dl_wake: spawn_downloader(tx.clone()),
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
            sources: ani::load_resolved(),
            fresh: HashSet::new(),
            local: HashMap::new(),
            elsewhere: HashMap::new(),
            resolving: HashMap::new(),
            dl_now: None,
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

    /// Queues a download, saved first so quitting early doesn't lose it.
    fn download(&mut self, mut job: Job) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        job.id = NEXT.fetch_add(1, Ordering::Relaxed);
        job.dub
            .get_or_insert_with(|| ani::dub(ani::audio(Some(job.media.id))));
        edit_queue(|q| q.push(job));
        ani::resume(); // asking for one is asking for downloads to go on
        let _ = self.dl_wake.send(());
    }

    /// The episode downloading, or paused, when ffmpeg has it, which can't go on from what it
    /// got: "Episode 3", or "Show E3" when `m`, the show on screen, isn't its show.
    fn restarts(&self, m: &Media) -> Option<String> {
        let n = self.dl_now.as_ref().filter(|n| !n.resumes)?;
        Some(match n.id == m.id {
            true => format!("Episode {}", n.ep),
            false => n.label.clone(),
        })
    }

    /// Whether episode `ep` of `m` is the one downloading, not paused.
    fn downloading(&self, m: &Media, ep: &str) -> bool {
        !ani::paused() && (self.dl_now.as_ref()).is_some_and(|n| n.id == m.id && n.ep == ep)
    }

    /// Whether enter on `stop` (queued episodes of `m`) is about pausing or resuming them all:
    /// on the one downloading, or with downloads paused, on any queued one.
    fn holds(&self, m: &Media, stop: &[String]) -> bool {
        match ani::paused() {
            true => !stop.is_empty(),
            false => (self.dl_now.as_ref()).is_some_and(|n| n.id == m.id && stop.contains(&n.ep)),
        }
    }

    /// Takes episodes of `m` out of the queue. The one downloading stops, and the worker
    /// deletes what it got of it.
    fn stop(&mut self, m: &Media, eps: &[String]) {
        let on_disk = self.local.get(&m.id).cloned().unwrap_or_default();
        let src = self.sources.get(&m.id);
        edit_queue(|q| {
            for j in q.iter_mut().filter(|j| j.media.id == m.id) {
                // a job the worker hasn't looked up yet gets its episodes listed, minus these
                if let (None, Some(s)) = (&j.eps, src) {
                    j.eps = Some(to_download(&j.media, &s.episodes, &on_disk));
                }
                if let Some(e) = &mut j.eps {
                    e.retain(|x| !eps.contains(x));
                }
            }
        });
        ani::stop_episodes(&ani::folder(m), eps);
        self.dl_now.take_if(|n| n.id == m.id && eps.contains(&n.ep));
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
            && let Some(ep) = self.dl_now.as_ref().map(|n| n.ep.clone())
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
            // a cached source gets re-checked by the worker anyway, a fresh one is ready
            let source = (self.fresh.contains(&m.id))
                .then(|| self.sources.get(&m.id).cloned())
                .flatten();
            // paused with something held, not just a pause left over from an emptied queue
            let going = if ani::paused() && dl_pending() > 0 {
                " · resuming downloads"
            } else {
                ""
            };
            self.toast(format!("Queued {}{going}", which(&get)), false);
            let (media, eps) = (m, Some(get));
            self.download(Job {
                id: 0,
                media,
                source,
                eps,
                dub: None,
            });
        } else if action == "stop" && !stop.is_empty() {
            self.toast(format!("Stopped downloading {}", which(&stop)), false);
            self.stop(&m, &stop);
        } else if action == "pause" && hold && !ani::paused() {
            // the one it was offered for (holds: dl_now); one that started since may be
            // ffmpeg's, whose pause asks first
            let ep = self.dl_now.as_ref().map_or("", |n| n.ep.as_str());
            if ani::pause(&ani::folder(&m), ep) {
                self.toast(
                    "Paused downloads · enter on a queued episode resumes them",
                    false,
                );
            }
        } else if action == "resume" && ani::paused() {
            ani::resume();
            let _ = self.dl_wake.send(());
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
            _ if self.holds(m, &stop) && ani::paused() => "resume…",
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
        let id = self.push(title, View::Shows(list, feed));
        if fresh {
            return;
        }
        self.top().loading = true;
        let cache = path.map(|p| (p, LIST_CHANGES.load(Ordering::Relaxed)));
        self.spawn(move || Msg::Shows(id, f(), cache));
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
        let me = std::process::id().to_string();
        self.elsewhere.clear();
        for p in queue_files() {
            let pid = p.file_stem().unwrap_or_default().to_string_lossy();
            if pid == me || !ani::running(&pid) {
                continue;
            }
            for j in read_queue(&p) {
                let eps = self.elsewhere.entry(j.media.id).or_default();
                eps.extend(j.eps.unwrap_or_default());
            }
        }
    }

    /// Where `m`'s episodes stand, as of the last rescan and the queue now.
    fn downloads(&self, m: &Media) -> Downloads {
        let on_disk = self.local.get(&m.id).cloned().unwrap_or_default();
        let src = self.sources.get(&m.id);
        let queued = queue(|q| {
            (q.iter().filter(|j| j.media.id == m.id))
                .flat_map(|j| match (&j.eps, src) {
                    (Some(eps), _) => eps.clone(),
                    // not looked up by the worker yet
                    (None, Some(s)) => to_download(&j.media, &s.episodes, &on_disk),
                    (None, None) => Vec::new(),
                })
                .collect()
        });
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
            || dl_pending() > 0 && !ani::paused()
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
            let asked = (c.token.clone(), LIST_CHANGES.load(Ordering::Relaxed));
            self.spawn(move || {
                let parts = c.parts(&m, key.0);
                Msg::Parts(m.id, key, asked.0, asked.1, parts)
            });
        }
    }

    /// The provider's last whole episode and AniList's count, when the provider has more.
    fn parts_key(&self, m: &Media) -> Option<(u32, u32)> {
        let src = self.sources.get(&m.id)?;
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

    /// The AniList entry episode `ep` of `m` counts toward, and its number there.
    fn part<'a>(&'a self, m: &'a Media, ep: &str) -> (&'a Media, String) {
        part(m, self.parts(m), ep)
    }

    fn progress_target<'a>(&'a self, m: &'a Media, ep: &str) -> Option<(&'a Media, f64)> {
        progress_target(m, self.parts(m), ep)
    }

    pub fn watched(&self, m: &Media, ep: &str) -> bool {
        watched(m, self.parts(m), ep)
    }

    /// What episode `ep` of `m` is called and about.
    pub fn ep_about(&self, m: &Media, ep: &str) -> Option<&EpInfo> {
        let (p, n) = self.part(m, ep);
        self.ep_info.get(&p.id)?.get(&n)
    }

    /// The episode after the last one watched, also in a sequel the provider runs on into.
    pub fn next_ep(&self, m: &Media) -> u32 {
        next_ep(m, self.parts(m))
    }

    /// Episodes out right now, with those of the sequels the provider runs on into.
    fn released(&self, m: &Media) -> Option<u32> {
        match self.parts(m).last() {
            Some((before, p)) => p.released().map(|r| before + r),
            None => m.released(),
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
            View::Shows(list, feed) => {
                let mut v: Vec<Item> = list
                    .iter()
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
                let dl = self.downloads(m);
                let src = self.sources.get(&m.id);
                let next = self.next_ep(m);
                let mut v = Vec::new();
                if self.released(m).is_none_or(|r| next <= r) {
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
                    let how = if ani::paused() { "paused" } else { "queued" };
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
                let parts = self.parts(m);
                // what a watched episode is called and about; nothing that spoils the rest
                let item = |key: &str, ep: &str, hint: String| {
                    let mut it = Item::new(key, format!("Episode {ep}"), hint);
                    let (p, n) = part(m, parts, ep);
                    it.done = watched(m, parts, ep);
                    let info = self.ep_info.get(&p.id).and_then(|i| i.get(&n));
                    if let Some(i) = info.filter(|_| it.done) {
                        if !i.title.is_empty() {
                            it.label = format!("{} · {}", it.label, i.title);
                        }
                        it.about = i.about.clone();
                    }
                    if p.id != m.id {
                        it.group = part_name(m.name(), p.name()).to_string();
                    }
                    it
                };
                // also what the provider no longer lists, so it can still be played or deleted
                let mut eps: Vec<String> = self
                    .sources
                    .get(&m.id)
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
                    v.push(match ani::paused() {
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
            View::Confirm(Pending::Progress(m, ep, part)) => {
                // what it saves, as that can be the entry's last (the second half of a
                // special) or a sequel's, and the list it puts the entry on
                let total = m.episodes.map_or("?".into(), |t| t.to_string());
                let mut hint = format!("saves {ep}/{total}");
                if !part.is_empty() {
                    hint += &format!(" to {part}");
                }
                vec![Item::new("no", "No", ""), Item::new("yes", "Yes", hint)]
            }
            View::Confirm(Pending::Stop(m, eps)) => {
                let hint = deletes(&ani::parts(&ani::folder(m), eps));
                vec![Item::new("no", "No", ""), Item::new("yes", "Yes", hint)]
            }
            View::Confirm(Pending::Quit) => {
                // when it asks about downloads, which it doesn't while they're paused
                let hint = if dl_pending() > 0 && !ani::paused() {
                    "kept for next start"
                } else {
                    ""
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
        let downloads = if ani::paused() { 0 } else { dl_pending() };
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
                    EpMode::Download => self.dl_act(m, keys, None),
                    EpMode::Watched => {
                        let title = format!("Watched up to episode {key} of {}?", m.name());
                        self.ask_progress(&m, &key, title);
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
                if let Err(e) = ani::save_override(m.id, q, &hits[i]) {
                    return self.toast(e.to_string(), true);
                }
                let m = m.clone();
                self.sources.remove(&m.id);
                self.parts.remove(&m.id);
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
                            id: 0,
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
                        if ani::pause(&ani::folder(m), ep) {
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
                    Pending::Progress(m, ep, _) => {
                        let (m, ep, c) = (m.clone(), *ep, self.client.clone());
                        let status = progress_status(&m, ep);
                        self.saves_pending += 1;
                        // the question showed `m`'s progress; Yes mustn't undo a save it didn't
                        // know of (an old copy: a cached list, the episode's start, a late lookup)
                        self.spawn(
                            move || match c.save_progress(m.id, status, m.progress(), ep) {
                                Ok(Ok(entry)) => Msg::Saved(m, Ok(Some(entry))),
                                Ok(Err(now)) => Msg::Moved(m, now),
                                Err(e) => Msg::Saved(m, Err(e)),
                            },
                        );
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
            "next" => self.play(m.clone(), self.next_ep(&m).to_string()),
            "play" => self.resolve(&m, Some(Want::Episodes(EpMode::Play))),
            // what's on disk can be deleted before ani-cli answers, or without it
            "download"
                if !self.sources.contains_key(&m.id) && !self.downloads(&m).on_disk.is_empty() =>
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

    /// Asks before saving the AniList progress up to `ep`. AniList keeps a count, not single
    /// episodes, so Yes is preselected only when that changes no other episode.
    fn ask_progress(&mut self, m: &Media, ep: &str, title: String) {
        let Some((p, n)) = self.progress_target(m, ep) else {
            return self.toast(
                format!("Episode {ep} isn't a number AniList can count"),
                true,
            );
        };
        let part = match p.id == m.id {
            true => String::new(),
            false => part_name(m.name(), p.name()).to_string(),
        };
        let only_this = recounts(p.progress(), n as u32).is_none();
        // what else Yes changes goes in the question, which wraps; a hint would be cut
        let also = [recounts(p.progress(), n as u32), list_move(p, n as u32)];
        let also = also.into_iter().flatten().collect::<Vec<_>>().join(" and ");
        let title = match also.split_at_checked(1) {
            Some((first, rest)) => format!("{title} {}{rest}.", first.to_uppercase()),
            None => title,
        };
        let ask = Pending::Progress(p.clone(), n as u32, part);
        self.push(title, View::Confirm(ask));
        self.top()
            .menu
            .select_key(if only_this { "yes" } else { "no" });
    }

    fn fulfill(&mut self, m: Media, want: Want) {
        match want {
            Want::Play(ep) => self.play(m, ep),
            Want::Episodes(mode) => {
                let next = self.next_ep(&m).to_string();
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
                | View::DlChoice(m, _)
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
        for (_, p) in self.parts.values_mut().flat_map(|p| &mut p.1) {
            if p.id == id {
                p.media_list_entry = entry.clone();
            }
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Shows(id, res, cache) => {
                let fallback = res
                    .as_ref()
                    .is_ok_and(|l| l.iter().any(|m| !m.on_anilist()))
                    && self.stack.iter().any(|s| s.id == id);
                let fetched = res.is_ok();
                self.fill(id, res, |v, list| {
                    if let View::Shows(l, feed) = v {
                        if let Feed::Pages(_, next, _) = feed {
                            *next = (list.len() >= PER_PAGE).then_some(2);
                        }
                        *l = list;
                    }
                });
                // cached only for a screen still open: a closed one's late answer would
                // overwrite what a newer one cached, its pages from More… too
                let screen = self.stack.iter().find(|s| s.id == id);
                if let (true, Some((path, changes)), Some(View::Shows(list, feed))) =
                    (fetched, cache, screen.map(|s| &s.view))
                {
                    let next = match feed {
                        Feed::Pages(_, next, _) => *next,
                        _ => None,
                    };
                    let _ = ani::write_json(&path, &(list, next));
                    // a save landed while this was in flight: keep it, but not as fresh
                    if LIST_CHANGES.load(Ordering::Relaxed) != changes {
                        let file = fs::File::options().write(true).open(&path);
                        let _ = file.map(|f| f.set_modified(std::time::UNIX_EPOCH));
                    }
                }
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
                if LIST_CHANGES.load(Ordering::Relaxed) != changes {
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
                        // before the menu is rebuilt, so it can't offer what went
                        self.want_parts(&m);
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
                        self.outdate_lists();
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
                self.outdate_lists();
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
                // not watched yet, as far as the show or the sequel it's in goes
                let ask = (self.progress_target(&m, &ep))
                    .is_some_and(|(p, n)| !m.watched(&ep) && n > p.progress() as f64);
                match res {
                    Err(e) => self.toast(format!("Couldn't play episode {ep} · {e}"), true),
                    Ok(()) if ask && self.user.is_some() && m.on_anilist() => {
                        let title = format!("Mark episode {ep} of {} as watched?", m.name());
                        self.ask_progress(&m, &ep, title);
                    }
                    Ok(()) => {}
                }
            }
            // a stopped episode's last lines may still come in
            Msg::DlProgress(now) => {
                if is_queued(now.id, &now.ep) {
                    self.dl_now = Some(now);
                }
            }
            Msg::DlDone(label, res) => {
                self.dl_now = None;
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

/// The AniList entry episode `ep` of `m` counts toward, and its number there: past `m`'s
/// own episodes it's in one of the sequels `parts` (each with the episodes before it).
fn part<'a>(m: &'a Media, parts: &'a [(u32, Media)], ep: &str) -> (&'a Media, String) {
    let n = ani::ep_num(ep);
    // a recap 12.5 comes after 12, so it's the 12-episode part's
    match parts
        .iter()
        .rev()
        .find(|(before, _)| n.floor() > *before as f64)
    {
        Some((before, p)) => (p, (n - *before as f64).to_string()),
        None => (m, ep.to_string()),
    }
}

/// What sets a part's name apart from its show's: "Part 2" of "Attack on Titan Season 3
/// Part 2", or all of it.
fn part_name<'a>(show: &str, part: &'a str) -> &'a str {
    let rest = part
        .strip_prefix(show)
        .map(|r| r.trim_start_matches([' ', ':', '-']));
    rest.filter(|r| !r.is_empty()).unwrap_or(part)
}

/// Up to its show's progress (which ran past its episodes before sequels counted) or its
/// part's.
fn watched(m: &Media, parts: &[(u32, Media)], ep: &str) -> bool {
    let (p, n) = part(m, parts, ep);
    m.watched(ep) || p.watched(&n)
}

/// The AniList entry that marking episode `ep` of `m` watched saves to, and the progress
/// there: never past that entry's episodes, which AniList would keep as progress like
/// 13/12. An episode after them (the second half of a special the provider splits in two,
/// an extra, one of a sequel not looked up yet) saves the entry's last. None for one that
/// isn't a number.
fn progress_target<'a>(
    m: &'a Media,
    parts: &'a [(u32, Media)],
    ep: &str,
) -> Option<(&'a Media, f64)> {
    ep.parse::<f64>().ok()?;
    let (p, n) = part(m, parts, ep);
    let n = ani::ep_num(&n);
    Some((p, p.episodes.map_or(n, |e| n.min(e as f64))))
}

/// The episode after the last one watched. It goes on into a part once everything before
/// it is, so rewatching the first part doesn't skip to the end of a finished second one.
fn next_ep(m: &Media, parts: &[(u32, Media)]) -> u32 {
    let mut next = m.progress() + 1;
    for (before, p) in parts {
        if next > *before {
            next = next.max(before + p.progress() + 1);
        }
    }
    next
}

/// What saving progress `to` over `from` does to the episodes between, since AniList counts
/// every one up to the progress as watched.
fn recounts(from: u32, to: u32) -> Option<String> {
    let span = |a: u32, b: u32| match a == b {
        true => a.to_string(),
        false => format!("{a}–{b}"),
    };
    match to {
        _ if to > from + 1 => Some(format!("also marks {}", span(from + 1, to - 1))),
        _ if to < from => Some(format!("unmarks {}", span(to + 1, from))),
        _ => None,
    }
}

/// The list a show moves to when episode `ep` is marked watched, if it does.
fn list_move(m: &Media, ep: u32) -> Option<String> {
    let to = progress_status(m, ep);
    match m.media_list_entry.as_ref().map(|e| e.status.as_str()) {
        Some(s) if s == to => None,
        Some(_) => Some(format!("moves it to {}", list_label(to))),
        None => Some(format!("adds it to {}", list_label(to))),
    }
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

    #[test]
    fn says_what_a_jump_in_progress_counts() {
        assert_eq!(super::recounts(5, 6), None); // the next one
        assert_eq!(super::recounts(5, 5), None);
        assert_eq!(super::recounts(5, 7).unwrap(), "also marks 6");
        assert_eq!(super::recounts(0, 1180).unwrap(), "also marks 1–1179");
        assert_eq!(super::recounts(1180, 5).unwrap(), "unmarks 6–1180");
        assert_eq!(super::recounts(6, 5).unwrap(), "unmarks 6");
    }

    #[test]
    fn episodes_past_a_split_season_count_toward_its_sequel() {
        let entry = |progress| {
            Some(super::Entry {
                id: 1,
                status: "CURRENT".into(),
                progress,
            })
        };
        let show = |id, episodes, progress| super::Media {
            id,
            episodes: Some(episodes),
            media_list_entry: entry(progress),
            ..Default::default()
        };
        // Attack on Titan Season 3 at ani-cli: 22 episodes, 12 and 10 on AniList
        let (s3, parts) = (show(1, 12, 12), [(12, show(2, 10, 3))]);
        let at = |ep: &str| {
            let (m, n) = super::part(&s3, &parts, ep);
            (m.id, n)
        };
        assert_eq!(at("5"), (1, "5".into()));
        assert_eq!(at("12.5"), (1, "12.5".into()), "a recap stays where it was");
        assert_eq!(at("13"), (2, "1".into()));
        assert_eq!(at("22"), (2, "10".into()));
        assert_eq!(super::part(&s3, &[], "13").1, "13", "no sequels known");
        let (m, n) = super::part(&s3, &parts, "15");
        assert!(m.watched(&n) && !super::part(&s3, &parts, "16").0.watched("4"));
        // nothing saves past an entry's episodes, as while its sequel is looked up: after
        // them it's the entry's last
        let save = |parts: &[(u32, super::Media)], ep| {
            super::progress_target(&s3, parts, ep).map(|(m, n)| (m.id, n))
        };
        assert_eq!(save(&parts, "13"), Some((2, 1.0)));
        assert_eq!(save(&parts, "12.5"), Some((1, 12.0)));
        assert_eq!(
            save(&[], "13"),
            Some((1, 12.0)),
            "the show's last, not 13/12"
        );
        assert_eq!(save(&parts, "23"), Some((2, 10.0)), "Part 2's last");
        assert_eq!(save(&parts, "SP"), None);
        let short = |part| super::part_name("Attack on Titan Season 3", part);
        assert_eq!(short("Attack on Titan Season 3 Part 2"), "Part 2");
        assert_eq!(
            short("Attack on Titan Season 3"),
            "Attack on Titan Season 3"
        );
        assert_eq!(short("The Final Season"), "The Final Season");
        // a one-episode special ani-cli has in two halves
        let special = show(3, 1, 0);
        let second_half = super::progress_target(&special, &[], "2").map(|(m, n)| (m.id, n));
        assert_eq!(second_half, Some((3, 1.0)));
        let airing = super::Media::default();
        assert!(
            super::progress_target(&airing, &[], "13").is_some(),
            "no count yet"
        );
        let next = |own, second| super::next_ep(&show(1, 12, own), &[(12, show(2, 10, second))]);
        assert_eq!(next(12, 3), 16);
        assert_eq!(next(12, 0), 13);
        assert_eq!(next(3, 10), 4, "rewatching the first part");
        assert_eq!(
            next(22, 0),
            23,
            "progress saved past 12 before sequels counted"
        );
        assert_eq!(next(12, 10), 23);
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
        // a whole-show job takes what's after the progress and not on disk
        let m = super::Media {
            media_list_entry: Some(super::Entry {
                id: 1,
                status: "CURRENT".into(),
                progress: 1,
            }),
            ..Default::default()
        };
        let all = s(&["1", "2", "3", "3.5"]);
        let on_disk = [("3".to_string(), file)];
        assert_eq!(super::to_download(&m, &all, &on_disk), s(&["2", "3.5"]));
    }
}
