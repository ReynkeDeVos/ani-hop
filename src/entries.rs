//! Entry changes: the user's AniList Entries as this session knows them, and the writes that
//! change them. Copies of a show's Entry are everywhere (cached lists, the Downloads index,
//! queued jobs, screens, messages still on their way), so everything reads Entries here: one
//! saved this session wins over every copy asked for before it, and what AniList says after it
//! wins over the save. Writes go one at a time across ani-hops, a Mark saves only while
//! AniList still has the progress its question showed, and answers for an earlier login change
//! nothing.

use crate::ani;
use crate::anilist::{Entry, Media};
use crate::lists;
use crate::progress::{Mark, Progress};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

/// Where Entries are written: AniList, or a stand-in in tests.
pub trait AniList: Send + Sync {
    /// The user's Entry of show `media` as it is now.
    fn entry(&self, media: u64) -> Result<Option<Entry>, String>;
    fn save(
        &self,
        media: u64,
        status: Option<&str>,
        progress: Option<u32>,
    ) -> Result<Entry, String>;
    /// Takes Entry `entry` off the user's list.
    fn remove(&self, entry: u64) -> Result<(), String>;
}

/// A change to the user's list.
pub enum Change {
    /// the show onto a list
    Status(Media, String),
    /// the show off the list, by its Entry's id
    Remove(Media, u64),
    /// progress, as the question showed it
    Progress(Mark),
}

impl Change {
    /// The show whose Entry it changes: a Mark's may be a part's.
    fn show(&self) -> &Media {
        match self {
            Change::Status(m, _) | Change::Remove(m, _) => m,
            Change::Progress(k) => &k.entry,
        }
    }
}

/// What the session knows of a show's Entry, and since when.
struct Known {
    /// None: not on the list
    entry: Option<Entry>,
    /// when AniList had it (`ani::tick`): a read's asking, a save's answer
    when: u64,
}

pub struct Entries {
    anilist: Arc<dyn AniList>,
    /// one more for every log in and out
    login: u64,
    known: HashMap<u64, Known>,
    /// the Entries kept for the Downloads list as last read (`catch_up`), by any ani-hop
    kept: ani::Learnt,
    pending: usize,
}

/// When a read was asked for: what it brings is older than what was saved or read after it.
#[derive(Clone, Copy)]
pub struct Stamp {
    login: u64,
    /// `ani::tick`
    asked: u64,
}

/// A write, to run in the background once.
pub struct Work {
    anilist: Arc<dyn AniList>,
    login: u64,
    change: Change,
}

/// A write's answer, for `Entries::finish`.
pub struct Done {
    login: u64,
    /// when AniList answered (`ani::tick`)
    when: u64,
    change: Change,
    res: Result<Answer, String>,
}

enum Answer {
    /// AniList's Entry after the write; None after a removal
    Saved(Option<Entry>),
    /// not saved: AniList's progress isn't what the question showed anymore
    Moved(Option<Entry>),
}

/// What to say about a write's answer.
pub struct Notice {
    pub text: String,
    pub error: bool,
    /// the show whose List status question it answers
    pub closes: Option<u64>,
}

impl Entries {
    pub fn new(anilist: Arc<dyn AniList>) -> Self {
        Entries {
            anilist,
            login: 0,
            known: HashMap::new(),
            kept: ani::learnt(),
            pending: 0,
        }
    }

    /// `m`'s Entry, the newest known: this session's, else the one `m` came with; but the one
    /// kept for the Downloads list when that's newer (another ani-hop's save or read since)
    /// and the same Entry, by its AniList id: another login's has an id of its own.
    pub fn entry<'a>(&'a self, m: &'a Media) -> Option<&'a Entry> {
        let known = self.known.get(&m.id);
        let here = known.map_or(m.media_list_entry.as_ref(), |k| k.entry.as_ref());
        match self.kept.get(&m.id) {
            // a copy without a Known is from a cache, which the kept Entry is never older than
            Some((Some(kept), when))
                if here.is_some_and(|e| e.id == kept.id)
                    && known.is_none_or(|k| k.when < *when) =>
            {
                Some(kept)
            }
            _ => here,
        }
    }

