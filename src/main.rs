#![allow(clippy::large_enum_variant)] // a handful of screens, size is irrelevant

mod ani;
mod anilist;
mod menu;
mod ui;

use ani::Source;
use anilist::{Client, Entry, Media};
use menu::{Act, Item, Menu};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    /// anime list; `true` = local downloads (rescanned on refresh)
    Shows(Vec<Media>, bool),
    Show(Media),
    Episodes(Media, EpMode),
    Status(Media),
    Sources(Media, String, Vec<String>),
    Bulk(Vec<Media>),
    Confirm(Pending),
    Input(Input),
}

#[derive(Clone)]
pub enum Input {
    Search,
    ClientId,
    Token,
    SourceQuery(Media),
}

pub enum Pending {
    Delete(Vec<PathBuf>),
    Progress(Media, String),
    Quit,
}

pub struct Screen {
    pub id: u64,
    pub title: String,
    pub view: View,
    pub menu: Menu,
    pub loading: bool,
}

pub enum Cover {
    Loading(Size),
    Ready(Size, Protocol),
    Failed,
}

enum Want {
    Episodes(EpMode),
    Play(String),
}

struct Job {
    media: Media,
    source: Option<Source>,
    /// None = everything after the AniList progress
    eps: Option<Vec<String>>,
}

enum Msg {
    Shows(u64, Result<Vec<Media>, String>),
    Hits(u64, Result<Vec<String>, String>),
    Viewer(Result<(u64, String), String>),
    Cover(u64, Size, Result<Protocol, String>),
    Resolved(Media, Result<Source, String>),
    Saved(Media, Result<Option<Entry>, String>),
    Played(Media, String, Result<(), String>),
    DlAdd(isize),
    DlProgress(String),
    DlDone(String, Result<(), String>),
}

pub struct App {
    pub stack: Vec<Screen>,
    next_id: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Client,
    pub user: Option<(u64, String)>,
    pub picker: Picker,
    pub covers: HashMap<u64, Cover>,
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
    pub playing: Option<String>,
    pub toast: Option<(String, bool, Instant)>,
    pub started: Instant,
    /// media whose title glint plays, and since when
    pub glint: (u64, Instant),
    pub missing: Vec<(&'static str, &'static str, &'static str)>,
    /// shell command to run in the real terminal (TUI paused), e.g. installing packages
    external: Option<String>,
    quit: bool,
}

fn main() -> std::io::Result<()> {
    ani::setup()?;
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty graphics only (Kitty and Ghostty); the query is just for font size and o=z support.
    let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    picker.set_protocol_type(ProtocolType::Kitty);

    let mut app = App::new(picker);
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
    ani::stop_download();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
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

fn list_cache(key: &str) -> PathBuf {
    ani::cache_dir().join("lists").join(format!("{key}.json"))
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
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Cover from the disk cache (or AniList), scaled and encoded for the terminal.
fn load_cover(picker: &Picker, id: u64, url: &str, size: Size) -> Result<Protocol, String> {
    let path = ani::cache_dir().join("covers").join(format!("{id}.jpg"));
    let bytes = fs::read(&path).or_else(|_| {
        let bytes = anilist::fetch(url)?;
        let _ = fs::create_dir_all(path.parent().unwrap());
        let _ = fs::write(&path, &bytes);
        Ok::<_, String>(bytes)
    })?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let resize = Resize::Scale(Some(image::imageops::FilterType::Triangle));
    picker
        .new_protocol(img, size, resize)
        .map_err(|e| e.to_string())
}

fn spawn_downloader(tx: Sender<Msg>) -> Sender<Job> {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        for job in job_rx {
            let m = job.media;
            let src = match job.source.map_or_else(|| ani::resolve(&m), Ok) {
                Ok(s) => s,
                Err(e) => {
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
                eps
            });
            for ep in eps {
                let label = format!("{} E{ep}", m.name());
                let _ = tx.send(Msg::DlProgress(format!("{label} · starting")));
                let res = ani::download(&m, &src, &ep, |p| {
                    _ = tx.send(Msg::DlProgress(format!("{label} · {p}")))
                });
                let _ = tx.send(Msg::DlDone(label, res));
            }
        }
    });
    job_tx
}

impl App {
    fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::channel();
        let token = fs::read_to_string(token_path())
            .ok()
            .map(|t| t.trim().to_string());
        let user: Option<(u64, String, bool)> = fs::read(user_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|_| token.is_some());
        if let Some((_, _, english)) = &user {
            anilist::ENGLISH_TITLES.store(*english, std::sync::atomic::Ordering::Relaxed);
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
            picker,
            covers: HashMap::new(),
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
            external: None,
            quit: false,
        };
        app.scan_local();
        app.push("ani-hop", View::Main);
        let welcomed = ani::config_dir().join("welcomed");
        let first = !welcomed.exists();
        if first || !app.missing.is_empty() {
            let _ = fs::create_dir_all(ani::config_dir());
            let _ = fs::write(&welcomed, "");
            app.push(if first { "Welcome" } else { "Setup" }, View::Setup);
        }
        if app.client.token.is_some() {
            let c = app.client.clone();
            app.spawn(move || Msg::Viewer(c.viewer()));
        }
        app
    }

