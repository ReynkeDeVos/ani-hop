//! Episode progress: which AniList entry an episode of a show counts toward, whether it's
//! watched, what comes next, and what marking it watched saves. ani-cli can list a season
//! AniList splits in two as one show, so its episodes run on past the show's own into
//! sequels, its parts (Attack on Titan Season 3: 22 at ani-cli, 12 and 10 on AniList).

use crate::ani::ep_num;
use crate::anilist::{Entry, Media};
use crate::entries::Entries;

/// The AniList sequels a show's provider entry runs on into, each with the episodes before it.
pub type Parts = Vec<(u32, Media)>;

/// A show's episodes as AniList counts them: its own entry, then its parts.
#[derive(Clone, Copy)]
pub struct Progress<'a> {
    show: &'a Media,
    parts: &'a [(u32, Media)],
    /// the newest Entries this session knows, over the ones the shows came with
    entries: Option<&'a Entries>,
}

/// What marking an episode watched saves, as the question shows it and Yes saves it.
#[derive(Clone)]
pub struct Mark {
    /// the entry it saves to: the show's, or a part's past the show's own episodes
    pub entry: Media,
    /// what to call that part ("Part 2"), empty for the show's own
    pub part: String,
    /// AniList's progress there as shown, and the progress it saves
    pub was: u32,
    pub to: u32,
    /// the list the entry is on afterwards
    pub status: &'static str,
}

impl<'a> Progress<'a> {
    /// With the Entries `show` and `parts` came with.
    pub fn of(show: &'a Media, parts: &'a [(u32, Media)]) -> Self {
        Progress {
            show,
            parts,
            entries: None,
        }
    }

    /// With the newest Entries `entries` knows.
    pub fn with(show: &'a Media, parts: &'a [(u32, Media)], entries: &'a Entries) -> Self {
        Progress {
            show,
            parts,
            entries: Some(entries),
        }
    }

    fn known(&self, m: &'a Media) -> Option<&'a Entry> {
        match self.entries {
            Some(all) => all.entry(m),
            None => m.media_list_entry.as_ref(),
        }
    }

    /// How many of `m`'s episodes AniList counts as watched.
    fn count(&self, m: &'a Media) -> u32 {
        self.known(m).map_or(0, |e| e.progress)
    }

    /// Up to its progress everything counts as watched.
    fn watched_in(&self, m: &'a Media, ep: &str) -> bool {
        let n = self.count(m);
        n > 0 && ep_num(ep) <= n as f64
    }

    /// The parts, each with the episodes before it.
    pub fn parts(&self) -> &'a [(u32, Media)] {
        self.parts
    }

    /// The entry episode `ep` counts toward, and its number there.
    pub fn entry(&self, ep: &str) -> (&'a Media, String) {
        let n = ep_num(ep);
        // a recap 12.5 comes after 12, so it's the 12-episode part's
        match (self.parts.iter().rev()).find(|(before, _)| n.floor() > *before as f64) {
            Some((before, p)) => (p, (n - *before as f64).to_string()),
            None => (self.show, ep.to_string()),
        }
    }

    /// What sets `part`'s name apart from its show's: "Part 2" of "Attack on Titan Season 3
    /// Part 2", or all of it.
    pub fn part_name<'b>(&self, part: &'b Media) -> &'b str {
        let rest = (part.name().strip_prefix(self.show.name()))
            .map(|r| r.trim_start_matches([' ', ':', '-']));
        rest.filter(|r| !r.is_empty()).unwrap_or(part.name())
    }

    /// The part episode `ep` is in, by name; empty for the show's own.
    pub fn group(&self, ep: &str) -> &'a str {
        match self.entry(ep).0 {
            p if p.id == self.show.id => "",
            p => self.part_name(p),
        }
    }

    /// Up to the show's progress (which ran past its episodes before parts counted) or its
    /// part's.
    pub fn watched(&self, ep: &str) -> bool {
        let (p, n) = self.entry(ep);
        self.watched_in(self.show, ep) || self.watched_in(p, &n)
    }

    /// The episode after the last one watched. It goes on into a part once everything before
    /// it is, so rewatching the first part doesn't skip to the end of a finished second one.
    pub fn next(&self) -> u32 {
        let mut next = self.count(self.show) + 1;
        for (before, p) in self.parts {
            if next > *before {
                next = next.max(before + self.count(p) + 1);
            }
        }
        next
    }

    /// Episodes out right now, the parts' included (None: unknown).
    pub fn released(&self) -> Option<u32> {
        match self.parts.last() {
            Some((before, p)) => p.released().map(|r| before + r),
            None => self.show.released(),
        }
    }

    /// What marking episode `ep` watched saves: never past its entry's episodes, which AniList
    /// would keep as progress like 13/12. An episode after them (the second half of a special
    /// the provider splits in two, an extra, one of a part not looked up yet) saves the
    /// entry's last. None for one that isn't a number.
    pub fn mark(&self, ep: &str) -> Option<Mark> {
        ep.parse::<f64>().ok()?;
        let (p, n) = self.entry(ep);
        let n = ep_num(&n);
        let to = p.episodes.map_or(n, |e| n.min(e as f64)) as u32;
        let part = match p.id == self.show.id {
            true => String::new(),
            false => self.part_name(p).to_string(),
        };
        let now = self.known(p);
        let mut entry = p.clone();
        entry.media_list_entry = now.cloned();
        Some(Mark {
            entry,
            part,
            was: self.count(p),
            to,
            status: status(now, p.episodes, to),
        })
    }

    /// Marking `ep` watched, when that adds anything: asked after playing it.
    pub fn unwatched(&self, ep: &str) -> Option<Mark> {
        self.mark(ep).filter(|k| k.to > k.was && !self.watched(ep))
    }
}