    /// `show`'s episodes as AniList counts them, with `parts`, by the newest Entries known.
    pub fn progress<'a>(&'a self, show: &'a Media, parts: &'a [(u32, Media)]) -> Progress<'a> {
        Progress::with(show, parts, self)
    }

    /// Writes still out, which quitting would cut short.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Taken right before AniList is asked for shows.
    pub fn stamp(&self) -> Stamp {
        Stamp {
            login: self.login,
            asked: ani::tick(),
        }
    }

    /// Takes in shows AniList sent for a read asked at `stamp`. Each one's Entry is learnt,
    /// here and for the Downloads list, unless a newer one is known (saved or read since);
    /// the shows then carry the newest, for the caches they go into. False for an earlier
    /// login's read, whose shows aren't the user's anymore.
    pub fn observe<'a>(
        &mut self,
        stamp: Stamp,
        shows: impl IntoIterator<Item = &'a mut Media>,
    ) -> bool {
        if stamp.login != self.login {
            return false;
        }
        let mut learnt = Vec::new();
        for m in shows.into_iter().filter(|m| m.on_anilist()) {
            match self.known.get(&m.id) {
                Some(k) if k.when > stamp.asked => m.media_list_entry = k.entry.clone(),
                _ => {
                    let entry = m.media_list_entry.clone();
                    learnt.push((m.id, entry.clone()));
                    let k = Known {
                        entry,
                        when: stamp.asked,
                    };
                    self.known.insert(m.id, k);
                }
            }
        }
        ani::learn(learnt, stamp.asked, false);
        true
    }

    /// A write to run in the background; its answer goes to `finish`.
    pub fn begin(&mut self, change: Change) -> Work {
        self.pending += 1;
        Work {
            anilist: self.anilist.clone(),
            login: self.login,
            change,
        }
    }

    /// Takes a write's answer: AniList's Entry now, for every reader, and what to say. None
    /// for an earlier login's write, which changes nothing here.
    pub fn finish(&mut self, done: Done) -> Option<Notice> {
        self.pending -= 1;
        if done.login != self.login {
            return None;
        }
        let m = done.change.show();
        let name = m.name();
        let total = m.episodes.map_or("?".into(), |t| t.to_string());
        let (entry, notice) = match done.res {
            Err(e) => {
                let not = match done.change {
                    Change::Remove(..) => "not removed",
                    _ => "not saved",
                };
                let text = format!("{name} {not} · {e}");
                return Some(notice(text, true, None));
            }
            Ok(Answer::Saved(entry)) => {
                let text = match &entry {
                    Some(e) => {
                        let list = crate::list_label(&e.status);
                        format!("Saved {name} · {list} · {}/{total}", e.progress)
                    }
                    None => format!("Removed {name} from your list"),
                };
                (entry, notice(text, false, Some(m.id)))
            }
            // what's on screen was old: AniList's Entry, and lists that refresh
            Ok(Answer::Moved(entry)) => {
                let now = entry.as_ref().map_or(0, |e| e.progress);
                let text = format!("{name} not saved · AniList has {now}/{total} now, try again");
                (entry, notice(text, true, None))
            }
        };
        lists::changed();
        // unless what's known is newer: a write that answered after this one, or a read asked
        // for after it answered
        if !self.known.get(&m.id).is_some_and(|k| k.when > done.when) {
            let when = done.when;
            self.known.insert(m.id, Known { entry, when });
        }
        Some(notice)
    }

    /// Keeps `m`'s Entry as this session knows it for the Downloads list, as a download of it
    /// is queued: a show's first one starts its Show folder with it.
    pub fn remember(&self, m: &Media) {
        if let Some(k) = self.known.get(&m.id) {
            ani::learn([(m.id, k.entry.clone())], k.when, true);
        }
    }

    /// Reads the Entries kept for the Downloads list again, as a rescan brings them there:
    /// another ani-hop's saves and reads since, which `entry` goes by where they're newer.
    pub fn catch_up(&mut self) {
        self.kept = ani::learnt();
    }

    /// Another login, or none: what was known was the old login's, and its writes still out
    /// change nothing when they answer.
    pub fn login(&mut self, anilist: Arc<dyn AniList>) {
        self.login += 1;
        self.known.clear();
        self.anilist = anilist;
    }
}

