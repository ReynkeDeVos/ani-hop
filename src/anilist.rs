use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://graphql.anilist.co";

/// Set from the viewer's AniList title-language preference.
pub static ENGLISH_TITLES: AtomicBool = AtomicBool::new(false);

const MEDIA: &str = "id idMal title { romaji english } synonyms coverImage { extraLarge color }
  episodes status format season seasonYear averageScore genres description(asHtml: false)
  studios { edges { isMain node { name } } } nextAiringEpisode { episode airingAt }
  mediaListEntry { id status progress }";

/// Shows per AniList page, the most it hands out at once.
pub const PER_PAGE: usize = 50;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Media {
    pub id: u64,
    pub id_mal: Option<u64>,
    pub title: Title,
    pub synonyms: Vec<String>,
    pub cover_image: Cover,
    pub episodes: Option<u32>,
    pub status: Option<String>,
    pub format: Option<String>,
    pub season: Option<String>,
    pub season_year: Option<u32>,
    pub average_score: Option<u32>,
    pub genres: Vec<String>,
    pub description: Option<String>,
    pub studios: Studios,
    pub next_airing_episode: Option<Airing>,
    pub media_list_entry: Option<Entry>,
    /// where ani-cli listed a show AniList didn't provide (its episodes stay empty)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ani_cli: Option<crate::source::Source>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Title {
    pub romaji: Option<String>,
    pub english: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cover {
    pub extra_large: Option<String>,
    pub color: Option<String>,
}

/// Every studio with whether it made the show: AniList ignores `studios(isMain: true)` in
/// list and relation queries (Frieren from a list: Toho, MADHOUSE, Shogakukan, …), so
/// `main` picks them here. A cache from before has none until it refreshes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Studios {
    pub edges: Vec<Studio>,
}

impl Studios {
    pub fn main(&self) -> impl Iterator<Item = &str> {
        (self.edges.iter())
            .filter(|e| e.is_main)
            .map(|e| e.node.name.as_str())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Studio {
    pub is_main: bool,
    pub node: Name,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Name {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Airing {
    pub episode: u32,
    /// when it airs, in seconds since 1970; 0 in a cache from before 0.7, which kept only
    /// how long it was from then
    #[serde(default)]
    pub airing_at: i64,
}

impl Airing {
    /// Seconds until it airs at `now` (seconds since 1970); None once it has, or if unknown.
    pub fn left(&self, now: i64) -> Option<i64> {
        (self.airing_at > 0)
            .then(|| self.airing_at - now)
            .filter(|s| *s > 0)
    }
}

/// Seconds since 1970.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub status: String,
    pub progress: u32,
}

/// Ids from here up are shows only ani-cli's provider knows (AniList's are 32-bit Ints).
const ANI_CLI_ONLY: u64 = 1 << 63;

impl Media {
    /// A show found through ani-cli alone. The id hashes the search and the entry, not the
    /// shown name, so the same pick has it again next time and a provider's own "X (2)" can't
    /// take a numbered one's.
    pub fn from_ani_cli(name: &str, at: crate::source::Source) -> Media {
        let key = format!("{}\n{}\n{}", at.query, at.title, at.nth);
        let hash = crate::ani::fnv1a(key.as_bytes());
        Media {
            id: ANI_CLI_ONLY | hash,
            title: Title {
                romaji: Some(name.into()),
                english: None,
            },
            ani_cli: Some(at),
            ..Default::default()
        }
    }

    pub fn on_anilist(&self) -> bool {
        self.id < ANI_CLI_ONLY
    }

    pub fn name(&self) -> &str {
        let (romaji, english) = (self.title.romaji.as_deref(), self.title.english.as_deref());
        let pick = if ENGLISH_TITLES.load(Ordering::Relaxed) {
            english.or(romaji)
        } else {
            romaji.or(english)
        };
        pick.unwrap_or("?")
    }

    /// Episodes that are out right now (None = unknown, 0 before the first one airs). A list
    /// from the cache may say an episode is next that has aired since.
    pub fn released(&self) -> Option<u32> {
        let out = |a: &Airing| match a.airing_at > 0 && a.airing_at <= now() {
            true => a.episode,
            false => a.episode.saturating_sub(1),
        };
        let unscheduled = match self.status.as_deref() {
            Some("NOT_YET_RELEASED") => Some(0),
            _ => self.episodes,
        };
        self.next_airing_episode.as_ref().map(out).or(unscheduled)
    }
}

/// One agent for everything: its connections stay open, so a poster after the first skips
/// the TLS handshake (165 ms -> 46 ms measured here).
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent("ani-hop")
            .build()
            .into()
    })
}