impl Mark {
    /// Whether it changes no other episode, so Yes can go first.
    pub fn only_this(&self) -> bool {
        recounts(self.was, self.to).is_none()
    }

    /// What else Yes changes, said in the question as AniList keeps one count per show:
    /// "Unmarks 6–12 and moves it to Watching.", or nothing.
    pub fn also(&self) -> String {
        let also = [recounts(self.was, self.to), self.list_move()];
        let also = also.into_iter().flatten().collect::<Vec<_>>().join(" and ");
        match also.split_at_checked(1) {
            Some((first, rest)) => format!("{}{rest}.", first.to_uppercase()),
            None => also,
        }
    }

    /// What Yes saves: "saves 3/12", "saves 1/10 to Part 2".
    pub fn hint(&self) -> String {
        let total = self.entry.episodes.map_or("?".into(), |t| t.to_string());
        match self.part.as_str() {
            "" => format!("saves {}/{total}", self.to),
            part => format!("saves {}/{total} to {part}", self.to),
        }
    }

    /// The list the entry moves to, if it does.
    fn list_move(&self) -> Option<String> {
        let to = crate::list_label(self.status);
        match self
            .entry
            .media_list_entry
            .as_ref()
            .map(|e| e.status.as_str())
        {
            Some(s) if s == self.status => None,
            Some(_) => Some(format!("moves it to {to}")),
            None => Some(format!("adds it to {to}")),
        }
    }
}

/// The list an entry is on once its progress is `to` of `episodes`.
fn status(entry: Option<&Entry>, episodes: Option<u32>, to: u32) -> &'static str {
    match entry.map(|e| e.status.as_str()) {
        _ if episodes == Some(to) => "COMPLETED",
        Some("REPEATING") => "REPEATING",
        _ => "CURRENT",
    }
}

