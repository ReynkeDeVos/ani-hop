//! Related shows: what AniList links a show to and what those link to, in the order a list
//! shows them. AniList only has a show's direct links (Attack on Titan Season 3 has Season 2 and
//! Part 2, not Season 1), so reaching the rest takes a request per step, batched.

use crate::anilist::{Media, PER_PAGE};
use std::collections::{HashMap, HashSet};
use std::iter::zip;

/// Enough for the largest series tried: Detective Conan's 127 shows took 8. More would hold up
/// everything else, at the 30 requests a minute AniList can come down to.
const MOST_REQUESTS: usize = 12;

/// A link to another show. Only "PREQUEL" and "SEQUEL" say which comes first.
pub struct Link {
    pub relation: String,
    pub to: u64,
}

/// A show and what it links to.
pub struct Linked {
    pub show: Media,
    pub links: Vec<Link>,
}

/// Every show a show leads to through its links.
pub struct Series {
    /// a sequel after what it follows, otherwise as they came out
    pub shows: Vec<Media>,
    /// the run of prequels and sequels each one is in
    runs: Vec<usize>,
    /// MOST_REQUESTS ran out with links left to follow
    pub cut: bool,
}

/// `seed`'s series, one request per PER_PAGE shows. Links go both ways and in circles, so each
/// show is asked for once.
pub fn walk(
    seed: u64,
    mut fetch: impl FnMut(&[u64]) -> Result<Vec<Linked>, String>,
) -> Result<Series, String> {
    let mut seen = HashSet::from([seed]);
    let mut waiting = vec![seed];
    let mut found = Vec::new();
    let mut asked = 0;
    while !waiting.is_empty() && asked < MOST_REQUESTS {
        let rest = waiting.split_off(waiting.len().min(PER_PAGE));
        let batch = std::mem::replace(&mut waiting, rest);
        asked += 1;
        for linked in fetch(&batch)? {
            let next = linked.links.iter().map(|l| l.to);
            waiting.extend(next.filter(|to| seen.insert(*to)));
            found.push(linked);
        }
    }
    Ok(Series::new(found, !waiting.is_empty()))
}

impl Series {
    fn new(mut found: Vec<Linked>, cut: bool) -> Series {
        let at: HashMap<u64, usize> = (found.iter().enumerate())
            .map(|(i, l)| (l.show.id, i))
            .collect();
        let mut before = vec![HashSet::new(); found.len()];
        let mut run: Vec<usize> = (0..found.len()).collect();
        for (i, linked) in found.iter().enumerate() {
            for link in &linked.links {
                let Some(&j) = at.get(&link.to) else {
                    continue; // one AniList didn't hand out, or the walk stopped before
                };
                let (first, then) = match link.relation.as_str() {
                    "PREQUEL" => (j, i),
                    "SEQUEL" => (i, j),
                    _ => continue,
                };
                before[then].insert(first);
                let (a, b) = (root(&mut run, i), root(&mut run, j));
                run[a] = b;
            }
        }
        let mut left: Vec<usize> = (0..found.len()).collect();
        left.sort_by_key(|&i| released(&found[i].show));
        let mut placed = vec![false; found.len()];
        let mut shows = Vec::with_capacity(found.len());
        let mut runs = Vec::with_capacity(found.len());
        while !left.is_empty() {
            // where AniList's links go in a circle, the earliest goes first anyway
            let free = |&i: &usize| before[i].iter().all(|&b| placed[b]);
            let i = left.remove(left.iter().position(free).unwrap_or(0));
            placed[i] = true;
            shows.push(std::mem::take(&mut found[i].show));
            runs.push(root(&mut run, i));
        }
        Series { shows, runs, cut }
    }

    /// The shows as `seed`'s list has them, and how many of the first are its seasons: the run
    /// it starts with, in story order. The rest come as they came out; with no such run there
    /// are no seasons to set apart, and all of them do.
    pub fn list(&self, seed: u64) -> (Vec<Media>, usize) {
        let main = self.main_run(seed);
        let (seasons, mut also): (Vec<_>, Vec<_>) =
            zip(&self.shows, &self.runs).partition(|(_, r)| Some(**r) == main);
        also.sort_by_key(|(m, _)| released(m));
        let count = seasons.len();
        let shows = seasons.into_iter().chain(also).map(|(m, _)| m.clone());
        (shows.collect(), count)
    }

