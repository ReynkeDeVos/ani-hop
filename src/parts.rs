//! Part knowledge: what this run knows of each show's parts, for the provider's episode count
//! it looked them up for, and the played episode whose Mark waits for them. Each lookup has a
//! ticket, so an answer only counts while nothing has made it stale: other counts, a source
//! picked by hand, another login.

use std::collections::HashMap;

use crate::ani::ep_num;
use crate::anilist::Media;
use crate::progress::Parts;
use crate::source::Source;

/// A lookup of a show's parts to run: `Client::parts` with the provider's `last` episode,
/// answered with `ticket`.
pub struct Ask {
    pub last: u32,
    pub ticket: u64,
}

/// What an answer to a lookup changed.
pub enum Answer {
    /// asked for other counts, another source or another login: nothing changed
    Stale,
    /// the parts are known now; with the episode played whose Mark waited for them
    Known(Vec<Media>, Option<(Media, String)>),
    /// they stay unknown until it's asked again; with the episode whose Mark waited
    Failed(Option<(Media, String)>),
}

#[derive(Default)]
pub struct PartKnowledge {
    shows: HashMap<u64, Lookup>,
    /// an episode that was played, whose Mark waits to know which part it's in
    waiting: Option<(Media, String)>,
    tickets: u64,
}

struct Lookup {
    counts: (u32, u32),
    ticket: u64,
    /// None while it's on its way
    parts: Option<Parts>,
}

/// The provider's last whole episode and AniList's count, when the provider has more: what
/// a lookup of `m`'s parts is good for.
fn counts(m: &Media, src: Option<&Source>) -> Option<(u32, u32)> {
    let last = src?.episodes.iter().filter_map(|e| e.parse().ok()).max()?;
    m.episodes.filter(|own| last > *own).map(|own| (last, own))
}

impl PartKnowledge {
    /// A lookup to run for `m`'s parts at `src`, unless one for these counts ran or runs.
    pub fn want(&mut self, m: &Media, src: Option<&Source>) -> Option<Ask> {
        let Some(counts) = counts(m, src) else {
            self.shows.remove(&m.id);
            return None;
        };
        if self.shows.get(&m.id).is_some_and(|l| l.counts == counts) {
            return None;
        }
        self.tickets += 1;
        let ticket = self.tickets;
        let parts = None;
        self.shows.insert(
            m.id,
            Lookup {
                counts,
                ticket,
                parts,
            },
        );
        Some(Ask {
            last: counts.0,
            ticket,
        })
    }

    /// `m`'s parts at `src`, while they're known for its counts; none otherwise.
    pub fn of(&self, m: &Media, src: Option<&Source>) -> &[(u32, Media)] {
        match self.shows.get(&m.id) {
            Some(Lookup {
                counts: c,
                parts: Some(parts),
                ..
            }) if Some(*c) == counts(m, src) => parts,
            _ => &[],
        }
    }

    /// Whether `of` is all of `m`'s parts: there are none to look for, or AniList answered.
    pub fn known(&self, m: &Media, src: Option<&Source>) -> bool {
        let answered =
            |c| matches!(self.shows.get(&m.id), Some(l) if l.counts == c && l.parts.is_some());
        counts(m, src).is_none_or(answered)
    }

    /// Whether AniList's count for episode `ep` is known: past the show's own episodes it
    /// counts toward a part, if there is one, which takes AniList's answer.
    pub fn counted(&self, m: &Media, src: Option<&Source>, ep: &str) -> bool {
        self.known(m, src) || m.episodes.is_some_and(|own| ep_num(ep) <= own as f64)
    }

    /// A lookup of `id`'s parts is on its way: what's next may change with its answer.
    pub fn looking(&self, id: u64) -> bool {
        matches!(self.shows.get(&id), Some(Lookup { parts: None, .. }))
    }

    /// Episode `ep` of `m` was played: its Mark waits for the answer, in place of any other.
    pub fn hold(&mut self, m: Media, ep: String) {
        self.waiting = Some((m, ep));
    }

    /// Takes in the answer to lookup `ticket` of `id`'s parts.
    pub fn answer(&mut self, id: u64, ticket: u64, res: Result<Parts, String>) -> Answer {
        let Some(lookup) = self.shows.get_mut(&id).filter(|l| l.ticket == ticket) else {
            return Answer::Stale;
        };
        let waiting = self.waiting.take_if(|(m, _)| m.id == id);
        match res {
            Ok(parts) => {
                let shows = parts.iter().map(|(_, p)| p.clone()).collect();
                lookup.parts = Some(parts);
                Answer::Known(shows, waiting)
            }
            // asked again when the show or its episodes open
            Err(_) => {
                self.shows.remove(&id);
                Answer::Failed(waiting)
            }
        }
    }

    /// `id`'s source was picked by hand: its parts are looked up again for it.
    pub fn forget_show(&mut self, id: u64) {
        self.shows.remove(&id);
    }

