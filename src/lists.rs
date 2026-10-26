//! Lists of shows: the user's AniList lists, searches, rankings in pages, and the download
//! folder. A list with a disk cache shows from it at once and refreshes from AniList in the
//! background; how fresh a cache is lives in its file's modification time.

use crate::ani;
use crate::anilist::{Media, PER_PAGE};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where a list of shows comes from.
pub enum Feed {
    /// the download folder, rescanned on refresh
    Local,
    /// an AniList list or search, all at once
    Whole,
    /// an AniList ranking (MediaSort) in pages: the next page if there is one, and whether
    /// it's on its way (the first page may refresh on its own, without "More…")
    Pages(String, Option<u32>, bool),
}

/// A list on screen: its shows, where they come from, and its cache.
pub struct List {
    pub shows: Vec<Media>,
    feed: Feed,
    cache: Option<PathBuf>,
}

/// A refresh on its way, and the list changes there were when it was asked for.
pub struct Ask(u64);

/// Bumped on every list change, so refreshes that started before it don't count as fresh.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// A cached list: its shows and, for a ranking, the page "More…" fetches next.
type Cached = (Vec<Media>, Option<u32>);

/// The cache file of a list, by key ("<user id>-CURRENT", "<user id>-TRENDING_DESC").
pub fn cache(key: &str) -> PathBuf {
    ani::cache_dir().join("lists").join(format!("{key}.json"))
}

/// List changes so far: something that started before one is older than what's on AniList.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::Relaxed)
}

/// The user's list changed: cached lists are old, so the next visit shows them and
/// refreshes, and refreshes on their way aren't fresh.
pub fn changed() {
    CHANGES.fetch_add(1, Ordering::Relaxed);
    let Ok(dir) = fs::read_dir(ani::cache_dir().join("lists")) else {
        return;
    };
    for e in dir.flatten() {
        outdate(&e.path());
    }
}

fn outdate(path: &PathBuf) {
    if let Ok(f) = fs::File::options().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH);
    }
}

impl List {
    /// The download folder's shows.
    pub fn local(shows: Vec<Media>) -> List {
        List {
            shows,
            feed: Feed::Local,
            cache: None,
        }
    }

    /// A list as its cache has it at `now`, and the refresh to ask for unless that's fresh
    /// enough. A ranking hardly moves in an hour and keeps every page loaded; a list of
    /// yours changes as you watch, so 2 minutes. Past that only the first page shows, which
    /// the refresh replaces.
    pub fn open(mut feed: Feed, cache: Option<PathBuf>, now: SystemTime) -> (List, Option<Ask>) {
        let cached: Option<Cached> = cache.as_ref().and_then(|p| ani::read_json(p));
        let age = (cache.as_ref()).and_then(|p| {
            now.duration_since(fs::metadata(p).ok()?.modified().ok()?)
                .ok()
        });
        let ttl = match feed {
            Feed::Pages(..) => 60 * 60,
            _ => 120,
        };
        let fresh = cached.is_some() && age.is_some_and(|a| a < Duration::from_secs(ttl));
        let (mut shows, next) = cached.unwrap_or_default();
        if let Feed::Pages(_, n, _) = &mut feed {
            match fresh {
                true => *n = next,
                false => shows.truncate(PER_PAGE),
            }
        }
        let ask = (!fresh).then(|| Ask(changes()));
        (List { shows, feed, cache }, ask)
    }

    /// Takes a refresh's shows, and caches them: as they were when asked for, or not as
    /// fresh when the user's list changed meanwhile. Only for a list still open: a closed
    /// one's late answer would overwrite what a newer one cached, its pages from More… too.
    pub fn refreshed(&mut self, ask: Ask, shows: Vec<Media>) {
        if let Feed::Pages(_, next, _) = &mut self.feed {
            *next = (shows.len() >= PER_PAGE).then_some(2);
        }
        self.shows = shows;
        let Some(path) = &self.cache else {
            return;
        };
        let _ = ani::write_json(path, &(&self.shows, self.more_page()));
        if changes() != ask.0 {
            outdate(path);
        }
    }

