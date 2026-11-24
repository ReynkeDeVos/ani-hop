//! Show downloads: where one show's episodes stand with downloads (on disk, queued here,
//! queued in another ani-hop, downloading or held by a pause) and what enter and `^o` (cast)
//! do with the ones picked. The episode rows, the footer, casting, the "which?" choices and
//! the questions before stopping, deleting or pausing all read it here, so they can't disagree;
//! the Download queue does the work.
//!
//! Pure: reads only the background scan snapshot and queue snapshot passed in, never touching
//! the download folder on the UI thread.

use crate::ani::{self, LocalEp};
use crate::anilist::Media;
use crate::downloads::{Now, Snapshot};
use crate::menu::Item;
use crate::plural;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where an episode stands with downloads.
enum Dl {
    Missing,
    /// in this ani-hop's queue, maybe downloading right now
    Queued,
    OnDisk(LocalEp),
    /// queued by another running ani-hop
    Elsewhere,
}

/// What enter does with the episodes picked.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// nothing, and why; no reason when what was chosen for them is gone
    Nothing(&'static str),
    /// asks which: they stand differently, or one is downloading (pause or stop)
    Choose,
    Download(Vec<String>),
    /// takes them out of the queue; asks first when that deletes what they got (`deletes_bytes`)
    Stop(Vec<String>),
    /// asks first, with their files
    Delete(Vec<(String, PathBuf)>),
    /// holds downloads while episode `.0` downloads; ffmpeg's starts over, which asks first and
    /// says which ("Episode 3")
    Pause(String, Option<String>),
    Resume,
}

/// What `^o` (cast to TV) does with an episode.
#[derive(Debug, PartialEq, Eq)]
pub enum CastAction {
    /// On disk and not still in any download queue: cast its file now.
    CastNow,
    /// Already in this ani-hop's download queue: wait until it finishes, then cast.
    Wait { paused: bool },
    /// Not on disk or queued: queue it to download, then cast once done.
    DownloadThenCast,
    /// Queued in another ani-hop: refuse (may lack subtitles yet, we won't hear when it's done).
    Elsewhere,
}

/// The episode `^o` asked to cast last: its turn number (so an earlier cast's messages are
/// ignored) and the episode waiting for its download to finish.
#[derive(Default)]
pub struct CastState {
    turn: Arc<AtomicU64>,
    waiting: Option<(Media, String)>,
}

impl CastState {
    /// Starts a new cast turn, replacing any previous cast or download wait.
    pub fn begin(&mut self) -> (Arc<AtomicU64>, u64) {
        self.waiting = None;
        let mine = self.turn.fetch_add(1, Ordering::Relaxed) + 1;
        (self.turn.clone(), mine)
    }

    /// Current turn handle, without advancing it (e.g. when casting after a waited download).
    pub fn turn(&self) -> (Arc<AtomicU64>, u64) {
        (self.turn.clone(), self.turn.load(Ordering::Relaxed))
    }

    /// Whether `turn` is still the latest `^o`.
    pub fn current(&self, turn: u64) -> bool {
        turn == self.turn.load(Ordering::Relaxed)
    }

    /// Remembers `(m, ep)` to cast once its download leaves the queue.
    pub fn wait(&mut self, m: Media, ep: String) {
        self.waiting = Some((m, ep));
    }

    /// The episode waiting for its download to finish, if any.
    pub fn pending(&self) -> Option<(&Media, &str)> {
        self.waiting.as_ref().map(|(m, ep)| (m, ep.as_str()))
    }

    /// Called after a download rescan with the waited episode's current `CastAction`: once it
    /// leaves the queue, clears the wait and returns it along with whether it's on disk to cast.
    pub fn after_scan(&mut self, action: CastAction) -> Option<(Media, String, bool)> {
        if self.waiting.is_none() || matches!(action, CastAction::Wait { .. }) {
            return None;
        }
        let (m, ep) = self.waiting.take().unwrap();
        Some((m, ep, action == CastAction::CastNow))
    }

    #[cfg(test)]
    pub fn waiting(&self) -> Option<(u64, &str)> {
        self.waiting.as_ref().map(|(m, ep)| (m.id, ep.as_str()))
    }
}

pub struct ShowDownloads<'a> {
    show: &'a Media,
    on_disk: Vec<LocalEp>,
    parts: Vec<(String, u64)>,
    queued: Vec<String>,
    elsewhere: Vec<String>,
    /// the queue as it was, read once
    queue: Snapshot,
}

