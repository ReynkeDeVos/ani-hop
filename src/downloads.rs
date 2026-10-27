//! The download queue: what this ani-hop still has to download, kept on disk so the next
//! start can resume it after quitting early, a crash or a closed terminal; the worker that
//! goes through it one episode at a time; and holding (pause), stopping and resuming the
//! running one. Several ani-hops can run at once, each with its own queue file.
//!
//! Everything shares one lock, so a stop or a pause can't slip between the worker checking
//! an episode is still wanted and starting it.

use crate::ani;
use crate::anilist::Media;
use crate::progress::{Parts, Progress};
use crate::source::{Lookup, Source};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// A download that was stopped, by quitting, pausing or taking it out of the queue.
const STOPPED: &str = "stopped";

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    /// tells apart jobs of one show while the worker and stops edit the queue
    #[serde(skip)]
    id: u64,
    pub media: Media,
    /// how the worker gets a source that works now
    #[serde(skip)]
    pub lookup: Lookup,
    /// None = every episode that isn't watched
    pub eps: Option<Vec<String>>,
    /// audio when queued: yt-dlp resumes a half-done episode in whatever audio it gets
    /// next, so it must not change in between (None in queues of older versions)
    #[serde(default)]
    pub dub: Option<bool>,
    /// the show's parts known when it was queued, whose watched episodes `eps: None` skips
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Parts,
}

impl Job {
    pub fn new(media: Media, eps: Option<Vec<String>>) -> Job {
        Job {
            id: 0,
            media,
            lookup: Lookup::Resolve,
            eps,
            dub: None,
            parts: Vec::new(),
        }
    }

    /// What it downloads when it has no episodes: every one not watched (in its show or the
    /// parts it was queued with) and not on disk yet.
    fn unwatched(&self, eps: &[String], on_disk: &[(String, PathBuf)]) -> Vec<String> {
        let run = Progress::of(&self.media, &self.parts);
        (eps.iter())
            .filter(|e| !run.watched(e) && !on_disk.iter().any(|(d, _)| d == *e))
            .cloned()
            .collect()
    }

    /// Gives it its id, and the audio of its show now if it has none yet.
    fn number(&mut self) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        self.id = NEXT.fetch_add(1, Ordering::Relaxed);
        (self.dub).get_or_insert_with(|| ani::dub(ani::audio(Some(self.media.id))));
    }

    fn has(&self, ep: &str) -> bool {
        self.eps.as_ref().is_some_and(|e| e.iter().any(|x| x == ep))
    }
}

/// The episode downloading, or held by a pause, and how far it got.
#[derive(Clone)]
pub struct Now {
    pub id: u64,
    pub ep: String,
    /// "Show E3", for the footer
    pub label: String,
    pub text: String,
    /// yt-dlp's, which goes on from what it got after a pause; ffmpeg starts over
    pub resumes: bool,
}

/// What the screens show of the queue, read at once.
pub struct Snapshot {
    pub now: Option<Now>,
    /// episodes still to download; a show whose episodes the worker hasn't looked up
    /// counts as one
    pub pending: usize,
    pub paused: bool,
}

/// Where downloads come from: ani-cli, or a stand-in in tests.
pub trait Fetch: Send + Sync + 'static {
    /// The show's download folder.
    fn folder(&self, m: &Media) -> PathBuf;
    /// Makes it, and puts it in the index, as `ani::prepare` does; it comes held.
    fn prepare(&self, m: &Media) -> Result<(PathBuf, ani::Hold), String>;
    /// Where the provider has the show now; with `all` (a job for every episode not
    /// watched) also the episodes it lists now.
    fn resolve(&self, m: &Media, lookup: Lookup, all: bool) -> Result<Source, String>;
    /// Starts downloading episode `ep` into `dir`, with its progress on stdout.
    fn start(&self, src: &Source, ep: &str, dub: bool, dir: &Path) -> Result<Child, String>;
}

/// ani-cli, which picks yt-dlp or ffmpeg for each episode.
pub struct AniCli;

impl Fetch for AniCli {
    fn folder(&self, m: &Media) -> PathBuf {
        ani::folder(m)
    }

    fn prepare(&self, m: &Media) -> Result<(PathBuf, ani::Hold), String> {
        ani::prepare(m)
    }

    fn resolve(&self, m: &Media, lookup: Lookup, all: bool) -> Result<Source, String> {
        // a cached source's episodes may be from before the latest aired
        let cached = matches!(lookup, Lookup::Check(_));
        let src = lookup.run(m)?;
        match all && cached {
            true => crate::source::with_episodes(src),
            false => Ok(src),
        }
    }

    fn start(&self, src: &Source, ep: &str, dub: bool, dir: &Path) -> Result<Child, String> {
        ani::start_download(src, ep, dub, dir)
    }
}

struct Running {
    pid: u32,
    id: u64,
    dir: PathBuf,
    ep: String,
}

#[derive(Default)]
struct State {
    /// oldest first, the running one included
    jobs: Vec<Job>,
    running: Option<Running>,
    now: Option<Now>,
    /// held by `pause` until `resume`
    paused: bool,
    /// quitting stopped downloads for good
    quit: bool,
}

struct Shared {
    state: Mutex<State>,
    /// the queue files of every ani-hop, and their locks
    dir: PathBuf,
    /// this one's queue file, `<name>.json`
    name: String,
    fetch: Box<dyn Fetch>,
    wake: Sender<()>,
}