/// Where `url` redirects to, without following it.
pub fn location(url: &str) -> Result<String, String> {
    let res = agent()
        .get(url)
        .config()
        .max_redirects(0)
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    let to = res.headers().get("location").and_then(|l| l.to_str().ok());
    to.map(str::to_string)
        .ok_or_else(|| format!("HTTP {}", res.status()))
}

/// Plain GET, e.g. cover images (from AniList's CDN, which has no request limit).
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let mut res = agent().get(url).call().map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let body = res.body_mut().with_config().limit(20 << 20);
    body.read_to_vec().map_err(|e| e.to_string())
}

/// What an episode is called and is about.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpInfo {
    pub title: String,
    pub about: String,
    /// its number in its TheTVDB season, which goes on from the first part's when TheTVDB
    /// counts a season split in two on AniList as one
    #[serde(default)]
    pub number: Option<u32>,
    /// when it first aired, as ani.zip has it ("1999-12-15", empty if it doesn't); None in a
    /// cache from before air dates, which is then fetched again
    #[serde(default)]
    pub aired: Option<String>,
}

impl EpInfo {
    /// Its title and summary once it's watched; before, they'd give things away (its air date
    /// doesn't).
    pub fn told(&self, watched: bool) -> (&str, &str) {
        match watched {
            true => (&self.title, &self.about),
            false => ("", ""),
        }
    }
}

/// Titles and summaries of every episode, from ani.zip (keyed by AniList id, one request a
/// show, served from Cloudflare's cache) as AniList has none per episode.
pub fn episodes(id: u64) -> Result<HashMap<String, EpInfo>, String> {
    let bytes = fetch(&format!("https://api.ani.zip/mappings?anilist_id={id}"))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(episode_info(&v))
}

fn episode_info(v: &Value) -> HashMap<String, EpInfo> {
    // AniDB writes ` for '
    let tidy = |s: Option<&str>| s.unwrap_or("").replace('`', "'").trim().to_string();
    let eps = v["episodes"].as_object().into_iter().flatten();
    eps.map(|(ep, e)| {
        let title = tidy(e["title"]["en"].as_str().or(e["title"]["x-jat"].as_str()));
        let about = e["overview"].as_str().or(e["summary"].as_str());
        let info = EpInfo {
            title: if title == format!("Episode {ep}") {
                String::new()
            } else {
                title
            },
            about: tidy(about.and_then(|a| a.split("\nSource:").next())),
            number: e["episodeNumber"].as_u64().map(|n| n as u32),
            aired: Some(tidy(e["airdate"].as_str())),
        };
        (ep.clone(), info)
    })
    .collect()
}

/// Until when AniList asked us to hold off; every API call waits for it.
static PAUSED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Seconds to hold off after a response: what a 429 says, or until the window resets once
/// only a few requests are left (AniList allows 90 a minute, at times just 30).
fn backoff(
    status: u16,
    retry_after: Option<u64>,
    remaining: Option<u64>,
    reset: Option<u64>,
    now: u64,
) -> Option<u64> {
    match (status, remaining) {
        (429, _) => Some(retry_after.unwrap_or(60)),
        (_, Some(r)) if r < 3 => Some(reset.map_or(60, |t| t.saturating_sub(now)).max(1)),
        _ => None,
    }
}

#[derive(Clone)]
pub struct Client {
    pub token: Option<String>,
}

