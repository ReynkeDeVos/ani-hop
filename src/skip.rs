//! Where an episode's opening and ending are, for mpv to jump over. The provider's own times come
//! first, as they're for the very stream being played; AniSkip's fill in what it doesn't have.

use crate::ani;
use crate::anilist::{Media, fetch_within};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// What each source gets to answer: a downloaded episode plays without skipping after twice this.
const LIMIT: Duration = Duration::from_millis(2500);

/// A longer one can only be a mistake, and would jump over the episode: of 1,274 from both
/// sources, the longest was 287 seconds.
const LONGEST: f64 = 300.0;

/// What the provider's page keeps its settings under: base64 of JSON, XORed with this key.
const PROVIDER_KEY: &[u8] = b"otaku-embed-v1";

/// What the helper standing in for ani-skip reads, set for a show's last episode.
pub const LAST_EPISODE: &str = "ANIHOP_LAST_EPISODE";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
struct Span {
    start: f64,
    end: f64,
}

/// From the network, so nothing about it is taken on trust.
fn usable(span: &Span) -> bool {
    span.start >= 0.0 && span.start < span.end && span.end - span.start <= LONGEST
}

#[derive(Debug, Default, PartialEq)]
pub struct Times {
    op: Option<Span>,
    ed: Option<Span>,
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut have) = (0u32, 0);
    for b in text.bytes().take_while(|b| *b != b'=') {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = bits << 6 | u32::from(v);
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    Some(out)
}

