#![allow(clippy::large_enum_variant)] // a handful of screens, size is irrelevant

mod ani;
mod anilist;
mod downloads;
mod entries;
mod kitty;
mod lists;
mod menu;
mod playback;
mod preview;
mod progress;
mod show_downloads;
mod source;
mod ui;

use anilist::{Client, EpInfo, Media};
use downloads::{AniCli, Job, Queue};
use entries::{Change, Entries, Stamp};
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
use show_downloads::{Action, ShowDownloads, which};
use source::{Lookup, Source, Sources};
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

enum Msg {
    /// a list's refreshed shows
    Shows(u64, Stamp, Result<Vec<Media>, String>, lists::Ask),
    /// the downloaded shows as AniList has them now
    Local(Stamp, Result<Vec<Media>, String>),
    /// the next page of a ranking
    More(u64, Stamp, Result<Vec<Media>, String>),
    Hits(u64, Result<Vec<String>, String>),
    /// who's logged in, asked with this token
    Viewer(Option<String>, Result<(u64, String, bool), String>),
    EpInfo(u64, HashMap<String, EpInfo>),
    /// sequels of a show, looked up for its episode counts (App::parts_key)
    Parts(u64, (u32, u32), Stamp, Result<Parts, String>),
    /// a lookup's answer, with its ticket (Sources::ask)
    Resolved(Media, u64, Result<Source, String>),
    /// a change to the user's list, answered
    Entry(entries::Done),
    /// (Playback attempt, what it waits on now; None = the player is up)
    PlayStep(u64, Option<&'static str>),
    /// (Playback attempt, show, episode, how it ended)
    Played(u64, Media, String, Result<(), String>),
    /// what a rescan of the download folder found: the shows and their episodes
    Scanned(Vec<(Media, OnDisk)>),
    /// an episode downloaded or failed: ("Show E3", result)
    DlDone(String, Result<(), String>),
    /// (version, update command) of a newer ani-hop
    Release(Option<(String, String)>),
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
    /// before it (empty while loading or when there are none), and the episode counts they
    /// were looked up for
    parts: HashMap<u64, ((u32, u32), Parts)>,
    pub sources: Sources,
    /// downloaded episodes per show, as the last rescan found them
    pub local: HashMap<u64, OnDisk>,
    /// the downloaded shows, as the last rescan found them (none before the first)
    downloaded: Option<Vec<Media>>,
    /// a rescan of the download folder is on its way, and (true) another was asked for since
    scanning: Option<bool>,
    /// shows rescanned on screen since the running rescan started, newer than what it finds:
    /// each one if it's downloaded
    synced: HashMap<u64, Option<Media>>,
    /// Downloads opened: AniList is asked about its shows once the rescan has them
    ask_local: bool,
    /// episodes per show other running ani-hops have queued, rescanned on refresh
    elsewhere: HashMap<u64, Vec<String>>,
    resolving: HashMap<u64, Option<Want>>,
    pub downloads: Queue,
    pub playback: Playback,
    pub toast: Option<(String, bool, Instant)>,
    pub started: Instant,
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
        let entries = Entries::new(Arc::new(client.clone()));
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
            preview: Preview::new(kitty::Posters::new(kitty::Tty, kitty::supported())),
            entries,
            ep_info: HashMap::new(),
            parts: HashMap::new(),
            sources: Sources::load(),
            local: HashMap::new(),
            downloaded: None,
            scanning: None,
            synced: HashMap::new(),
            ask_local: false,
            elsewhere: HashMap::new(),
            resolving: HashMap::new(),
            playback: Playback::new(Arc::new(playback::Mpv)),
            toast: None,
            started: Instant::now(),
            missing: ani::missing_deps(),
            old_ani_cli: ani::old_ani_cli(),
            release: None,
            external: None,
            parked: None,
            quit: false,
        };
        app.push("ani-hop", View::Main);
        let welcomed = ani::config_dir().join("welcomed");
        let first = !welcomed.exists();
        let needed = app.missing.iter().any(|d| !ani::optional(d.0));
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
            app.spawn(move || Msg::Viewer(c.token.clone(), c.viewer()));
        }
        app.spawn(|| Msg::Release(newer_release().map(|v| (v, ani::update_command()))));
        app
    }

    fn after_install(&mut self) {
        self.missing = ani::missing_deps();
        self.old_ani_cli = ani::old_ani_cli();
        let names: Vec<&str> = (self.missing.iter().map(|d| d.0))
            .filter(|&n| !ani::optional(n))
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
        self.entries.login(Arc::new(self.client.clone()));
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
            View::Episodes(m, EpMode::Download) => m,
            View::Episodes(_, EpMode::Play) => return Some("play"),
            View::Input(Input::Search | Input::SourceQuery(_)) => return Some("search"),
            View::Input(_) => return Some("save"),
            _ => return Some("select"),
        };
        let eps: Vec<String> = (screen.menu.selection().iter())
            .map(|&i| screen.menu.items[i].key.clone())
            .collect();
        self.show_downloads(m).verb(&eps)
    }

    fn spawn(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        thread::spawn(move || _ = tx.send(f()));
    }

    fn toast(&mut self, text: impl Into<String>, err: bool) {
        self.toast = Some((text.into(), err, Instant::now()));
    }

    fn push(&mut self, title: impl Into<String>, view: View) -> u64 {
        // episode titles load while the show is open, so its episode list has them right away
        if let View::Show(m) | View::Episodes(m, _) = &view {
            self.want_episodes(m);
        }
        let multi = matches!(view, View::Shows(..) | View::Episodes(_, EpMode::Download));
        self.scan_local(shown(&view));
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
            self.spawn(move || Msg::Shows(id, stamp, f(), ask));
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
    }

    /// Rescans what's downloaded and rebuilds the visible menu (keeps query and cursor).
    fn refresh(&mut self) {
        let m = shown(&self.stack.last().unwrap().view).cloned();
        self.scan_local(m.as_ref());
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

    /// Rescans what's downloaded: `m`'s folder at once, so what was just done to it shows,
    /// and every show folder in the background, as the download folder may be on a slow disk
    /// (a NAS, a USB disk asleep). Other ani-hops' queues are in the cache, read at once.
    fn scan_local(&mut self, m: Option<&Media>) {
        self.elsewhere = self.downloads.elsewhere();
        if let Some(m) = m {
            let dir = ani::folder(m);
            match ani::local_episodes(&dir) {
                eps if eps.is_empty() => _ = self.local.remove(&m.id),
                eps => _ = self.local.insert(m.id, eps),
            }
            self.synced.insert(m.id, ani::ours(&dir).then(|| m.clone()));
        }
        match &mut self.scanning {
            Some(again) => *again = true,
            None => {
                self.scanning = Some(false);
                self.synced.clear(); // what this rescan finds is newer
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
            self.parts.insert(m.id, (key, Vec::new()));
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
            Some((key, parts)) if Some(*key) == self.parts_key(m) => parts,
            _ => &[],
        }
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
                let local = self.downloaded.as_ref().map_or(0, Vec::len);
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
                audio_item("Default audio…", ani::audio(None)),
                Item::new(
                    "adult",
                    "Adult shows",
                    if ani::adult_on() { "on" } else { "off" },
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
                        if let Some(e) = self.entries.entry(m) {
                            hint.push(format!(
                                "{}/{}",
                                e.progress,
                                m.episodes.map_or("?".into(), |e| e.to_string())
                            ));
                        } else if let Some(e) = m.episodes {
                            hint.push(format!("{e} ep"));
                        }
                        hint.extend(m.season_year.map(|y| y.to_string()));
                        let mut it = Item::new(m.id.to_string(), m.name(), hint.join(" · "));
                        // found by its other titles too: "attack on titan", "aot"
                        it.about = m.titles().cloned().collect::<Vec<_>>().join(" · ");
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
                let (next, released) = (self.progress(m).next(), self.progress(m).released());
                let mut v = Vec::new();
                if released.is_none_or(|r| next <= r) {
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
                (eps.iter().zip(seasons))
                    .map(|(ep, season)| item(ep, ep, dl.hint(ep, sizes), season))
                    .collect()
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
                    Pending::Progress(mark) => ("Don't save", "Save", "", mark.hint()),
                    Pending::Quit => {
                        // when it asks about downloads, which it doesn't while they're paused
                        let s = self.downloads.snapshot();
                        let hint = match s.pending > 0 && !s.paused {
                            true => "downloads kept for next start",
                            false => "",
                        };
                        ("Stay", "Quit", "", hint.into())
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
        let saves = self.entries.pending();
        let busy = downloads > 0 || saves > 0;
        if busy && !matches!(self.top().view, View::Confirm(Pending::Quit)) {
            let mut left = Vec::new();
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
                self.spawn(move || Msg::Entry(work.run()));
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
                        self.spawn(move || Msg::Entry(work.run()));
                    }
                    Pending::Progress(k) => {
                        // saved only while AniList still has the progress the question showed
                        let work = self.entries.begin(Change::Progress(k.clone()));
                        self.spawn(move || Msg::Entry(work.run()));
                        self.pop(1);
                    }
                }
            }
            View::Input(input) => self.submit(input.clone(), query),
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
            "adult" => {
                let on = !ani::adult_on();
                if let Err(e) = ani::set_adult(on) {
                    return self.toast(e.to_string(), true);
                }
                lists::changed(); // rankings cached with or without them
                let state = if on { "on" } else { "off" };
                self.toast(
                    format!("Adult shows in searches and rankings: {state}"),
                    false,
                );
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

    /// The one-time setup of an AniList API client of the user's own; a saved client ID stays
    /// until another one is entered.
    fn ask_client_id(&mut self) {
        open_url("https://anilist.co/settings/developer");
        self.push("AniList setup", View::Input(Input::ClientId));
    }

    fn start_auth(&mut self, client_id: &str) {
        open_url(&format!(
            "https://anilist.co/api/v2/oauth/authorize?client_id={client_id}&response_type=token"
        ));
        self.push("AniList login", View::Input(Input::Token));
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
                self.push_loading(&text.clone(), feed, None, move || {
                    source::or_ani_cli(|q| c.search(q, 1), &text, source::search)
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
        self.push(m.name().to_string(), View::Show(m));
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
        let file = ani::local_episodes(&ani::folder(&m))
            .into_iter()
            .find(|(e, _)| *e == ep);
        let (lookup, audio) = (self.sources.lookup(m.id), ani::audio(Some(m.id)));
        match (self.playback).start(&m, &ep, file.map(|f| f.1), lookup, audio) {
            Start::Busy(title) => self.toast(format!("Still starting {title}…"), false),
            Start::Lookup => self.resolve(&m, Some(Want::Play(ep))),
            Start::Run(run) => {
                let (tx, attempt) = (self.tx.clone(), run.attempt);
                self.spawn(move || {
                    let res = run.run(&|step| _ = tx.send(Msg::PlayStep(attempt, step)));
                    Msg::Played(attempt, m, ep, res)
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
            Msg::Shows(id, stamp, res, ask) => {
                let res = self.observed(stamp, res);
                let fallback = res
                    .as_ref()
                    .is_ok_and(|l| l.iter().any(|m| !m.on_anilist()))
                    && self.stack.iter().any(|s| s.id == id);
                self.fill(id, |v| match v {
                    View::Shows(list) => list.refreshed(ask, res),
                    _ => Ok(()),
                });
                // a show's page on top shows their Entries too, the newest now
                self.rebuild();
                if fallback {
                    self.toast("AniList search failed, showing ani-cli's matches", true);
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
            Msg::Hits(id, res) => self.fill(id, |v| {
                if let View::Sources(_, _, h) = v {
                    *h = res?;
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
                self.user = Some((id, name));
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
            Msg::Parts(id, key, stamp, parts) => {
                // looked up for other episode counts, or an earlier login
                let wanted = self.parts.get(&id).is_some_and(|p| p.0 == key);
                let mut parts = parts.unwrap_or_default();
                if !wanted
                    || !self
                        .entries
                        .observe(stamp, parts.iter_mut().map(|p| &mut p.1))
                {
                    return;
                }
                for (_, p) in &parts {
                    self.want_episodes(p);
                }
                self.parts.insert(id, (key, parts));
                self.refresh();
            }
            Msg::Resolved(m, ticket, res) => {
                // a lookup from before the source was changed by hand
                if !self.sources.answer(m.id, ticket, &res) {
                    return;
                }
                let (id, want) = (m.id, self.resolving.remove(&m.id).flatten());
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
            }
            Msg::Entry(done) => {
                let Some(n) = self.entries.finish(done) else {
                    return; // an earlier login's
                };
                self.toast(n.text, n.error);
                // its list status question is answered, not another show's
                if matches!(&self.top().view, View::Status(s) if Some(s.id) == n.closes) {
                    self.stack.pop();
                }
                self.refresh();
            }
            Msg::PlayStep(attempt, step) => self.playback.step(attempt, step),
            Msg::Played(attempt, m, ep, res) => {
                self.playback.ended(attempt);
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
            Msg::Scanned(found) => {
                self.entries.catch_up();
                let again = self.scanning.take() == Some(true);
                let shows;
                (self.local, shows) = rescanned(found, &self.synced, &self.local);
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
                // and the one asked for meanwhile, for the rest
                if again {
                    self.scan_local(None);
                }
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

/// Episodes on disk per show, and the downloaded shows, by name, as a rescan `found` them,
/// but the shows `synced` on screen since it started as the screen found them (`local`):
/// their episodes, and whether they're downloaded at all.
fn rescanned(
    found: Vec<(Media, OnDisk)>,
    synced: &HashMap<u64, Option<Media>>,
    local: &HashMap<u64, OnDisk>,
) -> (HashMap<u64, OnDisk>, Vec<Media>) {
    let (mut shows, mut eps): (Vec<Media>, HashMap<_, _>) = (found.into_iter())
        .map(|(m, e)| (m.clone(), (m.id, e)))
        .unzip();
    for (id, show) in synced {
        shows.retain(|m| m.id != *id);
        shows.extend(show.clone());
        match local.get(id) {
            Some(e) => _ = eps.insert(*id, e.clone()),
            None => _ = eps.remove(id),
        }
    }
    shows.sort_by(|a, b| a.name().cmp(b.name()));
    (eps, shows)
}

/// The show a screen is about, whose downloads it shows.
fn shown(view: &View) -> Option<&Media> {
    match view {
        View::Show(m) | View::Episodes(m, _) | View::DlChoice(m, _) => Some(m),
        _ => None,
    }
}

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
    #[test]
    fn a_rescan_keeps_what_the_screen_found_since() {
        use std::path::PathBuf;
        let show = |id, name: &str| {
            let mut m = super::Media {
                id,
                ..Default::default()
            };
            m.title.romaji = Some(name.into());
            m
        };
        let eps = |e: &str| vec![(e.to_string(), PathBuf::from(format!("{e}.mp4")))];
        // it found A, deleted on screen since, and C; not B, downloaded on screen since
        let found = vec![(show(1, "A"), eps("1")), (show(3, "C"), eps("1"))];
        let synced = super::HashMap::from([(1, None), (2, Some(show(2, "B")))]);
        let local = super::HashMap::from([(2, eps("4"))]);
        let (now, shows) = super::rescanned(found, &synced, &local);
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["B", "C"]);
        assert!(!now.contains_key(&1) && now[&2] == eps("4") && now.contains_key(&3));
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
}
