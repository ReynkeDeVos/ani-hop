//! Where a show is at ani-cli's provider: searching it, matching AniList's titles to the
//! provider's, the sources picked by hand, and the ones known from earlier sessions.
//!
//! ani-cli has no machine-readable output, so we read its menus instead: with stdin not
//! being a tty it switches to rofi, and we put a stand-in `rofi` first in its PATH that
//! writes the menu to a file. That way the provider scraping stays ani-cli's business and
//! keeps working after `ani-cli -U`.

use crate::ani;
use crate::anilist::Media;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where an AniList entry lives on the provider, as ani-cli sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub query: String,
    pub index: usize,
    pub title: String,
    /// which of the entries with this title it is (1: the first); 0 if saved before 0.6.1
    #[serde(default)]
    pub nth: usize,
    pub episodes: Vec<String>,
}

impl Source {
    /// Entry `index` of `hits`, counting which of the entries with its title it is.
    pub fn at(query: &str, hits: &[String], index: usize, episodes: Vec<String>) -> Source {
        let title = hits[index - 1].clone();
        let nth = hits[..index].iter().filter(|h| **h == title).count();
        Source {
            query: query.into(),
            index,
            title,
            nth,
            episodes,
        }
    }
}

/// How to get a source that works right now. One checked this session is ready; a cached one
/// gets one search, as `-S` is positional and the provider's order shifts when new seasons
/// show up; without one the show is looked up.
#[derive(Clone, Default)]
pub enum Lookup {
    Ready(Source),
    Check(Source),
    #[default]
    Resolve,
}

impl Lookup {
    /// Blocks on ani-cli.
    pub fn run(self, m: &Media) -> Result<Source, String> {
        match self {
            Lookup::Ready(s) => Ok(s),
            Lookup::Check(s) => locate(m, &s),
            Lookup::Resolve => resolve(m),
        }
    }
}

/// The sources known for shows: from earlier sessions, so menus and playback don't wait for
/// ani-cli, or checked in this one; and the lookups running for them.
pub struct Sources {
    known: HashMap<u64, Source>,
    checked: HashSet<u64>,
    /// the lookup each show waits for: an older one's answer doesn't count anymore
    asked: HashMap<u64, u64>,
    tickets: u64,
}

impl Sources {
    pub fn load() -> Sources {
        Sources {
            known: ani::read_json(&resolved_path()),
            checked: HashSet::new(),
            asked: HashMap::new(),
            tickets: 0,
        }
    }

    /// The show's source as last known, maybe from an earlier session.
    pub fn get(&self, id: u64) -> Option<&Source> {
        self.known.get(&id)
    }

    /// How to get a usable source for the show.
    pub fn lookup(&self, id: u64) -> Lookup {
        match self.known.get(&id) {
            Some(s) if self.checked.contains(&id) => Lookup::Ready(s.clone()),
            Some(s) => Lookup::Check(s.clone()),
            None => Lookup::Resolve,
        }
    }

    /// A ticket for looking the show up (`resolve`) in the background, unless it was looked
    /// up this session or a lookup for it runs already.
    pub fn ask(&mut self, id: u64) -> Option<u64> {
        if self.checked.contains(&id) || self.asked.contains_key(&id) {
            return None;
        }
        self.tickets += 1;
        self.asked.insert(id, self.tickets);
        Some(self.tickets)
    }

    /// Takes the answer to lookup `ticket`, unless the source was changed by hand since:
    /// false then, and nothing changes.
    pub fn answer(&mut self, id: u64, ticket: u64, res: &Result<Source, String>) -> bool {
        if self.asked.get(&id) != Some(&ticket) {
            return false;
        }
        self.asked.remove(&id);
        if let Ok(src) = res {
            self.known.insert(id, src.clone());
            self.checked.insert(id);
            let _ = ani::write_json(&resolved_path(), &self.known);
        }
        true
    }