fn notice(text: String, error: bool, closes: Option<u64>) -> Notice {
    Notice {
        text,
        error,
        closes,
    }
}

impl Work {
    /// Blocks on AniList, in turn with every ani-hop's writes, and keeps AniList's Entry for
    /// the Downloads list before the next one's turn.
    pub fn run(self) -> Done {
        let mut answered = 0;
        let res = turn().and_then(|_turn| {
            let saved = |e| Answer::Saved(Some(e));
            let answer = match &self.change {
                Change::Status(m, status) => self.anilist.save(m.id, Some(status), None).map(saved),
                Change::Remove(_, entry) => {
                    self.anilist.remove(*entry).map(|()| Answer::Saved(None))
                }
                // the question showed `was`: Yes mustn't undo a save it didn't know of
                Change::Progress(k) => match self.anilist.entry(k.entry.id)? {
                    now if now.as_ref().map_or(0, |e| e.progress) != k.was => {
                        Ok(Answer::Moved(now))
                    }
                    _ => (self.anilist)
                        .save(k.entry.id, Some(k.status), Some(k.to))
                        .map(saved),
                },
            }?;
            let (Answer::Saved(entry) | Answer::Moved(entry)) = &answer;
            answered = ani::tick();
            ani::learn([(self.change.show().id, entry.clone())], answered, false);
            Ok(answer)
        });
        Done {
            login: self.login,
            when: answered,
            change: self.change,
            res,
        }
    }
}