/// The times on the provider's page for an episode, which ani-cli reads and doesn't use.
fn from_provider(page: &str) -> Times {
    #[derive(Deserialize)]
    struct Settings {
        skip: Option<Skip>,
    }
    #[derive(Deserialize)]
    struct Skip {
        intro: Option<Span>,
        outro: Option<Span>,
    }
    let blob = page
        .split_once("window.__P=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .and_then(|(blob, _)| base64(blob))
        .unwrap_or_default();
    let keys = PROVIDER_KEY.iter().cycle();
    let json: Vec<u8> = blob.iter().zip(keys).map(|(b, k)| b ^ k).collect();
    let settings = serde_json::from_slice::<Settings>(&json).ok();
    match settings.and_then(|s| s.skip) {
        Some(skip) => Times {
            op: skip.intro.filter(usable),
            ed: skip.outro.filter(usable),
        },
        None => Times::default(),
    }
}

/// AniSkip's answer to `/v2/skip-times`.
fn from_aniskip(body: &[u8]) -> Times {
    #[derive(Deserialize)]
    struct Answer {
        results: Vec<Found>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Found {
        interval: Interval,
        skip_type: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Interval {
        start_time: f64,
        end_time: f64,
    }
    let Ok(answer) = serde_json::from_slice::<Answer>(body) else {
        return Times::default();
    };
    let of = |kind: &str| {
        let found = answer.results.iter().find(|f| f.skip_type == kind)?;
        let span = Span {
            start: found.interval.start_time,
            end: found.interval.end_time,
        };
        usable(&span).then_some(span)
    };
    Times {
        op: of("op"),
        ed: of("ed"),
    }
}

/// The provider's times, with AniSkip asked only for what's missing, each part on its own.
fn merged(provider: Times, aniskip: impl FnOnce() -> Times) -> Times {
    let mut times = provider;
    if times.op.is_none() || times.ed.is_none() {
        let other = aniskip();
        times.op = times.op.or(other.op);
        times.ed = times.ed.or(other.ed);
    }
    // parts from two sources can overlap, and an ending can't start inside the opening
    if let (Some(op), Some(ed)) = (times.op, times.ed)
        && ed.start < op.end
    {
        times.ed = None;
    }
    times
}

pub fn times(mal: u64, ep: &str, dub: bool) -> Times {
    // goes into URLs, and comes from ani-cli, which got it from the provider's page
    if ep.is_empty() || !ep.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Times::default();
    }
    let mode = if dub { "dub" } else { "sub" };
    // shortcut: knows by name the one server ani-cli reads, so if that changes, AniSkip is what's
    // left; follow it when it does
    let page = format!("https://zokoanime.video/stream/mal/{mal}/{ep}/{mode}");
    let provider = fetch_within(&page, LIMIT)
        .map(|page| from_provider(&String::from_utf8_lossy(&page)))
        .unwrap_or_default();
    let aniskip = || {
        // episodeLength 0: any cut. shortcut: knowing the stream's length takes two more requests
        // and changed the answer for 1 episode of 20; send it if that turns out to matter
        let url = format!(
            "https://api.aniskip.com/v2/skip-times/{mal}/{ep}?types%5B%5D=op&types%5B%5D=ed&episodeLength=0"
        );
        fetch_within(&url, LIMIT)
            .map(|body| from_aniskip(&body))
            .unwrap_or_default()
    };
    merged(provider, aniskip)
}

/// A chapter on mpv's timeline: start, end, title.
type Chapter = (f64, f64, &'static str);

/// Where `times` put mpv's chapters, and the parts to jump over: with `jump`, all but the ending of
/// a show's `last` episode, as the story can go on after the credits there.
fn plan(times: &Times, jump: bool, last: bool) -> (Vec<Chapter>, Vec<(&'static str, Span)>) {
    let (mut chapters, mut skips) = (Vec::new(), Vec::new());
    let mut story = 0.0;
    let parts = [
        (times.op, "Opening", "op", jump),
        (times.ed, "Ending", "ed", jump && !last),
    ];
    for (span, title, part, skipped) in parts {
        let Some(span) = span else { continue };
        if story < span.start {
            chapters.push((story, span.start, "Episode"));
        }
        chapters.push((span.start, span.end, title));
        if skipped {
            skips.push((part, span));
        }
        story = span.end;
    }
    // mpv ignores a chapter's END, so without this one what follows the last part would count as
    // that part, like a scene after the ending. The episode's length isn't known here.
    if !chapters.is_empty() {
        chapters.push((story, story, "Episode"));
    }
    (chapters, skips)
}

/// The flags that tell mpv about `times` before it starts, for a player of the user's own: the
/// chapters mark them on its timeline whether or not it skips, and ani-hop's mpv script reads the
/// script-opts and jumps over them. No spaces, as ani-cli splits them on those, and inline, so
/// there's no file to clean up.
pub fn flags(times: &Times, jump: bool, last: bool) -> Vec<String> {
    let (chapters, skips) = plan(times, jump, last);
    if chapters.is_empty() {
        return Vec::new();
    }
    let mut text = String::from(";FFMETADATA1\n");
    for (start, end, title) in chapters {
        let (start, end) = (start * 1000.0, end * 1000.0);
        text +=
            &format!("[CHAPTER]\nTIMEBASE=1/1000\nSTART={start:.0}\nEND={end:.0}\nTITLE={title}\n");
    }
    let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
    let mut flags = vec![format!("--chapters-file=hex://{hex}")];
    if !skips.is_empty() {
        let opts: Vec<String> = (skips.iter())
            .map(|(part, s)| format!("skip-{part}_start={},skip-{part}_end={}", s.start, s.end))
            .collect();
        flags.push(format!("--script-opts={}", opts.join(",")));
    }
    flags
}

/// What ani-hop's mpv script hears when it asks for `times` once mpv has started: the chapters and
/// the parts to jump over.
fn json(times: &Times, jump: bool, last: bool) -> String {
    let (chapters, skips) = plan(times, jump, last);
    let chapters: Vec<_> = (chapters.iter())
        .map(|(time, _, title)| serde_json::json!({ "time": time, "title": title }))
        .collect();
    let mut skip = serde_json::Map::new();
    for (part, s) in skips {
        skip.insert(format!("{part}_start"), s.start.into());
        skip.insert(format!("{part}_end"), s.end.into());
    }
    serde_json::json!({ "chapters": chapters, "skip": skip }).to_string()
}

/// `ani-hop --skip-json`, which ani-hop's mpv script runs in the background.
pub fn json_for_script(mal: &str, ep: &str, dub: bool, last: bool, jump: bool) -> String {
    match mal.parse() {
        Ok(mal) => json(&times(mal, ep, dub), jump, last),
        Err(_) => json(&Times::default(), false, false),
    }
}

/// The flag that has ani-hop's mpv script ask for the times itself, so mpv starts without waiting
/// for them: the chapters arrive a moment later, and the jumps with them. Whether to jump is
/// decided now, so an episode fetched ahead keeps what it was fetched with.
pub fn ask_later(mal: u64, ep: &str, dub: bool, last: bool) -> Option<String> {
    // a comma would end the option, and it goes into URLs
    if ep.is_empty() || !ep.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let yes = |on: bool| if on { "yes" } else { "no" };
    let (dub, last, jump) = (yes(dub), yes(last), yes(ani::skip_on()));
    Some(format!(
        "--script-opts=ani-hop-mal={mal},ani-hop-ep={ep},ani-hop-dub={dub},ani-hop-last={last},ani-hop-jump={jump}"
    ))
}

/// What ani-cli's stand-in for ani-skip prints, for ani-cli to hand to mpv.
pub fn flags_for_ani_cli(mal: &str, ep: &str) -> String {
    let Ok(mal) = mal.parse() else {
        return String::new();
    };
    let dub = std::env::var("ANI_CLI_MODE").is_ok_and(|m| m == "dub");
    let last = std::env::var_os(LAST_EPISODE).is_some();
    let player = std::env::var("ANI_CLI_PLAYER").unwrap_or_default();
    // ani-hop's mpv stand-in, which loads the script
    if player == "mpv" {
        return ask_later(mal, ep, dub, last).unwrap_or_default();
    }
    let mut flags = flags(&times(mal, ep, dub), ani::skip_on(), last);
    let jumps = flags.iter().any(|f| f.starts_with("--script-opts="));
    if jumps && let Some(script) = script_flag(&player, &ani::skip_script()) {
        flags.push(script);
    }
    flags.join(" ")
}

/// The flag that loads `script` into a player of the user's own, which ani-hop's mpv stand-in never
/// sees. It goes with the times to jump over, the one thing ani-cli hands to every mpv it starts,
/// through Syncplay too. ani-cli splits them on spaces, so a path with one goes without.
fn script_flag(player: &str, script: &Path) -> Option<String> {
    let path = script.to_str()?;
    let own = !player.is_empty() && player != "mpv";
    (own && !path.contains(char::is_whitespace)).then(|| format!("--script={path}"))
}

/// Whether `ep` ends the show. `listed` is the provider's list of its episodes, which can go on
/// past AniList's count, as a season split in two on AniList can. Where the status is unknown, the
/// provider's last counts as well: credits cost less than a lost epilogue.
pub fn last_episode(m: &Media, listed: &[String], ep: &str) -> bool {
    let number = ep.parse::<f64>().ok();
    let count = m.episodes.map(f64::from);
    match listed.last() {
        // a downloaded file, which has only AniList's count to go by. shortcut: the end of a split
        // season goes unnoticed, a list of its episodes would need saving with the download
        None => number.is_some() && number == count,
        Some(last) if last != ep => false,
        Some(_) => {
            let over = matches!(m.status.as_deref(), None | Some("FINISHED" | "CANCELLED"));
            over || number.zip(count).is_some_and(|(n, count)| n >= count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: f64, end: f64) -> Option<Span> {
        Some(Span { start, end })
    }

    fn encode(bytes: &[u8]) -> String {
        const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n =
                chunk.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
            for i in 0..4 {
                match i <= chunk.len() {
                    true => out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char),
                    false => out.push('='),
                }
            }
        }
        out
    }

    /// A page as the provider serves it.
    fn page(json: &str) -> String {
        let keys = PROVIDER_KEY.iter().cycle();
        let bytes: Vec<u8> = json.bytes().zip(keys).map(|(b, k)| b ^ k).collect();
        format!(
            "<html><script>window.__P=\"{}\";window.other=\"x\"</script></html>",
            encode(&bytes)
        )
    }

    #[test]
    fn base64_reads_every_padding_and_refuses_what_is_not_base64() {
        for text in ["", "a", "ab", "abc", "abcd", "\u{ff}\u{0}?>"] {
            assert_eq!(base64(&encode(text.as_bytes())).unwrap(), text.as_bytes());
        }
        assert_eq!(base64("a b"), None);
    }

    #[test]
    fn the_provider_gives_its_times_for_each_part_it_has() {
        let both = page(
            r#"{"src":"x","skip":{"intro":{"start":67,"end":156},"outro":{"start":1370.5,"end":1459}},"auto_skip":false}"#,
        );
        let want = Times {
            op: span(67.0, 156.0),
            ed: span(1370.5, 1459.0),
        };
        assert_eq!(from_provider(&both), want);
        let no_ending = page(r#"{"skip":{"intro":{"start":0,"end":123},"outro":null}}"#);
        assert_eq!(
            from_provider(&no_ending),
            Times {
                op: span(0.0, 123.0),
                ed: None,
            }
        );
    }

    #[test]
    fn the_provider_having_nothing_or_nonsense_gives_nothing() {
        for bad in [
            "<html>an episode it doesn't have</html>".to_string(),
            "window.__P=\"not base64!\"".to_string(),
            "window.__P=\"unterminated".to_string(),
            page("not json"),
            page(r#"{"src":"x"}"#),
            page(r#"{"skip":null}"#),
            page(r#"{"skip":{"intro":{"start":"0; rm -rf","end":9}}}"#),
        ] {
            assert_eq!(from_provider(&bad), Times::default(), "{bad}");
        }
    }

    #[test]
    fn a_time_that_makes_no_sense_is_dropped_on_its_own() {
        for bad in [
            r#"{"start":0,"end":0}"#, // how the other server says none
            r#"{"start":90,"end":10}"#,
            r#"{"start":-5,"end":10}"#,
            r#"{"start":0,"end":1400}"#, // would jump over the episode
        ] {
            let json =
                format!(r#"{{"skip":{{"intro":{bad},"outro":{{"start":1300,"end":1390}}}}}}"#);
            assert_eq!(
                from_provider(&page(&json)),
                Times {
                    op: None,
                    ed: span(1300.0, 1390.0),
                },
                "{bad}"
            );
        }
    }

    #[test]
    fn aniskip_v2_answers_are_read() {
        let body = br#"{"found":true,"results":[{"interval":{"startTime":67.1,"endTime":157.1},"skipType":"op","skipId":"x","episodeLength":1470.078},{"interval":{"startTime":5,"endTime":9},"skipType":"recap","skipId":"y","episodeLength":1470}],"message":"Successfully found skip times","statusCode":200}"#;
        assert_eq!(
            from_aniskip(body),
            Times {
                op: span(67.1, 157.1),
                ed: None,
            }
        );
        let none =
            br#"{"found":false,"results":[],"message":"No skip times found","statusCode":404}"#;
        assert_eq!(from_aniskip(none), Times::default());
        assert_eq!(from_aniskip(b"<html>"), Times::default());
    }

    #[test]
    fn aniskip_fills_in_only_what_the_provider_lacks() {
        let op = |at| span(at, at + 90.0);
        let ed = |at| span(at, at + 90.0);
        let times = |op, ed| Times { op, ed };
        let never = || -> Times { panic!("AniSkip asked for nothing") };

        // the provider's win, with AniSkip not even asked
        assert_eq!(
            merged(times(op(10.0), ed(1300.0)), never),
            times(op(10.0), ed(1300.0))
        );
        // each part on its own
        let aniskip = || times(op(20.0), ed(1200.0));
        assert_eq!(
            merged(times(op(10.0), None), aniskip),
            times(op(10.0), ed(1200.0))
        );
        assert_eq!(
            merged(times(None, ed(1300.0)), aniskip),
            times(op(20.0), ed(1300.0))
        );
        assert_eq!(merged(Times::default(), aniskip), aniskip());
        assert_eq!(merged(Times::default(), Times::default), Times::default());
    }

    #[test]
    fn a_last_episode_keeps_its_ending_but_shows_where_it_is() {
        let both = Times {
            op: span(10.0, 100.0),
            ed: span(1300.0, 1390.0),
        };
        let f = flags(&both, true, true);
        assert_eq!(f[1], "--script-opts=skip-op_start=10,skip-op_end=100");
        assert!(chapters(&f[0]).contains("START=1300000\nEND=1390000\nTITLE=Ending\n"));
        // an ending alone has nothing to jump over, so no script
        let only_ed = Times { op: None, ..both };
        assert_eq!(flags(&only_ed, true, true).len(), 1);
    }

    #[test]
    fn the_chapters_are_there_with_skipping_off() {
        let both = Times {
            op: span(10.0, 100.0),
            ed: span(1300.0, 1390.0),
        };
        let off = flags(&both, false, false);
        assert_eq!(off.len(), 1, "{off:?}");
        assert_eq!(off[0], flags(&both, true, false)[0]);
        assert!(flags(&Times::default(), false, false).is_empty());
    }

    #[test]
    fn an_ending_inside_the_opening_goes() {
        let provider = Times {
            op: span(0.0, 100.0),
            ed: None,
        };
        let aniskip = || Times {
            op: None,
            ed: span(90.0, 180.0),
        };
        assert_eq!(merged(provider, aniskip).ed, None);
    }

    /// What the flags say, read back the way mpv does.
    fn chapters(flag: &str) -> String {
        let hex = flag.strip_prefix("--chapters-file=hex://").unwrap();
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn flags_name_the_times_for_the_script_and_the_timeline() {
        let f = flags(
            &Times {
                op: span(1.5, 91.5),
                ed: span(1300.0, 1390.25),
            },
            true,
            false,
        );
        assert_eq!(
            f[1],
            "--script-opts=skip-op_start=1.5,skip-op_end=91.5,skip-ed_start=1300,skip-ed_end=1390.25"
        );
        let want = [
            "START=0\nEND=1500\nTITLE=Episode\n",
            "START=1500\nEND=91500\nTITLE=Opening\n",
            "START=91500\nEND=1300000\nTITLE=Episode\n",
            "START=1300000\nEND=1390250\nTITLE=Ending\n",
            "START=1390250\nEND=1390250\nTITLE=Episode\n",
        ]
        .map(|part| format!("[CHAPTER]\nTIMEBASE=1/1000\n{part}"))
        .concat();
        assert_eq!(chapters(&f[0]), format!(";FFMETADATA1\n{want}"));
        // ani-cli splits them on spaces and hands them on unquoted
        assert!(f.iter().all(|flag| !flag.contains(char::is_whitespace)));
    }

    #[test]
    fn flags_say_only_what_is_known() {
        assert!(flags(&Times::default(), true, false).is_empty());
        let only_op = flags(
            &Times {
                op: span(0.0, 90.0),
                ed: None,
            },
            true,
            false,
        );
        assert_eq!(only_op[1], "--script-opts=skip-op_start=0,skip-op_end=90");
        let text = chapters(&only_op[0]);
        assert!(!text.contains("Ending"));
        // nothing comes before an opening at the very start
        assert!(!text.contains("START=0\nEND=0\n"), "{text}");
        assert!(
            text.contains("START=90000\nEND=90000\nTITLE=Episode\n"),
            "{text}"
        );
        let only_ed = flags(
            &Times {
                op: None,
                ed: span(1300.0, 1390.0),
            },
            true,
            false,
        );
        assert_eq!(
            only_ed[1],
            "--script-opts=skip-ed_start=1300,skip-ed_end=1390"
        );
        let text = chapters(&only_ed[0]);
        assert!(!text.contains("Opening"));
        assert!(
            text.contains("START=0\nEND=1300000\nTITLE=Episode\n"),
            "{text}"
        );
        assert!(
            text.contains("START=1390000\nEND=1390000\nTITLE=Episode\n"),
            "{text}"
        );
    }

    #[test]
    fn an_ending_right_after_the_opening_has_no_episode_between_them() {
        let f = flags(
            &Times {
                op: span(0.0, 90.0),
                ed: span(90.0, 180.0),
            },
            true,
            false,
        );
        assert_eq!(chapters(&f[0]).matches("TITLE=Episode").count(), 1);
    }

    #[test]
    fn the_script_hears_the_chapters_and_only_what_to_jump_over() {
        let both = Times {
            op: span(10.0, 100.0),
            ed: span(1300.0, 1390.0),
        };
        let read = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();
        let on = read(json(&both, true, false));
        let titles: Vec<_> = on["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Episode", "Opening", "Episode", "Ending", "Episode"]
        );
        assert_eq!(on["chapters"][3]["time"], 1300.0);
        assert_eq!(
            on["skip"],
            serde_json::json!({ "op_start": 10.0, "op_end": 100.0, "ed_start": 1300.0, "ed_end": 1390.0 })
        );
        let off = read(json(&both, false, false));
        assert_eq!(off["chapters"], on["chapters"]);
        assert_eq!(off["skip"], serde_json::json!({}));
        let last = read(json(&both, true, true));
        assert_eq!(
            last["skip"],
            serde_json::json!({ "op_start": 10.0, "op_end": 100.0 })
        );
        let none = read(json_for_script("x", "1", false, false, true));
        assert_eq!(none, serde_json::json!({ "chapters": [], "skip": {} }));
    }

    #[test]
    fn asking_later_says_the_episode_in_one_option() {
        let flag = ask_later(59978, "1", true, false).unwrap();
        assert!(flag.starts_with("--script-opts=ani-hop-mal=59978,ani-hop-ep=1,ani-hop-dub=yes,ani-hop-last=no,ani-hop-jump="), "{flag}");
        assert!(!flag.contains(char::is_whitespace));
        for ep in ["", "1,ani-hop-jump=yes", "12 3"] {
            assert_eq!(ask_later(1, ep, false, false), None, "{ep}");
        }
    }

    #[test]
    fn a_player_of_the_users_own_gets_the_script_with_the_times() {
        let script = Path::new("/c/bin/ani-hop-skip.lua");
        let flag = Some("--script=/c/bin/ani-hop-skip.lua".to_string());
        assert_eq!(script_flag("/opt/mpv/mpv", script), flag);
        assert_eq!(script_flag("syncplay", script), flag);
        // ani-hop's stand-in has it for plain mpv
        assert_eq!(script_flag("mpv", script), None);
        assert_eq!(script_flag("", script), None);
        // ani-cli would split it in two, and mpv play the second half as a file
        let spaced = Path::new("/home/a b/ani-hop-skip.lua");
        assert_eq!(script_flag("/opt/mpv/mpv", spaced), None);
    }

    #[test]
    fn an_episode_number_that_is_not_one_asks_nobody() {
        for ep in ["", "1/../../x", "1?a=b", "-1", "12 3"] {
            assert_eq!(times(1, ep, false), Times::default(), "{ep}");
        }
    }

    #[test]
    fn the_last_episode_is_where_the_provider_ends_on_a_show_with_nothing_to_come() {
        let show = |episodes, status: Option<&str>| Media {
            episodes,
            status: status.map(str::to_string),
            ..Media::default()
        };
        let upto = |n: u32| (1..=n).map(|e| e.to_string()).collect::<Vec<_>>();
        let done = show(Some(12), Some("FINISHED"));
        assert!(last_episode(&done, &upto(12), "12"));
        assert!(!last_episode(&done, &upto(12), "11"));
        // Attack on Titan Season 3: 22 at the provider, 12 and 10 on AniList
        for ep in ["12", "13", "21"] {
            assert!(!last_episode(&done, &upto(22), ep), "{ep}");
        }
        assert!(last_episode(&done, &upto(22), "22"));
        // airing: the newest is the last only once it's the final one
        let airing = show(Some(12), Some("RELEASING"));
        assert!(!last_episode(&airing, &upto(8), "8"));
        assert!(last_episode(&airing, &upto(12), "12"));
        // airing with no end in sight
        assert!(!last_episode(
            &show(None, Some("RELEASING")),
            &upto(12),
            "12"
        ));
        // done, without a count
        assert!(last_episode(&show(None, Some("FINISHED")), &upto(12), "12"));
        assert!(last_episode(&show(None, Some("CANCELLED")), &upto(7), "7"));
        // a show AniList doesn't know might be either: this errs on watching the credits
        assert!(last_episode(&show(None, None), &upto(12), "12"));
        assert!(!last_episode(&show(None, None), &upto(12), "11"));
    }

    #[test]
    fn a_downloaded_episode_has_only_anilists_count_to_go_by() {
        let done = Media {
            episodes: Some(12),
            status: Some("FINISHED".into()),
            ..Media::default()
        };
        assert!(last_episode(&done, &[], "12"));
        assert!(!last_episode(&done, &[], "11"));
        assert!(
            !last_episode(&done, &[], "13"),
            "the rest of a split season"
        );
        assert!(!last_episode(&done, &[], "special"));
        assert!(!last_episode(&Media::default(), &[], "12"));
    }
}
