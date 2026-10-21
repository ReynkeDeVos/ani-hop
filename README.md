# ani-hop

![ani-hop demo](demo.avif)

A fast terminal UI for anime: browse trending, top rated and search results, then stream or download episodes through [ani-cli](https://github.com/pystardust/ani-cli). Log in with [AniList](https://anilist.co) to get your lists and have your progress tracked after every episode.

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
| **Sub or dub** | Japanese or the English dub, each with or without English subtitles, for all shows or just one. Downloads follow the choice; if an episode has no dub yet, the stream falls back to subtitles. |
| **Skips openings and endings** | With ani-skip, in streams and downloads alike; switch it off in the settings. |

## Install

**Debian/Ubuntu, Fedora, Arch and their derivatives.** One line installs ani-hop and everything it needs. mpv and ffmpeg come from your distro, ani-cli and ani-skip from upstream into `~/.local/bin`, so `ani-cli -U` keeps ani-cli current without sudo. Run it again to update. On Arch with yay or paru it installs the AUR package instead. On other distros it installs ani-hop, ani-cli and ani-skip and lists the system packages to install yourself.

```sh
curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh
```

**Arch (AUR).** `yay -S ani-hop-bin`, then `yay -Syu` updates ani-hop and ani-cli together.

**Homebrew (macOS on Apple silicon, Linux).** ani-cli comes from its own tap:

```sh
brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git
brew install pystardust/ani-cli/ani-cli reynkedevos/tap/ani-hop
```

**Windows.** ani-hop runs in WSL. Install it with `wsl --install -d Ubuntu-26.04` in an admin PowerShell, restart, then run the `install.sh` line above in Ubuntu. The [WSL guide](docs/distribution.md#windows-via-wsl-step-by-step) has the details, including posters through kitty.

**From source.** `cargo install --locked --git https://github.com/ReynkeDeVos/ani-hop` (Rust 1.89 or newer). It then needs [ani-cli](https://github.com/pystardust/ani-cli) 5.1 or newer, [mpv](https://mpv.io), [yt-dlp](https://github.com/yt-dlp/yt-dlp), ffmpeg, curl, patch and xdg-utils; [ani-skip](https://github.com/synacktraa/ani-skip) is optional.

Posters need [Ghostty](https://ghostty.org) or [kitty](https://sw.kovidgoyal.net/kitty/). ani-hop checks its tools at the start and offers to install what's missing. It also says when ani-cli is too old or a new ani-hop is out, with the command that updates it.

## License

[GPL-3.0-or-later](LICENSE)