impl Client {
    fn gql(&self, query: &str, variables: Value) -> Result<Value, String> {
        let body = json!({ "query": query, "variables": variables });
        for _ in 0..3 {
            let until = *PAUSED_UNTIL.lock().unwrap();
            if let Some(wait) = until.and_then(|t| t.checked_duration_since(Instant::now())) {
                std::thread::sleep(wait);
            }
            let mut req = agent().post(API).header("Accept", "application/json");
            if let Some(t) = &self.token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            let mut res = req
                .send_json(&body)
                .map_err(|_| "Can't reach AniList".to_string())?;
            let num = |k: &str| res.headers().get(k)?.to_str().ok()?.parse::<u64>().ok();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let status = res.status().as_u16();
            let (retry, left, reset) = (
                num("retry-after"),
                num("x-ratelimit-remaining"),
                num("x-ratelimit-reset"),
            );
            if let Some(secs) = backoff(status, retry, left, reset, now) {
                *PAUSED_UNTIL.lock().unwrap() = Some(Instant::now() + Duration::from_secs(secs));
            }
            if status == 429 {
                continue;
            }
            let mut res: Value = res.body_mut().read_json().map_err(|_| {
                format!("AniList: unexpected response (HTTP {status}), try again later")
            })?;
            if let Some(err) = res["errors"][0]["message"].as_str() {
                return Err(format!("AniList: {err}"));
            }
            return Ok(res["data"].take());
        }
        Err("AniList: too many requests, try again in a minute".into())
    }

    /// Who's logged in: (id, name, whether they read English titles).
    pub fn viewer(&self) -> Result<(u64, String, bool), String> {
        let d = self.gql(
            "{ Viewer { id name options { titleLanguage } } }",
            json!({}),
        )?;
        let v = &d["Viewer"];
        let lang = v["options"]["titleLanguage"].as_str().unwrap_or("");
        Ok((
            v["id"].as_u64().unwrap_or(0),
            v["name"].as_str().unwrap_or("?").to_string(),
            lang.starts_with("ENGLISH"),
        ))
    }

    /// `sort` is an AniList MediaSort, e.g. TRENDING_DESC; pages count from 1.
    pub fn browse(
        &self,
        sort: &str,
        search: Option<&str>,
        page: u32,
    ) -> Result<Vec<Media>, String> {
        let q = format!(
            "query($sort: [MediaSort], $search: String, $page: Int) {{ Page(page: $page, perPage: {PER_PAGE}) {{
               media(type: ANIME, sort: $sort, search: $search, isAdult: false) {{ {MEDIA} }} }} }}"
        );
        let d = self.gql(
            &q,
            json!({ "sort": [sort], "search": search, "page": page }),
        )?;
        serde_json::from_value(d["Page"]["media"].clone()).map_err(|e| e.to_string())
    }

    /// The shows with these AniList ids as AniList has them now: their list entries too, so
    /// downloads saved weeks ago show the progress of today.
    pub fn shows(&self, ids: &[u64]) -> Result<Vec<Media>, String> {
        let q = format!(
            "query($ids: [Int]) {{ Page(perPage: {PER_PAGE}) {{ media(id_in: $ids, type: ANIME) {{ {MEDIA} }} }} }}"
        );
        let mut all = Vec::new();
        for ids in ids.chunks(PER_PAGE) {
            let d = self.gql(&q, json!({ "ids": ids }))?;
            let page: Vec<Media> =
                serde_json::from_value(d["Page"]["media"].clone()).map_err(|e| e.to_string())?;
            all.extend(page);
        }
        Ok(all)
    }

    pub fn list(&self, user: u64, status: &str) -> Result<Vec<Media>, String> {
        let q = format!(
            "query($user: Int, $status: MediaListStatus) {{
               MediaListCollection(userId: $user, type: ANIME, status: $status, sort: UPDATED_TIME_DESC) {{
                 lists {{ entries {{ media {{ {MEDIA} }} }} }} }} }}"
        );
        let d = self.gql(&q, json!({ "user": user, "status": status }))?;
        let lists = d["MediaListCollection"]["lists"]
            .as_array()
            .into_iter()
            .flatten();
        // a show in a custom list comes again in its status list
        let mut seen = HashSet::new();
        lists
            .flat_map(|l| l["entries"].as_array().into_iter().flatten())
            .map(|e| serde_json::from_value(e["media"].clone()).map_err(|e| e.to_string()))
            .filter(|r: &Result<Media, String>| match r {
                Ok(m) => seen.insert(m.id),
                Err(_) => true,
            })
            .collect()
    }

    /// The show that goes on where `id` ends as more of the same series (not a movie or a
    /// special), if AniList has one.
    fn sequel(&self, id: u64) -> Result<Option<Media>, String> {
        let q = format!(
            "query($id: Int) {{ Media(id: $id) {{ relations {{ edges {{ relationType node {{ type {MEDIA} }} }} }} }} }}"
        );
        let d = self.gql(&q, json!({ "id": id }))?;
        let mut edges = d["Media"]["relations"]["edges"]
            .as_array()
            .into_iter()
            .flatten();
        let next = edges.find(|e| {
            let n = &e["node"];
            e["relationType"] == "SEQUEL"
                && n["type"] == "ANIME"
                && matches!(n["format"].as_str(), Some("TV" | "TV_SHORT" | "ONA"))
        });
        next.map(|e| serde_json::from_value(e["node"].clone()).map_err(|e| e.to_string()))
            .transpose()
    }

    /// The sequels a provider that lists `m`'s split cours as one show runs on into, up to
    /// its episode `last`, each with the episodes before it.
    pub fn parts(&self, m: &Media, last: u32) -> Result<Vec<(u32, Media)>, String> {
        let first = |id| Ok(episodes(id)?.get("1").and_then(|e| e.number));
        follow_sequels(m, last, |id| self.sequel(id), first)
    }
}