impl<'a> ShowDownloads<'a> {
    /// `show`'s episodes and unfinished parts from the last scan, queued here and in other
    /// ani-hops, and the queue now.
    pub fn of(
        show: &'a Media,
        on_disk: Vec<LocalEp>,
        parts: Vec<(String, u64)>,
        queued: Vec<String>,
        elsewhere: Vec<String>,
        queue: Snapshot,
    ) -> Self {
        ShowDownloads {
            show,
            on_disk,
            parts,
            queued,
            elsewhere,
            queue,
        }
    }

    pub fn on_disk(&self) -> &[LocalEp] {
        &self.on_disk
    }

    fn state(&self, ep: &str) -> Dl {
        if let Some(local) = self.on_disk.iter().find(|e| e.ep == ep) {
            Dl::OnDisk(local.clone())
        } else if self.queued.iter().any(|e| e == ep) {
            Dl::Queued
        } else if self.elsewhere.iter().any(|e| e == ep) {
            Dl::Elsewhere
        } else {
            Dl::Missing
        }
    }

    /// `eps` by what enter can do with them: download, stop, delete (with the file and size).
    fn split(&self, eps: &[String]) -> (Vec<String>, Vec<String>, Vec<LocalEp>) {
        let (mut get, mut stop, mut delete) = (Vec::new(), Vec::new(), Vec::new());
        for ep in eps {
            match self.state(ep) {
                Dl::Missing => get.push(ep.clone()),
                Dl::Queued => stop.push(ep.clone()),
                Dl::OnDisk(local) => delete.push(local),
                Dl::Elsewhere => {}
            }
        }
        (get, stop, delete)
    }

    /// The provider's episodes `listed`, and those it no longer lists that are on disk or
    /// queued, so they can still be played or deleted; by number.
    pub fn episodes(&self, listed: &[String]) -> Vec<String> {
        let mut eps = listed.to_vec();
        let more = (self.on_disk.iter().map(|e| &e.ep)).chain(&self.queued);
        let more: Vec<String> = more.filter(|e| !eps.contains(e)).cloned().collect();
        eps.extend(more);
        eps.sort_by(|a, b| ani::ep_num(a).total_cmp(&ani::ep_num(b)));
        eps.dedup();
        eps
    }

    /// An episode row's hint, with the file's size where `sizes` (Downloads). The one
    /// downloading shows how far it got instead (`row`).
    pub fn hint(&self, ep: &str, sizes: bool) -> String {
        match self.state(ep) {
            Dl::OnDisk(local) if sizes => format!("on disk · {}", megabytes(local.bytes)),
            Dl::OnDisk(_) => "on disk".into(),
            Dl::Queued => "queued".into(),
            Dl::Elsewhere => "queued in another ani-hop".into(),
            Dl::Missing => String::new(),
        }
    }

    /// The episode of this show downloading or held, and what its row says: "↓ 18% · 2m
    /// left", "‖ paused 45%".
    pub fn row(&self) -> Option<(String, String)> {
        let n = self.queue.now.as_ref().filter(|n| n.id == self.show.id)?;
        let text = match self.queue.paused {
            true => paused(n),
            false => format!("↓ {}", n.text),
        };
        Some((n.ep.clone(), text))
    }

    /// The show page's "3 queued · 2 on disk".
    pub fn summary(&self) -> String {
        let mut state = Vec::new();
        if !self.queued.is_empty() {
            let how = if self.queue.paused {
                "paused"
            } else {
                "queued"
            };
            state.push(format!("{} {how}", self.queued.len()));
        }
        if !self.on_disk.is_empty() {
            state.push(format!("{} on disk", self.on_disk.len()));
        }
        state.join(" · ")
    }

    /// Whether episode `ep` is the one downloading, not paused.
    pub fn downloading(&self, ep: &str) -> bool {
        let now = self.queue.now.as_ref();
        !self.queue.paused && now.is_some_and(|n| n.id == self.show.id && n.ep == ep)
    }

    /// The episode downloading, or held, when ffmpeg has it, which can't go on from what it
    /// got: "Episode 3", or "Show E3" when it's another show's.
    fn restarts(&self) -> Option<String> {
        let n = self.queue.now.as_ref().filter(|n| !n.resumes)?;
        Some(match n.id == self.show.id {
            true => format!("Episode {}", n.ep),
            false => n.label.clone(),
        })
    }