/// What saving progress `to` over `from` does to the episodes between, since AniList counts
/// every one up to the progress as watched.
fn recounts(from: u32, to: u32) -> Option<String> {
    let span = |a: u32, b: u32| match a == b {
        true => a.to_string(),
        false => format!("{a}–{b}"),
    };
    match to {
        _ if to > from + 1 => Some(format!("also marks {}", span(from + 1, to - 1))),
        _ if to < from => Some(format!("unmarks {}", span(to + 1, from))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anilist::Entry;

    fn show(id: u64, episodes: u32, progress: Option<(u32, &str)>) -> Media {
        Media {
            id,
            episodes: Some(episodes),
            media_list_entry: progress.map(|(progress, status)| Entry {
                id,
                status: status.into(),
                progress,
            }),
            ..Default::default()
        }
    }

    fn watching(id: u64, episodes: u32, progress: u32) -> Media {
        show(id, episodes, Some((progress, "CURRENT")))
    }

    #[test]
    fn marking_an_episode_says_what_else_it_changes() {
        let mark = |m: &Media, ep| Progress::of(m, &[]).mark(ep).unwrap();
        let fresh = show(1, 12, None);
        let k = mark(&fresh, "1");
        assert_eq!((k.was, k.to, k.status), (0, 1, "CURRENT"));
        assert_eq!(k.also(), "Adds it to Watching.");
        assert!(k.only_this());
        let k = mark(&watching(1, 12, 5), "6");
        assert_eq!((k.also(), k.hint()), (String::new(), "saves 6/12".into()));
        // the last one completes it, a rewatch stays one
        assert_eq!(mark(&watching(1, 12, 11), "12").status, "COMPLETED");
        let again = show(1, 12, Some((2, "REPEATING")));
        assert_eq!(mark(&again, "3").status, "REPEATING");
        assert_eq!(
            mark(&show(1, 12, Some((2, "PAUSED"))), "3").status,
            "CURRENT"
        );
        // a jump says what AniList counts along with it, both ways
        let k = mark(&show(1, 1200, None), "1180");
        assert_eq!(k.also(), "Also marks 1–1179 and adds it to Watching.");
        assert!(!k.only_this());
        assert_eq!(mark(&watching(1, 12, 12), "5").also(), "Unmarks 6–12.");
        assert_eq!(mark(&watching(1, 12, 6), "5").also(), "Unmarks 6.");
        assert_eq!(mark(&watching(1, 12, 5), "7").also(), "Also marks 6.");
        assert!(Progress::of(&fresh, &[]).mark("SP").is_none());
    }

    #[test]
    fn episodes_past_a_split_season_count_toward_its_part() {
        // Attack on Titan Season 3 at ani-cli: 22 episodes, 12 and 10 on AniList
        let mut s3 = watching(1, 12, 12);
        s3.title.romaji = Some("Attack on Titan Season 3".into());
        let mut p2 = watching(2, 10, 3);
        p2.title.romaji = Some("Attack on Titan Season 3 Part 2".into());
        let parts = [(12, p2)];
        let run = Progress::of(&s3, &parts);
        let at = |ep| {
            let (m, n) = run.entry(ep);
            (m.id, n)
        };
        assert_eq!(at("5"), (1, "5".into()));
        assert_eq!(at("12.5"), (1, "12.5".into()), "a recap stays where it was");
        assert_eq!(at("13"), (2, "1".into()));
        assert_eq!(at("22"), (2, "10".into()));
        assert_eq!((run.group("12"), run.group("13")), ("", "Part 2"));
        assert!(run.watched("15") && !run.watched("16"));
        assert_eq!(Progress::of(&s3, &[]).entry("13").1, "13", "no parts known");
        // nothing saves past an entry's episodes: after them it's the entry's last
        let save = |parts: &[(u32, Media)], ep| {
            (Progress::of(&s3, parts).mark(ep)).map(|k| (k.entry.id, k.to, k.part))
        };
        assert_eq!(save(&parts, "13"), Some((2, 1, "Part 2".into())));
        assert_eq!(save(&parts, "12.5"), Some((1, 12, String::new())));
        assert_eq!(save(&[], "13"), Some((1, 12, String::new())), "not 13/12");
        assert_eq!(
            save(&parts, "23"),
            Some((2, 10, "Part 2".into())),
            "Part 2's last"
        );
        assert_eq!(run.mark("16").unwrap().hint(), "saves 4/10 to Part 2");
        // asked after playing only when it adds anything
        assert!(run.unwatched("15").is_none() && run.unwatched("12.5").is_none());
        assert_eq!(run.unwatched("16").map(|k| k.to), Some(4));
        // a one-episode special ani-cli has in two halves
        let special = watching(3, 1, 0);
        assert_eq!(
            Progress::of(&special, &[]).unwatched("2").map(|k| k.to),
            Some(1)
        );
        let airing = Media::default();
        assert!(
            Progress::of(&airing, &[]).mark("13").is_some(),
            "no count yet"
        );
        let next = |own, second| {
            let parts = [(12, watching(2, 10, second))];
            Progress::of(&watching(1, 12, own), &parts).next()
        };
        assert_eq!(next(12, 3), 16);
        assert_eq!(next(12, 0), 13);
        assert_eq!(next(3, 10), 4, "rewatching the first part");
        assert_eq!(
            next(22, 0),
            23,
            "progress saved past 12 before parts counted"
        );
        assert_eq!(next(12, 10), 23);
        let short = |name: &str| {
            let mut p = Media::default();
            p.title.romaji = Some(name.into());
            run.part_name(&p).to_string()
        };
        assert_eq!(
            short("Attack on Titan Season 3"),
            "Attack on Titan Season 3"
        );
        assert_eq!(short("The Final Season"), "The Final Season");
    }
}