/// This ani-hop's download queue; clones share it.
#[derive(Clone)]
pub struct Queue(Arc<Shared>);

impl Queue {
    /// Starts the worker, which waits until something is queued. `name` is this ani-hop's
    /// (its pid); `done` hears of every episode that finished or failed, as ("Show E3", result).
    pub fn start(
        dir: PathBuf,
        name: String,
        fetch: impl Fetch,
        done: impl Fn(String, Result<(), String>) + Send + 'static,
    ) -> Queue {
        let (wake, woken) = mpsc::channel::<()>();
        let q = Queue(Arc::new(Shared {
            state: Mutex::default(),
            dir,
            name,
            fetch: Box::new(fetch),
            wake,
        }));
        let worker = q.clone();
        thread::spawn(move || {
            for () in woken {
                while let Some(job) = worker.next_job() {
                    if !worker.work(job, &done) {
                        return; // quitting: what's left stays saved for the next start
                    }
                }
            }
        });
        q
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn file(&self) -> PathBuf {
        self.0.dir.join(format!("{}.json", self.0.name))
    }

    /// Writes the queue as it is now, or deletes its file once it's empty. Jobs with nothing
    /// left fall out first.
    fn save(&self, st: &mut State) -> Result<(), String> {
        st.jobs
            .retain(|j| j.eps.as_ref().is_none_or(|e| !e.is_empty()));
        let res = match st.jobs.is_empty() {
            true => fs::remove_file(self.file()).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }),
            false => ani::write_json(&self.file(), &st.jobs),
        };
        res.map_err(|e| format!("can't save the download queue: {e}"))
    }

    /// Queues a download, saved first: what can't be saved isn't queued, as quitting would
    /// lose it. Asking for one is asking for downloads to go on.
    pub fn enqueue(&self, mut job: Job) -> Result<(), String> {
        job.number();
        let mut st = self.lock();
        st.jobs.push(job);
        if let Err(e) = self.save(&mut st) {
            st.jobs.pop();
            return Err(e);
        }
        st.paused = false;
        drop(st);
        let _ = self.0.wake.send(());
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.lock();
        Snapshot {
            now: st.now.clone(),
            pending: (st.jobs.iter())
                .map(|j| j.eps.as_ref().map_or(1, Vec::len))
                .sum(),
            paused: st.paused,
        }
    }

    /// `m`'s episodes in this queue; a job the worker hasn't looked up yet lists what it
    /// will download once `src` (a known source) and `on_disk` are what they are now.
    pub fn queued(
        &self,
        m: &Media,
        src: Option<&Source>,
        on_disk: &[(String, PathBuf)],
    ) -> Vec<String> {
        let st = self.lock();
        (st.jobs.iter().filter(|j| j.media.id == m.id))
            .flat_map(|j| match (&j.eps, src) {
                (Some(eps), _) => eps.clone(),
                (None, Some(s)) => j.unwatched(&s.episodes, on_disk),
                (None, None) => Vec::new(),
            })
            .collect()
    }

    /// Takes episodes of `m` out of the queue. The one downloading stops, and the worker
    /// deletes what it got of it; the others lose what earlier tries left.
    pub fn stop(
        &self,
        m: &Media,
        eps: &[String],
        src: Option<&Source>,
        on_disk: &[(String, PathBuf)],
    ) -> Result<(), String> {
        let dir = self.0.fetch.folder(m);
        let mut st = self.lock();
        for j in st.jobs.iter_mut().filter(|j| j.media.id == m.id) {
            // a job the worker hasn't looked up yet gets its episodes listed, minus these
            if let (None, Some(s)) = (&j.eps, src) {
                j.eps = Some(j.unwatched(&s.episodes, on_disk));
            }
            if let Some(e) = &mut j.eps {
                e.retain(|x| !eps.contains(x));
            }
        }
        // stopping still happens when the queue can't be saved; the next start may ask about it
        let saved = self.save(&mut st);
        let running = (st.running.as_ref()).and_then(|r| (r.dir == dir).then(|| r.ep.clone()));
        if running.as_ref().is_some_and(|e| eps.contains(e))
            && let Some(r) = st.running.take()
        {
            kill(r.pid);
        }
        st.now.take_if(|n| n.id == m.id && eps.contains(&n.ep));
        let waiting: Vec<String> = (eps.iter())
            .filter(|e| running.as_ref() != Some(e))
            .cloned()
            .collect();
        // the running one's folder stays held: the worker deletes what it got once it's gone
        self.discard(&dir, &waiting);
        saved
    }

    /// Holds downloads: the running one stops where it is, keeping what it got, and none starts
    /// until `resume`. Only while that's episode `ep` of show `id`, the one the user was asked
    /// about or saw: by the time the answer is handled the next may run, maybe through ffmpeg,
    /// which would start over unasked. False then, and nothing changes.
    pub fn pause(&self, id: u64, ep: &str) -> bool {
        let mut st = self.lock();
        if !(st.running.as_ref()).is_some_and(|r| r.id == id && r.ep == ep) {
            return false;
        }
        st.paused = true;
        if let Some(r) = st.running.take() {
            kill(r.pid);
        }
        true
    }

    /// Lets downloads go on after `pause`.
    pub fn resume(&self) {
        self.lock().paused = false;
        let _ = self.0.wake.send(());
    }

    /// Stops the running download for good; yt-dlp goes on from its .part files next time,
    /// ffmpeg starts over. (Closing the terminal needs nothing: the hangup reaches our whole
    /// process group.)
    pub fn quit(&self) {
        let mut st = self.lock();
        st.quit = true;
        if let Some(r) = st.running.take() {
            kill(r.pid);
        }
    }

    fn queue_files(&self) -> Vec<PathBuf> {
        let Ok(dir) = fs::read_dir(&self.0.dir) else {
            return Vec::new();
        };
        (dir.flatten().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect()
    }

    /// Whose a queue file is, and whether that ani-hop is still running.
    fn alive(&self, file: &Path) -> (bool, bool) {
        let name = file.file_stem().unwrap_or_default().to_string_lossy();
        let mine = name == self.0.name.as_str();
        (mine, !mine && ani::running(&name))
    }

    /// Queues of ani-hops that are gone, as at the start: our own name can only be a leftover
    /// then too.
    pub fn leftovers(&self) -> (Vec<Job>, Vec<PathBuf>) {
        let files: Vec<PathBuf> = (self.queue_files().into_iter())
            .filter(|f| !self.alive(f).1)
            .collect();
        let jobs = files.iter().flat_map(|f| read_queue(f)).collect();
        (jobs, files)
    }

    /// Episodes per show other running ani-hops have queued.
    pub fn elsewhere(&self) -> HashMap<u64, Vec<String>> {
        let mut all: HashMap<u64, Vec<String>> = HashMap::new();
        for f in self.queue_files().iter().filter(|f| self.alive(f).1) {
            for j in read_queue(f) {
                all.entry(j.media.id)
                    .or_default()
                    .extend(j.eps.unwrap_or_default());
            }
        }
        all
    }

    /// Another ani-hop may be answering about the same leftovers: one at a time.
    fn claiming(&self) -> Option<fs::File> {
        fs::create_dir_all(&self.0.dir).ok()?;
        let lock = fs::File::create(self.0.dir.join("claim.lock")).ok()?;
        lock.lock().ok()?;
        Some(lock)
    }

    /// Takes over leftover queues: their jobs are saved in this one's queue before their
    /// files go, so a crash in between loses nothing. How many jobs it took (0: another
    /// ani-hop had them already).
    pub fn adopt(&self, files: &[PathBuf]) -> Result<usize, String> {
        let _turn = self.claiming();
        let files: Vec<&PathBuf> = files.iter().filter(|f| f.exists()).collect();
        let jobs: Vec<Job> = files.iter().flat_map(|f| read_queue(f)).collect();
        let n = jobs.len();
        if n > 0 {
            let mut st = self.lock();
            let before = st.jobs.len();
            for mut job in jobs {
                job.number();
                st.jobs.push(job);
            }
            if let Err(e) = self.save(&mut st) {
                st.jobs.truncate(before);
                return Err(e);
            }
            st.paused = false;
        }
        // one by our name (an earlier ani-hop's, with our pid) holds what was just saved
        for f in files.into_iter().filter(|f| **f != self.file()) {
            let _ = fs::remove_file(f);
        }
        let _ = self.0.wake.send(());
        Ok(n)
    }

    /// Drops leftover queues and what their unfinished downloads left, except in shows
    /// another running ani-hop downloads. How many jobs it dropped.
    pub fn drop_leftovers(&self, files: &[PathBuf]) -> usize {
        let _turn = self.claiming();
        let jobs: Vec<Job> = (files.iter())
            .flat_map(|f| {
                let jobs = read_queue(f);
                // one by our name (an earlier ani-hop's, with our pid) gets this one's queue
                let gone = match *f == self.file() {
                    true => self.save(&mut self.lock()).is_ok(),
                    false => fs::remove_file(f).is_ok(),
                };
                if gone { jobs } else { Vec::new() }
            })
            .collect();
        // a show another running ani-hop has queued keeps what it got, to go on from it
        let mut busy = self.elsewhere();
        busy.extend(self.lock().jobs.iter().map(|j| (j.media.id, Vec::new())));
        for j in jobs.iter().filter(|j| !busy.contains_key(&j.media.id)) {
            let eps = j.eps.as_deref().unwrap_or_default();
            self.discard(&self.0.fetch.folder(&j.media), eps);
        }
        jobs.len()
    }

    /// The first job, unless downloads are held.
    fn next_job(&self) -> Option<Job> {
        let st = self.lock();
        (!st.paused).then(|| st.jobs.first().cloned()).flatten()
    }

    /// Downloads a job's episodes, each looked up in the queue right before it starts, so
    /// stopped ones are skipped. Returns at a pause, false once quitting stopped the downloads.
    fn work(&self, job: Job, done: &impl Fn(String, Result<(), String>)) -> bool {
        let m = &job.media;
        let src = match self
            .0
            .fetch
            .resolve(m, job.lookup.clone(), job.eps.is_none())
        {
            Ok(s) => s,
            Err(e) => {
                let mut st = self.lock();
                // stopped while it was looked up: nothing to pause or say
                if !st.jobs.iter().any(|j| j.id == job.id) {
                    return true;
                }
                // offline: it stays queued, and downloads wait as after a pause
                if ani::unreachable(&e) {
                    st.paused = true;
                } else {
                    st.jobs.retain(|j| j.id != job.id);
                    let _ = self.save(&mut st);
                }
                drop(st);
                done(m.name().to_string(), Err(e));
                return true;
            }
        };
        let all = (job.eps.is_none()).then(|| {
            let on_disk = ani::local_episodes(&self.0.fetch.folder(m));
            job.unwatched(&src.episodes, &on_disk)
        });
        {
            let mut st = self.lock();
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                // after a pause it goes on from here, without looking the show up again
                j.lookup = Lookup::Ready(src.clone());
                // stopping one of them may have listed them already
                if let Some(all) = all {
                    j.eps.get_or_insert(all);
                }
            }
            let _ = self.save(&mut st);
        }
        let dub = job.dub.unwrap_or_default();
        loop {
            let next = {
                let st = self.lock();
                (st.jobs.iter().find(|j| j.id == job.id))
                    .and_then(|j| j.eps.as_ref()?.first().cloned())
            };
            let Some(ep) = next else {
                return true;
            };
            let label = format!("{} E{ep}", m.name());
            // held until what a stopped one got is deleted: another ani-hop may start it then
            let (res, _claim) = self.fetch_one(&job, &src, &ep, dub, &label);
            let mut st = self.lock();
            if res.is_err() && st.quit {
                return false;
            }
            // this job's, not just queued: stopped and queued again is another job, with its
            // audio
            let mine = |st: &State| (st.jobs.iter()).any(|j| j.id == job.id && j.has(&ep));
            // paused, and maybe resumed already: it stays first, to go on from what it got
            if res.as_ref().is_err_and(|e| e == STOPPED) && mine(&st) {
                match st.paused {
                    true => return true,
                    false => continue,
                }
            }
            st.now.take_if(|n| n.id == m.id && n.ep == ep);
            // gone from the job already: stopped (and maybe queued again since, which stays)
            let had = mine(&st);
            // offline: it stays queued, keeping what it got, and downloads wait as after a pause
            if had && res.as_ref().is_err_and(|e| ani::unreachable(e)) {
                st.paused = true;
                drop(st);
                drop(_claim);
                done(label, res);
                return true;
            }
            // done with it, also where another job of the show has it
            for j in st.jobs.iter_mut().filter(|j| had && j.media.id == m.id) {
                if let Some(e) = &mut j.eps {
                    e.retain(|x| *x != ep);
                }
            }
            let _ = self.save(&mut st);
            drop(st);
            match (had, _claim) {
                (true, claim) => {
                    drop(claim);
                    done(label, res);
                }
                (false, Some(claim)) => claim.discard(),
                // it never started: nothing of it here is this ani-hop's
                (false, None) => {}
            }
        }
    }

    /// Downloads one episode. It starts under the lock stopping and pausing take, after
    /// checking it's still wanted, so neither is ever missed. Also the episode's claim, if it
    /// got that far.
    fn fetch_one(
        &self,
        job: &Job,
        src: &Source,
        ep: &str,
        dub: bool,
        label: &str,
    ) -> (Result<(), String>, Option<EpisodeLock>) {
        let mut claim = None;
        let res = self.fetch(job, src, ep, dub, label, &mut claim);
        (res, claim)
    }

    fn fetch(
        &self,
        job: &Job,
        src: &Source,
        ep: &str,
        dub: bool,
        label: &str,
        claim: &mut Option<EpisodeLock>,
    ) -> Result<(), String> {
        let m = &job.media;
        let now = |text: &str, resumes| Now {
            id: m.id,
            ep: ep.to_string(),
            label: label.to_string(),
            text: text.into(),
            resumes,
        };
        let (child, dir) = {
            let mut st = self.lock();
            let wanted = (st.jobs.iter()).any(|j| j.id == job.id && j.has(ep));
            if st.quit || st.paused || !wanted {
                return Err(STOPPED.into());
            }
            st.now = Some(now("starting", true));
            // under the lock, so stopping another episode can't drop the folder in between;
            // also for an episode already there, so a folder found through ani-cli gets the
            // AniList show
            let (dir, hold) = self.0.fetch.prepare(m)?;
            // another ani-hop may have queued it too before either saw the other's queue
            let mut held = (self.claim(&dir, ep)?).ok_or("another ani-hop is downloading it")?;
            held.folder = Some(hold);
            *claim = Some(held);
            if ani::local_episodes(&dir).iter().any(|(e, _)| e == ep) {
                return Ok(()); // yt-dlp wouldn't replace it, so it stays the audio it was
            }
            ani::drop_half_pieces(&dir, ep);
            let child = self.0.fetch.start(src, ep, dub, &dir)?;
            st.running = Some(Running {
                pid: child.id(),
                id: m.id,
                dir: dir.clone(),
                ep: ep.to_string(),
            });
            (child, dir)
        };
        let mut lines = ani::Lines::default();
        let stderr = ani::follow(child, |l| {
            if let Some((text, resumes)) = lines.read(l) {
                let mut st = self.lock();
                // a stopped episode's last lines may still come in
                if (st.jobs.iter()).any(|j| j.id == job.id && j.has(ep)) {
                    st.now = Some(now(&text, resumes));
                }
            }
        })
        .1;
        // gone already: a stop or a pause killed it
        let killed = self.lock().running.take().is_none();
        match ani::finished(&dir, ep, dub)? {
            true => Ok(()),
            false if killed => Err(STOPPED.into()),
            false => Err(ani::last_error(&stderr)),
        }
    }

    /// Holds episode `ep` of the show in `dir` for this ani-hop: while it downloads it, and
    /// while what's left of a stopped one is deleted, as two writing (or one deleting) the
    /// same files would spoil them. None while another holds it, this one's worker included.
    /// A process this one starts right then holds a copy of the lock file until its exec
    /// closes it, so a claim that fails is tried again for about 100 ms, under whatever lock
    /// the caller holds.
    fn claim(&self, dir: &Path, ep: &str) -> Result<Option<EpisodeLock>, String> {
        for _ in 0..5 {
            if let Some(held) = self.try_claim(dir, ep)? {
                return Ok(Some(held));
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        self.try_claim(dir, ep)
    }

    fn try_claim(&self, dir: &Path, ep: &str) -> Result<Option<EpisodeLock>, String> {
        let hash = ani::fnv1a(format!("{}\n{ep}", dir.display()).as_bytes());
        let locks = self.0.dir.join("locks");
        fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
        let path = locks.join(format!("{hash:016x}.lock"));
        let file = fs::File::create(&path).map_err(|e| e.to_string())?;
        Ok(file.try_lock().ok().map(|()| EpisodeLock {
            path,
            _held: file,
            dir: dir.to_path_buf(),
            ep: ep.to_string(),
            folder: None,
        }))
    }

    /// Deletes what unfinished downloads of `eps` left in `dir`, of each one no ani-hop is
    /// downloading right now (this one's worker included), and the folder if that was all
    /// and no download holds it.
    fn discard(&self, dir: &Path, eps: &[String]) {
        if !ani::ours(dir) {
            return;
        }
        for ep in eps {
            let ep = std::slice::from_ref(ep);
            if !ani::parts(dir, ep).is_empty()
                && let Ok(Some(_held)) = self.claim(dir, &ep[0])
            {
                ani::discard_parts(dir, ep);
            }
        }
        let _ = ani::drop_if_empty(dir);
    }
}

/// An episode's claim. Its file goes only once the episode is on disk: one that opened the
/// file just before it went and locks it after finds the episode there and downloads
/// nothing. Gone while the episode isn't there, a third ani-hop's new file would let two
/// download it at once.
struct EpisodeLock {
    path: PathBuf,
    _held: fs::File,
    dir: PathBuf,
    ep: String,
    /// its folder, while it downloads into it
    folder: Option<ani::Hold>,
}

impl EpisodeLock {
    /// Deletes what a stopped episode got while it's still held, as another ani-hop may
    /// start it then, and the folder once it lets go, if that was all.
    fn discard(self) {
        if ani::ours(&self.dir) {
            ani::discard_parts(&self.dir, std::slice::from_ref(&self.ep));
        }
        let dir = self.dir.clone();
        drop(self);
        let _ = ani::drop_if_empty(&dir);
    }
}

impl Drop for EpisodeLock {
    fn drop(&mut self) {
        if ani::local_episodes(&self.dir)
            .iter()
            .any(|(e, _)| *e == self.ep)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read_queue(path: &Path) -> Vec<Job> {
    ani::read_json(path)
}

/// Ends an ani-cli download and what it runs.
fn kill(pid: u32) {
    let pid = pid.to_string();
    // without stderr: one that just ended would print "No such process" over the screen
    let quiet = |cmd: &str, args: &[&str]| {
        let _ = Command::new(cmd).args(args).stderr(Stdio::null()).status();
    };
    // The ani-cli shell first: it holds the signal until yt-dlp is gone and then exits.
    // Signalled after yt-dlp, it falls back to ffmpeg and keeps downloading after we quit.
    quiet("kill", &["-TERM", &pid]);
    quiet("pkill", &["-TERM", "-P", &pid]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    /// Downloads by running `script` with $OUT (the episode file) and $EP, into `root`.
    struct Fake {
        root: PathBuf,
        script: &'static str,
    }

    impl Fetch for Fake {
        fn folder(&self, m: &Media) -> PathBuf {
            self.root.join(m.name())
        }
        fn prepare(&self, m: &Media) -> Result<(PathBuf, ani::Hold), String> {
            ani::prepare_in(&self.root, m)
        }
        fn resolve(&self, m: &Media, lookup: Lookup, _: bool) -> Result<Source, String> {
            match lookup {
                Lookup::Ready(s) | Lookup::Check(s) => Ok(s),
                Lookup::Resolve if self.script == OFFLINE => Err("can't reach hianime.at".into()),
                // a lookup that takes a moment, then can't reach the provider
                Lookup::Resolve if m.name() == "Unreachable" => {
                    thread::sleep(Duration::from_millis(300));
                    Err("can't reach hianime.at".into())
                }
                Lookup::Resolve => Ok(source()),
            }
        }
        fn start(&self, _: &Source, ep: &str, _: bool, dir: &Path) -> Result<Child, String> {
            let out = dir.join(format!("Show Episode {ep}.mp4"));
            Command::new("sh")
                .args(["-c", self.script])
                .env("OUT", out)
                .env("EP", ep)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())
        }
    }

    const QUICK: &str =
        "printf '[download]  50.0%% of ~ 1.00MiB at 1.00MiB/s ETA 00:01\\n'; printf x > \"$OUT\"";
    /// what ani-cli says without a connection; a show to look up fails the same way
    const OFFLINE: &str = "echo 'Connection error: could not fetch https://hianime.at/x (no HTTP response; curl exit 6)' >&2; exit 1";
    /// writes a piece and waits to be stopped, unless it was stopped before: then it finishes
    const SLOW: &str = "if [ -e \"$OUT.again\" ]; then printf x > \"$OUT\"; exit; fi; : > \"$OUT.again\"; printf '[download]  10.0%% of ~ 1.00MiB at 1.00MiB/s ETA 00:09\\n'; : > \"$OUT.part\"; exec sleep 30";

    thread_local! {
        /// the show of this test: tests share one index, and a show's folder is found there
        static ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    fn temp(name: &str) -> PathBuf {
        // and of this run: a folder an earlier one left would be found too
        let run = std::process::id() as u64;
        let id = name
            .bytes()
            .fold(run << 20, |h, b| h.wrapping_mul(31) ^ b as u64);
        ID.set(id & 0xFFFF_FFFF_FFFF); // an AniList-like id
        let dir = std::env::temp_dir().join(format!("ani-hop-q-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn show() -> Media {
        let mut m = Media {
            id: ID.get(),
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        m
    }

    fn source() -> Source {
        Source {
            query: "show".into(),
            index: 1,
            title: "Show".into(),
            nth: 1,
            episodes: vec!["1".into(), "2".into()],
        }
    }

    fn job(eps: &[&str]) -> Job {
        let mut j = Job::new(show(), Some(eps.iter().map(|e| e.to_string()).collect()));
        j.dub = Some(false);
        j.lookup = Lookup::Ready(source());
        j
    }

    type Done = Receiver<(String, Result<(), String>)>;

    fn queue(dir: &Path, name: &str, script: &'static str) -> (Queue, Done) {
        let (tx, rx) = mpsc::channel();
        let fetch = Fake {
            root: dir.join("dl"),
            script,
        };
        let q = Queue::start(dir.join("queue"), name.into(), fetch, move |l, r| {
            let _ = tx.send((l, r));
        });
        (q, rx)
    }

    fn wait(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn running(q: &Queue, ep: &str) -> bool {
        q.lock().running.as_ref().is_some_and(|r| r.ep == ep)
    }

    #[test]
    fn downloads_in_order_and_forgets_the_queue_when_done() {
        let dir = temp("order");
        let (q, done) = queue(&dir, "a", QUICK);
        q.enqueue(job(&["1", "2"])).unwrap();
        assert!(dir.join("queue/a.json").exists(), "saved before it starts");
        let t = Duration::from_secs(10);
        assert_eq!(done.recv_timeout(t).unwrap(), ("Show E1".into(), Ok(())));
        assert_eq!(done.recv_timeout(t).unwrap(), ("Show E2".into(), Ok(())));
        assert!(dir.join("dl/Show/Show Episode 2.mp4").exists());
        wait("queue file gone", || !dir.join("queue/a.json").exists());
        let s = q.snapshot();
        assert!(s.pending == 0 && s.now.is_none() && !s.paused);
        let locks = || fs::read_dir(dir.join("queue/locks")).unwrap().count();
        wait("a finished episode's claim goes with it", || locks() == 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stopping_the_running_one_deletes_what_it_got() {
        let dir = temp("stop");
        let (q, done) = queue(&dir, "a", SLOW);
        q.enqueue(job(&["1"])).unwrap();
        wait("downloading", || running(&q, "1"));
        let part = dir.join("dl/Show/Show Episode 1.mp4.part");
        wait("a piece", || part.exists());
        q.stop(&show(), &["1".into()], None, &[]).unwrap();
        wait("its piece deleted", || !part.exists());
        assert!(q.snapshot().now.is_none() && q.snapshot().pending == 0);
        assert!(
            done.recv_timeout(Duration::from_millis(300)).is_err(),
            "no toast"
        );
        assert!(!dir.join("queue/a.json").exists());
        // the claim of an episode not on disk stays: deleting it could let two own it
        let locks = fs::read_dir(dir.join("queue/locks")).unwrap().count();
        assert_eq!(locks, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pause_keeps_what_it_got_and_resume_goes_on() {
        let dir = temp("pause");
        let (q, done) = queue(&dir, "a", SLOW);
        q.enqueue(job(&["1", "2"])).unwrap();
        let part = dir.join("dl/Show/Show Episode 1.mp4.part");
        wait("a piece", || running(&q, "1") && part.exists());
        // asked about 2, but 1 runs: nothing changes
        assert!(!q.pause(ID.get(), "2") && !q.snapshot().paused);
        assert!(q.pause(ID.get(), "1"));
        let s = q.snapshot();
        assert!(s.paused && s.pending == 2 && s.now.is_some_and(|n| n.ep == "1"));
        assert!(done.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(part.exists(), "kept");
        q.resume();
        let t = Duration::from_secs(10);
        assert_eq!(done.recv_timeout(t).unwrap(), ("Show E1".into(), Ok(())));
        q.quit();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quitting_keeps_the_queue_and_the_next_start_takes_it_over() {
        let dir = temp("quit");
        let (q, _done) = queue(&dir, "gone", SLOW);
        q.enqueue(job(&["1", "2"])).unwrap();
        let part = dir.join("dl/Show/Show Episode 1.mp4.part");
        wait("a piece", || running(&q, "1") && part.exists());
        q.quit();
        wait("stopped", || q.lock().running.is_none());
        let (next, done) = queue(&dir, "next", QUICK);
        let (jobs, files) = next.leftovers();
        assert_eq!((jobs.len(), files.len()), (1, 1));
        assert_eq!(
            jobs[0].eps.as_deref(),
            Some(&["1".to_string(), "2".into()][..])
        );
        assert_eq!(next.adopt(&files), Ok(1));
        assert!(!files[0].exists() && dir.join("queue/next.json").exists());
        let t = Duration::from_secs(10);
        assert_eq!(done.recv_timeout(t).unwrap(), ("Show E1".into(), Ok(())));
        assert_eq!(next.adopt(&files), Ok(0), "taken already");
        next.quit();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_another_ani_hop_downloads_is_never_deleted() {
        let dir = temp("theirs");
        let (a, _) = queue(&dir, "a", SLOW);
        let (b, _) = queue(&dir, "b", QUICK);
        a.enqueue(job(&["1"])).unwrap();
        let part = dir.join("dl/Show/Show Episode 1.mp4.part");
        wait("a downloads", || running(&a, "1") && part.exists());
        // b had it queued too and drops it (stopped, or its leftovers dropped)
        b.discard(&dir.join("dl/Show"), &["1".into()]);
        assert!(part.exists(), "a's piece stays while a holds the episode");
        a.quit();
        wait("a's claim released, then b deletes it", || {
            b.discard(&dir.join("dl/Show"), &["1".into()]);
            !part.exists()
        });
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stopped_episode_is_cleaned_up_while_it_is_still_held() {
        let dir = temp("held");
        let (q, _) = queue(&dir, "a", QUICK);
        let (folder, hold) = ani::prepare_in(&dir.join("dl"), &show()).unwrap();
        let part = folder.join("Show Episode 1.mp4.part");
        fs::write(&part, "x").unwrap();
        let mut claim = q.claim(&folder, "1").unwrap().unwrap();
        claim.folder = Some(hold);
        assert!(q.claim(&folder, "1").unwrap().is_none(), "held");
        // another ani-hop dropping it now leaves it alone
        q.discard(&folder, &["1".into()]);
        assert!(part.exists() && folder.is_dir());
        claim.discard();
        assert!(!part.exists(), "deleted while held");
        // a process another test forks right then holds the folder's lock file too, until its
        // exec closes it, and the folder waits for the next cleanup
        wait("then the empty folder", || {
            let _ = ani::drop_if_empty(&folder);
            !folder.exists()
        });
        wait("let go", || q.claim(&folder, "1").unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cleanup_waits_out_a_claim_held_for_a_moment() {
        let dir = temp("moment");
        let (q, _) = queue(&dir, "a", QUICK);
        let (folder, hold) = ani::prepare_in(&dir.join("dl"), &show()).unwrap();
        drop(hold);
        let part = folder.join("Show Episode 1.mp4.part");
        // held for 35 ms, as by a process starting right then, until its exec
        let briefly = || {
            fs::write(&part, "x").unwrap();
            let held = q.try_claim(&folder, "1").unwrap().unwrap();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(35));
                drop(held);
            })
        };
        // leftovers of an ani-hop that's gone, dropped
        let left = dir.join("queue/gone.json");
        ani::write_json(&left, &vec![job(&["1"])]).unwrap();
        let release = briefly();
        assert_eq!(q.drop_leftovers(std::slice::from_ref(&left)), 1);
        release.join().unwrap();
        assert!(!part.exists(), "dropped leftovers lose their pieces");
        // and stopped (nothing queued or running here): its folder went with the leftovers, so
        // it is made again
        drop(ani::prepare_in(&dir.join("dl"), &show()).unwrap());
        let release = briefly();
        q.stop(&show(), &["1".into()], None, &[]).unwrap();
        release.join().unwrap();
        assert!(!part.exists(), "a stopped one loses its pieces");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_leftover_by_our_own_name_keeps_what_was_taken_over() {
        let dir = temp("ownname");
        let (q, _) = queue(&dir, "me", SLOW);
        // an ani-hop that had our pid before left this
        let mine = dir.join("queue/me.json");
        ani::write_json(&mine, &vec![job(&["1", "2"])]).unwrap();
        let (jobs, files) = q.leftovers();
        assert_eq!(jobs.len(), 1);
        assert_eq!(files, std::slice::from_ref(&mine));
        assert_eq!(q.adopt(&files), Ok(1));
        assert!(mine.exists(), "it holds the queue now");
        assert_eq!(read_queue(&mine).len(), 1);
        q.quit();
        wait("stopped", || q.lock().running.is_none());
        // dropped, it holds this one's own queue: kept while that has something, else gone
        let (q, _) = queue(&dir.join("other"), "me", SLOW);
        let mine = dir.join("other/queue/me.json");
        ani::write_json(&mine, &vec![job(&["1"])]).unwrap();
        assert_eq!(q.drop_leftovers(std::slice::from_ref(&mine)), 1);
        assert!(!mine.exists());
        q.enqueue(job(&["2"])).unwrap();
        // running, so its worker has saved what it does before the folder goes below
        wait("downloading", || running(&q, "2"));
        assert_eq!(q.drop_leftovers(std::slice::from_ref(&mine)), 1);
        let kept = read_queue(&mine);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].eps.as_deref(), Some(&["2".to_string()][..]));
        q.quit();
        wait("stopped", || q.lock().running.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_show_stopped_while_looked_up_doesnt_pause_the_next() {
        let dir = temp("cancelled");
        let (q, done) = queue(&dir, "a", QUICK);
        let mut slow = job(&["1"]);
        slow.media.id += 1;
        slow.media.title.romaji = Some("Unreachable".into());
        slow.lookup = Lookup::Resolve;
        q.enqueue(slow.clone()).unwrap();
        thread::sleep(Duration::from_millis(50)); // being looked up
        q.stop(&slow.media, &["1".into()], None, &[]).unwrap();
        q.enqueue(job(&["2"])).unwrap();
        let (label, res) = done.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            (label.as_str(), res),
            ("Show E2", Ok(())),
            "nothing of the stopped one"
        );
        assert!(!q.snapshot().paused);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn going_offline_pauses_downloads_and_keeps_them() {
        // the show can't be looked up, or its episode can't be fetched
        for (name, lookup) in [
            ("offline-lookup", Lookup::Resolve),
            ("offline-episode", Lookup::Ready(source())),
        ] {
            let dir = temp(name);
            let (q, done) = queue(&dir, "a", OFFLINE);
            let mut j = job(&["1", "2"]);
            j.lookup = lookup;
            q.enqueue(j).unwrap();
            let (_, res) = done.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(res, Err("can't reach hianime.at".into()));
            wait("paused", || q.snapshot().paused);
            let quiet = done.recv_timeout(Duration::from_millis(300));
            assert!(quiet.is_err(), "{name}: one error, then it waits");
            assert_eq!(q.snapshot().pending, 2, "{name}: nothing dropped");
            let saved = read_queue(&q.file());
            assert_eq!(saved[0].eps.as_deref(), Some(&["1".into(), "2".into()][..]));
            q.quit();
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn what_cant_be_saved_isnt_queued() {
        let dir = temp("unsaved");
        let (q, _done) = queue(&dir, "a", QUICK);
        fs::write(dir.join("queue"), "a file where the folder should be").unwrap();
        assert!(q.enqueue(job(&["1"])).is_err());
        assert_eq!(q.snapshot().pending, 0);
        // a leftover stays where it is while it can't be taken over
        let (other, _) = queue(&dir.join("sub"), "b", QUICK);
        let left = dir.join("left.json");
        ani::write_json(&left, &vec![job(&["1"])]).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/queue"), "").unwrap();
        assert!(other.adopt(std::slice::from_ref(&left)).is_err());
        assert!(left.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_ani_hops_dont_download_one_episode_together() {
        let dir = temp("twice");
        let (a, _) = queue(&dir, "a", SLOW);
        let (b, done) = queue(&dir, "b", QUICK);
        a.enqueue(job(&["1"])).unwrap();
        wait("a downloads", || running(&a, "1"));
        b.enqueue(job(&["1"])).unwrap();
        let (label, res) = done.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            (label.as_str(), res),
            ("Show E1", Err("another ani-hop is downloading it".into()))
        );
        a.quit();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_whole_show_job_with_a_cached_source_lists_the_episodes_out_now() {
        if std::env::var_os("ANIHOP_CHILD").is_some() {
            fs::create_dir_all(ani::cache_dir().join("probe")).unwrap();
            let mut cached = source();
            cached.episodes = vec!["1".into(), "2".into()];
            let eps = |all| {
                let lookup = Lookup::Check(cached.clone());
                AniCli.resolve(&show(), lookup, all).unwrap().episodes
            };
            assert_eq!(
                eps(true),
                ["1", "2", "3"],
                "every episode: the provider's now"
            );
            assert_eq!(eps(false), ["1", "2"], "the episodes picked: one search");
            return;
        }
        let dir = temp("cached");
        // answers ani-cli's menus straight into the file ani-hop's rofi stand-in writes
        let stub = "#!/bin/sh\ncase \"$1\" in\n-S) printf 'Select episode: \\n1\\n2\\n3\\n' ;;\n*) printf 'Select anime: \\n1 Show\\n2 Other\\n' ;;\nesac > \"$ANIHOP_MENU_OUT\"\nexit 1\n";
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/ani-cli"), stub).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("bin/ani-cli"), fs::Permissions::from_mode(0o755)).unwrap();
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
        ];
        let name =
            "downloads::tests::a_whole_show_job_with_a_cached_source_lists_the_episodes_out_now";
        assert!(crate::in_child(name, &env));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn whole_show_jobs_skip_what_is_watched_in_parts_too() {
        let watching = |id, episodes, progress| Media {
            id,
            episodes: Some(episodes),
            media_list_entry: Some(crate::anilist::Entry {
                id,
                status: "CURRENT".into(),
                progress,
            }),
            ..Default::default()
        };
        let s = |v: &[&str]| v.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        let mut j = Job::new(watching(1, 3, 1), None);
        let on_disk = [("3".to_string(), PathBuf::from("A Episode 3.mp4"))];
        assert_eq!(
            j.unwatched(&s(&["1", "2", "3", "3.5"]), &on_disk),
            s(&["2", "3.5"])
        );
        // Attack on Titan Season 3 at 12/12, its Part 2 at 3/10
        j.media = watching(1, 12, 12);
        j.parts = vec![(12, watching(2, 10, 3))];
        let all: Vec<String> = (1..=22).map(|e| e.to_string()).collect();
        assert_eq!(j.unwatched(&all, &[]), all[15..]);
    }
}