    /// Makes entry `i` of `hits`, which searching `query` found, the show's source from now
    /// on; it's looked up again, and a lookup still running no longer counts.
    pub fn choose(
        &mut self,
        id: u64,
        query: &str,
        hits: &[String],
        i: usize,
    ) -> std::io::Result<()> {
        save_override(id, query, hits, i)?;
        self.known.remove(&id);
        self.checked.remove(&id);
        self.asked.remove(&id);
        Ok(())
    }
}

/// Runs ani-cli until it wants to show a menu and returns that menu as (prompt, lines).
/// `None` means ani-cli never asked (it went on to "play" with the debug player).
fn probe(args: &[&str]) -> Result<Option<(String, Vec<String>)>, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    // one at a time: the provider shouldn't see a burst when several shows are opened
    static TURN: Mutex<()> = Mutex::new(());
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let dir = ani::cache_dir().join("probe");
    let out = dir.join(format!(
        "menu-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let res = ani::ani_cli()
        .args(args)
        .env("ANIHOP_MENU_OUT", &out)
        .env("ANI_CLI_PLAYER", "debug")
        // a single-episode show gets its stream fetched here, and nearly every one has a sub
        .env("ANI_CLI_MODE", "sub")
        .env("ANI_CLI_HIST_DIR", &dir)
        .output()
        .map_err(|e| format!("ani-cli: {e}"))?;
    match fs::read_to_string(&out) {
        Ok(text) => {
            let _ = fs::remove_file(&out);
            let mut lines = text.lines().map(str::to_string);
            let prompt = lines.next().unwrap_or_default();
            Ok(Some((prompt, lines.filter(|l| !l.is_empty()).collect())))
        }
        Err(_) if res.status.success() => Ok(None),
        Err(_) => Err(ani::last_error(&res.stderr)),
    }
}

/// What a search found: provider titles, and the episodes if there was just one hit.
type Hits = (Vec<String>, Option<Vec<String>>);

/// Provider titles in ani-cli's order (index = position + 1). If there is just one hit,
/// ani-cli skips the menu, so we get its episodes instead and not its title.
pub fn search(query: &str) -> Result<Hits, String> {
    if ani::plain(query).trim().is_empty() {
        return Err("No results found!".into());
    }
    match probe(&[ani::plain(query)])? {
        Some((p, lines)) if p.starts_with("Select anime") => Ok((
            lines
                .iter()
                .map(|l| l.split_once(' ').map_or(l.as_str(), |(_, t)| t).to_string())
                .collect(),
            None,
        )),
        Some((_, eps)) => Ok((vec![query.to_string()], Some(eps))),
        None => Ok((vec![query.to_string()], Some(vec!["1".into()]))),
    }
}

/// AniList's results, or if AniList failed, ani-cli's: one show per provider entry, equal
/// titles numbered so each gets its own id and folder. `search` is `search`, passed in for tests.
pub fn or_ani_cli(
    anilist: Result<Vec<Media>, String>,
    query: &str,
    search: impl FnOnce(&str) -> Result<Hits, String>,
) -> Result<Vec<Media>, String> {
    let e = match anilist {
        Err(e) => e,
        ok => return ok, // no results is an answer too
    };
    let (hits, _) = search(query).map_err(|a| format!("{e} · ani-cli: {a}"))?;
    let mut taken: HashSet<String> = hits.iter().cloned().collect();
    let mut first = HashSet::new();
    let shows = hits.iter().enumerate().map(|(i, title)| {
        // later entries of a title get the lowest free number: the provider may have its own "X (2)"
        let name = match first.insert(title) {
            true => title.clone(),
            false => (2..)
                .map(|n| format!("{title} ({n})"))
                .find(|n| !taken.contains(n))
                .unwrap(),
        };
        taken.insert(name.clone());
        Media::from_ani_cli(&name, Source::at(query, &hits, i + 1, Vec::new()))
    });
    Ok(shows.collect())
}

fn episodes(query: &str, index: usize) -> Result<Vec<String>, String> {
    match probe(&["-S", &index.to_string(), ani::plain(query)])? {
        Some((_, eps)) => Ok(eps),
        None => Ok(vec!["1".into()]), // single-episode show, ani-cli doesn't ask
    }
}

/// Canonical title, so "Jujutsu Kaisen (TV)" = "JUJUTSU KAISEN" and
/// "My Hero Academia 2" = "My Hero Academia Season 2" = "... 2nd Season".
fn title_key(s: &str) -> String {
    let s = s.to_lowercase().replace('×', " x ");
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !matches!(*w, "" | "season" | "part" | "cour" | "the" | "tv"))
        .map(|w| {
            match w
                .strip_suffix(['t', 'd', 'h'])
                .and_then(|w| w.strip_suffix(['s', 'n', 'r', 't']))
            {
                Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => n,
                _ => w,
            }
        })
        .collect()
}