    /// The run a list starts with: `seed`'s, unless that's a movie or a special on its own,
    /// whose list starts with the show it belongs to, the run with the most on TV.
    fn main_run(&self, seed: u64) -> Option<usize> {
        let (own, run) = zip(&self.shows, &self.runs).find(|(m, _)| m.id == seed)?;
        let size = self.runs.iter().filter(|r| *r == run).count();
        if size > 1 || on_tv(own) {
            return Some(*run);
        }
        let tv = |run: usize| {
            zip(&self.shows, &self.runs)
                .filter(|(m, r)| **r == run && on_tv(m))
                .count()
        };
        // the last of the largest: reversed for the earliest
        let most = self.runs.iter().rev().max_by_key(|r| tv(**r))?;
        (tv(*most) > 0).then_some(*most)
    }
}

/// The first of the shows `run` has `i` run on from.
fn root(run: &mut [usize], mut i: usize) -> usize {
    while run[i] != i {
        run[i] = run[run[i]];
        i = run[i];
    }
    i
}

fn on_tv(m: &Media) -> bool {
    matches!(m.format.as_deref(), Some("TV" | "TV_SHORT"))
}

/// For sorting by when a show came out: without a date, it's yet to come.
fn released(m: &Media) -> (u32, u8, u64) {
    let season = match m.season.as_deref() {
        Some("WINTER") => 0,
        Some("SPRING") => 1,
        Some("SUMMER") => 2,
        Some("FALL") => 3,
        _ => 4,
    };
    (m.season_year.unwrap_or(u32::MAX), season, m.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A show out in `year`, with its links as (relation, id).
    fn show(id: u64, format: &str, year: u32, links: &[(&str, u64)]) -> Linked {
        Linked {
            show: Media {
                id,
                format: Some(format.into()),
                season_year: (year > 0).then_some(year),
                ..Default::default()
            },
            links: (links.iter())
                .map(|(relation, to)| Link {
                    relation: relation.to_string(),
                    to: *to,
                })
                .collect(),
        }
    }

    /// The series of `seed` in a world of `shows`, with the batches asked for.
    fn series(seed: u64, shows: impl Fn() -> Vec<Linked>) -> (Series, Vec<Vec<u64>>) {
        let mut asked = Vec::new();
        let walked = walk(seed, |ids| {
            asked.push(ids.to_vec());
            Ok((shows().into_iter())
                .filter(|l| ids.contains(&l.show.id))
                .collect())
        });
        (walked.unwrap(), asked)
    }

    fn ids(shows: &[Media]) -> Vec<u64> {
        shows.iter().map(|m| m.id).collect()
    }

    /// Attack on Titan, more or less: three seasons, a recap movie, and a side story.
    fn titan() -> Vec<Linked> {
        vec![
            show(1, "TV", 2013, &[("SEQUEL", 2), ("SUMMARY", 9)]),
            show(2, "TV", 2017, &[("PREQUEL", 1), ("SEQUEL", 3)]),
            show(3, "TV", 2018, &[("PREQUEL", 2), ("SIDE_STORY", 8)]),
            show(8, "OVA", 2014, &[("PARENT", 3)]),
            show(9, "MOVIE", 2020, &[("PARENT", 1)]),
        ]
    }

    #[test]
    fn a_later_season_reaches_the_first_and_each_show_is_asked_for_once() {
        let (s, asked) = series(3, titan);
        assert_eq!(asked, [vec![3], vec![2, 8], vec![1], vec![9]]);
        assert!(!s.cut);
        let (shows, seasons) = s.list(3);
        assert_eq!(ids(&shows), [1, 2, 3, 8, 9]);
        assert_eq!(seasons, 3);
    }

    #[test]
    fn story_order_beats_release_order_and_the_rest_come_as_they_came_out() {
        // Monogatari: Kizu came out years after Bake, and happens before it
        let world = || {
            vec![
                show(1, "TV", 2009, &[("PREQUEL", 2), ("SEQUEL", 3)]),
                show(2, "MOVIE", 2016, &[("SEQUEL", 1)]),
                show(3, "TV", 2012, &[("PREQUEL", 1), ("SIDE_STORY", 5)]),
                show(4, "SPECIAL", 2013, &[("PARENT", 1)]),
                show(5, "OVA", 2011, &[("PARENT", 3), ("ALTERNATIVE", 4)]),
            ]
        };
        let (shows, seasons) = series(1, world).0.list(1);
        assert_eq!((ids(&shows), seasons), (vec![2, 1, 3, 5, 4], 3));
    }

    #[test]
    fn links_that_go_in_a_circle_still_end() {
        let world = || {
            vec![
                show(1, "TV", 2001, &[("SEQUEL", 2), ("PREQUEL", 3)]),
                show(2, "TV", 2002, &[("SEQUEL", 3)]),
                show(3, "TV", 2003, &[("SEQUEL", 1), ("SEQUEL", 3)]),
            ]
        };
        let (shows, seasons) = series(2, world).0.list(2);
        assert_eq!((ids(&shows).len(), seasons), (3, 3));
    }

    #[test]
    fn a_movie_on_its_own_lists_the_show_it_belongs_to_first() {
        // Cowboy Bebop has no seasons: its movie's list starts with the show
        let world = || {
            vec![
                show(1, "TV", 1998, &[("SIDE_STORY", 2), ("SIDE_STORY", 3)]),
                show(2, "MOVIE", 2001, &[("PARENT", 1)]),
                show(3, "SPECIAL", 1999, &[("PARENT", 1)]),
            ]
        };
        let (shows, seasons) = series(2, world).0.list(2);
        assert_eq!((ids(&shows), seasons), (vec![1, 3, 2], 1));
        // a TV show is its own run, however alone
        let (shows, seasons) = series(1, world).0.list(1);
        assert_eq!((ids(&shows), seasons), (vec![1, 3, 2], 1));
        // and with nothing on TV none are set apart
        let world = || {
            vec![
                show(2, "MOVIE", 2001, &[("ALTERNATIVE", 3)]),
                show(3, "OVA", 1999, &[("ALTERNATIVE", 2)]),
            ]
        };
        let (shows, seasons) = series(2, world).0.list(2);
        assert_eq!((ids(&shows), seasons), (vec![3, 2], 0));
    }

    #[test]
    fn a_run_stays_together_whichever_of_its_shows_a_list_starts_from() {
        let (s, _) = series(9, titan);
        assert_eq!(ids(&s.list(2).0), [1, 2, 3, 8, 9]);
        // a side story's list: its show's seasons first, and it among the rest
        let (shows, seasons) = s.list(8);
        assert_eq!((ids(&shows), seasons), (vec![1, 2, 3, 8, 9], 3));
    }

    #[test]
    fn what_comes_next_without_a_date_is_last() {
        // Demon Slayer's last movies have no date yet
        let world = || {
            vec![
                show(
                    1,
                    "TV",
                    2019,
                    &[("SEQUEL", 2), ("SIDE_STORY", 4), ("SIDE_STORY", 5)],
                ),
                show(2, "MOVIE", 0, &[("PREQUEL", 1), ("SEQUEL", 3)]),
                show(3, "MOVIE", 0, &[("PREQUEL", 2)]),
                show(4, "OVA", 0, &[("PARENT", 1)]),
                show(5, "OVA", 2021, &[("PARENT", 1)]),
            ]
        };
        let (shows, seasons) = series(1, world).0.list(1);
        assert_eq!((ids(&shows), seasons), (vec![1, 2, 3, 5, 4], 3));
    }

    #[test]
    fn a_long_series_comes_in_batches_and_says_when_it_stopped() {
        // 50 at a time, as AniList hands them out: 12 requests reach 1 + 11 x 50 shows
        let world = || {
            let links: Vec<(&str, u64)> = (2..2000).map(|to| ("SIDE_STORY", to)).collect();
            let mut all = vec![show(1, "TV", 2000, &links)];
            all.extend((2..2000).map(|id| show(id, "OVA", 2001, &[("PARENT", 1)])));
            all
        };
        let (s, asked) = series(1, world);
        assert_eq!(asked.len(), MOST_REQUESTS);
        assert!(asked.iter().all(|b| b.len() <= PER_PAGE));
        assert!(s.cut);
        assert_eq!(s.shows.len(), 1 + 11 * PER_PAGE);
        // a failed request is no half a series
        let failed = walk(1, |_| Err("Can't reach AniList".to_string()));
        assert_eq!(failed.err().as_deref(), Some("Can't reach AniList"));
    }
}