    /// Another login: the parts carry the last one's progress, and its Mark isn't this one's.
    pub fn forget(&mut self) {
        self.shows.clear();
        self.waiting = None;
    }

    /// The ticket of the lookup on its way for `id`, as a test answers it.
    #[cfg(test)]
    pub fn asked(&self, id: u64) -> u64 {
        let lookup = self.shows.get(&id).filter(|l| l.parts.is_none());
        lookup.expect("a lookup on its way").ticket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(id: u64, episodes: u32) -> Media {
        Media {
            id,
            episodes: Some(episodes),
            ..Default::default()
        }
    }

    fn source(episodes: u32) -> Source {
        let eps = (1..=episodes).map(|e| e.to_string()).collect();
        Source::at("show", &["Show".into()], 1, eps)
    }

    fn waited(answer: Answer) -> Option<String> {
        match answer {
            Answer::Known(_, w) | Answer::Failed(w) => w.map(|(_, ep)| ep),
            Answer::Stale => panic!("stale"),
        }
    }

    #[test]
    fn an_earlier_logins_answer_changes_nothing() {
        let mut k = PartKnowledge::default();
        let (m, src) = (show(1, 12), source(22));
        let old = k.want(&m, Some(&src)).unwrap();
        k.forget();
        let new = k.want(&m, Some(&src)).unwrap();
        k.hold(m.clone(), "13".into());
        let failed = k.answer(m.id, old.ticket, Err("offline".into()));
        assert!(matches!(failed, Answer::Stale));
        assert!(k.looking(m.id) && !k.known(&m, Some(&src)));
        let part = show(2, 10);
        let known = k.answer(m.id, new.ticket, Ok(vec![(12, part)]));
        assert_eq!(waited(known).as_deref(), Some("13"));
        assert_eq!(k.of(&m, Some(&src))[0].1.id, 2);
    }

    #[test]
    fn other_counts_ask_again_and_the_first_answer_is_stale() {
        let mut k = PartKnowledge::default();
        let m = show(1, 12);
        let first = k.want(&m, Some(&source(22))).unwrap();
        assert!(k.want(&m, Some(&source(22))).is_none(), "one runs already");
        let second = k.want(&m, Some(&source(24))).unwrap();
        assert_eq!(second.last, 24);
        let stale = k.answer(m.id, first.ticket, Ok(vec![(12, show(2, 10))]));
        assert!(matches!(stale, Answer::Stale));
        k.answer(m.id, second.ticket, Ok(Vec::new()));
        assert!(k.known(&m, Some(&source(24))));
        assert!(
            !k.known(&m, Some(&source(22))),
            "known for 24 episodes only"
        );
        assert!(k.of(&m, Some(&source(22))).is_empty());
    }

    #[test]
    fn a_source_picked_by_hand_makes_the_lookup_running_stale() {
        let mut k = PartKnowledge::default();
        let (m, src) = (show(1, 12), source(22));
        let old = k.want(&m, Some(&src)).unwrap();
        k.forget_show(m.id);
        let new = k.want(&m, Some(&src)).unwrap();
        assert!(matches!(
            k.answer(m.id, old.ticket, Ok(Vec::new())),
            Answer::Stale
        ));
        assert!(matches!(
            k.answer(m.id, new.ticket, Ok(Vec::new())),
            Answer::Known(..)
        ));
    }

    #[test]
    fn a_failed_lookup_lets_the_mark_go_and_is_asked_again() {
        let mut k = PartKnowledge::default();
        let (m, src) = (show(1, 12), source(22));
        let ask = k.want(&m, Some(&src)).unwrap();
        k.hold(m.clone(), "14".into());
        let failed = k.answer(m.id, ask.ticket, Err("offline".into()));
        assert_eq!(waited(failed).as_deref(), Some("14"));
        assert!(!k.looking(m.id) && !k.counted(&m, Some(&src), "14"));
        assert!(k.counted(&m, Some(&src), "12"), "the show's own");
        assert!(k.want(&m, Some(&src)).is_some());
    }

    #[test]
    fn a_show_the_provider_has_no_more_of_has_no_parts_to_wait_for() {
        let mut k = PartKnowledge::default();
        let m = show(1, 12);
        assert!(k.want(&m, Some(&source(12))).is_none());
        assert!(k.want(&m, None).is_none());
        assert!(k.known(&m, Some(&source(12))) && k.counted(&m, None, "13"));
        assert!(!k.looking(m.id));
    }

    #[test]
    fn another_shows_answer_leaves_the_mark_waiting() {
        let mut k = PartKnowledge::default();
        let (a, b, src) = (show(1, 12), show(3, 12), source(22));
        let ask_a = k.want(&a, Some(&src)).unwrap();
        let ask_b = k.want(&b, Some(&src)).unwrap();
        k.hold(a.clone(), "13".into());
        assert_eq!(waited(k.answer(b.id, ask_b.ticket, Ok(Vec::new()))), None);
        assert_eq!(
            waited(k.answer(a.id, ask_a.ticket, Ok(Vec::new()))).as_deref(),
            Some("13")
        );
    }
}
