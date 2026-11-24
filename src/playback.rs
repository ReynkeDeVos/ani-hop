//! Episode and trailer playback: one Playback attempt from enter until its player closes,
//! including what happens when it ends (saving progress, asking to mark watched, holding for
//! parts, or autoplaying the next episode).
//!
//! A downloaded file plays as it is; otherwise the show's source is checked or looked up and
//! the episode streams through ani-cli, with subtitles when its dub isn't out yet. Each
//! attempt owns its show, kind, login stamp and whether the player reported opening, so an
//! earlier player's end never touches a later one, and a second enter while one starts doesn't
//! open a second player.

use crate::anilist::Media;
use crate::entries::Stamp;
use crate::mpv::{Held, Replayed};
use crate::prefs::{self, ShowPrefs};
use crate::preload::{Key, Preload};
use crate::progress::{Mark, Progress};
use crate::source::{self, Focus, Lookup, Source};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What an attempt waits on while it looks the show up: playing waits for that lookup.
pub const SEARCHING: &str = "searching ani-cli";
/// What an attempt waits on once the player runs and hasn't shown the episode yet.
pub const OPENING: &str = "opening the player";
/// What an attempt waits on while ani-cli gets the episode's links.
const FETCHING: &str = "fetching the stream";
/// What a trailer waits on while mpv resolves it through yt-dlp.
const TRAILER: &str = "fetching the trailer";

