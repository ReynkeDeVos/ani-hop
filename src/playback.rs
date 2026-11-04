//! Episode playback: one Playback attempt from enter until its player closes. A downloaded
//! file plays as it is; otherwise the show's source is checked or looked up and the episode
//! streams through ani-cli, with subtitles when its dub isn't out yet. Each attempt has an
//! id, so an earlier player's end never touches a later one, and a second enter while one
//! starts doesn't open a second player.

use crate::ani::{self, Held, Replayed};
use crate::anilist::Media;
use crate::preload::{Key, Preload};
use crate::source::{self, Focus, Lookup, Source};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What an attempt waits on while it looks the show up: playing waits for that lookup.
pub const SEARCHING: &str = "searching ani-cli";
/// What an attempt waits on once the player runs and hasn't shown the episode yet.
pub const OPENING: &str = "opening the player";
/// What an attempt waits on while ani-cli gets the episode's links.
const FETCHING: &str = "fetching the stream";

/// What plays episodes: ani-cli and mpv, or a script in tests.
pub trait Player: Send + Sync {
    /// Streams episode `ep` of `src`; `step` hears what it waits on, and None once the player
    /// shows it.
    fn stream(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        step: &dyn Fn(Option<&'static str>),
    ) -> Result<(), String>;
    /// Runs ani-cli up to where it would start the player, and keeps what it would start it with.
    fn hold(&self, m: &Media, src: &Source, ep: &str, audio: &str) -> Result<Held, String>;
    /// Starts the player with what `hold` kept; `step` hears the same as from `stream`.
    fn replay(&self, held: &Held, step: &dyn Fn(Option<&'static str>)) -> Replayed;
    /// Episode `ep` of `m`, if it's downloaded. It may be on a slow disk.
    fn on_disk(&self, m: &Media, ep: &str) -> Option<PathBuf>;
    /// Plays a downloaded episode.
    fn file(&self, m: &Media, ep: &str, file: &Path, up: &mut dyn FnMut()) -> Result<(), String>;
    /// Where the provider has the show now.
    fn locate(&self, m: &Media, lookup: Lookup) -> Result<Source, String>;
}

/// ani-cli for streams (with ani-skip), mpv for files.
pub struct Mpv;

impl Player for Mpv {
    fn stream(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        step: &dyn Fn(Option<&'static str>),
    ) -> Result<(), String> {
        ani::stream(m, src, ep, audio, step)
    }

    fn hold(&self, m: &Media, src: &Source, ep: &str, audio: &str) -> Result<Held, String> {
        ani::hold_stream(m, src, ep, audio)
    }

    fn replay(&self, held: &Held, step: &dyn Fn(Option<&'static str>)) -> Replayed {
        ani::play_held(held, || step(None))
    }

    fn on_disk(&self, m: &Media, ep: &str) -> Option<PathBuf> {
        let eps = ani::local_episodes(&ani::folder(m));
        eps.into_iter().find(|(e, _)| e == ep).map(|f| f.1)
    }

    fn file(&self, m: &Media, ep: &str, file: &Path, up: &mut dyn FnMut()) -> Result<(), String> {
        ani::play_file(m, ep, file, up)
    }

    fn locate(&self, m: &Media, lookup: Lookup) -> Result<Source, String> {
        lookup.run(m)
    }
}

/// The episode being started or watched.
pub struct Playing {
    pub id: u64,
    pub ep: String,
    pub title: String,
    /// what it waits on, None once the player shows it
    pub step: Option<&'static str>,
    /// when enter was pressed, for the pop-up
    pub since: Instant,
    pub color: Option<String>,
    attempt: u64,
}

pub struct Playback {
    player: Arc<dyn Player>,
    now: Option<Playing>,
    attempts: u64,
    /// the stream fetched ahead, if any
    pub preload: Preload,
}

/// What enter on an episode does.
pub enum Start {
    /// it plays: run this in the background
    Run(Run),
    /// the show has to be looked up first, then it's started again
    Lookup,
    /// another one is still starting, this one (its title)
    Busy(String),
}

/// An attempt to run in the background, once.
pub struct Run {
    player: Arc<dyn Player>,
    preload: Preload,
    show: Media,
    ep: String,
    /// what the last scan of the download folder said, which it says on the pop-up
    on_disk: bool,
    lookup: Lookup,
    audio: &'static str,
    pub attempt: u64,
}

/// Fetching a stream ahead, to run in the background, once.
pub struct Ahead {
    player: Arc<dyn Player>,
    preload: Preload,
    key: Key,
    show: Media,
    src: Source,
    ep: String,
    audio: &'static str,
    focus: Focus,
}

impl Ahead {
    /// Blocks in ani-cli's turn, unless the show has left the screen by then.
    pub fn run(self) {
        let res = source::in_turn(self.show.id, &self.focus, || {
            (self.player).hold(&self.show, &self.src, &self.ep, self.audio)
        });
        self.preload.done(&self.key, res);
    }
}

impl Playback {
    pub fn new(player: Arc<dyn Player>) -> Self {
        Playback {
            player,
            now: None,
            attempts: 0,
            preload: Preload::new(),
        }
    }

    /// Fetching episode `ep` of `m` ahead, unless that's there or on its way.
    pub fn ahead(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &'static str,
        focus: Focus,
    ) -> Option<Ahead> {
        let key = Key::new(m.id, ep, audio, src);
        self.preload.begin(&key).then(|| Ahead {
            player: self.player.clone(),
            preload: self.preload.clone(),
            key,
            show: m.clone(),
            src: src.clone(),
            ep: ep.to_string(),
            audio,
            focus,
        })
    }

    /// The episode being started or watched.
    pub fn now(&self) -> Option<&Playing> {
        self.now.as_ref()
    }

    /// Enter on episode `ep` of `m`: its file, if the last scan found it `on_disk`, else a
    /// stream from the source `lookup` gets, in `audio`. Waiting on a lookup for this very
    /// episode, it goes on with the same pop-up.
    pub fn start(
        &mut self,
        m: &Media,
        ep: &str,
        on_disk: bool,
        lookup: Lookup,
        audio: &'static str,
    ) -> Start {
        let waited = (self.now.as_ref())
            .filter(|p| p.id == m.id && p.ep == ep && p.step == Some(SEARCHING))
            .map(|p| p.since);
        // a second enter would open a second player
        if let Some(p) = self.now.as_ref().filter(|p| p.step.is_some())
            && waited.is_none()
        {
            return Start::Busy(p.title.clone());
        }
        let step = match (on_disk, &lookup) {
            (true, _) => OPENING,
            (false, Lookup::Resolve) => SEARCHING,
            (false, Lookup::Check(_)) => "checking the source",
            (false, Lookup::Ready(_)) => FETCHING,
        };
        self.attempts += 1;
        self.now = Some(Playing {
            id: m.id,
            ep: ep.to_string(),
            // the number first: a long title loses its end, not the episode
            title: format!("Episode {ep} · {}", m.name()),
            step: Some(step),
            since: waited.unwrap_or_else(Instant::now),
            color: m.cover_image.color.clone(),
            attempt: self.attempts,
        });
        // shortcut: a file the first scan hasn't got to yet waits for this lookup, and fails with
        // it offline, until a second enter; a scan takes milliseconds unless the disk sleeps
        if !on_disk && matches!(lookup, Lookup::Resolve) {
            return Start::Lookup;
        }
        Start::Run(Run {
            player: self.player.clone(),
            preload: self.preload.clone(),
            show: m.clone(),
            ep: ep.to_string(),
            on_disk,
            lookup,
            audio,
            attempt: self.attempts,
        })
    }

    /// What `attempt` waits on now; None once its player shows the episode.
    pub fn step(&mut self, attempt: u64, step: Option<&'static str>) {
        if let Some(p) = self.now.as_mut().filter(|p| p.attempt == attempt) {
            p.step = step;
        }
    }

    /// `attempt` is the one on, and its player hasn't said it shows the episode.
    pub fn starting(&self, attempt: u64) -> bool {
        (self.now.as_ref()).is_some_and(|p| p.attempt == attempt && p.step.is_some())
    }

    /// `attempt` ended, its player closed or it failed.
    pub fn ended(&mut self, attempt: u64) {
        self.now.take_if(|p| p.attempt == attempt);
    }

    /// The lookup show `id` waited on answered, and its episode wasn't started again (the
    /// lookup failed, or its page was left): the wait is over.
    pub fn give_up(&mut self, id: u64) {
        self.now
            .take_if(|p| p.id == id && p.step == Some(SEARCHING));
    }
}

impl Run {
    /// Blocks until the player closes; `step` hears what it waits on, and None once the
    /// player shows the episode.
    pub fn run(self, step: &dyn Fn(Option<&'static str>)) -> Result<(), String> {
        // only here, with the pop-up up, as the folder may be on a slow disk
        if let Some(file) = self.player.on_disk(&self.show, &self.ep) {
            step(Some(OPENING));
            return (self.player).file(&self.show, &self.ep, &file, &mut || step(None));
        }
        let src = match self.lookup {
            // the last scan found a file that's gone
            Lookup::Ready(src) if self.on_disk => {
                step(Some(FETCHING));
                src
            }
            Lookup::Ready(src) => src,
            lookup => {
                let src = self.player.locate(&self.show, lookup)?;
                step(Some(FETCHING));
                src
            }
        };
        let show = &self.show;
        let key = Key::new(show.id, &self.ep, self.audio, &src);
        if let Some(held) = self.preload.take(&key) {
            step(Some(OPENING));
            match self.player.replay(&held, step) {
                Replayed::Ended(res) => return res,
                Replayed::TooOld => step(Some(FETCHING)),
            }
        }
        match self.player.stream(show, &src, &self.ep, self.audio, step) {
            // dubs come later or not at all, the subtitled one is the next best
            Err(e) if ani::dub(self.audio) && e.contains("dub") => {
                step(Some("no dub yet, fetching it with subtitles"));
                self.player.stream(show, &src, &self.ep, "sub", step)
            }
            res => res,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Plays everything, except dubs it doesn't have; says what it played.
    #[derive(Default)]
    struct Script {
        no_dub: bool,
        /// what's downloaded
        files: Vec<String>,
        /// links that are too old by the time they're used
        old_links: bool,
        played: Mutex<Vec<String>>,
    }

    impl Player for Script {
        fn stream(
            &self,
            _: &Media,
            _: &Source,
            ep: &str,
            audio: &str,
            step: &dyn Fn(Option<&'static str>),
        ) -> Result<(), String> {
            if self.no_dub && audio == "dub" {
                return Err("No sources found for dub!".into());
            }
            step(None);
            self.played
                .lock()
                .unwrap()
                .push(format!("stream {ep} {audio}"));
            Ok(())
        }

        fn hold(&self, _: &Media, _: &Source, ep: &str, audio: &str) -> Result<Held, String> {
            self.played
                .lock()
                .unwrap()
                .push(format!("hold {ep} {audio}"));
            Ok(Held {
                args: vec![format!("link {ep}")],
                subs: None,
            })
        }

        fn replay(&self, held: &Held, step: &dyn Fn(Option<&'static str>)) -> Replayed {
            if self.old_links {
                return Replayed::TooOld;
            }
            step(None);
            self.played
                .lock()
                .unwrap()
                .push(format!("replay {}", held.args[0]));
            Replayed::Ended(Ok(()))
        }

        fn on_disk(&self, _: &Media, ep: &str) -> Option<PathBuf> {
            self.files.iter().find(|f| *f == ep).map(PathBuf::from)
        }

        fn file(&self, _: &Media, ep: &str, _: &Path, up: &mut dyn FnMut()) -> Result<(), String> {
            up();
            self.played.lock().unwrap().push(format!("file {ep}"));
            Ok(())
        }

        fn locate(&self, _: &Media, _: Lookup) -> Result<Source, String> {
            Ok(source())
        }
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

    fn show() -> Media {
        let mut m = Media {
            id: 1,
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        m
    }

    /// Runs `start`'s attempt; what it played and the steps it went through.
    fn run(script: &Arc<Script>, start: Start) -> (Vec<String>, Vec<Option<&'static str>>) {
        let Start::Run(run) = start else {
            panic!("not started");
        };
        let steps = Mutex::new(Vec::new());
        run.run(&|s| steps.lock().unwrap().push(s)).unwrap();
        let played = std::mem::take(&mut *script.played.lock().unwrap());
        (played, steps.into_inner().unwrap())
    }

    #[test]
    fn a_downloaded_episode_plays_its_file_and_a_missing_dub_plays_subtitled() {
        let script = Arc::new(Script {
            no_dub: true,
            files: vec!["1".into()],
            ..Default::default()
        });
        let mut playback = Playback::new(script.clone());
        let start = playback.start(&show(), "1", true, Lookup::Resolve, "sub");
        assert_eq!(playback.now().unwrap().step, Some(OPENING));
        assert_eq!(
            run(&script, start),
            (vec!["file 1".into()], vec![Some(OPENING), None])
        );
        playback.ended(playback.now().unwrap().attempt);
        let start = playback.start(&show(), "2", false, Lookup::Check(source()), "dub");
        let (played, steps) = run(&script, start);
        assert_eq!(played, ["stream 2 sub"]);
        let expected = [
            Some("fetching the stream"),
            Some("no dub yet, fetching it with subtitles"),
            None,
        ];
        assert_eq!(steps, expected);
    }

    #[test]
    fn a_file_the_last_scan_missed_still_plays_and_one_gone_since_streams() {
        let script = Arc::new(Script {
            files: vec!["1".into()],
            ..Default::default()
        });
        let mut playback = Playback::new(script.clone());
        let start = playback.start(&show(), "1", false, Lookup::Ready(source()), "sub");
        assert_eq!(run(&script, start).0, ["file 1"]);
        playback.ended(playback.now().unwrap().attempt);
        let start = playback.start(&show(), "2", true, Lookup::Ready(source()), "sub");
        let (played, steps) = run(&script, start);
        assert_eq!(played, ["stream 2 sub"]);
        assert_eq!(steps, [Some(FETCHING), None]);
    }

    #[test]
    fn a_stream_fetched_ahead_starts_the_player_without_ani_cli() {
        let script = Arc::new(Script::default());
        let mut playback = Playback::new(script.clone());
        let focus = Focus::default();
        focus.on_screen(1);
        let ahead = playback.ahead(&show(), &source(), "2", "sub", focus.clone());
        ahead.unwrap().run();
        assert!(
            (playback.ahead(&show(), &source(), "2", "sub", focus)).is_none(),
            "it's there"
        );
        let start = playback.start(&show(), "2", false, Lookup::Ready(source()), "sub");
        let (played, steps) = run(&script, start);
        assert_eq!(played, ["hold 2 sub", "replay link 2"]);
        assert_eq!(steps, [Some(OPENING), None]);
    }

    #[test]
    fn links_too_old_are_fetched_again_and_nothing_counts_as_watched() {
        let script = Arc::new(Script {
            old_links: true,
            ..Default::default()
        });
        let mut playback = Playback::new(script.clone());
        let focus = Focus::default();
        focus.on_screen(1);
        (playback
            .ahead(&show(), &source(), "2", "sub", focus)
            .unwrap())
        .run();
        let start = playback.start(&show(), "2", false, Lookup::Ready(source()), "sub");
        let (played, steps) = run(&script, start);
        assert_eq!(played, ["hold 2 sub", "stream 2 sub"]);
        assert_eq!(steps, [Some(OPENING), Some(FETCHING), None]);
    }

    #[test]
    fn a_show_that_left_the_screen_fetches_nothing_ahead() {
        let script = Arc::new(Script::default());
        let mut playback = Playback::new(script.clone());
        let focus = Focus::default();
        focus.on_screen(99);
        (playback
            .ahead(&show(), &source(), "2", "sub", focus)
            .unwrap())
        .run();
        assert!(script.played.lock().unwrap().is_empty());
        let start = playback.start(&show(), "2", false, Lookup::Ready(source()), "sub");
        assert_eq!(run(&script, start).0, ["stream 2 sub"]);
    }

    #[test]
    fn a_second_enter_while_one_starts_opens_no_second_player() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        let ready = || Lookup::Ready(source());
        assert!(matches!(
            playback.start(&show(), "1", false, ready(), "sub"),
            Start::Run(_)
        ));
        let again = playback.start(&show(), "2", false, ready(), "sub");
        assert!(matches!(again, Start::Busy(t) if t == "Episode 1 · Show"));
        // once its player is up, another one may start
        let first = playback.now().unwrap().attempt;
        playback.step(first, None);
        let Start::Run(second) = playback.start(&show(), "1", false, ready(), "sub") else {
            panic!("not started");
        };
        // and the first player's end leaves the second's pop-up alone
        playback.ended(first);
        playback.step(first, Some("late"));
        let now = playback.now().unwrap();
        assert_eq!((now.attempt, now.step), (second.attempt, Some(FETCHING)));
        playback.ended(second.attempt);
        assert!(playback.now().is_none());
    }

    #[test]
    fn a_play_waiting_on_the_lookup_goes_on_with_the_same_pop_up() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        assert!(matches!(
            playback.start(&show(), "1", false, Lookup::Resolve, "sub"),
            Start::Lookup
        ));
        let since = playback.now().unwrap().since;
        // found: started again, as the same pop-up
        let start = playback.start(&show(), "1", false, Lookup::Ready(source()), "sub");
        assert!(matches!(start, Start::Run(_)));
        playback.give_up(1);
        assert_eq!(
            playback.now().unwrap().since,
            since,
            "still the one that started"
        );
        // not found: the wait is over
        playback.ended(playback.now().unwrap().attempt);
        playback.start(&show(), "2", false, Lookup::Resolve, "sub");
        playback.give_up(1);
        assert!(playback.now().is_none());
    }
}