/// Best hits, equally good: same canonical title as on AniList, else the closest title
/// that contains one of AniList's (or is contained), else none.
fn best_matches(m: &Media, hits: &[String]) -> Vec<usize> {
    let titles: Vec<String> = [&m.title.english, &m.title.romaji]
        .into_iter()
        .flatten()
        .chain(&m.synonyms)
        .map(|t| title_key(t))
        .filter(|t| !t.is_empty())
        .collect();
    let hits: Vec<String> = hits.iter().map(|h| title_key(h)).collect();
    let closeness = |h: &String| {
        let near = titles
            .iter()
            .filter(|t| h.contains(t.as_str()) || t.contains(h.as_str()));
        near.map(|t| t.len().abs_diff(h.len())).min()
    };
    let scores: Vec<Option<usize>> = hits.iter().map(closeness).collect();
    match scores.iter().flatten().min() {
        Some(best) => (0..hits.len())
            .filter(|&i| scores[i] == Some(*best))
            .collect(),
        None => Vec::new(),
    }
}

/// Searches `queries` in turn up to the first whose hits have one of `m`'s titles, as one can
/// find only other shows ("Hagane no Renkinjutsushi": just Arpeggio). If none does, the first
/// that found anything. `search` is `search`, passed in for tests.
fn first_fit(
    m: &Media,
    queries: &[String],
    mut search: impl FnMut(&str) -> Result<Hits, String>,
) -> Result<(String, Hits), String> {
    let mut found = Err("no title to search for".to_string());
    for q in queries {
        let res = search(q).map(|r| (q.clone(), r));
        // a single hit shows no title to check, and it's the only show there is
        let fits = (res.as_ref())
            .is_ok_and(|(_, (hits, eps))| eps.is_some() || !best_matches(m, hits).is_empty());
        if fits || found.is_err() {
            found = res;
        }
        if fits {
            break;
        }
    }
    found
}

/// The show `best_matches` compares titles with: one ani-cli found by its provider title, as
/// its shown name may carry a number ("A (2)") that would read as a season.
fn matched(m: &Media) -> Media {
    let mut m = m.clone();
    if let Some(at) = &m.ani_cli {
        m.title.romaji = Some(at.title.clone());
    }
    m
}

fn overrides_path() -> PathBuf {
    ani::config_dir().join("sources.json")
}

/// A source chosen by hand: the search, the provider title and which of the entries with it
/// (saved before 0.7 without: the first).
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Pick {
    Nth(String, String, usize),
    First(String, String),
}

impl Pick {
    fn parts(self) -> (String, String, usize) {
        match self {
            Pick::Nth(q, t, n) => (q, t, n),
            Pick::First(q, t) => (q, t, 1),
        }
    }
}

/// Manually chosen sources by AniList id.
fn overrides() -> HashMap<String, Pick> {
    ani::read_json(&overrides_path())
}

