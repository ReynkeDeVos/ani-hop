use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://graphql.anilist.co";

/// Set from the viewer's AniList title-language preference.
pub static ENGLISH_TITLES: AtomicBool = AtomicBool::new(false);

const MEDIA: &str = "id idMal title { romaji english } synonyms coverImage { extraLarge color }
  episodes status format season seasonYear averageScore genres description(asHtml: false)
  studios(isMain: true) { nodes { name } } nextAiringEpisode { episode timeUntilAiring }
  mediaListEntry { id status progress }";

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Studios {
    pub nodes: Vec<Name>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Name {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Airing {
    pub episode: u32,
    pub time_until_airing: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub status: String,
    pub progress: u32,
}

impl Media {
    pub fn name(&self) -> &str {
        let (romaji, english) = (self.title.romaji.as_deref(), self.title.english.as_deref());
        let pick = if ENGLISH_TITLES.load(Ordering::Relaxed) {
            english.or(romaji)
        } else {
            romaji.or(english)
        };
        pick.unwrap_or("?")
    }

    pub fn progress(&self) -> u32 {
        self.media_list_entry.as_ref().map_or(0, |e| e.progress)
    }

    /// Episodes that are out right now (None = unknown).
    pub fn released(&self) -> Option<u32> {
        self.next_airing_episode
            .as_ref()
            .map(|a| a.episode - 1)
            .or(self.episodes)
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

/// Plain GET, e.g. cover images (from AniList's CDN, which has no request limit).
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let mut res = agent().get(url).call().map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let body = res.body_mut().with_config().limit(20 << 20);
    body.read_to_vec().map_err(|e| e.to_string())
}

/// Changes to the user's list go out one at a time, in the order they were made.
static SAVES: Mutex<()> = Mutex::new(());

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
            let mut res = req.send_json(&body).map_err(|e| e.to_string())?;
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
            let mut res: Value = res
                .body_mut()
                .read_json()
                .map_err(|_| "AniList: unexpected response".to_string())?;
            if let Some(err) = res["errors"][0]["message"].as_str() {
                return Err(format!("AniList: {err}"));
            }
            return Ok(res["data"].take());
        }
        Err("AniList: too many requests, try again in a minute".into())
    }

    pub fn viewer(&self) -> Result<(u64, String), String> {
        let d = self.gql(
            "{ Viewer { id name options { titleLanguage } } }",
            json!({}),
        )?;
        let v = &d["Viewer"];
        let lang = v["options"]["titleLanguage"].as_str().unwrap_or("");
        ENGLISH_TITLES.store(lang.starts_with("ENGLISH"), Ordering::Relaxed);
        Ok((
            v["id"].as_u64().unwrap_or(0),
            v["name"].as_str().unwrap_or("?").to_string(),
        ))
    }

    /// `sort` is an AniList MediaSort, e.g. TRENDING_DESC.
    pub fn browse(&self, sort: &str, search: Option<&str>) -> Result<Vec<Media>, String> {
        let q = format!(
            "query($sort: [MediaSort], $search: String) {{ Page(perPage: 50) {{
               media(type: ANIME, sort: $sort, search: $search, isAdult: false) {{ {MEDIA} }} }} }}"
        );
        let d = self.gql(&q, json!({ "sort": [sort], "search": search }))?;
        serde_json::from_value(d["Page"]["media"].clone()).map_err(|e| e.to_string())
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
        lists
            .flat_map(|l| l["entries"].as_array().into_iter().flatten())
            .map(|e| serde_json::from_value(e["media"].clone()).map_err(|e| e.to_string()))
            .collect()
    }

    pub fn save(
        &self,
        media: u64,
        status: Option<&str>,
        progress: Option<u32>,
    ) -> Result<Entry, String> {
        let q = "mutation($id: Int, $status: MediaListStatus, $progress: Int) {
                   SaveMediaListEntry(mediaId: $id, status: $status, progress: $progress) { id status progress } }";
        let _turn = SAVES.lock().unwrap_or_else(|e| e.into_inner());
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

    pub fn remove(&self, entry: u64) -> Result<(), String> {
        let _turn = SAVES.lock().unwrap_or_else(|e| e.into_inner());
        let q = "mutation($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }";
        self.gql(q, json!({ "id": entry })).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
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
