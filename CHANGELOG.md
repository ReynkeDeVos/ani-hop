# Changelog

## 0.6.2 – 2026-10-22

### Fixed

- Running the install line again no longer upgrades the whole system on Arch: it installs only the packages that are missing, so an update just replaces ani-hop, ani-cli and ani-skip. Omarchy, which allows system upgrades only through `omarchy update`, stopped the old script.
- "Install missing packages" no longer upgrades the system on Arch either, and on Omarchy the update hints say `omarchy update` instead of `yay -Syu`.

## 0.6.1 – 2026-10-22

### Fixed

- Search works while AniList is down: when AniList's search fails, ani-hop searches through ani-cli instead and lists what it finds, so you can still open a show, play and download episodes. These shows have no poster, details or AniList progress sync, which their preview says. Shows with the same title are listed apart, as "Hunter x Hunter" and "Hunter x Hunter (2)", and each plays the one you picked. Downloading one never replaces the poster and details Downloads keeps for an AniList show.
- A show whose title appears twice on ani-cli's provider stays on the entry it was matched to, instead of switching to the other one on a later start.

## 0.6.0 – 2026-10-21

### Added

- One line installs ani-hop on Debian/Ubuntu, Fedora and Arch: `curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh`. mpv and ffmpeg come from the distro, ani-cli and ani-skip from upstream, so `ani-cli -U` keeps ani-cli current. Running it again updates everything. On Fedora it offers RPM Fusion, without which most episodes don't play.
- Ready-made builds for Linux (x86_64 and ARM) and Apple-silicon Macs with every release, also in a Homebrew tap (`reynkedevos/tap/ani-hop`).
- Runs on macOS: the AniList login opens in your browser, two ani-hops running at once keep their downloads and posters apart, and downloads go to `~/Movies/Anime`.
- The setup screen says when ani-cli is older than 5.1, which ani-hop needs, and offers the right update command. The start screen shows when a new ani-hop is out and how to update it, checked at most once a day.
- Licensed under GPL-3.0-or-later. Release archives include the licences of the libraries inside.

### Changed

- Episodes play in mpv unless `ANI_CLI_PLAYER` says otherwise. ani-cli would pick IINA on macOS and Windows' mpv.exe under WSL.
- "Install missing packages" works beyond Arch: other distros get the install line, macOS gets Homebrew.
- ani-skip is optional: without it episodes play without skipping, instead of failing, and the setup screen no longer comes up at every start.
- Two ani-hops running at once now recognise each other through a lock file instead of /proc. Quit a running older ani-hop before starting this one, or the new one offers to resume its downloads.

## 0.5.0 – 2026-10-20

### Changed

- Settings and cache files are written atomically, so quitting or a crash mid-write can't leave a broken one. Internal cleanup otherwise; nothing changes in use.

## 0.4.1 – 2026-10-19

### Added

- Trending now, Top rated and All-time popular go past the first 50 shows: "More…" at the end of the list loads the next 50, with a spinner right on it while it loads, and the cursor lands on the first new one. A page loads only when you pick "More…", so AniList gets one request per page.
- "Download folder…" in the settings sets where new downloads go (`~` works, empty goes back to the default). It wins over `ANI_CLI_DOWNLOAD_DIR`; shows already downloaded stay in the old folder.

### Changed

- Audio has four choices: Japanese with or without English subtitles, and the English dub with or without them. Pick from a list: "Default audio…" in the settings, "Audio for this show…" on a show's page. Hidden subtitles are still loaded, v in mpv shows them. "English dub" now means without subtitles; before, mpv showed them anyway. Downloads remember whether they are the dub, so an episode downloaded in Japanese keeps its subtitles after the show is switched to the dub.

- Download folders hold only the episodes and their subtitles. What ani-hop keeps about a downloaded show is in `~/.config/ani-hop/downloads.json` now.

- The number above a list is just how many entries it has. Only while you type a filter does it show how many of them match, like 3/100.

### Fixed

- Deleting the downloads of several marked shows removes only what ani-hop downloaded. A folder of your own with the same name keeps its other files.
- A show that is also in one of your custom AniList lists no longer appears twice in its status list.
- Marks stay on their shows when a list updates in the background, instead of moving to whatever is now in that row.

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