    /// Whether enter on `stop` (queued episodes) is about pausing or resuming downloads: on
    /// the one downloading, or with downloads paused, on any queued one.
    fn holds(&self, stop: &[String]) -> bool {
        match self.queue.paused {
            true => !stop.is_empty(),
            false => (self.queue.now.as_ref())
                .is_some_and(|n| n.id == self.show.id && stop.contains(&n.ep)),
        }
    }

    /// What enter does with `eps`, or with the choice `chosen` made for them ("download",
    /// "stop", "delete", "pause", "resume").
    pub fn action(&self, eps: &[String], chosen: Option<&str>) -> Action {
        let (get, stop, delete) = self.split(eps);
        let hold = self.holds(&stop);
        let pick = match (chosen, get.is_empty(), stop.is_empty(), delete.is_empty()) {
            (Some(a), ..) => a,
            (None, ..) if hold => return Action::Choose,
            (None, false, true, true) => "download",
            (None, true, false, true) => "stop",
            (None, true, true, false) => "delete",
            (None, true, true, true) if eps.len() == 1 => {
                return Action::Nothing("Another ani-hop has it queued");
            }
            (None, true, true, true) => return Action::Nothing("Another ani-hop has them queued"),
            (None, ..) => return Action::Choose,
        };
        // a choice may be older than the states it was about
        match pick {
            "download" if !get.is_empty() => Action::Download(get),
            "stop" if !stop.is_empty() => Action::Stop(stop),
            "delete" if !delete.is_empty() => Action::Delete(
                delete
                    .into_iter()
                    .map(|local| (local.ep, local.path))
                    .collect(),
            ),
            "pause" if hold && !self.queue.paused => match &self.queue.now {
                Some(n) => Action::Pause(n.ep.clone(), self.restarts()),
                None => Action::Nothing(""),
            },
            "resume" if self.queue.paused => Action::Resume,
            _ => Action::Nothing(""),
        }
    }

    /// What `^o` (cast) does with episode `ep`. Queued states win over `on_disk` because a
    /// downloading episode's `.mp4` can land before its `.vtt` subtitles finish.
    pub fn cast(&self, ep: &str) -> CastAction {
        if self.elsewhere.iter().any(|e| e == ep) {
            CastAction::Elsewhere
        } else if self.queued.iter().any(|e| e == ep) {
            CastAction::Wait {
                paused: self.queue.paused,
            }
        } else if self.on_disk.iter().any(|e| e.ep == ep) {
            CastAction::CastNow
        } else {
            CastAction::DownloadThenCast
        }
    }

    /// What enter does with `eps`, for the footer: none when nothing.
    pub fn verb(&self, eps: &[String]) -> Option<&'static str> {
        Some(match self.action(eps, None) {
            Action::Nothing(_) => return None,
            Action::Choose if self.holds(&self.split(eps).1) => match self.queue.paused {
                true => "resume…",
                false => "pause…",
            },
            Action::Choose => "choose",
            Action::Download(_) => "download",
            Action::Stop(_) => "stop",
            Action::Delete(_) => "delete",
            Action::Pause(..) => "pause",
            Action::Resume => "resume",
        })
    }

    /// The choices for `eps` when enter asks which, keyed as `action` takes them.
    pub fn choices(&self, eps: &[String]) -> Vec<Item> {
        let (get, stop, delete) = self.split(eps);
        let mut v = Vec::new();
        // under the cursor, so → twice only pauses, and → twice again resumes
        if self.holds(&stop) {
            let over = self.restarts().map_or("", |_| "from the start");
            v.push(match self.queue.paused {
                true => Item::new("resume", "▶  Resume downloads", over),
                false => Item::new("pause", "‖  Pause downloads", ""),
            });
        }
        if !get.is_empty() {
            v.push(Item::new(
                "download",
                format!("↓  Download {}", which(&get)),
                "",
            ));
        }
        if !stop.is_empty() {
            // the title says which when it's one
            let which = match eps {
                [_] => String::new(),
                _ => format!(" {}", which(&stop)),
            };
            let label = format!("■  Stop downloading{which}");
            v.push(Item::new(
                "stop",
                label,
                deletes_hint(self.deletes_bytes(&stop)),
            ));
        }
        if !delete.is_empty() {
            let bytes: u64 = delete.iter().map(|local| local.bytes).sum();
            let names: Vec<String> = delete.into_iter().map(|local| local.ep).collect();
            v.push(Item::new(
                "delete",
                format!("✗  Delete {}", which(&names)),
                megabytes(bytes),
            ));
        }
        v
    }

    /// Those of `eps` still queued: what finished or was stopped meanwhile stays as it is.
    pub fn still_queued(&self, eps: &[String]) -> Vec<String> {
        self.split(eps).1
    }

    /// Total bytes of unfinished partial files that stopping `eps` deletes, or None if none
    /// exist on disk (as of the last scan).
    pub fn deletes_bytes(&self, eps: &[String]) -> Option<u64> {
        let mut found = false;
        let mut total = 0u64;
        for (ep, bytes) in &self.parts {
            if eps.iter().any(|e| e == ep) {
                found = true;
                total = total.saturating_add(*bytes);
            }
        }
        found.then_some(total)
    }
}