/// The user's list, written one change at a time by entries.rs.
impl crate::entries::AniList for Client {
    fn entry(&self, media: u64) -> Result<Option<Entry>, String> {
        let q = "query($id: Int) { Media(id: $id) { mediaListEntry { id status progress } } }";
        let mut d = self.gql(q, json!({ "id": media }))?;
        serde_json::from_value(d["Media"]["mediaListEntry"].take()).map_err(|e| e.to_string())
    }

    fn save(
        &self,
        media: u64,
        status: Option<&str>,
        progress: Option<u32>,
    ) -> Result<Entry, String> {
        let q = "mutation($id: Int, $status: MediaListStatus, $progress: Int) {
                   SaveMediaListEntry(mediaId: $id, status: $status, progress: $progress) { id status progress } }";
        // AniList rejects explicit nulls here, so only send what is set
        let mut vars = json!({ "id": media });
        if let Some(s) = status {
            vars["status"] = json!(s);
        }
        if let Some(p) = progress {
            vars["progress"] = json!(p);
        }
        let d = self.gql(q, vars)?;
        serde_json::from_value(d["SaveMediaListEntry"].clone()).map_err(|e| e.to_string())
    }

    fn remove(&self, entry: u64) -> Result<(), String> {
        let q = "mutation($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }";
        self.gql(q, json!({ "id": entry })).map(|_| ())
    }
}