    /// The next page to fetch for "More…": (its sort, its number), none while one is on its
    /// way, or why there's none. One page at a time; the first one's refresh would drop
    /// what comes after it.
    pub fn more(&mut self, refreshing: bool) -> Result<Option<(String, u32)>, &'static str> {
        let Feed::Pages(sort, next, fetching) = &mut self.feed else {
            return Ok(None);
        };
        let Some(page) = *next else {
            return Err("No more shows");
        };
        match (refreshing, *fetching) {
            (true, true) => Ok(None),
            (true, false) => Err("Still refreshing the first page…"),
            _ => {
                *fetching = true;
                Ok(Some((sort.clone(), page)))
            }
        }
    }

    /// A page "More…" asked for came, or didn't: the next one can be asked for.
    pub fn fetched(&mut self) {
        if let Feed::Pages(_, _, fetching) = &mut self.feed {
            *fetching = false;
        }
    }

    /// Adds the next page's shows; rankings shift between requests, so a show may come round
    /// again and is left out. The first one new, if any. Cached with every page, while the
    /// first page's age stays the cache's, so loading more never keeps that page longer
    /// than an hour.
    pub fn more_came(&mut self, more: Vec<Media>) -> Option<String> {
        if let Feed::Pages(_, next, _) = &mut self.feed {
            *next = next.filter(|_| more.len() >= PER_PAGE).map(|n| n + 1);
        }
        let more: Vec<Media> = (more.into_iter())
            .filter(|m| self.shows.iter().all(|o| o.id != m.id))
            .collect();
        let start = self.shows.len();
        self.shows.extend(more);
        if let Some(path) = &self.cache {
            let fetched = fs::metadata(path).and_then(|m| m.modified());
            let _ = ani::write_json(path, &(&self.shows, self.more_page()));
            if let Ok(t) = fetched {
                let file = fs::File::options().write(true).open(path);
                let _ = file.and_then(|f| f.set_modified(t));
            }
        }
        self.shows.get(start).map(|m| m.id.to_string())
    }

    /// The page "More…" fetches next, for a ranking that has one.
    pub fn more_page(&self) -> Option<u32> {
        match self.feed {
            Feed::Pages(_, next, _) => next,
            _ => None,
        }
    }

    /// A search's results, which have no cache.
    pub fn is_search(&self) -> bool {
        matches!(self.feed, Feed::Whole) && self.cache.is_none()
    }

    pub fn is_local(&self) -> bool {
        matches!(self.feed, Feed::Local)
    }

    /// A ranking whose next page is on its way.
    pub fn fetching(&self) -> bool {
        matches!(self.feed, Feed::Pages(_, _, true))
    }

    pub fn paged(&self) -> bool {
        matches!(self.feed, Feed::Pages(..))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ani-hop-lists-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shows(ids: std::ops::Range<u64>) -> Vec<Media> {
        ids.map(|id| Media {
            id,
            ..Default::default()
        })
        .collect()
    }

    fn trending() -> Feed {
        Feed::Pages("TRENDING_DESC".into(), Some(2), false)
    }

    /// When the cache was last written, as its freshness counts it.
    fn written(path: &PathBuf) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn rankings_keep_their_pages_for_an_hour() {
        let dir = temp("hour");
        let path = dir.join("0-TRENDING_DESC.json");
        ani::write_json(&path, &(shows(0..120), Some(4))).unwrap();
        let at = |min: u64| written(&path) + Duration::from_secs(min * 60);
        let (list, ask) = List::open(trending(), Some(path.clone()), at(59));
        assert!(ask.is_none() && list.shows.len() == 120);
        assert_eq!(list.more_page(), Some(4), "More… goes on with page 4");
        let (list, ask) = List::open(trending(), Some(path.clone()), at(61));
        assert!(
            ask.is_some() && list.shows.len() == 50,
            "past the hour: page 1, refreshed"
        );
        assert_eq!(list.more_page(), Some(2));
        let (list, ask) = List::open(Feed::Whole, Some(path.clone()), at(3));
        assert!(
            ask.is_some() && list.shows.len() == 120,
            "your own lists: 2 minutes, all shown"
        );
        let (list, ask) = List::open(Feed::Whole, None, at(0));
        assert!(ask.is_some() && list.shows.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn more_goes_on_page_by_page_without_repeats() {
        let dir = temp("more");
        let path = dir.join("0-TRENDING_DESC.json");
        let (mut list, ask) = List::open(trending(), Some(path.clone()), SystemTime::now());
        assert_eq!(list.more(true), Err("Still refreshing the first page…"));
        list.refreshed(ask.unwrap(), shows(0..50));
        let first = written(&path);
        assert_eq!(list.more(false), Ok(Some(("TRENDING_DESC".into(), 2))));
        assert_eq!(list.more(true), Ok(None), "on its way");
        list.fetched();
        // the ranking shifted: 45 to 49 come round again
        assert_eq!(list.more_came(shows(45..95)).as_deref(), Some("50"));
        assert_eq!((list.shows.len(), list.more_page()), (95, Some(3)));
        assert_eq!(
            written(&path),
            first,
            "the first page's age stays the cache's"
        );
        let (again, ask) = List::open(trending(), Some(path.clone()), first);
        assert!(ask.is_none() && again.shows.len() == 95 && again.more_page() == Some(3));
        // a short page is the last
        assert_eq!(list.more_came(shows(95..96)).as_deref(), Some("95"));
        assert_eq!(list.more(false), Err("No more shows"));
        assert_eq!(list.more_came(shows(0..3)), None, "nothing new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refresh_from_before_a_list_change_isnt_fresh() {
        let dir = temp("changed");
        let path = dir.join("1-CURRENT.json");
        let (mut list, ask) = List::open(Feed::Whole, Some(path.clone()), SystemTime::now());
        // what changed() does besides outdating the real cache
        CHANGES.fetch_add(1, Ordering::Relaxed);
        list.refreshed(ask.unwrap(), shows(0..3));
        assert_eq!(list.shows.len(), 3, "shown");
        assert_eq!(written(&path), UNIX_EPOCH, "but asked for again next time");
        let (mut list, ask) = List::open(Feed::Whole, Some(path.clone()), SystemTime::now());
        list.refreshed(ask.unwrap(), shows(0..2));
        assert!(written(&path) > UNIX_EPOCH);
        fs::remove_dir_all(&dir).unwrap();
    }
}