    fn after_install(&mut self) {
        self.missing = ani::missing_deps();
        let names: Vec<&str> = self.missing.iter().map(|d| d.0).collect();
        match names.is_empty() {
            true => self.toast("All set: every tool is installed", false),
            false => self.toast(format!("Still missing: {}", names.join(", ")), true),
        }
        self.refresh();
    }

    pub fn top(&mut self) -> &mut Screen {
        self.stack.last_mut().unwrap()
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
        cache: Option<String>,
        f: impl FnOnce() -> Result<Vec<Media>, String> + Send + 'static,
    ) {
        let path = cache.map(|k| list_cache(&k));
        let cached = path
            .as_ref()
            .and_then(|p| serde_json::from_slice(&fs::read(p).ok()?).ok());
        let id = self.push(title, View::Shows(cached.unwrap_or_default(), false));
        self.top().loading = true;
        self.spawn(move || {
            let res = f();
            if let (Ok(list), Some(p)) = (&res, path) {
                let _ = fs::create_dir_all(p.parent().unwrap());
                let _ = fs::write(p, serde_json::to_vec(list).unwrap_or_default());
            }
            Msg::Shows(id, res)
        });
    }

    fn pop(&mut self, n: usize) {
        self.stack
            .truncate(self.stack.len().saturating_sub(n).max(1));
        self.refresh();
    }

