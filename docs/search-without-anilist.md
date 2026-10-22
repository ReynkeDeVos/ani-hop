# Searching and streaming while AniList is down

Researched on 2026-10-22 on the branch `search-without-anilist` (based on `b0fca55`, release 0.6.0), and revised the same day after an independent review (section 8). The ani-cli source is master at [`21ed4a0`](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli) (version 5.1.5). It is byte-for-byte the copy installed in `~/.local/bin`. Every claim links to its source. Anything marked *(inferred)* was deduced from the code and not run. Built in 0.6.1 as recommended in section 5; section 7 says what was run.

## 1. Summary and recommendation

| Question | Answer |
|---|---|
| **Where the user gets stuck** | Search only asks AniList ([`src/main.rs:1449`](../src/main.rs#L1449)). On any error the empty results screen is removed and the error goes to the red line ([`src/main.rs:1629`](../src/main.rs#L1629)). ani-cli is never asked. |
| **What the red line says** | AniList documents its outage answer as a 403 with *"The AniList API has been temporarily disabled due to severe stability issues. Please check the official AniList Discord for more information."* ([considerations](https://docs.anilist.co/guide/considerations#api-stability)). ani-hop prints that as `AniList: The AniList API has been temporarily disabled…` ([`src/anilist.rs:243`](../src/anilist.rs#L243)). That this was yesterday's message is *(inferred)*: no string in ani-hop says "down". |
| **What already works without AniList** | Anything opened from a list cached earlier: the show page, the episode list, streaming, downloads, the Downloads view, audio per show and "Change source…". All of these run on ani-cli and local files (section 2.4). |
| **What ani-cli needs** | A search text and a position in its results. ani-cli 5.1.x scrapes hianime.at and never talks to AniList ([L442](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L442)). allanime, `search_anime` and `episodes_list` ended with 4.15 (section 3.1). |
| **New ani-cli plumbing** | No new ani-cli calls, flags or menu parsing: the existing `ani::search` and `ani::episodes` are enough (section 3.4). What's new is on ani-hop's side. It must remember which entry the user picked, because the provider lists different shows under the same title (`Hunter x Hunter` twice: 148 and 62 episodes). |
| **Recommendation** | When the AniList search fails, fill the same Search screen from ani-cli. Each provider entry becomes a `Media` with an id outside AniList's range and the entry it came from (query, position, title). Equal titles are numbered, "Hunter x Hunter (2)". `resolve` plays exactly that entry, and `locate` keeps the second of two equal titles. A show found through ani-cli never replaces an AniList show's saved data in Downloads. Its page says it isn't synced, and list status, "Open on AniList", "Mark as watched?" and the ani.zip request are skipped for it. |
| **Size** | About 70 lines of code in `anilist.rs`, `ani.rs`, `main.rs` and `ui.rs`, plus about 60 lines of tests (section 5). The first version of this note said 30 lines; that version lost Downloads data and could play the wrong show (section 8). |
| **Rejected** | A separate "Search ani-cli" entry as the main fix, an enum or `Option` for the id, a second metadata source, an "offline mode" flag, and reusing saved AniList shows for ani-cli hits (sections 4 and 8). |

## 2. Today's flow

### 2.1 Search, step by step

1. The main menu has "Search…" ([`src/main.rs:858`](../src/main.rs#L858)). Picking it pushes the input screen `View::Input(Input::Search)` ([`src/main.rs:1387`](../src/main.rs#L1387)).
2. Enter calls `submit` ([`src/main.rs:1373`](../src/main.rs#L1373)). For `Input::Search` it calls `push_loading` with the title "Search: …", `Feed::Whole`, no disk cache, and the job `c.browse("SEARCH_MATCH", Some(&text), 1)` ([`src/main.rs:1446-1451`](../src/main.rs#L1446-L1451)).
3. `push_loading` pushes an empty `View::Shows` screen, sets it loading, and runs the job on a thread that sends `Msg::Shows(id, result)` ([`src/main.rs:699-716`](../src/main.rs#L699-L716)). Without a cache key there is nothing to show meanwhile ([`src/main.rs:692-693`](../src/main.rs#L692-L693)).
4. `browse` sends one GraphQL query for 50 shows ([`src/anilist.rs:266-281`](../src/anilist.rs#L266-L281)) through `gql`. The shared agent treats HTTP errors as normal responses and gives up after 20 seconds ([`src/anilist.rs:116-117`](../src/anilist.rs#L116-L117)). On a 429, `gql` waits as long as AniList asks and tries up to 3 times ([`src/anilist.rs:213-248`](../src/anilist.rs#L213-L248)).
5. `Msg::Shows` goes to `fill` ([`src/main.rs:1673`](../src/main.rs#L1673)). On success the list fills in. An empty list is a success too, so "no results" shows an empty screen. On an error, a screen with no items is removed and the error becomes a red toast ([`src/main.rs:1629-1634`](../src/main.rs#L1629-L1634)). The footer shows it in red for 4 seconds ([`src/ui.rs:758-762`](../src/ui.rs#L758-L762)). The user is back on the search prompt with nothing to pick.

**This is the stop.** Nothing on this path calls ani-cli. `ani::search` only runs for "Change source…" ([`src/main.rs:1478-1482`](../src/main.rs#L1478-L1482)) and inside `resolve` and `locate` ([`src/ani.rs:489-563`](../src/ani.rs#L489-L563)). All three start from a `Media` that came from AniList.

### 2.2 The error texts

| Cause | Text in the red line | Source |
|---|---|---|
| AniList switched off (documented 403 with a GraphQL error) | `AniList: The AniList API has been temporarily disabled due to severe stability issues. …` | [`src/anilist.rs:243-245`](../src/anilist.rs#L243-L245), [AniList considerations](https://docs.anilist.co/guide/considerations#api-stability) ([source](https://github.com/AniList/docs/blob/03281c0a4bbf0c7f2097e0c935cddaed1096aa65/docs/guide/considerations.md#L14-L33)) |
| Any other GraphQL error, with any status code | `AniList: <message>` | AniList says errors can come even with a 200 ([errors](https://docs.anilist.co/guide/graphql/errors)) |
| A response that isn't JSON (for example a proxy's 502 page) | `AniList: unexpected response` | [`src/anilist.rs:239-242`](../src/anilist.rs#L239-L242) |
| Rate limited 3 times in a row | `AniList: too many requests, try again in a minute` | [`src/anilist.rs:248`](../src/anilist.rs#L248), [rate limiting](https://docs.anilist.co/guide/rate-limiting) |
| No connection, DNS failure, timeout | ureq's own text, **without** "AniList": `timeout: global`, `host not found`, `io: …` | [`src/anilist.rs:222`](../src/anilist.rs#L222), [ureq 3.4.2 `Display`](https://docs.rs/ureq/3.4.2/src/ureq/error.rs.html#232-241) |

### 2.3 Opening a show, the episode list and playing

1. Picking a show calls `open_show`. It starts `resolve(&m, None)` in the background, so "Episodes…" opens instantly later, and pushes `View::Show(m)` ([`src/main.rs:1486-1489`](../src/main.rs#L1486-L1489)).
2. `App::resolve` acts at once on a known source. Otherwise it records what the user wants in `resolving`, shows the toast "Searching ani-cli for …" ([`src/main.rs:1536`](../src/main.rs#L1536)), and runs `ani::resolve` on a thread ([`src/main.rs:1518-1544`](../src/main.rs#L1518-L1544)).
3. `ani::resolve` searches ani-cli with the AniList romaji title, then the English title, then a plain version ([`src/ani.rs:509-519`](../src/ani.rs#L509-L519)). It picks the closest title ([`src/ani.rs:427-449`](../src/ani.rs#L427-L449)). If several titles tie, a finished show's episode count decides ([`src/ani.rs:542-555`](../src/ani.rs#L542-L555)). The result is a `Source { query, index, title, episodes }` ([`src/ani.rs:31-38`](../src/ani.rs#L31-L38)).
4. `Msg::Resolved` stores the source under `m.id`, marks it fresh for the session, and saves all sources to `resolved.json` ([`src/main.rs:1779-1781`](../src/main.rs#L1779-L1781), [`src/ani.rs:474-485`](../src/ani.rs#L474-L485)). It then does what was waiting (`fulfill`), or parks it until the show's page is on top again ([`src/main.rs:1784-1789`](../src/main.rs#L1784-L1789)).
5. "Episodes…" lists `Source.episodes` plus episodes on disk ([`src/main.rs:1027-1047`](../src/main.rs#L1027-L1047)).
6. `play` picks a step ([`src/main.rs:1574-1579`](../src/main.rs#L1574-L1579)). With no file on disk and no source yet, the step is `SEARCHING` ([`src/main.rs:162`](../src/main.rs#L162)) and `play` hands over to `resolve(Want::Play)` ([`src/main.rs:1588-1590`](../src/main.rs#L1588-L1590)). When the source arrives, `Msg::Resolved` takes over the waiting pop-up and plays ([`src/main.rs:1773-1775`](../src/main.rs#L1773-L1775)). A source cached from an earlier session is checked again with one search first (`locate`, [`src/ani.rs:489-498`](../src/ani.rs#L489-L498)). Then `ani::stream` runs `ani-cli --exit-after-play -S <index> -e <ep> <query>` ([`src/ani.rs:695-708`](../src/ani.rs#L695-L708)).
7. After a stream ends and the user is logged in, ani-hop asks "Mark episode N of … as watched?" ([`src/main.rs:1835-1839`](../src/main.rs#L1835-L1839)). Yes saves to AniList ([`src/main.rs:1358-1369`](../src/main.rs#L1358-L1369)).

### 2.4 What depends on AniList

| Feature | Needs AniList | During an outage today | Source |
|---|---|---|---|
| Search | yes, only path | fails, nothing else is tried | 2.1 |
| Trending, Top rated, All-time popular | yes | a list cached earlier stays, with a red toast; with no cache the screen goes away | [`src/main.rs:1383-1384`](../src/main.rs#L1383-L1384), [`src/main.rs:1630`](../src/main.rs#L1630) |
| My lists (Watching is "continue watching") | yes, for a refresh | the cached copy shows, red toast | [`src/main.rs:1424-1429`](../src/main.rs#L1424-L1429) |
| Login check at start | yes | toast "AniList login failed: …"; the token stays, since only "Invalid token" or "Unauthorized" log out | [`src/main.rs:600-603`](../src/main.rs#L600-L603), [`src/main.rs:1738`](../src/main.rs#L1738) |
| Show page | no, built from the `Media` that was opened | works | [`src/main.rs:950-1002`](../src/main.rs#L950-L1002) |
| Episode list | no, from ani-cli; cached in `resolved.json` by AniList id | works | 2.3 |
| Episode titles and summaries | no AniList request; ani.zip, keyed by AniList id | works while ani.zip is up; cached for 1 or 30 days | [`src/anilist.rs:155-161`](../src/anilist.rs#L155-L161), [`src/main.rs:392-414`](../src/main.rs#L392-L414) |
| Posters | the URL comes from AniList; images come from AniList's CDN | covers on disk show; new ones need the CDN; without a poster the cover color, or the accent color, fills the space | [`src/main.rs:374-388`](../src/main.rs#L374-L388), [`src/ui.rs:619-632`](../src/ui.rs#L619-L632) |
| Streaming | no | works; ani-skip gets its MAL id from ani-cli's embed page | [`src/ani.rs:683-717`](../src/ani.rs#L683-L717), [ani-cli L232](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L232), [L367](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L367) |
| Downloads | no; the folder is the index entry with this id, else `m.name()` | works | [`src/ani.rs:806-812`](../src/ani.rs#L806-L812), [`src/ani.rs:922-974`](../src/ani.rs#L922-L974) |
| Downloads view, playing files | no; `downloads.json` keeps the whole `Media` per folder | works; ani-skip uses the saved MAL id | [`src/ani.rs:751-823`](../src/ani.rs#L751-L823), [`src/ani.rs:729`](../src/ani.rs#L729) |
| List status, "Mark as watched?", progress | yes | red toast; the change is not kept or retried | [`src/main.rs:1251-1266`](../src/main.rs#L1251-L1266), [`src/anilist.rs:306-331`](../src/anilist.rs#L306-L331) |
| Audio per show | no; `audio.json` by AniList id | works | [`src/ani.rs:624-676`](../src/ani.rs#L624-L676) |
| Change source | no; `sources.json` by AniList id | works | [`src/ani.rs:455-464`](../src/ani.rs#L455-L464) |
| Resuming unfinished downloads | no; the queue keeps the whole `Media` | works | [`src/main.rs:116-127`](../src/main.rs#L116-L127), [`src/main.rs:493`](../src/main.rs#L493) |
| Update check | no, GitHub | works | [`src/main.rs:289-307`](../src/main.rs#L289-L307) |

The code already means for cached entries to "stay usable offline" ([`src/main.rs:1617`](../src/main.rs#L1617)). Search is the one entry point with no cache and no other way in.

## 3. What ani-cli does without AniList

### 3.1 ani-cli 5.1.x scrapes hianime.at, not allanime

The names `search_anime`, `episodes_list` and allanime come from ani-cli 4.x. At [v4.15](https://github.com/pystardust/ani-cli/blob/72d7f724a1b4ae3a2b416f178a9b10a314dec796/ani-cli#L336) (2026-07-23) there are `search_anime` ([L336](https://github.com/pystardust/ani-cli/blob/72d7f724a1b4ae3a2b416f178a9b10a314dec796/ani-cli#L336)), `episodes_list` ([L358](https://github.com/pystardust/ani-cli/blob/72d7f724a1b4ae3a2b416f178a9b10a314dec796/ani-cli#L358)) and `allanime_base="allanime.day"` ([L496](https://github.com/pystardust/ani-cli/blob/72d7f724a1b4ae3a2b416f178a9b10a314dec796/ani-cli#L496)). [v5.0](https://github.com/pystardust/ani-cli/blob/a6ac60277fe863b093b18057174240dca102dee4/ani-cli#L381) switched to anidb.app (`anidb_search`, [L157](https://github.com/pystardust/ani-cli/blob/a6ac60277fe863b093b18057174240dca102dee4/ani-cli#L157)). [v5.1](https://github.com/pystardust/ani-cli/blob/c99221dcecb35c66815a7a77e5ebebde16c8d06a/ani-cli#L429), the version ani-hop requires ([`src/ani.rs:212`](../src/ani.rs#L212)), switched to hianime.at (`hianime_search`, [L186](https://github.com/pystardust/ani-cli/blob/c99221dcecb35c66815a7a77e5ebebde16c8d06a/ani-cli#L186)). Master is the same. The script never mentions AniList: a case-insensitive search for "anilist" in it finds nothing.

### 3.2 Search, pick, episodes (master `21ed4a0`)

1. Spaces in the query become `+` ([L611-L612](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L611-L612)).
2. `hianime_search` fetches `https://hianime.at/search?keyword=<query>` ([L443](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L443)). It turns every result into `slug<TAB>title` ([L199-L210](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L199-L210)). It is one request, so only the first results page *(inferred)*. A Cloudflare challenge ends in "Blocked by cloudflare." ([L202-L205](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L202-L205)).
3. No results: `die "No results found!"` ([L615](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L615)). No connection: `die "Connection error: could not fetch … (no HTTP response; curl exit N)"` ([L161-L167](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L161-L167)).
4. With `-S n` it takes the n-th line ([L624](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L624)). Otherwise it numbers the lines and shows the menu "Select anime: " ([L625](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L625)). The menu shows only number and title; the slug is cut away ([L26](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L26)). So ani-hop can tell two entries with the same title apart only by their position. With a single line, `nth` skips the menu ([L23](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L23)).
5. `hianime_episodes` fetches the episode list for the slug's number and yields `episode-id<TAB>episode` ([L213-L219](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L213-L219)). The episode comes from `-e` or the menu "Select episode: " ([L629-L632](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L629-L632)). No episodes at all ends in "Invalid episode selection".
6. Then it plays or downloads ([L642](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L642)). `play_episode` fetches the stream links *before* it looks at the player ([L366](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L366) before [L372](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L372)), and stops with "No sources found for sub!" when that fails ([L258](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L258)). The stream, the subtitles and the MAL id for ani-skip all come from hianime's embed page ([L222-L242](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L222-L242)).

### 3.3 How ani-hop drives it

Without a terminal on stdin, ani-cli uses rofi when it finds one ([L480-L483](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L480-L483)). ani-hop puts a fake `rofi` first on PATH. It writes the prompt and the lines to a file and exits 1, so ani-cli stops there ([`src/ani.rs:24-29`](../src/ani.rs#L24-L29), [`src/ani.rs:319-328`](../src/ani.rs#L319-L328)). `probe` sets the player to `debug`, the mode to sub and a private history folder ([`src/ani.rs:363-371`](../src/ani.rs#L363-L371)).

| ani-cli step | ani-hop | Arguments |
|---|---|---|
| search, "Select anime" | `ani::search` ([`src/ani.rs:386-398`](../src/ani.rs#L386-L398)) | `<query>` |
| episodes | `ani::episodes` ([`src/ani.rs:400-405`](../src/ani.rs#L400-L405)) | `-S <index> <query>` |
| stream | `ani::stream` ([`src/ani.rs:683-717`](../src/ani.rs#L683-L717)) | `--exit-after-play -S <index> -e <ep> <query>` |
| download | `ani::download` ([`src/ani.rs:922-974`](../src/ani.rs#L922-L974)) | `-d -S <index> -e <ep> <query>` |

Each of these needs only a query and an index, which is what `Source` holds. So a show found through ani-cli alone needs no new ani-cli calls. ani-hop does have to keep the query and index the user picked, not search by title again (section 4.1).

Two things about a single hit, both existing behaviour:
- ani-cli prints no title then, so ani-hop uses the query as the title ([`src/ani.rs:395-396`](../src/ani.rs#L395-L396)).
- With a single hit *and* a single episode, ani-cli skips both menus and goes on to the debug player, which first fetches the stream links (3.2, step 6). The code knows: "a single-episode show gets its stream fetched here" ([`src/ani.rs:367`](../src/ani.rs#L367)). If that fetch fails, `ani::search` returns ani-cli's error ([`src/ani.rs:380`](../src/ani.rs#L380)) although the show exists. ani-cli has no option to stop before playing ([option list, L493-L561](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L493-L561)), so this limit stays.

### 3.4 Run here on 2026-10-22

A shell copy of `probe` (same fake rofi, same `ANIHOP_MENU_OUT`, `ANI_CLI_PLAYER=debug`, `ANI_CLI_MODE=sub`, private `ANI_CLI_HIST_DIR`, stdin from `/dev/null`), against ani-cli 5.1.5. Nothing was played or downloaded; the debug player only prints links.

| Run | Result | Time |
|---|---|---|
| `ani-cli frieren` | "Select anime: " with 4 lines, `1 Frieren: Beyond Journey's End` first | 182 ms |
| `ani-cli -S 2 frieren` | "Select episode: " with 10 episodes | 295 ms |
| `ani-cli "Frieren: Beyond Journey's End Season 2"` | single hit, several episodes: straight to "Select episode: " | 333 ms |
| `ani-cli "Hunter x Hunter Pilot"` | single hit, one episode: no menu; "hianime.at links fetched", then the debug output; exit 0 | 994 ms |
| `ani-cli "Sousou no Frieren 3rd Season"` | single hit, no episodes yet: no menu, "Invalid episode selection" | 356 ms |
| `ani-cli xqzvnotananime` | no menu, "No results found!" | — |
| `ani-cli "hunter x hunter"` | 10 lines; `Hunter x Hunter` at 1 and 2. The search page has the slugs `hunter-x-hunter-1-1393` and `hunter-x-hunter-1229` for them | 305 ms |
| `ani-cli -S 1 "hunter x hunter"` / `-S 2` | 148 episodes / 62 episodes: two different shows | 295 / 301 ms |
| `ani-cli frieren` with a dead `https_proxy` | "Connection error: could not fetch https://hianime.at/search?keyword=frieren (no HTTP response; curl exit 7)" | 19 ms |

AniList answered normally at the time (HTTP 200).

## 4. Design options

### 4.1 Questions every option has to answer

**A show with no AniList id.** Close to 70 lines use a media id (`m.id`, `media.id`) as a key: screens, posters, sources, downloads, audio and the queue. Three files on disk use it as well: `resolved.json`, `audio.json` and `sources.json`. `downloads.json` and the queue save the whole `Media`. A synthetic `u64` keeps all of that working unchanged:

- **Top bit set** (`1 << 63 | hash`). AniList ids are GraphQL `Int`s, and the spec defines those as signed 32-bit ([GraphQL spec](https://spec.graphql.org/October2021/#sec-Int)). So these ids can't collide with a real show. If one ever reached a save, AniList would reject it rather than change another show's entry *(inferred)*.
- **A hash of the search text and the name, not a counter.** The id is saved (sources, audio, downloads), so the same pick must give the same id in the next run. A counter would hand a saved id to another show the next day. With the search text in the hash, `Hunter x Hunter` from two different searches gets two ids. Each can be a different provider entry, so neither can pick up the other's cached source in `resolved.json`.
- **FNV-1a, not `DefaultHasher`.** std says `DefaultHasher`'s algorithm "is not specified, and so it and its hashes should not be relied upon over releases" ([std docs](https://doc.rust-lang.org/std/hash/struct.DefaultHasher.html)). FNV-1a is one fold over the bytes.
- **The entry it came from travels with it.** A new field `ani_cli: Option<Source>` on `Media` holds the query, the position and the provider title (episodes stay empty there). AniList never sends it, so it is `None` for every AniList show. Saved copies in `downloads.json` and the queue keep it, so a resumed download also finds the right entry. The field is skipped when empty, so the files of today stay the same.
- **Why not "Change source…".** `sources.json` keeps only (query, title), and `resolve` then takes the *first* hit with that title ([`src/ani.rs:537`](../src/ani.rs#L537)). That can't point at the second `Hunter x Hunter`. Adding the index to that tuple would make every existing `sources.json` unreadable, and `read_json` would quietly drop all saved choices ([`src/ani.rs:73-78`](../src/ani.rs#L73-L78)).
- **Equal titles get numbers:** `Hunter x Hunter` and `Hunter x Hunter (2)`, in ani-cli's order. The number is the lowest one not already a title in the list, as the provider may have its own `X (2)` (8.2). The name is what the list shows, what the id hashes and what the folder is called, so the two never share a menu key ([`src/menu.rs:66-80`](../src/menu.rs#L66-L80)) or a download folder. After resolving, the preview tells them apart: "ani-cli: Hunter x Hunter · 62 episodes" ([`src/ui.rs:701-706`](../src/ui.rs#L701-L706)).
- **Playing exactly that entry.** `resolve` uses the stored entry and doesn't match titles. It runs one search with the stored query and finds the entry again: the nth entry with its title, so the second `Hunter x Hunter` stays the second when other shows move in front (8.2). A single hit is taken as it is, and an entry that can't be found, or one picked as a single hit whose title ani-cli never showed, is matched by title the way AniList shows are. Then it reads that entry's episodes. A source cached from an earlier run goes through `locate`. Before, `locate` took the first equal title ([`src/ani.rs:491`](../src/ani.rs#L491)); it now uses the same rule. "Change source…" still wins: `resolve` uses the stored entry only when no source was chosen.

**Downloads must keep AniList's data.** `ani::folder` falls back to `download_root()/name` ([`src/ani.rs:811`](../src/ani.rs#L811)). `download` writes the show into `downloads.json` before it checks whether the episode is already there ([`src/ani.rs:931-935`](../src/ani.rs#L931-L935)). So without a rule, downloading from an ani-cli hit named like an existing AniList folder replaces that folder's saved `Media`. Downloads would lose its poster, details and progress, and downloaded files would lose ani-skip, which needs the saved MAL id ([`src/ani.rs:729`](../src/ani.rs#L729)). The rule: a show found through ani-cli is written only into a new entry; an AniList show is always written. In the other direction, downloading the same show later from its AniList page upgrades the entry to the AniList data. The folder is still shared when the names match. That is right for the same show. For a different show with the same name, its episodes would mix into that folder, and an episode number already there would be skipped (section 5.3).

What the id-keyed parts then do with such a show:

| Part | Behaviour |
|---|---|
| Results list | titles only, numbered when equal, no year or episode count ([`src/main.rs:920-939`](../src/main.rs#L920-L939)) |
| Show page | "Play episode 1", "Episodes…", "Download episodes…", "Audio for this show…", "Change source…". "List status…" and "Open on AniList" are hidden ([`src/main.rs:982-1000`](../src/main.rs#L982-L1000)) |
| Preview | the name, a line "Found through ani-cli: no AniList details or progress sync", the accent color where the poster goes (no cover URL, [`src/main.rs:801`](../src/main.rs#L801)), and once resolved "ani-cli: <title> · N episodes" |
| Episode list, streaming, ani-skip while streaming | as for any show; ani-cli finds the MAL id itself |
| Episode titles | not requested: ani.zip is keyed by AniList id and answered 404 for such an id (tried) ([`src/main.rs:806-811`](../src/main.rs#L806-L811)) |
| Watched marks, "Continue with" | none: progress is always 0 ([`src/anilist.rs:92-94`](../src/anilist.rs#L92-L94)), so the page keeps offering episode 1. The preview line says why, also when the show is reopened from Downloads |
| "Mark as watched?" | skipped ([`src/main.rs:1835`](../src/main.rs#L1835)) |
| Downloads | a new folder named after the show, or the existing one with that name, whose saved AniList data stays. Listed with its name and no poster. A downloaded file plays without ani-skip, as there is no MAL id |
| Audio, Change source, resolved cache, resumed downloads | work, keyed by the synthetic id |

**Telling the failures apart.** No sorting of error kinds is needed:
- *No results on AniList* is `Ok(vec![])` and shows an empty list. That stays as it is; ani-cli isn't asked.
- *AniList failed* (403, 5xx, timeout, GraphQL error, rate limit) is any `Err`. Try ani-cli.
- *Network down entirely*: ani-cli fails too, with "Connection error: …" (run above). Show both errors in one line: `<AniList error> · ani-cli: <ani-cli error>`.
- *AniList failed, and ani-cli found no results*: `… · ani-cli: No results found!`.

**Back to normal.** Every search asks AniList first, so nothing has to be switched back. Shows found through ani-cli live only on their results screen, in the resolved cache, and in Downloads if downloaded. Downloading the same show later from its AniList page reuses the folder when the names match, and the saved entry then becomes the AniList one. If the names differ there are two folders. Audio and source choices made on the ani-cli entry don't carry over (different id).

### 4.2 Options

| Option | Pros | Cons |
|---|---|---|
| **A. Fall back to ani-cli inside Search when AniList fails** (recommended) | Nothing to learn: the same Search works. No change while AniList is up. Reuses the screen, `fill`, `ani::search`, `ani::episodes` and the whole play and download path. | When AniList hangs instead of answering, the fallback starts after the 20 s timeout ([`src/anilist.rs:117`](../src/anilist.rs#L117)). The documented 403 is immediate. The exact AniList message is replaced by a fixed one, unless variant A2 below is used. |
| A2. As A, but in two steps: show AniList's error at once, then fill the same screen from ani-cli | Keeps AniList's own text in the red line; the user sees why it takes longer | A new `Feed` variant holding the query, and a second branch in `Msg::Shows` that must not let `fill` remove the screen. About 15 more lines. |
| B. A separate "Search with ani-cli…" entry | Always there; also useful when AniList finds the wrong show | The user first hits the error, then has to go back and know about the other entry. One more main-menu row and input mode for a rare case. "Change source…" already covers a wrong match per show. Same id, entry and folder work as A. |
| C. A and B together | Covers both cases | Twice the surface for the same outcome |
| D. Ask "Search ani-cli instead?" after the error | Explicit | One more keypress for no gain; needs a new `Pending` variant |
| E. Also fall back when AniList finds nothing | Covers titles AniList lacks or hides (`isAdult: false`, [`src/anilist.rs:274`](../src/anilist.rs#L274)); ani-cli's search is fast (about 0.3 s above) | Mixes "AniList is down" with "AniList has no such show". A typo then shows provider results without details. Easy to add to A later if asked. |
| F. Remember an outage for a few minutes and search ani-cli straight away | No repeated 20 s waits while AniList hangs | Needs state, a timer and a recovery check; the documented outage answer is immediate anyway |
| G. Make the id an enum or `Option<u64>` | Type-safe | Touches about 70 uses and the format of 3 saved files plus the saved `Media`. It is a big diff, and it still needs the stored entry and the Downloads rule |
| H. A second metadata source (Kitsu, MAL) for posters and details | Keeps the details | A new API and id space, matching between sources; the user accepts no details |
| I. Folders of their own for ani-cli shows, e.g. "Frieren (ani-cli)" | AniList folders can never be touched or mixed | The same show ends up in two folders once AniList is back, and odd folder names; the Downloads rule above keeps the data safe with less churn |
| J. Reuse a saved AniList show from Downloads when its name equals an ani-cli hit | Real id, poster and cached source for shows already downloaded | Provider titles and AniList names match only sometimes. Its progress would be stale, and "Mark as watched?" would come back and fail during the outage. About 3 lines; a candidate for later |

## 5. Recommendation: option A

### 5.1 What to change

| File | Function | Change |
|---|---|---|
| `src/anilist.rs` | `Media`, `impl Media` | field `ani_cli: Option<ani::Source>` (skipped when empty); `Media::from_ani_cli(name, source)` with the synthetic id; `Media::on_anilist()`: id below `1 << 63` |
| `src/ani.rs` | new `or_ani_cli` | AniList's result, or on `Err` ani-cli's hits as numbered shows, or both errors joined. It takes the search as a parameter, so tests needn't run ani-cli |
| `src/ani.rs` | new `position`, used by `locate` ([L489](../src/ani.rs#L489)) | the old position while the title is still there, else the first equal title |
| `src/ani.rs` | `resolve` ([L500](../src/ani.rs#L500)) | without a "Change source…" choice, a show with `ani_cli` gets that entry: one search, `position`, its episodes |
| `src/ani.rs` | `download` ([L931](../src/ani.rs#L931)), new `replaces` | an ani-cli show is written only into a new index entry |
| `src/main.rs` | `submit`, `Input::Search` ([L1449](../src/main.rs#L1449)) | the job becomes `ani::or_ani_cli(c.browse(…), &text, ani::search)` |
| `src/main.rs` | `on_msg`, `Msg::Shows` ([L1673](../src/main.rs#L1673)) | for ani-cli results, a red toast: "AniList search failed, showing ani-cli's matches without posters or details" |
| `src/main.rs` | `items`, `View::Show` ([L982](../src/main.rs#L982), [L1000](../src/main.rs#L1000)) | "List status…" and "Open on AniList" only when `m.on_anilist()` |
| `src/main.rs` | `Msg::Played` ([L1835](../src/main.rs#L1835)), `want_episodes` ([L806](../src/main.rs#L806)) | no "Mark as watched?" and no ani.zip request unless `m.on_anilist()` |
| `src/ui.rs` | `media_preview` ([L683](../src/ui.rs#L683)) | the line "Found through ani-cli: no AniList details or progress sync" |
| `CHANGELOG.md`, `Cargo.toml` | — | entry and version bump, as for every PR |

### 5.2 Sketch

The built code differs where the code review changed it (8.2): `Source` has an `nth` field and a constructor `Source::at`, `position` uses `nth`, `or_ani_cli` skips numbers that are already titles, and `resolve` falls back to title matching instead of failing.

```rust
// anilist.rs
/// Ids from here up are shows only ani-cli's provider knows (AniList's are 32-bit Ints).
const ANI_CLI_ONLY: u64 = 1 << 63;

// in struct Media:
    /// where ani-cli listed a show AniList didn't provide (its episodes stay empty)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ani_cli: Option<crate::ani::Source>,

impl Media {
    /// A show found through ani-cli alone. The id hashes the search and the name, so the same
    /// pick has it again next time (FNV-1a: std's hasher may change between releases).
    pub fn from_ani_cli(name: &str, at: crate::ani::Source) -> Media {
        let key = format!("{}\n{name}", at.query);
        let hash = (key.bytes())
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        let title = Title { romaji: Some(name.into()), english: None };
        Media { id: ANI_CLI_ONLY | hash, title, ani_cli: Some(at), ..Default::default() }
    }

    pub fn on_anilist(&self) -> bool {
        self.id < ANI_CLI_ONLY
    }
}

// ani.rs
/// AniList's results, or if AniList failed, ani-cli's: one show per provider entry, equal
/// titles numbered so each gets its own id and folder. `search` is `search`, passed in for tests.
pub fn or_ani_cli(
    anilist: Result<Vec<Media>, String>,
    query: &str,
    search: impl FnOnce(&str) -> Result<(Vec<String>, Option<Vec<String>>), String>,
) -> Result<Vec<Media>, String> {
    let e = match anilist {
        Err(e) => e,
        ok => return ok, // no results is an answer too
    };
    let (hits, _) = search(query).map_err(|a| format!("{e} · ani-cli: {a}"))?;
    let mut seen: HashMap<&String, usize> = HashMap::new();
    let shows = hits.iter().enumerate().map(|(i, title)| {
        let n = *seen.entry(title).and_modify(|n| *n += 1).or_insert(1);
        let name = if n == 1 { title.clone() } else { format!("{title} ({n})") };
        let at = Source { query: query.into(), index: i + 1, title: title.clone(), episodes: Vec::new() };
        Media::from_ani_cli(&name, at)
    });
    Ok(shows.collect())
}

/// Where `src` is in fresh `hits`: its old place while the title is still there (the second of
/// two equal titles stays the second), else the first with that title.
fn position(src: &Source, hits: &[String]) -> Option<usize> {
    if hits.get(src.index.wrapping_sub(1)) == Some(&src.title) {
        return Some(src.index);
    }
    hits.iter().position(|h| *h == src.title).map(|i| i + 1)
}

// locate: `match position(src, &hits) { Some(index) => Ok(Source { index, ..src.clone() }), None => resolve(m) }`

// resolve, right after `let saved = …`:
    // picked from ani-cli's own results: that very entry, unless "Change source…" chose another
    if saved.is_none() && let Some(at) = &m.ani_cli {
        let (hits, eps) = search(&at.query)?;
        let index = position(at, &hits).ok_or(format!("{} is gone from ani-cli's results", at.title))?;
        let episodes = match eps { Some(eps) => eps, None => episodes(&at.query, index)? };
        return Ok(Source { index, episodes, ..at.clone() });
    }

/// Whether `m` may become the show saved for a download folder: one ani-cli found never
/// replaces one AniList knows, whose details and MAL id (for ani-skip) Downloads needs.
fn replaces(m: &Media, saved: &Media) -> bool {
    m.on_anilist() || saved.id == 0 // 0: a new entry
}
// download: `let d = all.entry(dir.clone()).or_default(); if replaces(m, &d.media) { d.media = m.clone() }`
```

`locate` can't recurse forever: for an ani-cli show it falls back to `resolve`, whose branch above never calls `locate`.

### 5.3 Edge cases

- **AniList hangs:** up to 20 s of loading skeleton before the fallback. Rate limiting can take longer, as `gql` waits as long as AniList asks, up to 3 times ([`src/anilist.rs:213-237`](../src/anilist.rs#L213-L237)). AniList is up in that case.
- **Equal titles:** both are listed, as "Hunter x Hunter" and "Hunter x Hunter (2)", and each plays its own entry. The number depends on the search, though. Another query may list the 62-episode one first, and then *it* is called "Hunter x Hunter". Its id differs (different query), so cached sources can't mix. But its folder name is the same, so downloads of both shows through different searches would share one folder *(inferred)*.
- **Same name as an AniList download folder:** the AniList data stays (rule above). Episodes land in that folder, which is right for the same show. For a different show with the same name, an episode number that is already there is skipped as downloaded ([`src/ani.rs:933-935`](../src/ani.rs#L933-L935)).
- **The provider's order changes later:** `position` finds the nth entry with the title, so shows moving in front don't matter. If two equal titles swap places, the wrong one plays; ani-cli shows no slug that would tell them apart. If the entry is gone, `resolve` matches titles and the preview shows which entry it found; "Change source…" still works.
- **Single hit:** ani-cli shows no title, so the show is called by the typed query, and so is its download folder.
- **Single hit with one episode:** the search itself fetches the stream links (about 1 s, run above). If that fails, the fallback shows `<AniList error> · ani-cli: No sources found for sub!` although the show exists (section 3.3). This is an existing limit of the menu reader.
- **Single hit with no episodes yet:** `<AniList error> · ani-cli: Invalid episode selection` (run above).
- **Invalid AniList token:** `browse` sends the token, so the search falls back while the login check logs out ([`src/main.rs:1738`](../src/main.rs#L1738)). That is fine.
- **Logged in, show from a cached list:** "Mark as watched?" still comes, and saving fails with a red toast as today. Keeping such saves for later is a separate change.
- **Queued downloads of ani-cli shows** resume in a later run with their stored entry, with no AniList request ([`src/main.rs:493`](../src/main.rs#L493)).
- **Ids above 2^53 in JSON:** serde_json reads and writes `u64` exactly. The test pins it.

### 5.4 Tests

Unit tests, next to the existing ones:

| Test | File | Checks |
|---|---|---|
| `ani_cli_shows_keep_their_id_and_stay_off_anilist` | `anilist.rs` | `from_ani_cli("Hunter x Hunter (2)", <query "hunter x hunter">)` has id `10_417_355_497_785_652_780` (FNV-1a of `hunter x hunter\nHunter x Hunter (2)` with the top bit set, worked out separately). Another query gives another id. `!on_anilist()`, `Media::default().on_anilist()`, `name()` is the name. A JSON round trip keeps the id (above 2^53) and the entry. A `Media` without `aniCli` in its JSON reads back with `None` |
| `falls_back_to_ani_cli_only_when_anilist_fails` | `ani.rs` | `Ok(list)` and `Ok(vec![])` come back unchanged, and the search closure panics if called. `Err` plus hits `[A, B, A]` gives names `A`, `B`, `A (2)`, three different ids and entries 1, 2, 3, with both `A` entries titled `A`. A single hit `([query], Some(eps))` gives one show at entry 1. `Err` plus a failing search gives `"<AniList error> · ani-cli: No results found!"` |
| `keeps_the_second_of_two_equal_titles` | `ani.rs` | `position`: entry 3 of `[HxH, Fantasy, HxH]` stays 3; with the list cut to 2 it becomes 1; with the title gone, `None` |
| `ani_cli_shows_never_replace_anilist_downloads` | `ani.rs` | `replaces`: ani-cli show into a new entry yes; into an AniList entry no; AniList show into an ani-cli entry yes; AniList into AniList yes (unchanged) |

The 19 existing tests pass today (`cargo test`, 2026-10-22).

By hand. Blocking AniList in `/etc/hosts` only tests a refused connection, not the documented 403. The 403 takes the same `Err` path, which the unit test covers; how `gql` reads that 403 body is existing code, read but not run.
1. Add `127.0.0.1 graphql.anilist.co` and `::1 graphql.anilist.co` to `/etc/hosts`. Search "frieren": provider titles and the red toast. Open one: the preview line says it isn't synced, and "Episodes…" lists the episodes. Play episode 1. Download episode 2. Downloads lists the show. There is no "List status…", no "Open on AniList" and no "Mark as watched?".
2. Search "hunter x hunter": "Hunter x Hunter" and "Hunter x Hunter (2)". Their previews say 148 and 62 episodes. Quit and start again, then play episode 1 of "(2)": it is still the 62-episode show.
3. Single hits: "Frieren: Beyond Journey's End Season 2" gives one show with 10 episodes. "Hunter x Hunter Pilot" gives one show with one episode, after about 1 s. "Sousou no Frieren 3rd Season" gives the joined error.
4. With AniList reachable, download one episode of an AniList show. Block AniList again, search for the same title and download another episode from the ani-cli hit with the same name. Downloads still shows the AniList poster and details for that folder.
5. Remove the `/etc/hosts` lines and search again: AniList results with posters.
6. Turn the network off and search: one red line with both errors. Downloaded episodes still play.

## 6. Side finding: `locate` and same-named entries (now part of the change)

`locate` found a cached source again by the first hit with the same title ([`src/ani.rs:491`](../src/ani.rs#L491)). The provider lists `Hunter x Hunter` twice (run above). Suppose `resolve` once picked the second by episode count, for the AniList show. The next run's `locate` would switch to the first *(inferred, not reproduced)*. The new `position` fixes that for AniList and ani-cli shows alike. "Change source…" still can't select the second of two same-named entries ([`src/ani.rs:537`](../src/ani.rs#L537)), for the format reason in 4.1. The fallback doesn't need it, so it stays out.

## 7. Not verified

- **What AniList sent on 2026-10-21.** AniList announces outages on its Discord ([considerations](https://docs.anilist.co/guide/considerations#api-stability)), which was not checked. The 403 text above is the documented example.
- **Whether AniList's image CDN was down too.** That decides whether posters not yet on disk load during an outage.
- **Run with 0.6.1, with AniList blocked** through a private hosts file: the search lists ani-cli's titles with the red line, `Hunter x Hunter (2)` opens the 62-episode show before and after a restart, an episode plays through ani-cli's debug player, an episode downloads into a new folder, and a download into a folder whose saved show is an AniList one keeps that show. Not run: the documented 403 itself, and playback in mpv.
- **Search results beyond hianime's first page.** ani-cli makes one request per search *(inferred)*, so a very broad query may miss shows. Typing more of the title helps.
- **Size in the build:** about 135 added lines of code, mostly rustfmt's layout of the sketch in 5.2, and about 100 lines of tests.

## 8. Review

### 8.1 Review of this note

An independent review of the first version found seven problems. Each was checked against the code and ani-cli before changing this note.

| # | Finding | Checked | How this note handles it |
|---|---|---|---|
| 1 | Downloads can lose an AniList show's saved data when an ani-cli hit has the same name | True: `folder` falls back to the name ([`src/ani.rs:811`](../src/ani.rs#L811)), and `download` writes the index before the "already there" check ([`src/ani.rs:931-935`](../src/ani.rs#L931-L935)) | **Accepted.** The `replaces` rule and its test, both directions (4.1, 5). A separate folder per ani-cli show (option I) and reusing the saved AniList show (option J) were weighed and **rejected** for now: I splits one show across two folders, and J matches names unreliably and brings back a save prompt that can't save. |
| 2 | Dropping repeated titles hides playable shows, and re-searching by title picks the first | True: slugs `hunter-x-hunter-1-1393` (148 episodes) and `hunter-x-hunter-1229` (62) | **Accepted, with a different mechanism.** The suggested `sources.json` route can't hold a position without breaking the file (4.1). Instead the entry travels in `Media.ani_cli`, equal titles are numbered, and `position` keeps them apart in `resolve` and `locate`. The "no new plumbing" claim now says: no new ani-cli calls, but new code on ani-hop's side. |
| 3 | One test only covered hashing | True | **Accepted.** The fallback is the function `or_ani_cli`, with the search passed in, and is tested without network. Further tests cover `position` and `replaces`. The note says that `/etc/hosts` only tests a refused connection (5.4). |
| 4 | A 4-second toast doesn't explain the lasting lack of sync | True: progress is always 0 ([`src/anilist.rs:92-94`](../src/anilist.rs#L92-L94)), and Downloads shows no toast | **Accepted.** A permanent line in the preview, and the toast now says "AniList search failed". |
| 5 | Single hit with one episode reaches the debug player, which fetches links first; the sketch dropped the episodes of a single hit | True (ani-cli [L366](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L366) before [L372](https://github.com/pystardust/ani-cli/blob/21ed4a0a6354688622b5a967d93ba96d851d0ef2/ani-cli#L372); run: 994 ms, exit 0) | **Accepted** as a documented existing limit, with runs for 0, 1 and several episodes (3.3, 3.4, 5.3). The source info is kept (`ani_cli`). **Rejected:** keeping a single hit's episode list in the `Media`. `resolve`'s one search returns it again, and a saved copy would go stale in `downloads.json` and the queue. |
| 6 | Skipping ani.zip for ani-cli shows should be part of the change | True: keyed by AniList id, 404 for these | **Accepted** (5.1). |
| 7 | "Neither knows the title" contradicted the empty-result rule | True | **Accepted,** reworded (4.1). |

The review agreed with the automatic fallback and the high-bit ids, and so does this version. The size went from about 30 lines and one test to about 70 lines and 60 lines of tests.

### 8.2 Review of the code (PR #9)

Two independent reviews of the built code: the same reviewer as in 8.1, and a second one whose findings were each checked by a separate skeptic. All findings below held up.

| # | Finding | Found by | Fix |
|---|---|---|---|
| 1 | Hits `[A, A, A (2)]` named two entries `A (2)`, with one id: picking the third opened the second | both | Later entries of a title get the lowest number that isn't already a title in the list: `A`, `A (3)`, `A (2)`. Test with three distinct ids |
| 2 | A new show in front of two equal titles made the second resolve to the first | both | `Source.nth` (which of the entries with this title), and `position` takes the nth. Sources saved before 0.6.1 have `nth` 0: their old place, their only match, or a fresh resolve when it's ambiguous |
| 3 | A show picked as a single hit is saved under the typed query, so it was "gone" once the query found more shows, and a picked entry failed once the query found only one | both | A single hit is taken as it is; an entry not found falls through to title matching, as for AniList shows. Test: the typed query finds "Frieren: Beyond Journey's End Season 2" among two hits |
| 4 | "Change source…" offered the numbered name ("Hunter x Hunter (2)") as the search | second | `default_query` uses the stored query for ani-cli shows |
| 5 | Bulk download said "unwatched" for ani-cli shows, but they have no progress, so every episode is queued | second | It says "all" unless every marked show is on AniList |

Left as limits: two equal titles that swap places, and a single-episode single hit that needs its stream links to be found (3.3).
