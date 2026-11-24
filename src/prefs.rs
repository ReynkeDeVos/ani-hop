//! Playback settings: audio, skipping openings/endings, autoplay and preloading.
//!
//! Each per-show setting keeps a choice of its own only while it differs from Settings' global
//! choice, so changing the default in Settings carries over to every show that follows it.
//! On disk these live in `ani::config_dir()` (`audio.json`, `no-skip`/`skip.json`,
//! `autoplay`/`autoplay.json`, `preload`) so multiple ani-hop windows stay in sync.

use crate::ani;
use crate::menu::Item;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// What a show's row says when its choice differs from every show's.
pub const OWN: &str = "this show only";

/// What a show plays with, as (key, label, the label for a narrow row). The provider has
/// Japanese and English audio, both with English subtitles, which mpv shows or keeps hidden
/// (v switches).
pub const AUDIO: [(&str, &str, &str); 4] = [
    ("raw", "Japanese · no subtitles", "no subs"),
    ("sub", "Japanese · English subtitles", "subtitled"),
    ("dub", "English dub", "dubbed"),
    ("dub+sub", "English dub · English subtitles", "dub + subs"),
];

pub fn audio_label(audio: &str) -> &'static str {
    AUDIO.iter().find(|a| a.0 == audio).map_or("", |a| a.1)
}

/// The audio's label for a row too narrow for `audio_label`, which says the same.
pub fn audio_short(audio: &str) -> &'static str {
    AUDIO.iter().find(|a| a.0 == audio).map_or("", |a| a.2)
}

pub fn dub(audio: &str) -> bool {
    audio.starts_with("dub")
}

pub fn subs(audio: &str) -> bool {
    audio.ends_with("sub")
}

/// Whether a downloaded episode starts with its subtitles hidden. What the file is counts,
/// not what the show is set to now: a Japanese one keeps them unless none are wanted at all.
pub fn hide_subs(audio: &str, dub_file: bool) -> bool {
    audio == "raw" || (dub_file && !subs(audio))
}

/// A yes-or-no playback setting that applies globally in Settings and can be overridden per show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    /// Whether mpv jumps over openings and endings. Its chapters mark them either way.
    Skip,
    /// Whether an episode played to its end saves it watched and plays the next.
    Autoplay,
}

impl Toggle {
    pub const fn key(self) -> &'static str {
        match self {
            Toggle::Skip => "skip",
            Toggle::Autoplay => "autoplay",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "skip" => Some(Toggle::Skip),
            "autoplay" => Some(Toggle::Autoplay),
            _ => None,
        }
    }

    /// (menu label, toast name, suffix when switched on).
    pub const fn words(self) -> (&'static str, &'static str, &'static str) {
        match self {
            // short enough for its on/off at 80 columns
            Toggle::Skip => ("Skip openings/endings", "Skipping openings and endings", ""),
            Toggle::Autoplay => (
                "Autoplay next episode",
                "Autoplay",
                " · an episode that ends is marked watched",
            ),
        }
    }

    const fn files(self) -> (&'static str, bool, &'static str) {
        match self {
            // flag_on = false: absence of `no-skip` means skipping is on (for existing installs)
            Toggle::Skip => ("no-skip", false, "skip.json"),
            Toggle::Autoplay => ("autoplay", true, "autoplay.json"),
        }
    }
}

/// All playback settings resolved once for one show, plus which ones differ from Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShowPrefs {
    pub audio: &'static str,
    pub audio_own: bool,
    pub skip: bool,
    pub skip_own: bool,
    pub autoplay: bool,
    pub autoplay_own: bool,
}

impl ShowPrefs {
    pub fn dub(self) -> bool {
        dub(self.audio)
    }

    /// The "♪  Playback…" row on a show's page: shows audio, plus any per-show skip or autoplay
    /// overrides so nothing hides behind the submenu.
    pub fn playback_row(self, key: &str) -> Item {
        let mut it = Item::new(key, "♪  Playback…", "");
        if !self.skip_own && !self.autoplay_own {
            it.hint = match self.audio_own {
                true => format!("{} · {OWN}", audio_label(self.audio)),
                false => audio_label(self.audio).into(),
            };
            it.short = audio_short(self.audio).into();
            return it;
        }
        let audio = audio_short(self.audio);
        let mut overrides = Vec::new();
        if self.audio_own {
            overrides.push(audio);
        }
        if self.skip_own {
            overrides.push(if self.skip { "skip on" } else { "skip off" });
        }
        if self.autoplay_own {
            overrides.push(if self.autoplay {
                "autoplay on"
            } else {
                "autoplay off"
            });
        }
        let mut full = Vec::new();
        if !self.audio_own {
            full.push(audio);
        }
        full.extend_from_slice(&overrides);
        full.push(OWN);
        it.hint = full.join(" · ");
        // overrides first so narrow 80-column rows drop trailing audio rather than hiding them
        if !self.audio_own {
            overrides.push(audio);
        }
        it.short = overrides.join(" · ");
        it
    }
}

