# Changelog

## 0.4.0 – 2026-10-19

### Added

- Audio: Japanese with English subtitles or the English dub, the two the provider has. Set it for all shows in the settings and for a single show on its page; a show only keeps its own choice while it differs from the one for all. Streams and downloads both follow it, and a queued download keeps the audio it was queued with. When an episode has no dub yet, the stream falls back to subtitles; a download reports it instead. Without a choice, ani-cli's `ANI_CLI_MODE` decides.

- `ani-hop --version` prints the version. Any other argument prints a short usage instead of opening the UI.

### Changed

- Skip openings and endings, audio and the AniList login moved from the main menu into "Settings & AniList…". The switches there flip with enter and →; ← goes back, as in every sub menu.
- Episode summaries, alternative titles and watched episodes in lists are easier to read: faint main text instead of the theme's muted gray. The key hints stay muted, and the selected row is always at full contrast.

## 0.3.2 – 2026-10-19

### Added

- Watched episodes show their title in the episode list and their summary next to the show's details, so you can find one again by what happens in it. The filter searches the summaries too, word by word. Episodes you haven't watched stay blank, so nothing spoils. The texts come from ani.zip, one request per show, cached on disk.

### Changed

- ← and ctrl+h flip "Skip openings and endings" as well, like enter and →.
- The help on the start page lists the arrows and ctrl+h/j/k/l in one line.
- "Why ani-hop" in the README is a table and mentions herdr.

## 0.3.1 – 2026-10-19

### Added

- → and ctrl+l select, like enter; ← and ctrl+h go back.

## 0.3.0 – 2026-10-18

### Added

- Pressing enter on an episode pops up a box at once: the episode, what it is waiting on and a running bar, until mpv shows it.
- Watched episodes (up to your AniList progress) are marked with a ✓ and dimmed in the episode, download and delete lists.
- "Skip openings and endings" in the main menu switches ani-skip on and off, for streams and downloaded episodes alike.

## 0.2.0 – 2026-10-18

### Added

- Unfinished downloads come back. If ani-hop quits, crashes or its terminal closes while downloads are running, the next start asks whether to resume them. Yes continues where yt-dlp stopped; No removes the partial files.
- Starting an episode shows what it is waiting on (searching ani-cli, checking the source, fetching the stream, opening the player) right on the episode and in the status bar, then `▶ playing` once mpv is up.

### Fixed

- Quitting during a download no longer leaves ani-cli's ffmpeg fallback running in the background, writing a partial episode that looks finished.
- Pressing enter again while an episode is starting no longer opens a second player.
- A download whose source lookup fails no longer leaves the "left" counter stuck, so quitting doesn't ask about downloads that aren't running.

## 0.1.0 – 2026-10-18

First release: trending, top rated and search with posters in Ghostty and kitty, streaming and bulk downloads through ani-cli with ani-skip, AniList lists and progress sync, fzf-style menus.