fn save_override(id: u64, query: &str, hits: &[String], i: usize) -> std::io::Result<()> {
    let nth = hits[..=i].iter().filter(|h| **h == hits[i]).count();
    let mut all = overrides();
    let pick = Pick::Nth(query.to_string(), hits[i].clone(), nth);
    all.insert(id.to_string(), pick);
    ani::write_json(&overrides_path(), &all)
}

pub fn default_query(m: &Media) -> String {
    if let Some(at) = &m.ani_cli {
        return at.query.clone();
    }
    m.title
        .romaji
        .clone()
        .or(m.title.english.clone())
        .unwrap_or_default()
}

fn resolved_path() -> PathBuf {
    ani::cache_dir().join("resolved.json")
}

/// Re-checks a (possibly cached) source right before ani-cli runs: `-S` is positional and
/// the provider's order shifts when new seasons show up. One search, not a full resolve.
pub fn locate(m: &Media, src: &Source) -> Result<Source, String> {
    let (hits, _) = search(&src.query)?;
    match position(src, &hits) {
        Some(index) => Ok(Source::at(&src.query, &hits, index, src.episodes.clone())),
        None => resolve(m),
    }
}

/// Where `src` is in fresh `hits`: the nth entry with its title, as equal titles (a remake)
/// keep their order when others move in. Saved before 0.6.1: only an unambiguous match.
fn position(src: &Source, hits: &[String]) -> Option<usize> {
    let same: Vec<usize> = (1..=hits.len())
        .filter(|&i| hits[i - 1] == src.title)
        .collect();
    match src.nth {
        0 if same.len() == 1 => Some(same[0]),
        0 => None, // which of the equal titles is unknown: resolve again
        n => same.get(n - 1).copied(),
    }
}

