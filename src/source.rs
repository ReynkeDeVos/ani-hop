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
    /// shows whose source was picked by hand, so a lookup has something to go by
    picked: HashSet<u64>,
}

impl Sources {
    pub fn load() -> Sources {
        Sources {
            known: ani::read_json(&resolved_path()),
            checked: HashSet::new(),
            asked: HashMap::new(),
            tickets: 0,
            picked: (overrides().keys())
                .filter_map(|id| id.parse().ok())
                .collect(),
        }
    }

    /// Whether the show's source was picked by hand, maybe in an earlier session whose
    /// lookup of it never finished.
    pub fn picked(&self, id: u64) -> bool {
        self.picked.contains(&id)
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
        self.picked.insert(id);
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
    match probe(&[&ani::plain(query)])? {
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

/// AniList's results for `query`, or if AniList failed, ani-cli's: one show per provider entry,
/// equal titles numbered so each gets its own id and folder. AniList finds nothing for a typo
/// or words run together ("frieran", "onepiece") that the provider does find: then AniList's
/// results for the provider's top title, if one of theirs starts like `query` and the title
/// has no number `query` hasn't, as the provider has a guess for anything ("Soso no Furiren":
/// Wooser; "jujutsu kaisen s2": the 3rd season). `anilist` searches AniList and `search` is
/// `search`, passed in for tests.
pub fn or_ani_cli(
    mut anilist: impl FnMut(&str) -> Result<Vec<Media>, String>,
    query: &str,
    search: impl FnOnce(&str) -> Result<Hits, String>,
) -> Result<Vec<Media>, String> {
    let e = match anilist(query) {
        Ok(none) if none.is_empty() => {
            // a single hit shows no title to search for
            let top = (search(query).ok())
                .filter(|(_, eps)| eps.is_none())
                .and_then(|(hits, _)| hits.into_iter().next());
            // AniList finds no "Jujutsu Kaisen (TV)"
            let top = top.as_deref().map(|t| t.split(" (").next().unwrap_or(t));
            let numbers = |s: &str| -> HashSet<String> {
                let runs = s.split(|c: char| !c.is_ascii_digit());
                runs.filter(|n| !n.is_empty()).map(String::from).collect()
            };
            let Some(top) = top.filter(|t| numbers(t).is_subset(&numbers(query))) else {
                return Ok(none); // no results is an answer too
            };
            let found = anilist(top)?;
            let start = |s: &str| -> String {
                let alnum = s.chars().filter(|c| c.is_alphanumeric());
                alnum.flat_map(char::to_lowercase).take(3).collect()
            };
            let typed = start(query);
            let close = |t: &String| !typed.is_empty() && start(t).starts_with(&typed);
            let found_it = found.iter().flat_map(Media::titles).any(close);
            return Ok(if found_it { found } else { none });
        }
        Err(e) => e,
        ok => return ok,
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
    match probe(&["-S", &index.to_string(), &ani::plain(query)])? {
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

/// How close each hit is to one of AniList's titles, 0 the closest: spelled the same (case and
/// spaces aside), as canonical titles lose what tells some seasons apart ("Kaguya-sama: Love
/// is War?" is the second); 1, the same canonical title; more, a title that contains one of
/// AniList's (or is contained), the more the longer or shorter. None: not close.
fn closeness(m: &Media, hits: &[String]) -> Vec<Option<usize>> {
    let names: Vec<&String> = m.titles().collect();
    let spelled = |s: &str| {
        s.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let spellings: HashSet<String> = names.iter().map(|t| spelled(t)).collect();
    let titles: Vec<String> = (names.iter())
        .map(|t| title_key(t))
        .filter(|t| !t.is_empty())
        .collect();
    let close = |h: &String| {
        if spellings.contains(&spelled(h)) {
            return Some(0);
        }
        let h = title_key(h);
        let near = titles
            .iter()
            .filter(|t| h.contains(t.as_str()) || t.contains(h.as_str()));
        near.map(|t| 1 + t.len().abs_diff(h.len())).min()
    };
    hits.iter().map(close).collect()
}

/// Best hits, equally good: spelled as on AniList, else the same canonical title, else the
/// closest title that contains one of AniList's (or is contained), else none.
fn best_matches(m: &Media, hits: &[String]) -> Vec<usize> {
    let scores = closeness(m, hits);
    match scores.iter().flatten().min() {
        Some(best) => (0..hits.len())
            .filter(|&i| scores[i] == Some(*best))
            .collect(),
        None => Vec::new(),
    }
}

/// Searches `queries` in turn up to the first whose hits have one of `m`'s titles, as one can
/// find only other shows ("Hagane no Renkinjutsushi": just Arpeggio), or only ones whose titles
/// contain `m`'s ("Pocket Monsters": Pokémon Horizons, not Pokémon). If none does, the first
/// with a title that contains one, else the first that found anything. `search` is `search`,
/// passed in for tests.
fn first_fit(
    m: &Media,
    queries: &[String],
    mut search: impl FnMut(&str) -> Result<Hits, String>,
) -> Result<(String, Hits), String> {
    let mut found = Err("no title to search for".to_string());
    // how well `found` fits: 2 one of `m`'s titles, 1 one containing one, 0 none
    let mut fit = None;
    for q in queries {
        let res = search(q).map(|r| (q.clone(), r));
        let f = res.as_ref().ok().map(|(_, (hits, eps))| match eps {
            Some(_) => 2, // a single hit shows no title to check, and it's the only show there is
            None => match closeness(m, hits).into_iter().flatten().min() {
                Some(0 | 1) => 2,
                Some(_) => 1,
                None => 0,
            },
        });
        if f > fit || found.is_err() {
            (found, fit) = (res, f);
        }
        if f == Some(2) {
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

/// What ani-cli searches for `query`, typed to change `m`'s source: a nickname AniList knows
/// it by ("aot", "fmab") is its title, as the provider finds another show for one, or none.
pub fn source_query(m: &Media, query: &str) -> String {
    let nickname = m
        .synonyms
        .iter()
        .any(|s| s.eq_ignore_ascii_case(query.trim()));
    match m.title.english.as_ref().or(m.title.romaji.as_ref()) {
        Some(title) if nickname => title.clone(),
        _ => query.to_string(),
    }
}

/// A finished show's source when its search found a single hit with another episode count than
/// its `n`, which may be another season ("86: Eighty Six" finds just the 2nd): a hit with `n`
/// episodes for its title up to the colon ("86"), that title or one containing it, closest
/// first. None if there's none, and the single hit stays. `search` and `episodes` are theirs,
/// passed in for tests.
fn by_count(
    m: &Media,
    n: usize,
    mut search: impl FnMut(&str) -> Result<Hits, String>,
    mut episodes: impl FnMut(&str, usize) -> Result<Vec<String>, String>,
) -> Option<Source> {
    // not "Re:Zero"'s "Re"
    let titles = [&m.title.romaji, &m.title.english].into_iter().flatten();
    let mut tried = HashSet::new();
    for short in titles
        .filter_map(|t| t.split_once(": "))
        .map(|(s, _)| s.trim())
    {
        if short.is_empty() || !tried.insert(short) {
            continue;
        }
        let Ok((hits, None)) = search(short) else {
            continue; // a single hit shows no title to check
        };
        let close = closeness(m, &hits);
        let mut near: Vec<usize> = (0..hits.len()).filter(|&i| close[i].is_some()).collect();
        near.sort_by_key(|&i| close[i]);
        for i in near.into_iter().take(3) {
            match episodes(short, i + 1) {
                Ok(eps) if eps.len() == n => return Some(Source::at(short, &hits, i + 1, eps)),
                _ => {}
            }
        }
    }
    None
}

/// A finished show's source by `by_count`, when its search found a single hit with `eps`, of
/// another length than the show's; none for a source picked by hand.
fn recount(m: &Media, eps: &[String]) -> Option<Source> {
    let n = m
        .episodes
        .filter(|_| m.status.as_deref() == Some("FINISHED"))? as usize;
    let by_hand = || overrides().contains_key(&m.id.to_string());
    if eps.len() == n || by_hand() {
        return None;
    }
    by_count(m, n, search, episodes)
}

fn resolved_path() -> PathBuf {
    ani::cache_dir().join("resolved.json")
}

/// Re-checks a (possibly cached) source right before ani-cli runs: `-S` is positional and
/// the provider's order shifts when new seasons show up. One search, not a full resolve.
pub fn locate(m: &Media, src: &Source) -> Result<Source, String> {
    let (hits, eps) = search(&src.query)?;
    // cached before `resolve` doubted a single hit of another length (86's 2nd season)
    if let Some(better) = eps.and_then(|eps| recount(m, &eps)) {
        return Ok(better);
    }
    match position(src, &hits) {
        Some(index) => Ok(Source::at(&src.query, &hits, index, src.episodes.clone())),
        None => resolve(m),
    }
}

/// `src` with the episodes the provider lists now: a cached one's may be from before the
/// latest aired. One search.
pub fn with_episodes(src: Source) -> Result<Source, String> {
    let episodes = episodes(&src.query, src.index)?;
    Ok(Source { episodes, ..src })
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
        if let Some(better) = recount(m, &eps) {
            return Ok(better);
        }
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
        let one = |_: &str| Ok(vec![Media::default()]);
        assert_eq!(or_ani_cli(one, "x", unused).unwrap().len(), 1);
        let down = |_: &str| Err("AniList: down".to_string());

        let hits = |_: &str| Ok((vec!["A".into(), "B".into(), "A".into()], None));
        let shows = or_ani_cli(down, "a", hits).unwrap();
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
        let shows = or_ani_cli(down, "a", clash).unwrap();
        let names: Vec<&str> = shows.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["A", "A (3)", "A (2)"]);
        let ids: HashSet<u64> = shows.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 3);
        // the same entries from an earlier search without the provider's "A (2)" keep their ids
        let before = |_: &str| Ok((vec!["A".into(), "A".into()], None));
        let before = or_ani_cli(down, "a", before).unwrap();
        assert_eq!(before[1].name(), "A (2)");
        assert_eq!(before[1].id, shows[1].id); // the second A, now "A (3)"
        assert_ne!(before[1].id, shows[2].id); // not the provider's "A (2)"

        // one hit: ani-cli shows no title, only its episodes
        let single = |q: &str| Ok((vec![q.to_string()], Some(vec!["1".into(), "2".into()])));
        let shows = or_ani_cli(down, "Frieren 2", single).unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].name(), "Frieren 2");
        assert_eq!(shows[0].ani_cli.as_ref().unwrap().index, 1);

        let none = |_: &str| Err("No results found!".to_string());
        let e = or_ani_cli(down, "a", none).unwrap_err();
        assert_eq!(e, "AniList: down · ani-cli: No results found!");
    }

    #[test]
    fn a_typo_is_searched_again_as_the_provider_spells_it() {
        let show = |romaji: &str, synonym: &str| {
            let mut m = Media::default();
            m.title.romaji = Some(romaji.into());
            m.synonyms = vec![synonym.into()];
            m
        };
        let catalog = |q: &str| {
            Ok(match q {
                "Jujutsu Kaisen" => vec![show("Jujutsu Kaisen", "JJK")],
                "Wooser's Hand-to-Mouth Life" => {
                    vec![show("Wooser no Sono Higurashi", "Hand-to-Mouth Life")]
                }
                _ => Vec::new(),
            })
        };
        let mut asked = Vec::new();
        let anilist = |q: &str| {
            asked.push(q.to_string());
            catalog(q)
        };
        let hits = |q: &str| {
            let hits = match q {
                "jujutsukaisen" | "jujutsukaisen 2020" => {
                    ["Jujutsu Kaisen (TV)", "Jujutsu Kaisen 2nd Season"]
                }
                "jujutsu kaisen s2" => [
                    "Jujutsu Kaisen 3rd Season: The Culling Game Part 1",
                    "Jujutsu Kaisen 2nd Season",
                ],
                _ => [
                    "Wooser's Hand-to-Mouth Life",
                    "Wooser's Hand-to-Mouth Life (Dub)",
                ],
            };
            Ok((hits.map(String::from).into(), None))
        };
        let shows = or_ani_cli(anilist, "jujutsukaisen", hits).unwrap();
        assert!(shows.len() == 1 && shows[0].on_anilist());
        assert_eq!(asked, ["jujutsukaisen", "Jujutsu Kaisen"]);
        // the provider's guess for a show it doesn't have either
        let guess = or_ani_cli(catalog, "Soso no Furiren", hits).unwrap();
        assert!(guess.is_empty());
        // one of its other titles starts like the typo
        assert_eq!(or_ani_cli(catalog, "hand to muoth", hits).unwrap().len(), 1);
        // nor another season, asked for with a number the guess hasn't: AniList isn't asked again
        let mut asked = 0;
        let counted = |q: &str| {
            asked += 1;
            catalog(q)
        };
        assert!(
            or_ani_cli(counted, "jujutsu kaisen s2", hits)
                .unwrap()
                .is_empty()
        );
        assert_eq!(asked, 1);
        // one only typed is fine
        assert_eq!(
            or_ani_cli(catalog, "jujutsukaisen 2020", hits)
                .unwrap()
                .len(),
            1
        );

        // nothing at the provider either, or one hit without a title: no results, no error
        for found in [
            Err("No results found!".to_string()),
            Ok((vec!["xyz".into()], Some(vec!["1".into()]))),
        ] {
            let mut asked = 0;
            let anilist = |_: &str| {
                asked += 1;
                Ok(Vec::new())
            };
            assert!(or_ani_cli(anilist, "xyz", |_| found).unwrap().is_empty());
            assert_eq!(asked, 1);
        }
    }

    #[test]
    fn a_title_as_anilist_spells_it_beats_a_canonical_one() {
        // the second season: its "?" is all canonical titles would tell it apart by
        let mut kaguya = Media::default();
        kaguya.title.english = Some("Kaguya-sama: Love is War?".into());
        kaguya.title.romaji =
            Some("Kaguya-sama wa Kokurasetai?: Tensaitachi no Renai Zunousen".into());
        kaguya.synonyms = vec!["Kaguya-sama: Love is War Season 2".into()];
        let hits = [
            "Kaguya-sama: Love is War",
            "Kaguya-sama: Love is War Season 2",
            "Kaguya-sama: Love is War - Ultra Romantic",
        ];
        assert_eq!(best_matches(&kaguya, &hits.map(String::from)), [1]);
        let first = pick(
            "Kaguya-sama wa Kokurasetai",
            "Kaguya-sama: Love is War",
            &hits,
        );
        assert_eq!(first, 0);
        // case and spaces aside
        let hits = [
            "Kaguya-sama: Love is War",
            "KAGUYA-SAMA:  love is war season 2",
        ];
        assert_eq!(best_matches(&kaguya, &hits.map(String::from)), [1]);
    }

    #[test]
    fn searches_on_when_a_title_only_contains_the_show() {
        let mut pokemon = Media::default();
        pokemon.title.romaji = Some("Pocket Monsters".into());
        pokemon.title.english = Some("Pokémon".into());
        let queries = ["Pocket Monsters", "Pokémon"].map(String::from);
        let provider = |q: &str| match q {
            "Pocket Monsters" => [
                "Pocket Monsters - The Hallowed God, Arceus",
                "Pokémon Horizons: The Series",
            ],
            _ => ["Pokémon", "Pokémon Horizons: The Series"],
        };
        let search = |q: &str| Ok((provider(q).map(String::from).into(), None));
        assert_eq!(first_fit(&pokemon, &queries, search).unwrap().0, "Pokémon");
        // no query finds the very title: the first one that found a title containing it
        let loose = |q: &str| match q {
            "Pocket Monsters" => Ok((vec!["Arpeggio of Blue Steel".into()], None)),
            _ => Ok((vec!["Pokémon Horizons: The Series".into()], None)),
        };
        assert_eq!(first_fit(&pokemon, &queries, loose).unwrap().0, "Pokémon");
        let loose = |_: &str| Ok((vec!["Pokémon Horizons: The Series".into()], None));
        assert_eq!(
            first_fit(&pokemon, &queries, loose).unwrap().0,
            "Pocket Monsters"
        );
    }

    #[test]
    fn a_single_hit_of_another_length_gives_way_to_a_shorter_title() {
        let mut m = Media::default();
        m.title.romaji = Some("86: Eighty Six".into());
        m.title.english = Some("86 EIGHTY-SIX".into());
        let hits = [
            "86",
            "86 EIGHTY-SIX Season 2 Recap",
            "Eighty Six: 2nd Season",
            "86-Eighty Six- Special Edition",
        ];
        let lengths = [11, 3, 12, 1];
        let searched = std::cell::RefCell::new(Vec::new());
        let search = |q: &str| {
            searched.borrow_mut().push(q.to_string());
            Ok((hits.map(String::from).into(), None))
        };
        let episodes =
            |_: &str, i: usize| Ok((1..=lengths[i - 1]).map(|e| e.to_string()).collect());
        // the recap is closest, but has 3
        let src = by_count(&m, 11, search, episodes).unwrap();
        assert_eq!((src.query.as_str(), src.index), ("86", 1));
        assert_eq!(*searched.borrow(), ["86"], "the English title has no colon");
        // none with the show's length: the single hit stays
        assert!(by_count(&m, 13, search, episodes).is_none());
        let single = |_: &str| Ok((vec!["86".into()], Some(vec!["1".into()])));
        assert!(by_count(&m, 11, single, episodes).is_none());
    }

    #[test]
    fn a_nickname_changes_the_source_by_its_title() {
        let mut aot = Media::default();
        aot.title.romaji = Some("Shingeki no Kyojin".into());
        aot.title.english = Some("Attack on Titan".into());
        aot.synonyms = vec!["SnK".into(), "AoT".into()];
        assert_eq!(source_query(&aot, "aot"), "Attack on Titan");
        assert_eq!(source_query(&aot, "SNK "), "Attack on Titan");
        assert_eq!(source_query(&aot, "aotu"), "aotu");
        aot.title.english = None;
        assert_eq!(source_query(&aot, "AoT"), "Shingeki no Kyojin");
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
            picked: HashSet::new(),
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
    fn a_pick_from_an_earlier_session_counts() {
        if std::env::var_os("ANIHOP_CHILD").is_some() {
            let all = Sources::load();
            assert!(all.picked(7) && !all.picked(8));
            return;
        }
        let dir = std::env::temp_dir().join(format!("ani-hop-picks-{}", std::process::id()));
        let picks = dir.join("config/ani-hop/sources.json");
        fs::create_dir_all(picks.parent().unwrap()).unwrap();
        fs::write(&picks, r#"{"7": ["show", "Show", 1]}"#).unwrap();
        let env = [
            ("HOME", dir.clone().into_os_string()),
            ("XDG_CONFIG_HOME", dir.join("config").into_os_string()),
            ("XDG_CACHE_HOME", dir.join("cache").into_os_string()),
        ];
        assert!(crate::in_child(
            "source::tests::a_pick_from_an_earlier_session_counts",
            &env
        ));
        let none = crate::in_child("source::tests::no_such_test", &env);
        assert!(!none, "a name that matches no test fails");
        fs::remove_dir_all(&dir).unwrap();
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
        assert_eq!(ani::plain("Re:Zero -Starting"), "Re%3AZero -Starting");
        assert_eq!(search("--"), Err("No results found!".into()));
        // and reach the provider's URL as they are
        assert_eq!(ani::plain("HUNTER×HUNTER"), "HUNTER%C3%97HUNTER");
        assert_eq!(ani::plain("Pokémon"), "Pok%C3%A9mon");
        assert_eq!(ani::plain("A & B+ [1] 100%"), "A %26 B%2B %5B1%5D 100%25");
        assert_eq!(ani::plain("葬送"), "%E8%91%AC%E9%80%81");
    }
}