    /// Rebuilds the visible menu from current state (keeps query and cursor).
    fn refresh(&mut self) {
        self.scan_local();
        if let View::Shows(list, true) = &mut self.top().view {
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

    pub fn want_cover(&mut self, m: &Media) {
        let size = self.poster;
        let fresh = match self.covers.get(&m.id) {
            Some(Cover::Loading(s) | Cover::Ready(s, _)) => *s == size,
            Some(Cover::Failed) => true,
            None => false,
        };
        if fresh || size.width == 0 {
            return;
        }
        // at most 4 posters in flight; the frame after one arrives asks again for the rest
        if self
            .covers
            .values()
            .filter(|c| matches!(c, Cover::Loading(_)))
            .count()
            >= 4
        {
            return;
        }
        // ponytail: drop everything past ~100 posters (~40 MB); they reload from disk cache in ms
        if self.covers.len() > 100 {
            self.covers.clear();
        }
        match m.cover_image.extra_large.clone() {
            Some(url) => {
                self.covers.insert(m.id, Cover::Loading(size));
                let (id, picker) = (m.id, self.picker.clone());
                self.spawn(move || Msg::Cover(id, size, load_cover(&picker, id, &url, size)));
            }
            None => _ = self.covers.insert(m.id, Cover::Failed),
        }
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
                v.push(match (&self.client.token, &self.user) {
                    (Some(_), user) => Item::new(
                        "logout",
                        "Log out of AniList",
                        user.as_ref().map_or("", |u| u.1.as_str()),
                    ),
                    (None, _) => Item::new("login", "Log in to AniList", ""),
                });
                v.push(Item::new("quit", "Quit", ""));
                v
            }
            View::Setup => {
                let mut v = Vec::new();
                let pkgs: Vec<&str> = self.missing.iter().map(|d| d.1).collect();
                if let Some(cmd) = ani::install_command(&pkgs).filter(|_| !pkgs.is_empty()) {
                    v.push(Item::new("install", "Install missing packages", cmd));
                }
                if self.user.is_none() {
                    v.push(Item::new(
                        "login",
                        "Log in to AniList",
                        "your lists + progress sync",
                    ));
                }
                v.push(Item::new("continue", "Continue", ""));
                v
            }
            View::Shows(list, _) => list
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
                .collect(),
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
                if self.user.is_some() {
                    let status = m
                        .media_list_entry
                        .as_ref()
                        .map(|e| list_label(&e.status))
                        .unwrap_or("not on list");
                    v.push(Item::new("status", "★  List status…", status));
                }
                v.push(Item::new(
                    "source",
                    "⇄  Change source…",
                    src.map(|s| s.title.clone()).unwrap_or_default(),
                ));
                v.push(Item::new("web", "↗  Open on AniList", ""));
                v
            }
            View::Episodes(m, mode) => {
                let local = ani::local_episodes(&ani::folder(m));
                if *mode == EpMode::Delete {
                    return local
                        .into_iter()
                        .map(|(ep, p)| {
                            let mb = fs::metadata(&p).map_or(0, |md| md.len() >> 20);
                            Item::new(
                                p.to_string_lossy(),
                                format!("Episode {ep}"),
                                format!("{mb} MB"),
                            )
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
                        let mut hint = Vec::new();
                        if local.iter().any(|(e, _)| *e == ep) {
                            hint.push("on disk");
                        }
                        if ani::ep_num(&ep) <= m.progress() as f64 {
                            hint.push("watched");
                        }
                        Item::new(ep.clone(), format!("Episode {ep}"), hint.join(" · "))
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
                    v.push(Item::new("remove", "Remove from list", ""));
                }
                v
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
                let which = if self.user.is_some() {
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
                        format!("✗  Delete downloads of {local} shows"),
                        "",
                    ));
                }
                v
            }
            View::Confirm(_) => vec![Item::new("no", "No", ""), Item::new("yes", "Yes", "")],
            View::Input(_) => Vec::new(),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'q'))
        {
            return self.request_quit();
        }
        match self.top().menu.key(k) {
            Some(Act::Accept) => self.accept(),
            Some(Act::Back) => self.pop(1),
            None => {}
        }
    }

    fn request_quit(&mut self) {
        if self.dl_pending > 0 && !matches!(self.top().view, View::Confirm(Pending::Quit)) {
            self.push(
                format!(
                    "{} left, quit anyway?",
                    plural(self.dl_pending as usize, "download")
                ),
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
            View::Main => self.main_action(&key, &label),
            View::Setup => match key.as_str() {
                "install" => self.external = screen.menu.current().map(|i| i.hint.clone()),
                "login" => self.main_action("login", ""),
                _ => self.pop(1),
            },
            View::Shows(list, _) => {
                let picked: Vec<Media> = keys
                    .iter()
                    .filter_map(|k| list.iter().find(|m| m.id.to_string() == *k).cloned())
                    .collect();
                self.top().menu.marked.clear();
                if picked.len() > 1 {
                    self.push(format!("{} shows", picked.len()), View::Bulk(picked));
                } else if let Some(m) = picked.into_iter().next() {
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
                        let _ = self.dl_tx.send(Job {
                            media: m,
                            source,
                            eps: Some(keys),
                        });
                        self.dl_pending += n as isize;
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
            View::Status(m) => {
                let (m, c) = (m.clone(), self.client.clone());
                match (key.as_str(), m.media_list_entry.as_ref().map(|e| e.id)) {
                    ("remove", Some(entry)) => {
                        self.spawn(move || Msg::Saved(m, c.remove(entry).map(|_| None)))
                    }
                    (status, _) => {
                        let status = status.to_string();
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
                    for m in list {
                        let _ = self.dl_tx.send(Job {
                            media: m,
                            source: None,
                            eps: None,
                        });
                        self.dl_pending += 1;
                    }
                    self.toast("Queued, looking up episodes…", false);
                    self.pop(1);
                } else {
                    let dirs: Vec<PathBuf> = list
                        .iter()
                        .map(ani::folder)
                        .filter(|d| d.exists())
                        .collect();
                    self.push(
                        format!("Delete all downloads of {}?", plural(dirs.len(), "show")),
                        View::Confirm(Pending::Delete(dirs)),
                    );
                }
            }
            View::Confirm(p) => {
                if key != "yes" {
                    return self.pop(1);
                }
                match p {
                    Pending::Quit => self.quit = true,
                    Pending::Delete(paths) => {
                        let mut failed = 0;
                        for p in paths {
                            let res = if p.is_dir() {
                                fs::remove_dir_all(p)
                            } else {
                                ani::delete_episode(p)
                            };
                            failed += res.is_err() as usize;
                        }
                        let n = paths.len();
                        self.toast(format!("Deleted {} of {n}", n - failed), failed > 0);
                        self.pop(2);
                    }
                    Pending::Progress(m, ep) => {
                        let (m, ep, c) = (m.clone(), ani::ep_num(ep) as u32, self.client.clone());
                        let status = match m.media_list_entry.as_ref().map(|e| e.status.as_str()) {
                            _ if m.episodes == Some(ep) => "COMPLETED",
                            Some("REPEATING") => "REPEATING",
                            _ => "CURRENT",
                        };
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

    fn main_action(&mut self, key: &str, label: &str) {
        let c = self.client.clone();
        if let Some(sort) = key.strip_prefix("browse:") {
            let sort = sort.to_string();
            return self.push_loading(label, Some(sort.clone()), move || c.browse(&sort, None));
        }
        match key {
            "search" => _ = self.push("Search", View::Input(Input::Search)),
            "downloads" => {
                let list = ani::local_shows().into_iter().map(|(m, _)| m).collect();
                self.push("Downloads", View::Shows(list, true));
            }
            "login" => match client_id() {
                Some(id) => self.start_auth(&id),
                None => {
                    open_url("https://anilist.co/settings/developer");
                    self.push("AniList setup", View::Input(Input::ClientId));
                }
            },
            "logout" => {
                let _ = fs::remove_file(token_path());
                let _ = fs::remove_file(user_path());
                (self.client.token, self.user) = (None, None);
                self.toast("Logged out", false);
                self.refresh();
            }
            "quit" => self.request_quit(),
            list => {
                let status = list.trim_start_matches("list:").to_string();
                let user = self.user.as_ref().map_or(0, |u| u.0);
                let cache = format!("{user}-{status}");
                self.push_loading(list_label(&status), Some(cache), move || {
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
        if text.is_empty() {
            return;
        }
        match input {
            Input::Search => {
                let c = self.client.clone();
                let title = format!("Search: {text}");
                self.push_loading(&title, None, move || c.browse("SEARCH_MATCH", Some(&text)));
            }
            Input::ClientId => {
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
        if let Some(slot) = self.resolving.get_mut(&m.id) {
            if want.is_some() {
                *slot = want;
            }
            return;
        }
        if want.is_some() {
            self.toast(format!("Searching ani-cli for {}…", m.name()), false);
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
        let local = ani::local_episodes(&ani::folder(&m))
            .into_iter()
            .find(|(e, _)| *e == ep);
        let src = self.sources.get(&m.id).cloned();
        if local.is_none() && src.is_none() {
            return self.resolve(&m, Some(Want::Play(ep)));
        }
        self.playing = Some(format!("{} · Episode {ep}", m.name()));
        let cached = !self.fresh.contains(&m.id);
        self.spawn(move || {
            let res = match (&local, src) {
                (Some((_, file)), _) => ani::play_file(&m, &ep, file),
                (_, Some(src)) if cached => {
                    ani::locate(&m, &src).and_then(|s| ani::stream(&s, &ep))
                }
                (_, Some(src)) => ani::stream(&src, &ep),
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

    fn update_media(&mut self, id: u64, entry: Option<Entry>) {
        for s in &mut self.stack {
            let media: Vec<&mut Media> = match &mut s.view {
                View::Show(m) | View::Episodes(m, _) | View::Status(m) | View::Sources(m, ..) => {
                    vec![m]
                }
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
            Msg::Shows(id, res) => self.fill(id, res, |v, list| *v = View::Shows(list, false)),
            Msg::Hits(id, res) => self.fill(id, res, |v, hits| {
                if let View::Sources(_, _, h) = v {
                    *h = hits;
                }
            }),
            Msg::Viewer(Ok(user)) => {
                let english = anilist::ENGLISH_TITLES.load(std::sync::atomic::Ordering::Relaxed);
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
                    self.stack.truncate(1);
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
                    let _ = fs::remove_file(token_path());
                    let _ = fs::remove_file(user_path());
                    (self.client.token, self.user) = (None, None);
                    self.stack.truncate(1);
                }
                self.toast(format!("AniList login failed: {e}"), true);
                self.refresh();
            }
            Msg::Cover(id, size, res) => {
                if matches!(self.covers.get(&id), Some(Cover::Loading(s)) if *s == size) {
                    self.covers
                        .insert(id, res.map_or(Cover::Failed, |p| Cover::Ready(size, p)));
                }
            }
            Msg::Resolved(m, res) => {
                let want = self.resolving.remove(&m.id).flatten();
                let here = matches!(&self.top().view, View::Show(s) | View::Episodes(s, _) if s.id == m.id);
                match res {
                    Ok(src) => {
                        self.sources.insert(m.id, src);
                        self.fresh.insert(m.id);
                        ani::save_resolved(&self.sources);
                        if let (Some(w), true) = (want, here) {
                            self.fulfill(m, w);
                        }
                    }
                    Err(e) if want.is_some() || here => self.toast(format!("ani-cli: {e}"), true),
                    Err(_) => {}
                }
                self.refresh();
            }
            Msg::Saved(m, res) => match res {
                Ok(entry) => {
                    self.toast(format!("Saved {}", m.name()), false);
                    self.update_media(m.id, entry);
                    if matches!(self.top().view, View::Status(_)) {
                        self.stack.pop();
                    }
                    self.refresh();
                }
                Err(e) => self.toast(e, true),
            },
            Msg::Played(m, ep, res) => {
                self.playing = None;
                match res {
                    Err(e) => self.toast(e, true),
                    Ok(()) if self.user.is_some() && ani::ep_num(&ep) > m.progress() as f64 => {
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
        }
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
}