/// This write's turn among every ani-hop's, until it's dropped: a progress check and its save
/// mustn't have another ani-hop's save between them.
fn turn() -> Result<fs::File, String> {
    let dir = ani::config_dir();
    let lock = fs::create_dir_all(&dir).and_then(|()| fs::File::create(dir.join("saves.lock")));
    let lock = lock.and_then(|f| f.lock().map(|()| f));
    lock.map_err(|e| format!("can't take the save lock: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    /// AniList's Entries by show; `slow` reads give another ani-hop time to save meanwhile.
    #[derive(Default)]
    struct Fake {
        all: Mutex<HashMap<u64, Entry>>,
        slow: bool,
        down: bool,
    }

    impl AniList for Fake {
        fn entry(&self, media: u64) -> Result<Option<Entry>, String> {
            let now = self.all.lock().unwrap().get(&media).cloned();
            if self.slow {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(now)
        }

        fn save(&self, media: u64, status: Option<&str>, to: Option<u32>) -> Result<Entry, String> {
            if self.down {
                return Err("Can't reach AniList".into());
            }
            let mut all = self.all.lock().unwrap();
            let e = all.entry(media).or_insert(Entry {
                id: media * 10,
                status: "CURRENT".into(),
                progress: 0,
            });
            e.status = status.map_or(e.status.clone(), String::from);
            e.progress = to.unwrap_or(e.progress);
            Ok(e.clone())
        }

        fn remove(&self, entry: u64) -> Result<(), String> {
            if self.down {
                return Err("Can't reach AniList".into());
            }
            self.all.lock().unwrap().retain(|_, e| e.id != entry);
            Ok(())
        }
    }

    fn watching(id: u64, progress: u32) -> Media {
        let mut m = Media {
            id,
            episodes: Some(12),
            media_list_entry: Some(Entry {
                id: id * 10,
                status: "CURRENT".into(),
                progress,
            }),
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        m
    }

    /// AniList with `show` at `progress`, and this session's Entries of it.
    fn anilist(show: &Media, slow: bool) -> (Arc<Fake>, Entries) {
        let fake = Fake {
            slow,
            ..Default::default()
        };
        let e = show.media_list_entry.clone().unwrap();
        fake.all.lock().unwrap().insert(show.id, e);
        let fake = Arc::new(fake);
        (fake.clone(), Entries::new(fake))
    }

    fn mark(entries: &mut Entries, show: &Media, ep: &str) -> Notice {
        let k = entries.progress(show, &[]).mark(ep).unwrap();
        let done = entries.begin(Change::Progress(k)).run();
        entries.finish(done).unwrap()
    }

    #[test]
    fn a_save_wins_over_copies_from_before_it() {
        // a downloaded show at 5, as the Downloads index and a queued job have it
        let copy = watching(70_001, 5);
        let root = ani::home().join("entries-dl");
        let (dir, _hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut entries) = anilist(&copy, false);
        let before = entries.stamp();
        let n = mark(&mut entries, &copy, "6");
        assert_eq!(n.text, "Saved Show · Watching · 6/12");
        assert_eq!(
            entries.entry(&copy).unwrap().progress,
            6,
            "not the copy's 5"
        );
        assert_eq!(entries.progress(&copy, &[]).mark("7").unwrap().was, 6);
        // a list refresh asked before the save brings 5: it's shown and cached as 6
        let mut late = [watching(70_001, 5)];
        assert!(entries.observe(before, &mut late));
        assert_eq!(late[0].media_list_entry.as_ref().unwrap().progress, 6);
        // the Downloads index has it, and a queued job's copy doesn't take it back
        let indexed = || {
            let shows = ani::local_shows().into_iter().find(|s| s.0.id == copy.id);
            shows.unwrap().0.media_list_entry.unwrap().progress
        };
        assert_eq!(indexed(), 6);
        drop(ani::prepare_in(&root, &copy).unwrap());
        assert_eq!(indexed(), 6);
        // what AniList says after the save wins: a change on its website
        fake.save(copy.id, None, Some(9)).unwrap();
        let after = entries.stamp();
        assert!(entries.observe(after, &mut [watching(70_001, 9)]));
        assert_eq!(entries.entry(&copy).unwrap().progress, 9);
        drop(_hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn answers_for_an_earlier_login_change_nothing() {
        let copy = watching(70_002, 5);
        let (fake, mut entries) = anilist(&copy, false);
        let asked = entries.stamp();
        let k = entries.progress(&copy, &[]).mark("6").unwrap();
        let done = entries.begin(Change::Progress(k)).run();
        entries.login(fake.clone());
        assert_eq!(entries.pending(), 1);
        assert!(entries.finish(done).is_none(), "no toast, no closed screen");
        assert_eq!(entries.pending(), 0, "and nothing left to wait for");
        assert_eq!(entries.entry(&copy).unwrap().progress, 5);
        assert!(!entries.observe(asked, &mut [watching(70_002, 6)]));
        assert_eq!(entries.entry(&copy).unwrap().progress, 5);
    }

    #[test]
    fn two_ani_hops_save_progress_one_at_a_time() {
        // both asked "Watched up to…" at 5: one saves 6, the other 7, at the same moment
        let copy = watching(70_003, 5);
        let (fake, _) = anilist(&copy, true);
        let runs: Vec<_> = ["6", "7"]
            .map(|ep| {
                let mut entries = Entries::new(fake.clone());
                let k = entries.progress(&copy, &[]).mark(ep).unwrap();
                let work = entries.begin(Change::Progress(k));
                thread::spawn(move || entries.finish(work.run()).unwrap())
            })
            .into_iter()
            .collect();
        let said: Vec<Notice> = runs.into_iter().map(|t| t.join().unwrap()).collect();
        let saved: Vec<&Notice> = said.iter().filter(|n| !n.error).collect();
        assert_eq!(saved.len(), 1, "the second sees the first's save");
        let now = fake.all.lock().unwrap()[&copy.id].progress;
        assert!(saved[0].text.ends_with(&format!(" {now}/12")));
        let moved = said.iter().find(|n| n.error).unwrap();
        assert!(moved.text.contains(&format!("AniList has {now}/12 now")));
    }

    #[test]
    fn the_write_that_ran_last_stays_whatever_order_answers_come_in() {
        let copy = watching(70_004, 5);
        let (_, mut entries) = anilist(&copy, false);
        let status = |s: &str| Change::Status(copy.clone(), s.into());
        let first = entries.begin(status("PAUSED")).run();
        let last = entries.begin(status("DROPPED")).run();
        entries.finish(last).unwrap();
        // a list refreshed after both says so too, and the late answer is still older
        let mut fresh = copy.clone();
        fresh.media_list_entry.as_mut().unwrap().status = "DROPPED".into();
        assert!(entries.observe(entries.stamp(), [&mut fresh]));
        entries.finish(first).unwrap();
        assert_eq!(entries.entry(&copy).unwrap().status, "DROPPED");
    }

    #[test]
    fn the_downloads_list_shows_the_newest_entry_whoever_kept_it() {
        // a show's first download, queued at 5
        let copy = watching(70_008, 5);
        let root = ani::home().join("entries-newest");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut entries) = anilist(&copy, false);
        let shown = || {
            let show = ani::local_shows().into_iter().find(|s| s.0.id == copy.id);
            show.unwrap().0.media_list_entry.unwrap().progress
        };
        // Downloads asked AniList at 5, the save to 6 ran, then the refresh came in
        let asked = entries.stamp();
        let k = entries.progress(&copy, &[]).mark("6").unwrap();
        let done = entries.begin(Change::Progress(k)).run();
        ani::learn(
            [(copy.id, copy.media_list_entry.clone())],
            asked.asked,
            false,
        );
        assert_eq!(shown(), 6, "the refresh was older than the save");
        entries.finish(done).unwrap();
        // AniList's website moved it on to 9, a list showed that, then an episode got queued
        fake.save(copy.id, None, Some(9)).unwrap();
        let mut seen = watching(70_008, 9);
        assert!(entries.observe(entries.stamp(), [&mut seen]));
        assert_eq!(
            shown(),
            9,
            "a read is kept for the Downloads list too, a Part's as well"
        );
        entries.remember(&copy);
        assert_eq!(shown(), 9);
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn the_newest_of_reads_and_saves_wins_in_any_order() {
        let copy = watching(70_009, 5);
        let (fake, mut entries) = anilist(&copy, false);
        let at = |progress| watching(70_009, progress);
        // two reads asked for one after the other, answered the other way round
        let older = entries.stamp();
        assert!(entries.observe(entries.stamp(), [&mut at(9)]));
        assert!(entries.observe(older, [&mut at(6)]));
        assert_eq!(entries.progress(&copy, &[]).mark("10").unwrap().was, 9);
        // a save answered, a read asked for after it saw AniList's website move on, then the
        // save's answer came in
        fake.save(copy.id, None, Some(9)).unwrap();
        let k = entries.progress(&copy, &[]).mark("10").unwrap();
        let done = entries.begin(Change::Progress(k)).run();
        assert!(entries.observe(entries.stamp(), [&mut at(12)]));
        entries.finish(done).unwrap();
        assert_eq!(entries.entry(&copy).unwrap().progress, 12);
        // ticks come in order, also many in one ms
        let ticks: Vec<u64> = (0..100).map(|_| ani::tick()).collect();
        assert!(ticks.windows(2).all(|t| t[0] < t[1]));
    }

    #[test]
    fn the_downloads_index_follows_the_order_saves_ran_in_across_ani_hops() {
        let copy = watching(70_007, 5);
        let root = ani::home().join("entries-order");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut a) = anilist(&copy, false);
        let mut b = Entries::new(fake);
        let status = |s: &str| Change::Status(copy.clone(), s.into());
        // a saves first and answers last
        let first = a.begin(status("PAUSED")).run();
        let last = b.begin(status("DROPPED")).run();
        b.finish(last).unwrap();
        a.finish(first).unwrap();
        let indexed = ani::local_shows().into_iter().find(|s| s.0.id == copy.id);
        let status = indexed.unwrap().0.media_list_entry.unwrap().status;
        assert_eq!(status, "DROPPED", "as AniList has it");
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    /// `m`'s progress as the Downloads list has it.
    fn kept(m: &Media) -> u32 {
        let show = ani::local_shows().into_iter().find(|s| s.0.id == m.id);
        show.unwrap().0.media_list_entry.unwrap().progress
    }

    /// An hour past this ani-hop's clock: kept before the clock was set back and ani-hop
    /// started again, or by another ani-hop whose clock is ahead.
    fn ahead() -> u64 {
        ani::tick() + 3_600_000
    }

    #[test]
    fn a_read_is_kept_after_the_clock_went_back() {
        let copy = watching(70_010, 5);
        let root = ani::home().join("entries-read-ahead");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (_, mut entries) = anilist(&copy, false);
        ani::learn([(copy.id, copy.media_list_entry.clone())], ahead(), false);
        // the first read since, before anything rescanned the downloads: AniList has 9
        assert!(entries.observe(entries.stamp(), [&mut watching(70_010, 9)]));
        assert_eq!(kept(&copy), 9);
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn a_save_is_kept_after_another_ani_hops_whose_clock_is_ahead() {
        let copy = watching(70_011, 6);
        let root = ani::home().join("entries-save-ahead");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (_, mut entries) = anilist(&copy, false);
        // the other ani-hop saved 6, then this one marks 7
        ani::learn([(copy.id, copy.media_list_entry.clone())], ahead(), false);
        assert_eq!(
            mark(&mut entries, &copy, "7").text,
            "Saved Show · Watching · 7/12"
        );
        assert_eq!(kept(&copy), 7);
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn a_rescan_catches_up_on_another_ani_hops_save() {
        let copy = watching(70_012, 5);
        let root = ani::home().join("entries-catch-up");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut entries) = anilist(&copy, false);
        assert!(entries.observe(entries.stamp(), [&mut copy.clone()]));
        // another ani-hop marks 9, and this one is offline: only a rescan can tell
        let mut other = Entries::new(fake);
        assert_eq!(
            mark(&mut other, &copy, "9").text,
            "Saved Show · Watching · 9/12"
        );
        assert_eq!(entries.entry(&copy).unwrap().progress, 5);
        entries.catch_up();
        assert_eq!(entries.progress(&copy, &[]).mark("10").unwrap().was, 9);
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn a_rescan_catches_up_on_a_page_open_from_downloads() {
        // a downloaded show's page, open offline: its copy at 5, nothing read this session
        let copy = watching(70_014, 5);
        let root = ani::home().join("entries-open-page");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut entries) = anilist(&copy, false);
        mark(&mut Entries::new(fake), &copy, "9");
        assert_eq!(entries.progress(&copy, &[]).next(), 6);
        entries.catch_up();
        assert_eq!(
            entries.progress(&copy, &[]).next(),
            10,
            "Continue with episode 10"
        );
        assert_eq!(entries.progress(&copy, &[]).mark("10").unwrap().was, 9);
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn a_rescan_leaves_another_logins_entries_out() {
        let copy = watching(70_013, 5);
        let root = ani::home().join("entries-other-login");
        let (dir, hold) = ani::prepare_in(&root, &copy).unwrap();
        let (fake, mut entries) = anilist(&copy, false);
        // a save to 6 goes out, then another account logs in, which has the show at 2
        let k = entries.progress(&copy, &[]).mark("6").unwrap();
        let work = entries.begin(Change::Progress(k));
        entries.login(fake);
        let mut theirs = watching(70_013, 2);
        theirs.media_list_entry.as_mut().unwrap().id = 99;
        assert!(entries.observe(entries.stamp(), [&mut theirs]));
        // the old login's save answers last, and is kept for the Downloads list
        assert!(entries.finish(work.run()).is_none());
        assert_eq!(kept(&copy), 6);
        entries.catch_up();
        assert_eq!(
            entries.entry(&copy).unwrap().progress,
            2,
            "not the old login's 6"
        );
        drop(hold);
        let _ = ani::delete_show(&dir);
    }

    #[test]
    fn notices_say_what_happened() {
        let copy = watching(70_005, 5);
        let (fake, mut entries) = anilist(&copy, false);
        // AniList moved on to 7 since the question showed 5
        fake.save(copy.id, None, Some(7)).unwrap();
        let n = mark(&mut entries, &copy, "6");
        assert_eq!(
            (n.text.as_str(), n.error, n.closes),
            (
                "Show not saved · AniList has 7/12 now, try again",
                true,
                None
            )
        );
        assert_eq!(entries.entry(&copy).unwrap().progress, 7);
        let done = entries
            .begin(Change::Remove(copy.clone(), copy.id * 10))
            .run();
        let n = entries.finish(done).unwrap();
        assert_eq!(
            (n.text.as_str(), n.closes),
            ("Removed Show from your list", Some(copy.id))
        );
        assert!(
            entries.entry(&copy).is_none(),
            "off the list, whatever copies say"
        );
        let down = Arc::new(Fake {
            down: true,
            ..Default::default()
        });
        entries.login(down);
        let done = entries.begin(Change::Remove(copy.clone(), 1)).run();
        let n = entries.finish(done).unwrap();
        assert_eq!(n.text, "Show not removed · Can't reach AniList");
    }
}
