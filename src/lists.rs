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
    /// an AniList list, all at once
    Whole,
    /// an AniList ranking (MediaSort) or search in pages: its sort, what it searches, the next
    /// page if there is one, and whether it's on its way (the first page may refresh on its
    /// own, without "More…")
    Pages(String, Option<String>, Option<u32>, bool),
}

/// A list on screen: its shows, where they come from, and its cache.
pub struct List {
    pub shows: Vec<Media>,
    feed: Feed,
    cache: Option<PathBuf>,
    /// its refresh is on its way
    refreshing: bool,
}

/// A refresh on its way, and the list changes there were when it was asked for.
pub struct Ask(u64);

/// Bumped on every list change, so refreshes that started before it don't count as fresh.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// A page "More…" asks for: its sort, what it searches, its number.
type Page = (String, Option<String>, u32);

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
    /// The download folder's shows, as its last rescan found them; before the first one,
    /// loading until `scanned` brings them.
    pub fn local(shows: Option<Vec<Media>>) -> List {
        List {
            refreshing: shows.is_none(),
            shows: shows.unwrap_or_default(),
            feed: Feed::Local,
            cache: None,
        }
    }

    /// What a rescan of the download folder found.
    pub fn scanned(&mut self, shows: Vec<Media>) {
        (self.shows, self.refreshing) = (shows, false);
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
        if let Feed::Pages(_, _, n, _) = &mut feed {
            match fresh {
                true => *n = next,
                false => shows.truncate(PER_PAGE),
            }
        }
        let ask = (!fresh).then(|| Ask(changes()));
        let refreshing = ask.is_some();
        let list = List {
            shows,
            feed,
            cache,
            refreshing,
        };
        (list, ask)
    }

    /// Takes a refresh's answer: its shows, cached as they were when asked for, or not as
    /// fresh when the user's list changed meanwhile. A failed one keeps what's shown (the
    /// cache, usable offline) and says why. Only for a list still open: a closed one's late
    /// answer would overwrite what a newer one cached, its pages from More… too.
    pub fn refreshed(&mut self, ask: Ask, res: Result<Vec<Media>, String>) -> Result<(), String> {
        self.refreshing = false;
        let shows = res?;
        if let Feed::Pages(_, _, next, _) = &mut self.feed {
            *next = (shows.len() >= PER_PAGE).then_some(2);
        }
        self.shows = shows;
        if let Some(path) = &self.cache {
            let _ = ani::write_json(path, &(&self.shows, self.more_page()));
            if changes() != ask.0 {
                outdate(path);
            }
        }
        Ok(())
    }

    /// The next page to fetch for "More…", none while one is on its way, or why there's none.
    /// One page at a time; the first one's refresh would drop what comes after it.
    pub fn more(&mut self) -> Result<Option<Page>, &'static str> {
        let Feed::Pages(sort, search, next, fetching) = &mut self.feed else {
            return Ok(None);
        };
        let Some(page) = *next else {
            return Err("No more shows");
        };
        match (*fetching, self.refreshing) {
            (true, _) => Ok(None),
            (false, true) => Err("Still refreshing the first page…"),
            (false, false) => {
                *fetching = true;
                Ok(Some((sort.clone(), search.clone(), page)))
            }
        }
    }

    /// Takes the page "More…" asked for: its shows (rankings shift between requests, so a
    /// show may come round again and is left out) and the first one new, if any. A failed one
    /// adds nothing and can be asked for again. Cached with every page, while the first
    /// page's age stays the cache's, so loading more never keeps that page longer than an
    /// hour.
    pub fn more_came(&mut self, res: Result<Vec<Media>, String>) -> Result<Option<String>, String> {
        let Feed::Pages(_, _, next, fetching) = &mut self.feed else {
            return Ok(None);
        };
        *fetching = false;
        let more = res?;
        *next = next.filter(|_| more.len() >= PER_PAGE).map(|n| n + 1);
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
        Ok(self.shows.get(start).map(|m| m.id.to_string()))
    }

    /// The page "More…" fetches next, for a ranking or search that has one.
    pub fn more_page(&self) -> Option<u32> {
        match self.feed {
            Feed::Pages(_, _, next, _) => next,
            _ => None,
        }
    }

    /// A search's results.
    pub fn is_search(&self) -> bool {
        matches!(self.feed, Feed::Pages(_, Some(_), ..))
    }

    pub fn is_local(&self) -> bool {
        matches!(self.feed, Feed::Local)
    }

    /// A ranking or search whose next page is on its way.
    pub fn fetching(&self) -> bool {
        matches!(self.feed, Feed::Pages(.., true))
    }

    /// Its refresh or its next page is on its way.
    pub fn loading(&self) -> bool {
        self.refreshing || self.fetching()
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
        Feed::Pages("TRENDING_DESC".into(), None, Some(2), false)
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
        assert!(list.loading());
        assert_eq!(list.more(), Err("Still refreshing the first page…"));
        list.refreshed(ask.unwrap(), Ok(shows(0..50))).unwrap();
        assert!(!list.loading());
        let first = written(&path);
        let page = |n| Ok(Some(("TRENDING_DESC".to_string(), None, n)));
        assert_eq!(list.more(), page(2));
        assert_eq!(list.more(), Ok(None), "on its way");
        assert!(list.loading());
        // the ranking shifted: 45 to 49 come round again
        assert_eq!(list.more_came(Ok(shows(45..95))), Ok(Some("50".into())));
        assert_eq!((list.shows.len(), list.more_page()), (95, Some(3)));
        // a page that fails adds nothing and can be asked for again
        assert_eq!(list.more(), page(3));
        assert_eq!(list.more_came(Err("offline".into())), Err("offline".into()));
        assert!(!list.loading() && list.shows.len() == 95);
        assert_eq!(list.more(), page(3));
        assert_eq!(
            written(&path),
            first,
            "the first page's age stays the cache's"
        );
        let (again, ask) = List::open(trending(), Some(path.clone()), first);
        assert!(ask.is_none() && again.shows.len() == 95 && again.more_page() == Some(3));
        // a short page is the last
        assert_eq!(list.more_came(Ok(shows(95..96))), Ok(Some("95".into())));
        assert_eq!(list.more(), Err("No more shows"));
        assert_eq!(list.more_came(Ok(shows(0..3))), Ok(None), "nothing new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_search_goes_on_with_what_it_searches() {
        let feed = Feed::Pages("SEARCH_MATCH".into(), Some("gundam".into()), None, false);
        let (mut list, ask) = List::open(feed, None, SystemTime::now());
        assert!(list.is_search() && list.loading());
        list.refreshed(ask.unwrap(), Ok(shows(0..50))).unwrap();
        let page = ("SEARCH_MATCH".to_string(), Some("gundam".to_string()), 2);
        assert_eq!(list.more(), Ok(Some(page)));
        assert_eq!(list.more_came(Ok(shows(50..54))), Ok(Some("50".into())));
        assert_eq!(list.more(), Err("No more shows"));
        let (ranking, _) = List::open(trending(), None, SystemTime::now());
        assert!(!ranking.is_search());
    }

    #[test]
    fn a_refresh_from_before_a_list_change_isnt_fresh() {
        let dir = temp("changed");
        let path = dir.join("1-CURRENT.json");
        let (mut list, ask) = List::open(Feed::Whole, Some(path.clone()), SystemTime::now());
        // what changed() does besides outdating the real cache
        CHANGES.fetch_add(1, Ordering::Relaxed);
        list.refreshed(ask.unwrap(), Ok(shows(0..3))).unwrap();
        assert_eq!(list.shows.len(), 3, "shown");
        assert_eq!(written(&path), UNIX_EPOCH, "but asked for again next time");
        // fresh when no list changed meanwhile, which other tests do at any time
        let fresh = (0..20).any(|_| {
            // a try that lost its race may have left a fresh cache, which asks for nothing
            let _ = fs::remove_file(&path);
            let (mut list, ask) = List::open(Feed::Whole, Some(path.clone()), SystemTime::now());
            let asked = changes();
            list.refreshed(ask.unwrap(), Ok(shows(0..2))).unwrap();
            changes() == asked && written(&path) > UNIX_EPOCH
        });
        assert!(fresh);
        // a refresh that fails keeps the cache on screen, offline too
        let (mut list, ask) = List::open(
            Feed::Whole,
            Some(path.clone()),
            UNIX_EPOCH + Duration::from_secs(1 << 40),
        );
        assert!(list.loading() && list.shows.len() == 2);
        assert!(list.refreshed(ask.unwrap(), Err("offline".into())).is_err());
        assert!(!list.loading() && list.shows.len() == 2);
        fs::remove_dir_all(&dir).unwrap();
    }
}
