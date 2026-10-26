# Changelog

## 0.6.9 – 2026-10-26

### Fixed

- A season that AniList splits in two but ani-cli lists as one show, like Attack on Titan Season 3 (22 episodes at ani-cli, 12 and 10 in two parts on AniList), had no titles or summaries past episode 12. Marking one of those episodes watched also saved a progress like 22/12 on the first part and took it off Completed, and moving it back to Completed made AniList cut that progress to 12, so episodes 13 to 22 showed as unwatched again. Those episodes now belong to the second part on AniList, once it has finished airing: they get its titles and summaries, count as watched as far as its progress goes, and marking one watched saves it there, so episode 13 is episode 1 of Season 3 Part 2. The preview says which episodes are the second part's, and "Continue with" goes on into it. An episode past a show's count that isn't in such a part, like a recap ani-cli has as episode 13 of 12, doesn't ask to be marked watched anymore.

## 0.6.8 – 2026-10-24

### Fixed

- A show's description was cut off at the bottom of the preview without a sign, which a zoomed-in terminal made easy to hit. It now starts beside the poster, in the rows under the facts that stayed empty, and goes on full width below it, so it fits whole far more often. When it still doesn't, the preview's border says "shift↓ read on": shift+↓ and shift+↑ page through the preview like fzf's, and the next show starts at its top. An episode's summary and a show's facts aren't cut off beside the poster anymore either.

## 0.6.7 – 2026-10-23

### Changed

- A download says only how far along it is and how long the rest takes, like "18% · 2m left", the same through yt-dlp and ffmpeg, or "18% · retrying" while yt-dlp retries a piece. Before, it showed yt-dlp's whole line with its size estimate, speed and piece count. The footer starts with that, then the episode and how many more are queued.

### Fixed

- A download that went on after a pause, quitting or a crash could get stuck on "Got error: HTTP Error 416: Range Not Satisfiable. Retrying fragment …", forever. yt-dlp can't go on with a piece it had written whole but not renamed yet when it stopped, and its retry asks the same way again. ani-hop now deletes the pieces yt-dlp was writing before it goes on, so they're fetched again whole (at most 16, a few MB each); the finished ones stay. A download stuck like this gets going again with pause and resume.

## 0.6.6 – 2026-10-23

### Added

- Downloads can be paused: enter on the episode downloading offers "Pause downloads" first, then "Stop downloading". Paused, it keeps what it got and nothing else starts; enter on any queued episode offers "Resume downloads". → twice pauses, → twice again resumes. A chooser whose choice went away meanwhile, because the episode finished, goes back to the list instead of offering something else under the cursor. Queuing an episode resumes too, and quitting while paused doesn't ask, since the next start offers to resume them. A download through yt-dlp goes on from where it was. One through ffmpeg (ani-cli's choice for shows whose audio is a separate track) starts that episode over, so pausing it asks first, like stopping.
- A download through ffmpeg shows how far along it is, like "93% · 26:16 of 28:13", instead of only "26:16 so far".

### Changed

- Stopping a download that got something asks first, with the cursor on No, and says how much it deletes. Taking an episode that hasn't started out of the queue still needs no question, since nothing is lost.
- "Another ani-hop has it queued" instead of "is downloading it", since that one may be paused.

## 0.6.5 – 2026-10-23

### Added

- Downloads can be stopped: enter on a queued or downloading episode takes it out of the queue and deletes what it got so far.

### Changed

- "Download episodes…" and "Delete downloads…" are one "Downloads…". It lists every episode with its state (not downloaded, queued, downloading with its progress, on disk with its size), and enter does what that state allows: download, stop, or delete after asking. Marked episodes in different states ask which. What's on disk or queued is never downloaded again, also when a whole show is queued, and an episode another running ani-hop has queued says so.
- Episodes… shows which episodes are queued and how far the one downloading got.

### Fixed

- Shows whose audio comes as a separate track, like Cyberpunk: Edgerunners 2, downloaded without sound. ani-cli after 5.1 fetches that track ([pystardust/ani-cli#1967](https://github.com/pystardust/ani-cli/pull/1967), not in a release yet) through ffmpeg. ani-hop now has that download written to a `.part` file like yt-dlp's, so one cut short by quitting, a dropped connection, or with parts ffmpeg reports missing or broken, is downloaded again instead of kept as finished, and shows how far it got. With an older ani-cli, such an episode is deleted and reported as having no sound instead of kept silent.
- The download line in the footer ends in "…" before the key hints instead of running into them.

## 0.6.4 – 2026-10-23

### Added

- ^f loads the next page of Trending now, Top rated and All-time popular from anywhere in the list, not only through "More…" at its end.

### Changed

- Trending now, Top rated and All-time popular stay cached for an hour with every page loaded through More…, so opening one again sends AniList no request. Before, only the first page was kept, for 2 minutes.
- Empty lists say so, and errors say what failed: "Can't reach AniList (…)", the show that wasn't saved, the episode that didn't play, Change source… when ani-cli can't find a show.
- The resume question says No deletes the partial files. Marking an episode watched says when that adds the show to Watching or moves it to Completed.
- Each thing is said once: the screen's name in the breadcrumb, the AniList state in the header, an episode's start in its pop-up. Input screens no longer list keys that don't apply there.
- "Default audio…" and "Audio for this show…" are now "Audio…", so the chosen audio fits. Footer keys follow the screen (esc clear, enter download), cut titles end in "…", and setup shows the full command it runs and marks ani-skip and posters as optional.

### Fixed

- A download folder or source title with wide characters, like the Japanese `~/ビデオ`, no longer crashes the app at around 80 columns.
- A toast too long for the footer takes the whole line until it fades, instead of being cut off by the key hints.
- Going back from the screen for several marked shows keeps their marks.
- Pasting the client secret instead of the client ID is refused; before, it blocked the AniList login until the saved ID was deleted by hand.
- Starting offline no longer says the AniList login failed.

## 0.6.3 – 2026-10-22

### Changed

- The install line takes ani-cli and ani-skip from your distro where it has them new enough, so they update with the system: on Arch from the AUR through yay or paru, installing only what's missing. Debian, Ubuntu and Fedora have no recent enough ani-cli yet, so there they still come from upstream. Copies an earlier run put into `~/.local/bin` are removed when the package replaces them, and so is a second copy of the skip script for mpv.

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
