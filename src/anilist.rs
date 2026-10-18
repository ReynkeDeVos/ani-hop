use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// curl is a hard requirement anyway (ani-cli), so it does our HTTP too.
fn curl(args: &[&str], stdin: Option<String>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("curl")
        // -q must come first: ignore ~/.curlrc (it could log the token or change the output)
        .args([
            "-q",
            "-sS",
            "-g",
            "--compressed",
            "--proto",
            "=https",
            "--max-time",
            "20",
        ])
        .args(["-A", "ani-hop"])
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    match out.status.success() {
        true => Ok(out.stdout),
        false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// Plain GET, e.g. cover images. URLs can come from a `.media.json` in the download
/// folder, so only plain https gets through: no "-option", no "@file" URL list.
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("not an https URL: {url}"));
    }
    curl(&["--fail", "--max-filesize", "20M", "--url", url], None)
}

/// A quoted string for curl's config syntax.
fn quoted(s: &str) -> String {
    let s = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", s.replace('\n', "\\n").replace('\r', "\\r"))
}

#[derive(Clone)]
pub struct Client {
    pub token: Option<String>,
}

impl Client {
    fn gql(&self, query: &str, variables: Value) -> Result<Value, String> {
        // headers and body go through stdin (`--config -`), so the token never shows up in `ps`
        let body = json!({ "query": query, "variables": variables }).to_string();
        let mut config = format!(
            "header = \"Content-Type: application/json\"\nheader = \"Accept: application/json\"\ndata-binary = {}\n",
            quoted(&body)
        );
        if let Some(t) = &self.token {
            config += &format!(
                "header = {}\n",
                quoted(&format!("Authorization: Bearer {t}"))
            );
        }
        let out = curl(&["--config", "-", "--url", API], Some(config))?;
        let mut res: Value =
            serde_json::from_slice(&out).map_err(|_| "AniList: unexpected response".to_string())?;
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
}

#[cfg(test)]
mod tests {
    #[test]
    fn fetch_only_does_https() {
        let dir = std::env::temp_dir();
        let (cfg, out) = (dir.join("ani-hop-test.cfg"), dir.join("ani-hop-test.out"));
        let evil = format!(
            "output = \"{}\"\nurl = \"file:///etc/hostname\"\n",
            out.display()
        );
        std::fs::write(&cfg, evil).unwrap();
        assert!(super::fetch(&format!("-K{}", cfg.display())).is_err());
        assert!(!out.exists(), "a URL must never be read as a curl option");
        assert!(super::fetch("file:///etc/hostname").is_err());
        assert!(super::fetch("@/etc/hostname").is_err());
        let _ = std::fs::remove_file(cfg);
    }

    #[test]
    fn quotes_for_curl_config() {
        assert_eq!(super::quoted(r#"{"a":"b\nc"}"#), r#""{\"a\":\"b\\nc\"}""#);
    }
}