#[cfg(test)]
#[derive(Default)]
struct Memory {
    audio: HashMap<String, String>,
    skip_default: bool,
    skip: HashMap<String, bool>,
    autoplay_default: bool,
    autoplay: HashMap<String, bool>,
    preload: bool,
}

enum Store {
    /// Reads and writes `ani::config_dir()` (or a custom dir in tests) so multiple ani-hops stay
    /// in sync.
    Disk(Option<PathBuf>),
    /// Isolated in-memory store for `App` tests so parallel tests share no config files.
    #[cfg(test)]
    Memory(Memory),
}

pub struct Prefs {
    store: Store,
}

impl Prefs {
    pub fn disk() -> Self {
        Prefs {
            store: Store::Disk(None),
        }
    }

    #[cfg(test)]
    pub fn in_dir(dir: PathBuf) -> Self {
        Prefs {
            store: Store::Disk(Some(dir)),
        }
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Prefs {
            store: Store::Memory(Memory {
                skip_default: true,
                ..Default::default()
            }),
        }
    }

    fn dir(dir: &Option<PathBuf>) -> PathBuf {
        dir.clone().unwrap_or_else(ani::config_dir)
    }

    pub fn for_show(&self, id: u64) -> ShowPrefs {
        let default_audio = self.audio(None);
        let audio = self.audio(Some(id));
        let skip_own = self.toggle_own(Toggle::Skip, Some(id));
        let autoplay_own = self.toggle_own(Toggle::Autoplay, Some(id));
        ShowPrefs {
            audio,
            audio_own: audio != default_audio,
            skip: skip_own.unwrap_or_else(|| self.toggle(Toggle::Skip, None)),
            skip_own: skip_own.is_some(),
            autoplay: autoplay_own.unwrap_or_else(|| self.toggle(Toggle::Autoplay, None)),
            autoplay_own: autoplay_own.is_some(),
        }
    }

