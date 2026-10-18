use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    pub token: Option<String>,
}

impl Client {
    pub fn new(token: Option<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build();
        Client {
            agent: config.into(),
            token,
        }
    }

    fn gql(&self, query: &str, variables: Value) -> Result<Value, String> {
        let mut req = self.agent.post(API).header("Accept", "application/json");
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut res: Value = req
            .send_json(json!({ "query": query, "variables": variables }))
            .map_err(|e| e.to_string())?
            .body_mut()
            .read_json()
            .map_err(|e| e.to_string())?;
        if let Some(err) = res["errors"][0]["message"].as_str() {
            return Err(format!("AniList: {err}"));
        }
        Ok(res["data"].take())
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
        let mut out = Vec::new();
        for list in d["MediaListCollection"]["lists"]
            .as_array()
            .into_iter()
            .flatten()
        {
            for e in list["entries"].as_array().into_iter().flatten() {
                out.push(serde_json::from_value(e["media"].clone()).map_err(|e| e.to_string())?);
            }
        }
        Ok(out)
    }

    pub fn save(
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

    pub fn remove(&self, entry: u64) -> Result<(), String> {
        let q = "mutation($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }";
        self.gql(q, json!({ "id": entry })).map(|_| ())
    }

    pub fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut res = self.agent.get(url).call().map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status()));
        }
        res.body_mut()
            .with_config()
            .limit(20 << 20)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }
}
