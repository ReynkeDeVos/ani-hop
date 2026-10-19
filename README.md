# ani-hop

![ani-hop demo](demo.avif)

A fast terminal UI for anime: browse trending, top rated and search results with posters, then stream or download episodes through [ani-cli](https://github.com/pystardust/ani-cli). Log in with [AniList](https://anilist.co) to get your lists and have your progress tracked after every episode.

## Why ani-hop

| | |
|---|---|
| **No scraper of its own** | Every source comes from ani-cli, which a large community keeps working when the sites change. Updating ani-cli is enough; ani-hop itself doesn't need a new release. |
| **Maintained** | FastAnime, later renamed Viu, the closest TUI of this kind, was archived in 2026. |
| **One binary** | Written in Rust, with no Python environment and no fzf or rofi needed. It starts instantly. |
| **Posters in the terminal** | Drawn with the Kitty graphics protocol, even inside tmux/herdr. |
| **AniList built in** | Your lists, watched episodes marked, and progress saved after every episode. |
| **Find an episode by what happens in it** | Watched episodes show their title and summary, and the filter searches them. Episodes you haven't seen stay blank, so nothing spoils. |
| **Downloads in the background** | Keep browsing while they run; unfinished ones resume after a restart. |
| **Skips openings and endings** | With ani-skip, in streams and downloads alike; switch it off in the menu. |

**Needs** [ani-cli](https://github.com/pystardust/ani-cli), [ani-skip](https://github.com/synacktraa/ani-skip), [mpv](https://mpv.io), [yt-dlp](https://github.com/yt-dlp/yt-dlp), ffmpeg, curl, xdg-utils and [Ghostty](https://ghostty.org) or [kitty](https://sw.kovidgoyal.net/kitty/) for the posters.

```sh
cargo install --git https://github.com/ReynkeDeVos/ani-hop
```