/// Sequels after `m` until they reach episode `last`. None unless they have all finished
/// airing, add up to `last` exactly, and TheTVDB (through ani.zip, `first`) numbers each
/// one's first episode as the provider does: a provider's extra episode (a recap, an OVA)
/// mustn't count as the next season's first, which the first episodes of one still airing
/// could pass for.
fn follow_sequels(
    m: &Media,
    last: u32,
    mut sequel: impl FnMut(u64) -> Result<Option<Media>, String>,
    mut first: impl FnMut(u64) -> Result<Option<u32>, String>,
) -> Result<Vec<(u32, Media)>, String> {
    let mut parts: Vec<(u32, Media)> = Vec::new();
    let mut end = m.episodes.unwrap_or(last);
    // a few requests at most, even for a long run of seasons
    while end < last && parts.len() < 8 {
        let id = parts.last().map_or(m.id, |p| p.1.id);
        let Some(next) = sequel(id)? else { break };
        let (Some("FINISHED"), Some(n)) = (next.status.as_deref(), next.episodes) else {
            break;
        };
        let before = end;
        end = before + n;
        parts.push((before, next));
    }
    if end != last {
        return Ok(Vec::new());
    }
    for (before, p) in &parts {
        if first(p.id)? != Some(before + 1) {
            return Ok(Vec::new());
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn episodes_past_a_split_season_are_its_sequels() {
        use super::Media;
        let show = |id: u64, episodes: Option<u32>| Media {
            id,
            episodes,
            status: Some("FINISHED".into()),
            ..Default::default()
        };
        // Attack on Titan Season 3, its Part 2 (maybe still airing, with the episode next),
        // then The Final Season
        let chain = |airing: Option<u32>| {
            let mut p2 = show(104_578, Some(10));
            if let Some(episode) = airing {
                p2.status = Some("RELEASING".into());
                p2.next_airing_episode = Some(super::Airing {
                    episode,
                    airing_at: super::now() + 3600,
                });
            }
            move |id: u64| {
                Ok(match id {
                    99_147 => Some(p2.clone()),
                    104_578 => Some(show(110_277, Some(16))),
                    _ => None,
                })
            }
        };
        let s3 = show(99_147, Some(12));
        // TheTVDB counts Season 3 Part 2 on from 13, The Final Season from 1
        let tvdb = |id| {
            Ok(match id {
                104_578 => Some(13),
                _ => Some(1),
            })
        };
        let ids = |last: u32, airing: Option<u32>| {
            let parts = super::follow_sequels(&s3, last, chain(airing), tvdb).unwrap();
            (parts.iter()).map(|(b, m)| (*b, m.id)).collect::<Vec<_>>()
        };
        assert_eq!(ids(22, None), [(12, 104_578)]);
        assert_eq!(ids(38, None), [], "TheTVDB starts The Final Season over");
        assert_eq!(ids(15, None), [], "13 to 15 aren't all of Part 2");
        // a recap as 13 could pass for the first episode of one still airing
        assert_eq!(ids(13, Some(2)), []);
        assert_eq!(ids(15, Some(4)), [], "not until it has finished");
        let s1 = show(16_498, Some(25));
        let next_season = |_| Ok(Some(show(20_958, Some(12))));
        let numbered = |_| Ok(Some(26));
        let found = super::follow_sequels(&s1, 37, next_season, numbered).unwrap();
        assert_eq!(found.len(), 1, "a season TheTVDB counts on from 26");
        let restarts = |_| Ok(Some(1));
        let found = super::follow_sequels(&s1, 37, next_season, restarts).unwrap();
        assert!(found.is_empty(), "a season of its own");
        assert_eq!(ids(40, None), [], "past the last sequel");
        let unknown = |_| Ok(Some(show(104_578, None)));
        assert!(
            super::follow_sequels(&s3, 13, unknown, tvdb)
                .unwrap()
                .is_empty()
        );
        let no_more = |_| Ok(None);
        assert!(
            super::follow_sequels(&s3, 13, no_more, tvdb)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reads_episode_titles_and_summaries() {
        let v = serde_json::json!({ "episodes": {
            "1": { "title": { "en": "The Journey`s End", "x-jat": "Bouken no Owari" },
                   "summary": "It`s over.\nSource: Crunchyroll", "airdate": "2023-09-29" },
            "2": { "title": { "x-jat": "Betsu ni" }, "overview": "Magic lessons.", "episodeNumber": 14 },
            "S1": { "title": { "en": "Episode S1" } },
        }});
        let eps = super::episode_info(&v);
        assert_eq!(eps["1"].title, "The Journey's End");
        assert_eq!(eps["1"].about, "It's over.");
        assert_eq!(
            (eps["2"].title.as_str(), eps["2"].about.as_str()),
            ("Betsu ni", "Magic lessons.")
        );
        assert_eq!((eps["1"].number, eps["2"].number), (None, Some(14)));
        // a cache from before air dates has None, a show without one an empty date
        assert_eq!(eps["1"].aired.as_deref(), Some("2023-09-29"));
        assert_eq!(eps["2"].aired.as_deref(), Some(""));
        assert_eq!(
            (eps["S1"].title.as_str(), eps["S1"].about.as_str()),
            ("", "")
        );
    }

    #[test]
    fn ani_cli_shows_keep_their_id_and_stay_off_anilist() {
        let at = |query: &str| crate::source::Source {
            query: query.into(),
            index: 2,
            title: "Hunter x Hunter".into(),
            nth: 2,
            episodes: Vec::new(),
        };
        let m = super::Media::from_ani_cli("Hunter x Hunter (2)", at("hunter x hunter"));
        // saved in resolved.json, audio.json and downloads.json, so it must never change
        assert_eq!(m.id, 10_198_325_430_496_578_357);
        let other = super::Media::from_ani_cli("Hunter x Hunter (2)", at("hunter"));
        assert_ne!(m.id, other.id);
        assert!(!m.on_anilist() && super::Media::default().on_anilist());
        assert_eq!(m.name(), "Hunter x Hunter (2)");
        let back: super::Media = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.id, m.id); // above 2^53
        assert_eq!(back.ani_cli.unwrap().index, 2);
        let anilist = serde_json::to_string(&super::Media::default()).unwrap();
        assert!(!anilist.contains("aniCli"));
        let old: super::Media = serde_json::from_str(r#"{"id": 1}"#).unwrap();
        assert!(old.ani_cli.is_none());
    }

    #[test]
    fn an_episode_that_aired_since_the_list_was_cached_is_out() {
        let airing = |at| super::Airing {
            episode: 5,
            airing_at: at,
        };
        let now = super::now();
        assert_eq!(airing(now + 90).left(now), Some(90));
        assert_eq!(
            airing(now - 90).left(now),
            None,
            "aired since: no countdown"
        );
        assert_eq!(airing(0).left(now), None, "a cache from before 0.7");
        let show = |at| super::Media {
            episodes: Some(12),
            next_airing_episode: Some(airing(at)),
            ..Default::default()
        };
        assert_eq!(show(now + 90).released(), Some(4));
        assert_eq!(show(now - 90).released(), Some(5));
        assert_eq!(show(0).released(), Some(4));
        let old: super::Airing =
            serde_json::from_str(r#"{"episode": 5, "timeUntilAiring": 90}"#).unwrap();
        assert_eq!((old.episode, old.airing_at), (5, 0));
    }

    #[test]
    fn nothing_is_out_before_the_first_episode_airs() {
        let now = super::now();
        let show = |status: &str, episodes, at: Option<i64>| super::Media {
            status: Some(status.into()),
            episodes,
            next_airing_episode: at.map(|airing_at| super::Airing {
                episode: 1,
                airing_at,
            }),
            ..Default::default()
        };
        let upcoming = |episodes, at| show("NOT_YET_RELEASED", episodes, at).released();
        assert_eq!(upcoming(Some(12), None), Some(0), "announced, no date yet");
        assert_eq!(upcoming(None, None), Some(0));
        assert_eq!(upcoming(Some(12), Some(now + 90)), Some(0));
        assert_eq!(
            upcoming(Some(12), Some(now - 90)),
            Some(1),
            "a cache from before it aired"
        );
        assert_eq!(
            upcoming(Some(12), Some(0)),
            Some(0),
            "a cache from before 0.7"
        );
        assert_eq!(show("RELEASING", Some(12), None).released(), Some(12));
        assert_eq!(show("FINISHED", Some(12), None).released(), Some(12));
        assert_eq!(super::Media::default().released(), None);
    }

    #[test]
    fn backs_off_when_anilist_says_so() {
        assert_eq!(super::backoff(429, Some(46), Some(0), None, 0), Some(46));
        assert_eq!(super::backoff(429, None, None, None, 0), Some(60));
        assert_eq!(
            super::backoff(200, None, Some(2), Some(1_030), 1_000),
            Some(30)
        );
        assert_eq!(
            super::backoff(200, None, Some(2), Some(990), 1_000),
            Some(1)
        );
        assert_eq!(
            super::backoff(200, None, Some(50), Some(1_030), 1_000),
            None
        );
    }
}