    /// A show's audio (None: the default): its own choice, else the default, else ani-cli's
    /// ANI_CLI_MODE.
    pub fn audio(&self, id: Option<u64>) -> &'static str {
        match &self.store {
            Store::Disk(dir) => {
                let all: HashMap<String, String> =
                    ani::read_json(&Self::dir(dir).join("audio.json"));
                audio_in(&all, id)
            }
            #[cfg(test)]
            Store::Memory(mem) => audio_in(&mem.audio, id),
        }
    }

    /// Sets audio for show `id` (None: global default). Returns true when a show's choice now
    /// matches the default and follows it again.
    pub fn set_audio(&mut self, id: Option<u64>, audio: &str) -> std::io::Result<bool> {
        match &mut self.store {
            Store::Disk(dir) => {
                let mut follows = false;
                ani::edit_json(
                    &Self::dir(dir).join("audio.json"),
                    |all: &mut HashMap<String, String>| {
                        follows = choose_audio(all, id, audio);
                    },
                )?;
                Ok(follows)
            }
            #[cfg(test)]
            Store::Memory(mem) => Ok(choose_audio(&mut mem.audio, id, audio)),
        }
    }

    /// Show `id`'s value for `toggle` (None: every show's).
    pub fn toggle(&self, toggle: Toggle, id: Option<u64>) -> bool {
        self.toggle_own(toggle, id)
            .unwrap_or_else(|| self.toggle_default(toggle))
    }

    /// What show `id` has of its own for `toggle`, if it differs from Settings.
    pub fn toggle_own(&self, toggle: Toggle, id: Option<u64>) -> Option<bool> {
        let id = id?;
        match &self.store {
            Store::Disk(dir) => {
                let (_, _, shows_file) = toggle.files();
                let shows: HashMap<String, bool> = ani::read_json(&Self::dir(dir).join(shows_file));
                shows.get(&id.to_string()).copied()
            }
            #[cfg(test)]
            Store::Memory(mem) => {
                let map = match toggle {
                    Toggle::Skip => &mem.skip,
                    Toggle::Autoplay => &mem.autoplay,
                };
                map.get(&id.to_string()).copied()
            }
        }
    }

    fn toggle_default(&self, toggle: Toggle) -> bool {
        match &self.store {
            Store::Disk(dir) => {
                let (flag, flag_on, _) = toggle.files();
                Self::dir(dir).join(flag).exists() == flag_on
            }
            #[cfg(test)]
            Store::Memory(mem) => match toggle {
                Toggle::Skip => mem.skip_default,
                Toggle::Autoplay => mem.autoplay_default,
            },
        }
    }

    /// Sets `toggle` for show `id` (None: every show). Returns true when a show's choice now
    /// matches Settings and follows it again.
    pub fn set_toggle(
        &mut self,
        toggle: Toggle,
        id: Option<u64>,
        on: bool,
    ) -> std::io::Result<bool> {
        match &mut self.store {
            Store::Disk(dir) => {
                let dir = Self::dir(dir);
                let (flag, flag_on, shows_file) = toggle.files();
                // every show's first: if that fails, the shows' own still differ from it
                if id.is_none() {
                    write_flag(&dir, flag, on == flag_on)?;
                }
                let all = dir.join(flag).exists() == flag_on;
                ani::edit_json(
                    &dir.join(shows_file),
                    |shows: &mut HashMap<String, bool>| {
                        set_override(shows, id, on, &all);
                    },
                )?;
                Ok(id.is_some() && on == all)
            }
            #[cfg(test)]
            Store::Memory(mem) => {
                let (default, map) = match toggle {
                    Toggle::Skip => (&mut mem.skip_default, &mut mem.skip),
                    Toggle::Autoplay => (&mut mem.autoplay_default, &mut mem.autoplay),
                };
                if id.is_none() {
                    *default = on;
                }
                let all = *default;
                set_override(map, id, on, &all);
                Ok(id.is_some() && on == all)
            }
        }
    }

    /// Whether the next episode's stream is fetched ahead (Settings): off saves the requests.
    pub fn preload(&self) -> bool {
        match &self.store {
            Store::Disk(dir) => Self::dir(dir).join("preload").exists(),
            #[cfg(test)]
            Store::Memory(mem) => mem.preload,
        }
    }

    pub fn set_preload(&mut self, on: bool) -> std::io::Result<()> {
        match &mut self.store {
            Store::Disk(dir) => write_flag(&Self::dir(dir), "preload", on),
            #[cfg(test)]
            Store::Memory(mem) => {
                mem.preload = on;
                Ok(())
            }
        }
    }

    /// The row for choosing show `id`'s audio (None: every show's), saying the one now: shorter
    /// where the row is narrow.
    pub fn audio_item(&self, label: &str, id: Option<u64>) -> Item {
        let audio = self.audio(id);
        let mut it = Item::new("audio", label, audio_label(audio));
        it.short = audio_short(audio).into();
        if id.is_some() && audio != self.audio(None) {
            it.hint = format!("{} · {OWN}", it.hint);
        }
        it
    }

    /// A yes-or-no setting's row, for show `id` (None: every show).
    pub fn toggle_item(&self, toggle: Toggle, id: Option<u64>) -> Item {
        let state = if self.toggle(toggle, id) { "on" } else { "off" };
        let mut it = Item::new(toggle.key(), toggle.words().0, state);
        if self.toggle_own(toggle, id).is_some() {
            it.hint = format!("{state} · {OWN}");
            it.short = state.into();
        }
        it
    }
}

/// Global skip setting from disk, for `--skip-flags` invoked by an older ani-hop without
/// `ANIHOP_JUMP`.
pub fn skip_all() -> bool {
    Prefs::disk().toggle(Toggle::Skip, None)
}

/// Show audio from disk, for legacy queue files saved before `Job.dub` existed.
pub fn audio_from_disk(id: Option<u64>) -> &'static str {
    Prefs::disk().audio(id)
}