/// "episode 3", or "4 episodes".
pub fn which(eps: &[String]) -> String {
    match eps {
        [one] => format!("episode {one}"),
        _ => plural(eps.len(), "episode"),
    }
}

/// What stopping deletes, as "deletes 230 MB", or nothing when nothing's there.
pub fn deletes_hint(bytes: Option<u64>) -> String {
    match bytes {
        None => String::new(),
        Some(b) => format!("deletes {}", megabytes(b)),
    }
}

/// A held download's row: how far it got, without the stale speed of its last line, when
/// that's kept (yt-dlp; ffmpeg starts over).
pub fn paused(n: &Now) -> String {
    let at = got_so_far(&n.text).filter(|_| n.resumes);
    at.map_or("‖ paused".into(), |p| format!("‖ paused {p}"))
}

/// How far a download got, from its last progress line ("45% · 2m left", "93%").
fn got_so_far(text: &str) -> Option<String> {
    let pct: f64 = text
        .split_whitespace()
        .next()?
        .strip_suffix('%')?
        .parse()
        .ok()?;
    Some(format!("{}%", pct as u32))
}

/// Size of files on disk, as "910 MB".
fn megabytes(bytes: u64) -> String {
    format!("{} MB", bytes >> 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|e| e.to_string()).collect()
    }

    fn show() -> Media {
        Media {
            id: 1,
            ..Default::default()
        }
    }

    /// Episode 1 on disk (100 MB), 2 queued here (with 50 MB partial), 3 in another ani-hop,
    /// 4 missing; the queue on `now`.
    fn standing<'a>(m: &'a Media, now: Option<(&str, bool)>, paused: bool) -> ShowDownloads<'a> {
        let now = now.map(|(ep, resumes)| Now {
            id: 1,
            ep: ep.into(),
            label: format!("Show E{ep}"),
            text: "45% · 2m left".into(),
            resumes,
        });
        let queue = Snapshot {
            now,
            pending: 1,
            paused,
        };
        let disk = vec![LocalEp {
            ep: "1".into(),
            path: PathBuf::from("Show Episode 1.mp4"),
            bytes: 100 << 20,
        }];
        let parts = vec![("2".into(), 50 << 20)];
        ShowDownloads::of(m, disk, parts, s(&["2"]), s(&["3"]), queue)
    }

    #[test]
    fn enter_does_what_the_picked_episodes_allow() {
        let m = show();
        let dl = standing(&m, None, false);
        let act = |eps: &[&str]| dl.action(&s(eps), None);
        assert_eq!(act(&["4"]), Action::Download(s(&["4"])));
        assert_eq!(act(&["2"]), Action::Stop(s(&["2"])));
        let file = PathBuf::from("Show Episode 1.mp4");
        assert_eq!(act(&["1"]), Action::Delete(vec![("1".into(), file)]));
        assert_eq!(
            act(&["3"]),
            Action::Nothing("Another ani-hop has it queued")
        );
        assert_eq!(act(&["1", "4"]), Action::Choose);
        // a choice from a question older than what it was about does nothing
        assert_eq!(
            dl.action(&s(&["1", "4"]), Some("download")),
            Action::Download(s(&["4"]))
        );
        assert_eq!(dl.action(&s(&["1"]), Some("download")), Action::Nothing(""));
        // the footer says what enter does, and nothing when it does nothing
        let verb = |eps: &[&str]| dl.verb(&s(eps));
        assert_eq!(
            [verb(&["4"]), verb(&["2"]), verb(&["1"]), verb(&["1", "4"])],
            [
                Some("download"),
                Some("stop"),
                Some("delete"),
                Some("choose")
            ]
        );
        assert_eq!(verb(&["3"]), None);
        let choices = dl.choices(&s(&["1", "2", "4"]));
        let keys: Vec<_> = choices.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["download", "stop", "delete"]);
        assert_eq!(choices[1].hint, "deletes 50 MB");
        assert_eq!(choices[2].hint, "100 MB");
        // a finished file wins over a queue entry still naming it
        assert_eq!(dl.episodes(&s(&["2", "3"])), ["1", "2", "3"]);
        assert_eq!(dl.hint("1", false), "on disk");
        assert_eq!(dl.hint("1", true), "on disk · 100 MB");
        assert_eq!(dl.hint("3", true), "queued in another ani-hop");
    }

    #[test]
    fn the_one_downloading_asks_pause_or_stop() {
        let m = show();
        // yt-dlp's, which goes on from what it got
        let dl = standing(&m, Some(("2", true)), false);
        assert_eq!(dl.action(&s(&["2"]), None), Action::Choose);
        assert_eq!(dl.verb(&s(&["2"])), Some("pause…"));
        let keys: Vec<String> = dl.choices(&s(&["2"])).into_iter().map(|i| i.key).collect();
        assert_eq!(keys, ["pause", "stop"]);
        assert_eq!(
            dl.action(&s(&["2"]), Some("pause")),
            Action::Pause("2".into(), None)
        );
        assert!(dl.downloading("2") && !dl.downloading("1"));
        assert_eq!(dl.row(), Some(("2".into(), "↓ 45% · 2m left".into())));
        // ffmpeg's starts over, so pausing asks first
        let dl = standing(&m, Some(("2", false)), false);
        let restarts = Some("Episode 2".into());
        assert_eq!(
            dl.action(&s(&["2"]), Some("pause")),
            Action::Pause("2".into(), restarts)
        );
        // paused: any queued one resumes them, and says how far it got
        let dl = standing(&m, Some(("2", true)), true);
        assert_eq!(dl.verb(&s(&["2"])), Some("resume…"));
        assert_eq!(dl.action(&s(&["2"]), Some("resume")), Action::Resume);
        assert_eq!(dl.row(), Some(("2".into(), "‖ paused 45%".into())));
        assert_eq!(dl.summary(), "1 paused · 1 on disk");
        let ffmpeg = standing(&m, Some(("2", false)), true);
        assert_eq!(ffmpeg.choices(&s(&["2"]))[0].hint, "from the start");
        assert_eq!(ffmpeg.row(), Some(("2".into(), "‖ paused".into())));
    }

    #[test]
    fn casting_agrees_with_show_downloads_and_waits_for_subtitles() {
        let m = show();
        let active = standing(&m, Some(("2", true)), false);
        assert_eq!(active.cast("1"), CastAction::CastNow);
        assert_eq!(active.cast("2"), CastAction::Wait { paused: false });
        assert_eq!(active.cast("3"), CastAction::Elsewhere);
        assert_eq!(active.cast("4"), CastAction::DownloadThenCast);
        let paused = standing(&m, Some(("2", true)), true);
        assert_eq!(paused.cast("2"), CastAction::Wait { paused: true });
        // an .mp4 already on disk while the episode is still queued (fetching .vtt) still waits
        let subs_pending = ShowDownloads::of(
            &m,
            vec![LocalEp {
                ep: "2".into(),
                path: PathBuf::from("Show Episode 2.mp4"),
                bytes: 100 << 20,
            }],
            vec![],
            s(&["2"]),
            vec![],
            Snapshot {
                now: None,
                pending: 1,
                paused: false,
            },
        );
        assert_eq!(subs_pending.cast("2"), CastAction::Wait { paused: false });

        // CastState tracks turns and resolves waits when a scan shows the download finished or failed
        let mut state = CastState::default();
        let (_, t1) = state.begin();
        assert!(state.current(t1));
        state.wait(m.clone(), "2".into());
        assert!(state.after_scan(active.cast("2")).is_none());
        let ready = state.after_scan(CastAction::CastNow);
        assert_eq!(
            ready.map(|(s, ep, ok)| (s.id, ep, ok)),
            Some((1, "2".into(), true))
        );
        assert_eq!(state.waiting(), None);
        // a failed download leaves the queue without landing on disk: cleared without casting
        state.wait(m.clone(), "5".into());
        let failed = state.after_scan(CastAction::DownloadThenCast);
        assert_eq!(
            failed.map(|(s, ep, ok)| (s.id, ep, ok)),
            Some((1, "5".into(), false))
        );
        // a new ^o invalidates the previous turn and clears any wait
        state.wait(m, "6".into());
        let (_, t2) = state.begin();
        assert!(!state.current(t1) && state.current(t2) && state.waiting().is_none());
    }
}
