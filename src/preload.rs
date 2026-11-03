//! The next episode's stream, fetched ahead (Settings: Preload next episode). While a show's
//! page is open, ani-cli runs up to where it would start the player, so enter starts mpv at
//! once. Its requests are wasted on an episode that doesn't get played, which is why it's the
//! user's choice. One episode at a time, as what it needs is long gone by the next.

use crate::ani::{self, Held};
use crate::source::Source;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long what was fetched is trusted. Links were seen working after 9 minutes, and one that
/// fails starts over; this is for not finding that out at all.
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);

/// How long enter waits for what's on its way, before fetching it the usual way: longer than
/// that takes, unless something stalled, which enter mustn't wait a minute for.
const JOIN_FOR: Duration = Duration::from_secs(15);

/// What a fetched stream is for: changing any of this makes it another stream.
#[derive(Clone, PartialEq)]
pub struct Key {
    show: u64,
    ep: String,
    audio: &'static str,
    skip: bool,
    source: (String, usize, String),
}

impl Key {
    pub fn new(show: u64, ep: &str, audio: &'static str, src: &Source) -> Key {
        Key {
            show,
            ep: ep.to_string(),
            audio,
            skip: ani::skip_on(),
            source: (src.query.clone(), src.index, src.title.clone()),
        }
    }
}

enum Slot {
    Empty,
    Fetching(Key),
    Ready(Key, Held, Instant),
}

/// The one stream fetched ahead, or on its way; clones share it.
#[derive(Clone)]
pub struct Preload(Arc<(Mutex<Slot>, Condvar)>);

impl Preload {
    pub fn new() -> Preload {
        Preload(Arc::new((Mutex::new(Slot::Empty), Condvar::new())))
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `key` has to be fetched now: not while it's there or on its way. What was there
    /// for another episode goes.
    pub fn begin(&self, key: &Key) -> bool {
        let mut slot = self.slot();
        let has = match &*slot {
            Slot::Fetching(k) => k == key,
            Slot::Ready(k, _, at) => k == key && at.elapsed() < FRESH_FOR,
            Slot::Empty => false,
        };
        if !has {
            *slot = Slot::Fetching(key.clone());
        }
        !has
    }

    /// Takes what fetching `key` got, unless another was asked for since.
    pub fn done(&self, key: &Key, res: Result<Held, String>) {
        let mut slot = self.slot();
        if matches!(&*slot, Slot::Fetching(k) if k == key) {
            *slot = match res {
                Ok(held) => Slot::Ready(key.clone(), held, Instant::now()),
                Err(_) => Slot::Empty,
            };
            self.0.1.notify_all();
        }
    }

    /// `key`'s stream, also waiting for one on its way. None when there's none, as a stream
    /// failing here just means it's fetched the usual way.
    pub fn take(&self, key: &Key) -> Option<Held> {
        let slot = self.slot();
        let (mut slot, _) = (self.0.1)
            .wait_timeout_while(
                slot,
                JOIN_FOR,
                |s| matches!(s, Slot::Fetching(k) if k == key),
            )
            .unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut *slot, Slot::Empty) {
            Slot::Ready(k, held, at) if k == *key => (at.elapsed() < FRESH_FOR).then_some(held),
            // stuck: nothing to wait for anymore
            Slot::Fetching(k) if k == *key => None,
            other => {
                *slot = other;
                None
            }
        }
    }

    /// Lets go of what's there or on its way, as preloading was switched off.
    pub fn clear(&self) {
        *self.slot() = Slot::Empty;
        self.0.1.notify_all();
    }

    /// Whether `key` is what's being fetched.
    #[cfg(test)]
    pub fn fetching(&self, key: &Key) -> bool {
        matches!(&*self.slot(), Slot::Fetching(k) if k == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> Source {
        Source {
            query: "show".into(),
            index: 1,
            title: "Show".into(),
            nth: 1,
            episodes: vec!["1".into(), "2".into()],
        }
    }

    fn held(subs: Option<std::path::PathBuf>) -> Held {
        Held {
            args: vec!["link".into()],
            subs,
        }
    }

    #[test]
    fn one_stream_is_fetched_once_and_taken_once() {
        let p = Preload::new();
        let key = Key::new(1, "2", "sub", &src());
        assert!(p.begin(&key), "nothing there yet");
        assert!(!p.begin(&key), "on its way");
        p.done(&key, Ok(held(None)));
        assert!(!p.begin(&key), "there");
        assert_eq!(p.take(&key).unwrap().args, ["link"]);
        assert!(p.take(&key).is_none(), "taken");
    }

    #[test]
    fn another_episode_audio_or_source_is_another_stream() {
        let p = Preload::new();
        let key = Key::new(1, "2", "sub", &src());
        assert!(p.begin(&key));
        p.done(&key, Ok(held(None)));
        let moved = Source { index: 2, ..src() };
        for other in [
            Key::new(1, "3", "sub", &src()),
            Key::new(1, "2", "dub", &src()),
            Key::new(2, "2", "sub", &src()),
            Key::new(1, "2", "sub", &moved),
        ] {
            assert!(p.take(&other).is_none());
        }
        assert!(p.take(&key).is_some(), "still there for what it's for");
    }

    #[test]
    fn enter_waits_for_the_stream_on_its_way_and_a_late_one_counts_for_nothing() {
        let p = Preload::new();
        let (key, newer) = (
            Key::new(1, "2", "sub", &src()),
            Key::new(1, "3", "sub", &src()),
        );
        assert!(p.begin(&key));
        let (later, k) = (p.clone(), key.clone());
        let fetching = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            later.done(&k, Ok(held(None)));
        });
        assert!(p.take(&key).is_some(), "it waited");
        fetching.join().unwrap();
        // asked for another meanwhile: the first's answer goes, with its subtitles
        let subs = std::env::temp_dir().join(format!("ani-hop-preload-{}", std::process::id()));
        std::fs::write(&subs, "WEBVTT").unwrap();
        assert!(p.begin(&key));
        assert!(p.begin(&newer));
        p.done(&key, Ok(held(Some(subs.clone()))));
        assert!(p.fetching(&newer));
        assert!(!subs.exists());
    }

    #[test]
    fn a_failed_or_old_stream_leaves_nothing_to_wait_for() {
        let p = Preload::new();
        let key = Key::new(1, "2", "sub", &src());
        assert!(p.begin(&key));
        p.done(&key, Err("offline".into()));
        assert!(p.take(&key).is_none());
        // too old to trust: fetched again, not played
        let old = Instant::now().checked_sub(FRESH_FOR).unwrap();
        *p.slot() = Slot::Ready(key.clone(), held(None), old);
        assert!(p.begin(&key));
        *p.slot() = Slot::Ready(key.clone(), held(None), old);
        assert!(p.take(&key).is_none());
    }

    #[test]
    fn switching_it_off_ends_the_wait() {
        let p = Preload::new();
        let key = Key::new(1, "2", "sub", &src());
        assert!(p.begin(&key));
        let later = p.clone();
        let off = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            later.clear();
        });
        assert!(p.take(&key).is_none());
        off.join().unwrap();
    }
}