fn write_flag(dir: &Path, name: &str, present: bool) -> std::io::Result<()> {
    let path = dir.join(name);
    match present {
        true => fs::create_dir_all(dir).and_then(|()| fs::write(path, "")),
        false => match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// A show keeps a choice of its own only while it differs from every show's (`default`).
fn set_override<T: PartialEq>(map: &mut HashMap<String, T>, id: Option<u64>, val: T, default: &T) {
    match id {
        Some(id) if val == *default => {
            map.remove(&id.to_string());
        }
        Some(id) => {
            map.insert(id.to_string(), val);
        }
        None => map.retain(|_, own| own != &val),
    }
}

fn audio_in(all: &HashMap<String, String>, id: Option<u64>) -> &'static str {
    let known = |a: Option<&String>| {
        AUDIO
            .iter()
            .map(|k| k.0)
            .find(|k| a.is_some_and(|a| a == k))
    };
    let env = std::env::var("ANI_CLI_MODE").is_ok_and(|m| m == "dub");
    known(id.and_then(|i| all.get(&i.to_string())))
        .or_else(|| known(all.get("all")))
        .unwrap_or(if env { "dub" } else { "sub" })
}

/// A show keeps an audio choice of its own only while it differs from the default.
/// Returns true when a show's choice now matches the default.
fn choose_audio(all: &mut HashMap<String, String>, id: Option<u64>, audio: &str) -> bool {
    let default = audio_in(all, None).to_string();
    set_override(all, id, audio.to_string(), &default);
    if id.is_none() {
        all.insert("all".into(), audio.to_string());
    }
    id.is_some() && audio == default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_show_keeps_its_own_toggle_only_while_it_differs() {
        let dir = std::env::temp_dir().join(format!("ani-hop-prefs-toggle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // writing a flag on a brand-new config directory creates it instead of failing ENOENT
        let mut p = Prefs::in_dir(dir.clone());
        assert!(!p.toggle(Toggle::Autoplay, None) && !p.toggle(Toggle::Autoplay, Some(1)));
        assert!(!p.set_toggle(Toggle::Autoplay, None, true).unwrap());
        assert!(p.toggle(Toggle::Autoplay, None));
        assert!(!p.set_toggle(Toggle::Autoplay, None, false).unwrap());
        // per-show override
        assert!(!p.set_toggle(Toggle::Autoplay, Some(1), true).unwrap());
        assert_eq!(
            (
                p.toggle(Toggle::Autoplay, Some(1)),
                p.toggle_own(Toggle::Autoplay, Some(1)),
                p.toggle(Toggle::Autoplay, Some(2))
            ),
            (true, Some(true), false)
        );
        p.set_toggle(Toggle::Autoplay, None, true).unwrap();
        assert_eq!(
            p.toggle_own(Toggle::Autoplay, Some(1)),
            None,
            "show 1 matches every show's now and follows it"
        );
        assert!(p.toggle(Toggle::Autoplay, Some(2)));
        assert!(!p.set_toggle(Toggle::Autoplay, Some(2), false).unwrap());
        assert!(
            p.set_toggle(Toggle::Autoplay, Some(2), true).unwrap(),
            "reports following Settings again"
        );
        assert_eq!(
            p.toggle_own(Toggle::Autoplay, Some(2)),
            None,
            "back to every show's, nothing of its own left"
        );
        p.set_toggle(Toggle::Autoplay, None, false).unwrap();
        p.set_toggle(Toggle::Autoplay, None, false).unwrap();
        assert!(
            !p.toggle(Toggle::Autoplay, Some(1)),
            "switching off twice is fine"
        );
        // Skip defaults to on (flag_on = false, creates `no-skip` when turned off)
        assert!(p.toggle(Toggle::Skip, None));
        p.set_toggle(Toggle::Skip, None, false).unwrap();
        assert!(!p.toggle(Toggle::Skip, None) && dir.join("no-skip").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_per_show_only_while_it_differs() {
        let mut all = HashMap::from([("all".to_string(), "sub".to_string())]);
        assert!(!choose_audio(&mut all, Some(1), "dub"));
        assert_eq!(
            (audio_in(&all, Some(1)), audio_in(&all, Some(2))),
            ("dub", "sub")
        );
        choose_audio(&mut all, None, "dub");
        assert_eq!(
            all.len(),
            1,
            "show 1 matches the default now and follows it"
        );
        choose_audio(&mut all, None, "raw");
        assert_eq!(audio_in(&all, Some(1)), "raw");
        assert!(!choose_audio(&mut all, Some(1), "dub+sub"));
        assert!(choose_audio(&mut all, Some(1), "raw"));
        assert_eq!(all.len(), 1, "back to the default, nothing of its own left");
        all.insert("2".into(), "klingon".into());
        assert_eq!(audio_in(&all, Some(2)), "raw", "unknown choices fall back");
        assert!(dub("dub+sub") && subs("dub+sub") && !subs("dub") && subs("sub") && !dub("raw"));
        // downloaded: Japanese files keep their subtitles under "English dub"
        assert!(!hide_subs("dub", false) && hide_subs("dub", true) && hide_subs("raw", false));
        assert!(!hide_subs("dub+sub", true) && !hide_subs("sub", true));
    }

    #[test]
    fn playback_row_surfaces_per_show_skip_and_autoplay_overrides() {
        let mut p = Prefs::memory();
        p.set_audio(None, "sub").unwrap();
        p.set_toggle(Toggle::Skip, None, false).unwrap();
        let plain = p.for_show(1).playback_row("playback");
        assert_eq!(plain.hint, "Japanese · English subtitles");
        assert_eq!(plain.short, "subtitled");

        p.set_toggle(Toggle::Skip, Some(1), true).unwrap();
        let skip = p.for_show(1).playback_row("playback");
        assert_eq!(skip.hint, "subtitled · skip on · this show only");
        assert_eq!(skip.short, "skip on · subtitled");

        p.set_audio(Some(1), "dub").unwrap();
        p.set_toggle(Toggle::Autoplay, Some(1), true).unwrap();
        let all = p.for_show(1).playback_row("playback");
        assert_eq!(all.hint, "dubbed · skip on · autoplay on · this show only");
        assert_eq!(all.short, "dubbed · skip on · autoplay on");
    }
}