/// What plays episodes, trailers and casts: `mpv::Mpv` in production, or a script in tests.
pub trait Player: Send + Sync {
    /// Streams episode `ep` of `src`; `step` hears what it waits on, and None once the player
    /// shows it. Ok(true): it played to the end.
    fn stream(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        skip: bool,
        step: &dyn Fn(Option<&'static str>),
    ) -> Result<bool, String>;
    /// Runs ani-cli up to where it would start the player, and keeps what it would start it with.
    fn hold(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        audio: &str,
        skip: bool,
    ) -> Result<Held, String>;
    /// Starts the player with what `hold` kept; `step` hears the same as from `stream`.
    fn replay(&self, held: &Held, step: &dyn Fn(Option<&'static str>)) -> Replayed;
    /// Episode `ep` of `m`, if it's downloaded. It may be on a slow disk.
    fn on_disk(&self, m: &Media, ep: &str) -> Option<PathBuf>;
    /// Plays a downloaded episode. Ok(true): to the end.
    fn file(
        &self,
        m: &Media,
        ep: &str,
        file: &Path,
        audio: &str,
        skip: bool,
        up: &mut dyn FnMut(),
    ) -> Result<bool, String>;
    /// Plays a show's trailer URL; `up` hears once the player shows it.
    fn trailer(&self, m: &Media, url: &str, up: &mut dyn FnMut()) -> Result<(), String>;
    /// Casts a downloaded episode to a Chromecast.
    fn cast(
        &self,
        m: &Media,
        ep: &str,
        audio: &str,
        wanted: &dyn Fn() -> bool,
        asked: &dyn Fn(String),
    ) -> Result<(), String>;
    /// Where the provider has the show now.
    fn locate(&self, m: &Media, lookup: Lookup) -> Result<Source, String>;
}

/// Whether a Playback attempt is an episode or a show's trailer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    Episode(String),
    Trailer,
}

/// The episode or trailer being started or watched.
pub struct Playing {
    pub id: u64,
    pub kind: AttemptKind,
    pub title: String,
    /// what it waits on, None once the player shows it
    pub step: Option<&'static str>,
    /// when enter was pressed, for the pop-up
    pub since: Instant,
    pub color: Option<String>,
    attempt: u64,
}

impl Playing {
    /// Episode number if this attempt is playing an episode (None for a trailer).
    pub fn episode(&self) -> Option<&str> {
        match &self.kind {
            AttemptKind::Episode(ep) => Some(ep),
            AttemptKind::Trailer => None,
        }
    }

    #[cfg(test)]
    pub fn attempt(&self) -> u64 {
        self.attempt
    }
}

struct InFlight {
    show: Media,
    kind: AttemptKind,
    stamp: Stamp,
    /// whether the player ever reported opening (`step(None)`)
    shown: bool,
}

pub struct Playback {
    player: Arc<dyn Player>,
    now: Option<Playing>,
    in_flight: HashMap<u64, InFlight>,
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

/// An episode or trailer attempt to run in the background, once.
pub struct Run {
    pub attempt: u64,
    task: Task,
}

enum Task {
    Episode {
        player: Arc<dyn Player>,
        preload: Preload,
        show: Media,
        ep: String,
        /// what the last scan of the download folder said, which it says on the pop-up
        on_disk: bool,
        lookup: Lookup,
        audio: &'static str,
        skip: bool,
    },
    Trailer {
        player: Arc<dyn Player>,
        show: Media,
        url: String,
    },
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
    skip: bool,
    focus: Focus,
}

impl Ahead {
    /// Blocks in ani-cli's turn, unless the show has left the screen by then.
    pub fn run(self) {
        let res = source::in_turn(self.show.id, &self.focus, || {
            (self.player).hold(&self.show, &self.src, &self.ep, self.audio, self.skip)
        });
        self.preload.done(&self.key, res);
    }
}

/// What `Playback::finish` hands back when a background attempt ends.
pub enum Ended {
    Trailer(Result<(), String>),
    Episode(EpisodeEnd),
}

/// Everything known about an episode attempt that just finished.
pub struct EpisodeEnd {
    pub show: Media,
    pub ep: String,
    pub stamp: Stamp,
    /// still the latest attempt on screen (no newer player started over it)
    pub latest: bool,
    /// the player never reported opening (`VO:`): a custom player still counts as played on Ok
    pub unsaid: bool,
    /// Ok(true): played to the end
    pub res: Result<bool, String>,
}

/// What to do after an episode finishes, decided once in pure logic.
pub struct Decision {
    /// count the show as played on the Continue row (for players that never print `VO:`)
    pub record_played: Option<Media>,
    pub after: AfterEpisode,
}

pub enum AfterEpisode {
    Failed {
        ep: String,
        error: String,
    },
    Autoplay {
        show: Media,
        ep: String,
        next: String,
        mark: Option<Mark>,
    },
    LastEpisode {
        show: Media,
        ep: String,
        mark: Option<Mark>,
    },
    AskWatched {
        show: Media,
        ep: String,
        mark: Mark,
    },
    HoldForParts {
        show: Media,
        ep: String,
    },
    Nothing,
}

impl EpisodeEnd {
    /// Pure post-episode policy: whether to record Continue, save quietly and autoplay, ask to
    /// mark watched, wait for AniList parts, or report a failure.
    pub fn decide(
        &self,
        same_login: bool,
        logged_in: bool,
        autoplay_on: bool,
        counted: bool,
        progress: &Progress<'_>,
        episodes: &[String],
    ) -> Decision {
        let ours = same_login && logged_in && self.show.on_anilist();
        let record_played = (ours && self.unsaid && self.res.is_ok()).then(|| self.show.clone());
        let autoplay = self.latest && same_login && autoplay_on;
        let after = match &self.res {
            Err(error) => AfterEpisode::Failed {
                ep: self.ep.clone(),
                error: error.clone(),
            },
            Ok(true) if autoplay => {
                if ours && !counted {
                    AfterEpisode::HoldForParts {
                        show: self.show.clone(),
                        ep: self.ep.clone(),
                    }
                } else {
                    let mark = progress.unwatched(&self.ep).filter(|_| ours);
                    // a recap like 4.5 saves 4, and an extra after the last episode the last:
                    // maybe unwatched, so ask instead of saving quietly
                    let as_is = |k: &Mark| progress.entry(&self.ep).1 == k.to.to_string();
                    let quiet = mark.as_ref().is_some_and(|k| k.only_this() && as_is(k));
                    match mark {
                        Some(mark) if !quiet => AfterEpisode::AskWatched {
                            show: self.show.clone(),
                            ep: self.ep.clone(),
                            mark,
                        },
                        mark => match episodes
                            .iter()
                            .skip_while(|e| **e != self.ep)
                            .nth(1)
                            .cloned()
                        {
                            Some(next) => AfterEpisode::Autoplay {
                                show: self.show.clone(),
                                ep: self.ep.clone(),
                                next,
                                mark,
                            },
                            None => AfterEpisode::LastEpisode {
                                show: self.show.clone(),
                                ep: self.ep.clone(),
                                mark,
                            },
                        },
                    }
                }
            }
            Ok(_) if ours => {
                if !counted {
                    AfterEpisode::HoldForParts {
                        show: self.show.clone(),
                        ep: self.ep.clone(),
                    }
                } else if let Some(mark) = progress.unwatched(&self.ep) {
                    AfterEpisode::AskWatched {
                        show: self.show.clone(),
                        ep: self.ep.clone(),
                        mark,
                    }
                } else {
                    AfterEpisode::Nothing
                }
            }
            Ok(_) => AfterEpisode::Nothing,
        };
        Decision {
            record_played,
            after,
        }
    }
}

impl Playback {
    pub fn new(player: Arc<dyn Player>) -> Self {
        Playback {
            player,
            now: None,
            in_flight: HashMap::new(),
            attempts: 0,
            preload: Preload::new(),
        }
    }

    pub fn player(&self) -> Arc<dyn Player> {
        self.player.clone()
    }

    /// Fetching episode `ep` of `m` ahead, unless that's there or on its way.
    pub fn ahead(
        &self,
        m: &Media,
        src: &Source,
        ep: &str,
        prefs: ShowPrefs,
        focus: Focus,
    ) -> Option<Ahead> {
        let key = Key::new(m.id, ep, prefs.audio, prefs.skip, src);
        self.preload.begin(&key).then(|| Ahead {
            player: self.player.clone(),
            preload: self.preload.clone(),
            key,
            show: m.clone(),
            src: src.clone(),
            ep: ep.to_string(),
            audio: prefs.audio,
            skip: prefs.skip,
            focus,
        })
    }

    /// The episode or trailer being started or watched.
    pub fn now(&self) -> Option<&Playing> {
        self.now.as_ref()
    }

    #[cfg(test)]
    pub fn settle(&mut self) {
        if let Some(p) = &mut self.now {
            p.since -= std::time::Duration::from_secs(1);
        }
    }

    /// Enter on episode `ep` of `m`: its file, if the last scan found it `on_disk`, else a
    /// stream from the source `lookup` gets, with `prefs`. Waiting on a lookup for this very
    /// episode, it goes on with the same pop-up.
    pub fn start(
        &mut self,
        m: &Media,
        ep: &str,
        on_disk: bool,
        lookup: Lookup,
        prefs: ShowPrefs,
        stamp: Stamp,
    ) -> Start {
        let waited = (self.now.as_ref())
            .filter(|p| p.id == m.id && p.episode() == Some(ep) && p.step == Some(SEARCHING))
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
        let attempt = self.attempts;
        let kind = AttemptKind::Episode(ep.to_string());
        self.now = Some(Playing {
            id: m.id,
            kind: kind.clone(),
            // the number first: a long title loses its end, not the episode
            title: format!("Episode {ep} · {}", m.name()),
            step: Some(step),
            since: waited.unwrap_or_else(Instant::now),
            color: m.cover_image.color.clone(),
            attempt,
        });
        // shortcut: a file the first scan hasn't got to yet waits for this lookup, and fails with
        // it offline, until a second enter; a scan takes milliseconds unless the disk sleeps
        if !on_disk && matches!(lookup, Lookup::Resolve) {
            return Start::Lookup;
        }
        self.in_flight.insert(
            attempt,
            InFlight {
                show: m.clone(),
                kind,
                stamp,
                shown: false,
            },
        );
        Start::Run(Run {
            attempt,
            task: Task::Episode {
                player: self.player.clone(),
                preload: self.preload.clone(),
                show: m.clone(),
                ep: ep.to_string(),
                on_disk,
                lookup,
                audio: prefs.audio,
                skip: prefs.skip,
            },
        })
    }

    /// Enter on Watch trailer: opens `url` in mpv, unless something else is starting or playing.
    pub fn start_trailer(&mut self, m: &Media, url: String, stamp: Stamp) -> Result<Run, String> {
        if let Some(p) = self.now.as_ref() {
            return Err(match p.step {
                Some(_) => format!("Still starting {}…", p.title),
                None => format!("Already playing {}", p.title),
            });
        }
        self.attempts += 1;
        let attempt = self.attempts;
        self.now = Some(Playing {
            id: m.id,
            kind: AttemptKind::Trailer,
            title: format!("Trailer · {}", m.name()),
            step: Some(TRAILER),
            since: Instant::now(),
            color: m.cover_image.color.clone(),
            attempt,
        });
        self.in_flight.insert(
            attempt,
            InFlight {
                show: m.clone(),
                kind: AttemptKind::Trailer,
                stamp,
                shown: false,
            },
        );
        Ok(Run {
            attempt,
            task: Task::Trailer {
                player: self.player.clone(),
                show: m.clone(),
                url,
            },
        })
    }

    /// What `attempt` waits on now; None once its player shows the episode or trailer.
    /// Returns `Some((show, stamp))` the first time an episode's player opens, so Continue can
    /// record it; trailers return `None`.
    pub fn step(&mut self, attempt: u64, step: Option<&'static str>) -> Option<(Media, Stamp)> {
        if let Some(p) = self.now.as_mut().filter(|p| p.attempt == attempt) {
            p.step = step;
        }
        if step.is_none()
            && let Some(flight) = self.in_flight.get_mut(&attempt)
        {
            let first = !flight.shown;
            flight.shown = true;
            if first && matches!(flight.kind, AttemptKind::Episode(_)) {
                return Some((flight.show.clone(), flight.stamp));
            }
        }
        None
    }

    /// `attempt` ended (its player closed or failed). Returns its owned outcome so the caller
    /// never carries attempt metadata across messages.
    pub fn finish(&mut self, attempt: u64, res: Result<bool, String>) -> Option<Ended> {
        let latest = self.now.take_if(|p| p.attempt == attempt).is_some();
        let flight = self.in_flight.remove(&attempt)?;
        Some(match flight.kind {
            AttemptKind::Trailer => Ended::Trailer(res.map(|_| ())),
            AttemptKind::Episode(ep) => Ended::Episode(EpisodeEnd {
                show: flight.show,
                ep,
                stamp: flight.stamp,
                latest,
                unsaid: !flight.shown,
                res,
            }),
        })
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
    /// player shows it. Ok(true): an episode played to the end.
    pub fn run(self, step: &dyn Fn(Option<&'static str>)) -> Result<bool, String> {
        match self.task {
            Task::Trailer { player, show, url } => {
                player.trailer(&show, &url, &mut || step(None))?;
                Ok(false)
            }
            Task::Episode {
                player,
                preload,
                show,
                ep,
                on_disk,
                lookup,
                audio,
                skip,
            } => {
                // only here, with the pop-up up, as the folder may be on a slow disk
                if let Some(file) = player.on_disk(&show, &ep) {
                    step(Some(OPENING));
                    return player.file(&show, &ep, &file, audio, skip, &mut || step(None));
                }
                let src = match lookup {
                    // the last scan found a file that's gone
                    Lookup::Ready(src) if on_disk => {
                        step(Some(FETCHING));
                        src
                    }
                    Lookup::Ready(src) => src,
                    lookup => {
                        let src = player.locate(&show, lookup)?;
                        step(Some(FETCHING));
                        src
                    }
                };
                let key = Key::new(show.id, &ep, audio, skip, &src);
                if let Some(held) = preload.take(&key) {
                    step(Some(OPENING));
                    match player.replay(&held, step) {
                        Replayed::Ended(res) => return res,
                        Replayed::TooOld => step(Some(FETCHING)),
                    }
                }
                match player.stream(&show, &src, &ep, audio, skip, step) {
                    // dubs come later or not at all, the subtitled one is the next best
                    Err(e) if prefs::dub(audio) && e.contains("dub") => {
                        step(Some("no dub yet, fetching it with subtitles"));
                        player.stream(&show, &src, &ep, "sub", skip, step)
                    }
                    res => res,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anilist::Entry;
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
            _: bool,
            step: &dyn Fn(Option<&'static str>),
        ) -> Result<bool, String> {
            if self.no_dub && audio == "dub" {
                return Err("No sources found for dub!".into());
            }
            step(None);
            self.played
                .lock()
                .unwrap()
                .push(format!("stream {ep} {audio}"));
            Ok(true)
        }

        fn hold(
            &self,
            _: &Media,
            _: &Source,
            ep: &str,
            audio: &str,
            _: bool,
        ) -> Result<Held, String> {
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
            Replayed::Ended(Ok(true))
        }

        fn on_disk(&self, _: &Media, ep: &str) -> Option<PathBuf> {
            self.files.iter().find(|f| *f == ep).map(PathBuf::from)
        }

        fn file(
            &self,
            _: &Media,
            ep: &str,
            _: &Path,
            _: &str,
            _: bool,
            up: &mut dyn FnMut(),
        ) -> Result<bool, String> {
            up();
            self.played.lock().unwrap().push(format!("file {ep}"));
            Ok(true)
        }

        fn trailer(&self, _: &Media, url: &str, up: &mut dyn FnMut()) -> Result<(), String> {
            up();
            self.played.lock().unwrap().push(format!("trailer {url}"));
            Ok(())
        }

        fn cast(
            &self,
            _: &Media,
            _: &str,
            _: &str,
            _: &dyn Fn() -> bool,
            _: &dyn Fn(String),
        ) -> Result<(), String> {
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
            episodes: Some(12),
            ..Default::default()
        };
        m.title.romaji = Some("Show".into());
        m
    }

    fn prefs(audio: &'static str) -> ShowPrefs {
        ShowPrefs {
            audio,
            audio_own: false,
            skip: true,
            skip_own: false,
            autoplay: false,
            autoplay_own: false,
        }
    }

    fn stamp() -> Stamp {
        crate::entries::Entries::new(Arc::new(crate::anilist::Client { token: None })).stamp()
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
        let start = playback.start(&show(), "1", true, Lookup::Resolve, prefs("sub"), stamp());
        assert_eq!(playback.now().unwrap().step, Some(OPENING));
        assert_eq!(
            run(&script, start),
            (vec!["file 1".into()], vec![Some(OPENING), None])
        );
        playback.finish(playback.now().unwrap().attempt, Ok(true));
        let start = playback.start(
            &show(),
            "2",
            false,
            Lookup::Check(source()),
            prefs("dub"),
            stamp(),
        );
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
        let start = playback.start(
            &show(),
            "1",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
        assert_eq!(run(&script, start).0, ["file 1"]);
        playback.finish(playback.now().unwrap().attempt, Ok(true));
        let start = playback.start(
            &show(),
            "2",
            true,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
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
        let ahead = playback.ahead(&show(), &source(), "2", prefs("sub"), focus.clone());
        ahead.unwrap().run();
        assert!(
            (playback.ahead(&show(), &source(), "2", prefs("sub"), focus)).is_none(),
            "it's there"
        );
        let start = playback.start(
            &show(),
            "2",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
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
            .ahead(&show(), &source(), "2", prefs("sub"), focus)
            .unwrap())
        .run();
        let start = playback.start(
            &show(),
            "2",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
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
            .ahead(&show(), &source(), "2", prefs("sub"), focus)
            .unwrap())
        .run();
        assert!(script.played.lock().unwrap().is_empty());
        let start = playback.start(
            &show(),
            "2",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
        assert_eq!(run(&script, start).0, ["stream 2 sub"]);
    }

    #[test]
    fn a_second_enter_while_one_starts_opens_no_second_player() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        let ready = || Lookup::Ready(source());
        assert!(matches!(
            playback.start(&show(), "1", false, ready(), prefs("sub"), stamp()),
            Start::Run(_)
        ));
        let again = playback.start(&show(), "2", false, ready(), prefs("sub"), stamp());
        assert!(matches!(again, Start::Busy(t) if t == "Episode 1 · Show"));
        // once its player is up, another one may start
        let first = playback.now().unwrap().attempt;
        assert!(playback.step(first, None).is_some());
        let Start::Run(second) =
            playback.start(&show(), "1", false, ready(), prefs("sub"), stamp())
        else {
            panic!("not started");
        };
        // and the first player's end leaves the second's pop-up alone and knows it's not latest
        let Some(Ended::Episode(end)) = playback.finish(first, Ok(true)) else {
            panic!("episode end");
        };
        assert!(!end.latest && !end.unsaid);
        assert!(playback.step(first, Some("late")).is_none());
        let now = playback.now().unwrap();
        assert_eq!((now.attempt, now.step), (second.attempt, Some(FETCHING)));
        let Some(Ended::Episode(end2)) = playback.finish(second.attempt, Ok(false)) else {
            panic!("episode end");
        };
        assert!(end2.latest && end2.unsaid);
        assert!(playback.now().is_none());
    }

    #[test]
    fn a_play_waiting_on_the_lookup_goes_on_with_the_same_pop_up() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        assert!(matches!(
            playback.start(&show(), "1", false, Lookup::Resolve, prefs("sub"), stamp()),
            Start::Lookup
        ));
        let since = playback.now().unwrap().since;
        // found: started again, as the same pop-up
        let start = playback.start(
            &show(),
            "1",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        );
        assert!(matches!(start, Start::Run(_)));
        playback.give_up(1);
        assert_eq!(
            playback.now().unwrap().since,
            since,
            "still the one that started"
        );
        // not found: the wait is over
        playback.finish(playback.now().unwrap().attempt, Ok(false));
        playback.start(&show(), "2", false, Lookup::Resolve, prefs("sub"), stamp());
        playback.give_up(1);
        assert!(playback.now().is_none());
    }

    #[test]
    fn a_trailer_starts_in_its_own_attempt_and_blocks_a_second_start() {
        let script = Arc::new(Script::default());
        let mut playback = Playback::new(script.clone());
        let url = "https://www.youtube.com/watch?v=4MYo8FfiXMA";
        let run = playback
            .start_trailer(&show(), url.into(), stamp())
            .unwrap();
        let now = playback.now().unwrap();
        assert_eq!(now.kind, AttemptKind::Trailer);
        assert!(now.title == "Trailer · Show" && now.step == Some(TRAILER));
        assert_eq!(
            playback
                .start_trailer(&show(), url.into(), stamp())
                .err()
                .as_deref(),
            Some("Still starting Trailer · Show…")
        );
        assert!(matches!(
            playback.start(
                &show(),
                "1",
                false,
                Lookup::Ready(source()),
                prefs("sub"),
                stamp(),
            ),
            Start::Busy(t) if t == "Trailer · Show"
        ));
        let shown = std::cell::Cell::new(false);
        let attempt = run.attempt;
        assert_eq!(
            run.run(&|s| if s.is_none() {
                shown.set(true);
            }),
            Ok(false)
        );
        assert!(shown.get());
        assert_eq!(*script.played.lock().unwrap(), [format!("trailer {url}")]);
        // opening a trailer does not count as playing an episode for Continue
        assert!(playback.step(attempt, None).is_none());
        assert!(playback.now().unwrap().step.is_none());
        assert_eq!(
            playback
                .start_trailer(&show(), url.into(), stamp())
                .err()
                .as_deref(),
            Some("Already playing Trailer · Show")
        );
        assert!(matches!(
            playback.finish(attempt, Ok(false)),
            Some(Ended::Trailer(Ok(())))
        ));
        assert!(playback.now().is_none());
        // while an episode is playing, starting a trailer leaves that episode's attempt intact
        let Start::Run(ep) = playback.start(
            &show(),
            "1",
            false,
            Lookup::Ready(source()),
            prefs("sub"),
            stamp(),
        ) else {
            panic!("not started");
        };
        assert!(playback.step(ep.attempt, None).is_some());
        assert_eq!(
            playback
                .start_trailer(&show(), url.into(), stamp())
                .err()
                .as_deref(),
            Some("Already playing Episode 1 · Show")
        );
        assert!(playback.finish(ep.attempt, Ok(false)).is_some() && playback.now().is_none());
    }

    #[test]
    fn post_episode_decision_covers_autoplay_recaps_parts_and_stale_attempts() {
        let mut m = show();
        m.media_list_entry = Some(Entry {
            id: 1,
            status: "CURRENT".into(),
            progress: 3,
        });
        let progress = Progress::of(&m, &[]);
        let eps: Vec<String> = (1..=12).map(|n| n.to_string()).collect();
        let end = |ep: &str, latest: bool, unsaid: bool, res: Result<bool, String>| EpisodeEnd {
            show: m.clone(),
            ep: ep.into(),
            stamp: stamp(),
            latest,
            unsaid,
            res,
        };
        // closed early: asks, even with autoplay on
        let d = end("4", true, false, Ok(false)).decide(true, true, true, true, &progress, &eps);
        assert!(d.record_played.is_none() && matches!(d.after, AfterEpisode::AskWatched { .. }));
        // played to end with autoplay: quietly marks 4 and autoplays 5
        let d = end("4", true, true, Ok(true)).decide(true, true, true, true, &progress, &eps);
        assert!(d.record_played.is_some());
        assert!(matches!(
            d.after,
            AfterEpisode::Autoplay { ref next, mark: Some(_), .. } if next == "5"
        ));
        // earlier player that ended after another started: asks instead of autoplaying over it
        let d = end("4", false, false, Ok(true)).decide(true, true, true, true, &progress, &eps);
        assert!(matches!(d.after, AfterEpisode::AskWatched { .. }));
        // skipping episodes (ep 6 from progress 3): asks, no autoplay
        let d = end("6", true, false, Ok(true)).decide(true, true, true, true, &progress, &eps);
        assert!(matches!(d.after, AfterEpisode::AskWatched { .. }));
        // recap 4.5 would mark 4: asks, no autoplay
        let mut with_recap = eps.clone();
        with_recap.insert(4, "4.5".into());
        let d = end("4.5", true, false, Ok(true)).decide(
            true,
            true,
            true,
            true,
            &progress,
            &with_recap,
        );
        assert!(matches!(d.after, AfterEpisode::AskWatched { .. }));
        // last episode out with autoplay: saves quietly and reports LastEpisode
        let mut at_11 = m.clone();
        at_11.media_list_entry.as_mut().unwrap().progress = 11;
        let p11 = Progress::of(&at_11, &[]);
        let d = EpisodeEnd {
            show: at_11.clone(),
            ep: "12".into(),
            stamp: stamp(),
            latest: true,
            unsaid: false,
            res: Ok(true),
        }
        .decide(true, true, true, true, &p11, &eps);
        assert!(matches!(
            d.after,
            AfterEpisode::LastEpisode { mark: Some(_), .. }
        ));
        // uncounted episode past show's count: holds for parts
        let d = end("13", true, false, Ok(true)).decide(true, true, true, false, &progress, &eps);
        assert!(matches!(d.after, AfterEpisode::HoldForParts { .. }));
        // logged out with autoplay: still autoplays without saving a mark
        let d = end("4", true, true, Ok(true)).decide(true, false, true, true, &progress, &eps);
        assert!(d.record_played.is_none());
        assert!(matches!(
            d.after,
            AfterEpisode::Autoplay { ref next, mark: None, .. } if next == "5"
        ));
        // earlier login: neither records Continue nor autoplays nor asks
        let d = end("4", true, true, Ok(true)).decide(false, true, true, true, &progress, &eps);
        assert!(d.record_played.is_none() && matches!(d.after, AfterEpisode::Nothing));
        // failed playback: surfaces error, never records Continue
        let d = end("4", true, true, Err("offline".into()))
            .decide(true, true, true, true, &progress, &eps);
        assert!(d.record_played.is_none() && matches!(d.after, AfterEpisode::Failed { .. }));
        // already-watched episode from a custom player still records Continue
        let d = end("2", true, true, Ok(false)).decide(true, true, false, true, &progress, &eps);
        assert!(d.record_played.is_some() && matches!(d.after, AfterEpisode::Nothing));
    }
}
