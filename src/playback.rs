//! Episode playback: one Playback attempt from enter until its player closes. A downloaded
//! file plays as it is; otherwise the show's source is checked or looked up and the episode
//! streams through ani-cli, with subtitles when its dub isn't out yet. Each attempt has an
//! id, so an earlier player's end never touches a later one, and a second enter while one
//! starts doesn't open a second player.

use crate::ani;
use crate::anilist::Media;
use crate::source::{Lookup, Source};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What an attempt waits on while it looks the show up: playing waits for that lookup.
pub const SEARCHING: &str = "searching ani-cli";

/// What plays episodes: ani-cli and mpv, or a script in tests.
pub trait Player: Send + Sync {
    /// Streams episode `ep` of `src`; `up` once the player shows it.
    fn stream(
        &self,
        src: &Source,
        ep: &str,
        audio: &str,
        up: &mut dyn FnMut(),
    ) -> Result<(), String>;
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
        src: &Source,
        ep: &str,
        audio: &str,
        up: &mut dyn FnMut(),
    ) -> Result<(), String> {
        ani::stream(src, ep, audio, up)
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
    show: Media,
    ep: String,
    file: Option<PathBuf>,
    lookup: Lookup,
    audio: &'static str,
    pub attempt: u64,
}

impl Playback {
    pub fn new(player: Arc<dyn Player>) -> Self {
        Playback {
            player,
            now: None,
            attempts: 0,
        }
    }

    /// The episode being started or watched.
    pub fn now(&self) -> Option<&Playing> {
        self.now.as_ref()
    }

    /// Enter on episode `ep` of `m`: its `file` on disk, else a stream from the source
    /// `lookup` gets, in `audio`. Waiting on a lookup for this very episode, it goes on with
    /// the same pop-up.
    pub fn start(
        &mut self,
        m: &Media,
        ep: &str,
        file: Option<PathBuf>,
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
        let step = match (&file, &lookup) {
            (Some(_), _) => "opening the player",
            (None, Lookup::Resolve) => SEARCHING,
            (None, Lookup::Check(_)) => "checking the source",
            (None, Lookup::Ready(_)) => "fetching the stream",
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
        if file.is_none() && matches!(lookup, Lookup::Resolve) {
            return Start::Lookup;
        }
        Start::Run(Run {
            player: self.player.clone(),
            show: m.clone(),
            ep: ep.to_string(),
            file,
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
        let mut up = || step(None);
        if let Some(file) = &self.file {
            return self.player.file(&self.show, &self.ep, file, &mut up);
        }
        let src = match self.lookup {
            Lookup::Ready(src) => src,
            lookup => {
                let src = self.player.locate(&self.show, lookup)?;
                step(Some("fetching the stream"));
                src
            }
        };
        match self.player.stream(&src, &self.ep, self.audio, &mut up) {
            // dubs come later or not at all, the subtitled one is the next best
            Err(e) if ani::dub(self.audio) && e.contains("dub") => {
                step(Some("no dub yet, fetching it with subtitles"));
                self.player.stream(&src, &self.ep, "sub", &mut up)
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
        played: Mutex<Vec<String>>,
    }

    impl Player for Script {
        fn stream(
            &self,
            _: &Source,
            ep: &str,
            audio: &str,
            up: &mut dyn FnMut(),
        ) -> Result<(), String> {
            if self.no_dub && audio == "dub" {
                return Err("No sources found for dub!".into());
            }
            up();
            self.played
                .lock()
                .unwrap()
                .push(format!("stream {ep} {audio}"));
            Ok(())
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
            ..Default::default()
        });
        let mut playback = Playback::new(script.clone());
        let file = Some(PathBuf::from("Show Episode 1.mp4"));
        let start = playback.start(&show(), "1", file, Lookup::Resolve, "sub");
        assert_eq!(playback.now().unwrap().step, Some("opening the player"));
        assert_eq!(run(&script, start), (vec!["file 1".into()], vec![None]));
        playback.ended(playback.now().unwrap().attempt);
        let start = playback.start(&show(), "2", None, Lookup::Check(source()), "dub");
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
    fn a_second_enter_while_one_starts_opens_no_second_player() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        let ready = || Lookup::Ready(source());
        assert!(matches!(
            playback.start(&show(), "1", None, ready(), "sub"),
            Start::Run(_)
        ));
        let again = playback.start(&show(), "2", None, ready(), "sub");
        assert!(matches!(again, Start::Busy(t) if t == "Episode 1 · Show"));
        // once its player is up, another one may start
        let first = playback.now().unwrap().attempt;
        playback.step(first, None);
        let Start::Run(second) = playback.start(&show(), "1", None, ready(), "sub") else {
            panic!("not started");
        };
        // and the first player's end leaves the second's pop-up alone
        playback.ended(first);
        playback.step(first, Some("late"));
        let now = playback.now().unwrap();
        assert_eq!(
            (now.attempt, now.step),
            (second.attempt, Some("fetching the stream"))
        );
        playback.ended(second.attempt);
        assert!(playback.now().is_none());
    }

    #[test]
    fn a_play_waiting_on_the_lookup_goes_on_with_the_same_pop_up() {
        let mut playback = Playback::new(Arc::new(Script::default()));
        assert!(matches!(
            playback.start(&show(), "1", None, Lookup::Resolve, "sub"),
            Start::Lookup
        ));
        let since = playback.now().unwrap().since;
        // found: started again, as the same pop-up
        let start = playback.start(&show(), "1", None, Lookup::Ready(source()), "sub");
        assert!(matches!(start, Start::Run(_)));
        playback.give_up(1);
        assert_eq!(
            playback.now().unwrap().since,
            since,
            "still the one that started"
        );
        // not found: the wait is over
        playback.ended(playback.now().unwrap().attempt);
        playback.start(&show(), "2", None, Lookup::Resolve, "sub");
        playback.give_up(1);
        assert!(playback.now().is_none());
    }
}