pub fn resolve(m: &Media) -> Result<Source, String> {
    let saved = overrides().remove(&m.id.to_string()).map(Pick::parts);
    // picked from ani-cli's own results: that very entry, unless "Change source…" chose another
    let picked = m.ani_cli.as_ref().filter(|_| saved.is_none());
    let mut found = None;
    if let Some(at) = picked {
        let (hits, eps) = search(&at.query)?;
        if let Some(eps) = eps {
            // a single hit: ani-cli shows no title to check, and it's the only show there is
            return Ok(Source::at(&at.query, &hits, 1, eps));
        }
        if let Some(index) = position(at, &hits) {
            return Ok(Source::at(
                &at.query,
                &hits,
                index,
                episodes(&at.query, index)?,
            ));
        }
        // gone, or picked as a single hit whose title ani-cli never showed: match titles below
        found = Some((at.query.clone(), (hits, None)));
    }
    // provider search chokes on some symbols (HUNTER×HUNTER), so fall back to plainer queries
    let plain = |t: &String| {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut queries: Vec<String> = match &saved {
        Some((q, ..)) => vec![q.clone()],
        None if picked.is_some() => Vec::new(), // searched above
        None => [
            m.title.romaji.clone(),
            m.title.english.clone(),
            m.title.romaji.as_ref().map(plain),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    let mut seen = HashSet::new();
    queries.retain(|q| seen.insert(q.clone()));
    let (query, (hits, eps)) = match found {
        Some(found) => found,
        None => first_fit(&matched(m), &queries, search)?,
    };
    if let Some(eps) = eps {
        return Ok(Source::at(&query, &hits, 1, eps));
    }
    // the very entry picked, the second of two equal titles too
    let exact = (saved.map(|(_, t, n)| (t, n))).or(picked.map(|at| (at.title.clone(), at.nth)));
    let at = |(title, nth): (String, usize)| {
        let same = hits.iter().enumerate().filter(|(_, h)| **h == title);
        same.map(|(i, _)| i).nth(nth.saturating_sub(1))
    };
    let mut candidates = match exact.and_then(at) {
        Some(pos) => vec![pos],
        None => best_matches(&matched(m), &hits),
    };
    if candidates.is_empty() {
        candidates.push(0); // nothing close: trust the provider's ranking
    }
    // same title twice (HUNTER×HUNTER 1999/2011): a finished show's episode count decides
    let want = m
        .episodes
        .filter(|_| m.status.as_deref() == Some("FINISHED"));
    let mut pick = None;
    for &i in candidates.iter().take(3) {
        let eps = episodes(&query, i + 1)?;
        let fits = want.is_none_or(|n| eps.len() == n as usize);
        if pick.is_none() || fits {
            pick = Some((i, eps));
        }
        if fits {
            break;
        }
    }
    let (i, episodes) = pick.unwrap();
    Ok(Source::at(&query, &hits, i + 1, episodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(romaji: &str, english: &str, hits: &[&str]) -> usize {
        let mut m = Media::default();
        m.title.romaji = Some(romaji.into());
        m.title.english = Some(english.into());
        best_matches(&m, &hits.iter().map(|h| h.to_string()).collect::<Vec<_>>())[0]
    }

    #[test]
    fn matches_provider_titles() {
        let aot = [
            "Attack on Titan: No Regrets",
            "Attack on Titan",
            "Attack on Titan Season 2",
        ];
        assert_eq!(pick("Shingeki no Kyojin", "Attack on Titan", &aot), 1);
        let jjk = [
            "Jujutsu Kaisen 3rd Season: The Culling Game Part 1",
            "Jujutsu Kaisen (TV)",
            "Jujutsu Kaisen 2nd Season",
        ];
        assert_eq!(pick("Jujutsu Kaisen", "JUJUTSU KAISEN", &jjk), 1);
        assert_eq!(
            pick("Jujutsu Kaisen 2nd Season", "JUJUTSU KAISEN Season 2", &jjk),
            2
        );
        let mha = [
            "My Hero Academia the Movie 2: Heroes Rising",
            "My Hero Academia 2",
            "My Hero Academia Season 2: Hero Notebook",
        ];
        assert_eq!(
            pick("Boku no Hero Academia 2", "My Hero Academia Season 2", &mha),
            1
        );
        assert_eq!(
            pick(
                "HUNTER×HUNTER (2011)",
                "Hunter x Hunter (2011)",
                &["Hunter x Hunter (2011)"]
            ),
            0
        );
        let mut hxh = Media::default();
        hxh.title.english = Some("Hunter x Hunter".into());
        let hits = [
            "Hunter x Hunter",
            "Hunter x Hunter",
            "Hunter x Hunter Movie 1: Phantom Rouge",
        ]
        .map(String::from);
        assert_eq!(best_matches(&hxh, &hits), [0, 1]); // tie, resolved by episode count
        let mut kimi = Media::default();
        kimi.title.english = Some("Your Name.".into());
        let hits = ["Weathering With You", "Suzume"].map(String::from);
        assert!(best_matches(&kimi, &hits).is_empty()); // nothing close
    }

    #[test]
    fn searches_on_when_no_hit_is_the_show() {
        let mut fma = Media::default();
        fma.title.romaji = Some("Hagane no Renkinjutsushi".into());
        fma.title.english = Some("Fullmetal Alchemist".into());
        let queries = ["Hagane no Renkinjutsushi", "Fullmetal Alchemist", "FMA"].map(String::from);
        let provider = |q: &str| match q {
            "Fullmetal Alchemist" => ["Fullmetal Alchemist: Brotherhood", "Fullmetal Alchemist"],
            _ => [
                "Aoki Hagane no Arpeggio: Ars Nova DC",
                "Arpeggio of Blue Steel",
            ],
        };
        let mut searched = Vec::new();
        let search = |q: &str| {
            searched.push(q.to_string());
            Ok((provider(q).map(String::from).into(), None))
        };
        let (query, _) = first_fit(&fma, &queries, search).unwrap();
        assert_eq!(query, "Fullmetal Alchemist");
        assert_eq!(searched, queries[..2]); // and no further

        // found by none: the first search that found anything, the provider's top hit stands
        let failing = |q: &str| match q {
            "Hagane no Renkinjutsushi" => Err("No results found!".into()),
            _ => Ok((vec!["Arpeggio of Blue Steel".into()], None)),
        };
        let (query, _) = first_fit(&fma, &queries, failing).unwrap();
        assert_eq!(query, "Fullmetal Alchemist");
        // a single hit shows no title to check, so it stands
        let single = |_: &str| Ok((vec!["?".into()], Some(vec!["1".into()])));
        assert_eq!(first_fit(&fma, &queries, single).unwrap().0, queries[0]);
        let none = |_: &str| Err("No results found!".to_string());
        assert_eq!(
            first_fit(&fma, &queries, none).unwrap_err(),
            "No results found!"
        );
        assert!(first_fit(&fma, &[], none).is_err());
    }

    #[test]
    fn falls_back_to_ani_cli_only_when_anilist_fails() {
        let unused = |_: &str| -> Result<Hits, String> {
            panic!("ani-cli searched although AniList answered")
        };
        let list = vec![Media::default()];
        assert_eq!(or_ani_cli(Ok(list), "x", unused).unwrap().len(), 1);
        assert!(or_ani_cli(Ok(vec![]), "x", unused).unwrap().is_empty());

        let hits = |_: &str| Ok((vec!["A".into(), "B".into(), "A".into()], None));
        let shows = or_ani_cli(Err("AniList: down".into()), "a", hits).unwrap();
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["A", "B", "A (2)"]);
        assert!(shows.iter().all(|m| !m.on_anilist()));
        assert!(shows[0].id != shows[2].id && shows[0].id != shows[1].id);
        let at: Vec<(usize, &str, usize)> = (shows.iter())
            .map(|m| m.ani_cli.as_ref().unwrap())
            .map(|s| (s.index, s.title.as_str(), s.nth))
            .collect();
        assert_eq!(at, [(1, "A", 1), (2, "B", 1), (3, "A", 2)]);

        // the provider's own "A (2)" keeps its name, so the second A takes the next number
        let clash = |_: &str| Ok((vec!["A".into(), "A".into(), "A (2)".into()], None));
        let shows = or_ani_cli(Err("e".into()), "a", clash).unwrap();
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["A", "A (3)", "A (2)"]);
        let ids: HashSet<u64> = shows.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 3);
        // the same entries from an earlier search without the provider's "A (2)" keep their ids
        let before = |_: &str| Ok((vec!["A".into(), "A".into()], None));
        let before = or_ani_cli(Err("e".into()), "a", before).unwrap();
        assert_eq!(before[1].name(), "A (2)");
        assert_eq!(before[1].id, shows[1].id); // the second A, now "A (3)"
        assert_ne!(before[1].id, shows[2].id); // not the provider's "A (2)"

        // one hit: ani-cli shows no title, only its episodes
        let single = |q: &str| Ok((vec![q.to_string()], Some(vec!["1".into(), "2".into()])));
        let shows = or_ani_cli(Err("e".into()), "Frieren 2", single).unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].name(), "Frieren 2");
        assert_eq!(shows[0].ani_cli.as_ref().unwrap().index, 1);

        let none = |_: &str| Err("No results found!".to_string());
        let e = or_ani_cli(Err("AniList: down".into()), "a", none).unwrap_err();
        assert_eq!(e, "AniList: down · ani-cli: No results found!");
    }

    #[test]
    fn keeps_the_second_of_two_equal_titles() {
        let hits: Vec<String> = ["HxH", "Fantasy", "HxH"].map(String::from).into();
        let src = Source::at("hxh", &hits, 3, Vec::new());
        assert_eq!((src.title.as_str(), src.nth), ("HxH", 2));
        assert_eq!(position(&src, &hits), Some(3));
        // a new show in front: still the second HxH
        let moved: Vec<String> = ["New", "HxH", "Fantasy", "HxH"].map(String::from).into();
        assert_eq!(position(&src, &moved), Some(4));
        assert_eq!(position(&src, &hits[..2]), None); // the second one is gone
        // saved before 0.6.1: its only match, or resolve again when it's one of several
        let old = Source { nth: 0, ..src };
        assert_eq!(position(&old, &hits), None);
        assert_eq!(position(&old, &hits[1..]), Some(2));
    }

    #[test]
    fn a_single_hit_is_found_again_by_its_title() {
        // picked as the only hit: ani-cli showed no title, so the typed query stands in for it
        let query = "frieren beyond journey's end season 2";
        let at = Source::at(query, &[query.to_string()], 1, Vec::new());
        let m = Media::from_ani_cli(query, at);
        let hits = [
            "Frieren: Beyond Journey's End Season 2 Specials",
            "Frieren: Beyond Journey's End Season 2",
        ]
        .map(String::from);
        assert_eq!(position(m.ani_cli.as_ref().unwrap(), &hits), None);
        assert_eq!(best_matches(&matched(&m), &hits), [1]); // what resolve falls back to

        // a numbered one is matched by its provider title, not "A (2)" (= "A Season 2")
        let hits = ["A", "A", "A Season 2"].map(String::from);
        let at = Source::at("a", &hits, 2, Vec::new());
        let m = Media::from_ani_cli("A (2)", at);
        let renamed = ["A TV", "A Season 2"].map(String::from); // no exact "A" left
        assert_eq!(best_matches(&matched(&m), &renamed), [0]);
        assert_eq!(best_matches(&m, &renamed), [1]);
    }

    #[test]
    fn a_source_changed_by_hand_wins_over_a_lookup_still_running() {
        let src = |title: &str| Source {
            query: "q".into(),
            index: 1,
            title: title.into(),
            nth: 1,
            episodes: Vec::new(),
        };
        let mut all = Sources {
            known: HashMap::new(),
            checked: HashSet::new(),
            asked: HashMap::new(),
            tickets: 0,
        };
        let old = all.ask(7).unwrap();
        assert_eq!(all.ask(7), None, "one runs already");
        // what choose does besides saving the pick
        all.asked.remove(&7);
        let new = all.ask(7).unwrap();
        assert!(!all.answer(7, old, &Ok(src("Old"))));
        assert!(all.get(7).is_none());
        assert!(all.answer(7, new, &Err("offline".into())));
        let again = all.ask(7).unwrap();
        assert!(all.answer(7, again, &Ok(src("New"))));
        assert!(matches!(all.lookup(7), Lookup::Ready(s) if s.title == "New"));
        assert_eq!(all.ask(7), None, "checked this session");
    }

    #[test]
    fn a_pick_remembers_which_of_two_equal_titles() {
        let mut old: HashMap<String, Pick> =
            serde_json::from_str(r#"{"1": ["q", "HxH"]}"#).unwrap();
        let first = old.remove("1").unwrap().parts();
        assert_eq!(first, ("q".into(), "HxH".into(), 1));
        let hits: Vec<String> = ["HxH", "Fantasy", "HxH"].map(String::from).into();
        let nth = hits[..=2].iter().filter(|h| **h == hits[2]).count();
        let saved = serde_json::to_string(&Pick::Nth("q".into(), hits[2].clone(), nth)).unwrap();
        assert_eq!(saved, r#"["q","HxH",2]"#);
    }

    #[test]
    fn queries_never_become_ani_cli_options() {
        assert_eq!(ani::plain("-D"), "D");
        assert_eq!(ani::plain("--update"), "update");
        assert_eq!(ani::plain("Re:Zero -Starting"), "Re:Zero -Starting");
        assert_eq!(search("--"), Err("No results found!".into()));
    }
}
