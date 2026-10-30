# Changelog

## 0.8.2 – 2026-10-30

### Added

- Search says what it takes before you type: a show's name in English or Japanese, or a nickname like "aot". It also lists the filters for the shows it finds: `'titan` for an exact match, `^attack` for how a name starts, `titan$` for how it ends, `!season` to leave shows out, and `fri | aot` for either one.

## 0.8.1 – 2026-10-30

### Removed

- The "Adult shows" setting from 0.8.0. ani-cli's provider doesn't have the shows AniList marks as adult, and opening one played an unrelated show instead, like "Boku no Hero Academia: Training of the Dead" for "Boku no Pico". Searches and rankings leave them out again.

## 0.8.0 – 2026-10-30

### Added

- Settings has "Adult shows", off by default. Turned on, searches and rankings include the shows AniList marks as adult, as AniList's own 18+ setting does.
- Searches show More… past their first 50 shows, as rankings do: "gundam" has 54.
- List filters take fzf's `|`: "frieren | titan" keeps both, and "fri 2$ | 3$" keeps the second and third seasons.

### Changed

- Search finds a show through a typo or words run together, like "frieran", "onepiece" or "jujutsukaisen": when AniList finds nothing, it searches ani-cli's spelling again. A guess that doesn't start like what you typed, or has a number you didn't type, isn't shown: "jujutsu kaisen s2" doesn't show the third season.
- Search also looks for what you typed without accents, curly quotes and full-width letters: "Pokémon" finds the original series, "JoJo’s" and "Ｓｔｅｉｎｓ；Ｇａｔｅ" find their shows.
- Filtering a list finds a show by its other titles: "attack on titan" or "aot" on Shingeki no Kyojin. A filter's words can match a title and an episode summary together, like "season sleeps", and `'sleeps` finds a word in a summary too.
- Change source takes a nickname AniList knows the show by, like "aot", "fmab" or "jjk", and searches its title.

### Fixed

- Titles with ×, accents, Japanese, curly apostrophes, & or [ ] failed at the provider with HTTP 400 or a curl error, in Change source, opening, playing and downloading: HUNTER×HUNTER, Pokémon, 葬送のフリーレン, Fate/stay night [Unlimited Blade Works].
- Kaguya-sama: Love is War season 2 played season 1, the original Pokémon played Pokémon Horizons, and 86 EIGHTY-SIX played its second season. A source already saved as the wrong 86 season is checked again.
- A filter word hidden with `!` came back when an episode summary had the same text.

## 0.7.6 – 2026-10-28

### Added

- The episode list of a long show that AniList keeps as one, like One Piece, Naruto or Detective Conan, shows where each of its seasons on TheTVDB starts: "── Season 2 ──", as "── Part 2 ──" marks where a season AniList splits in two goes on. For One Piece the seasons follow its story arcs (Season 21 is Wano). Recaps and episodes TheTVDB hasn't sorted in yet stay in the season around them.

## 0.7.5 – 2026-10-27

### Changed

- Questions name what each answer does: "Keep" or "Delete", "Keep downloading" or "Stop", "Keep on list" or "Remove", "Stay" or "Quit", "Drop them" or "Resume", "Don't save" or "Save", instead of No and Yes.
- A show title too long for its row keeps its season or part: "Attack on Titan Fi…Special 2", where four seasons all read "Attack on Titan Final Seas…" at 80 columns. The current show in the header is cut the same way and no longer runs into your name.
- On a text field the footer says "enter search" or "enter save". On episodes another ani-hop has queued, where enter only says so, it leaves enter out.
- At 80 columns the audio hint says "subtitled", "no subs", "dubbed" or "dub + subs", where it said "Japanese" with or without subtitles.
- The help under text fields wraps cleanly at 80 columns.
- The download folder is rescanned in the background, and the show on screen at once, so one on a NAS or a sleeping USB disk doesn't hold up opening a screen.
- The code behind list changes, episode downloads, playing, the preview and list loading is reorganized into one part each, with tests through it. Running the tests no longer touches your own config or cache.

### Fixed

- After saving progress, Downloads showed the show's old progress again, and the next "Mark as watched" failed with "not saved · AniList has 6/12 now". A queued download also wrote back the progress from when it was queued.
- Two ani-hops marking episodes at the same moment could set the progress back on AniList. Saves now go one at a time across ani-hops, and the later one says "not saved · AniList has 7/12 now".
- Going offline, also in the middle of an episode, or starting before the Wi-Fi was up, emptied the download queue: every queued episode failed and was dropped. Downloads now pause and keep their queue: "Paused downloads · can't reach hianime.at · enter on a queued episode resumes them".
- A save still running while you logged out and in as someone else changed the new login's screens.
- Starting an episode again while its first player was still open could end the second start's pop-up when the first player closed.
- A mistyped AniList client ID blocked logging in, with no way to enter another one. Enter on the empty token field now goes back to the client ID step.
- A download that ended just as it was stopped could print "kill: No such process" over the screen.
- A cache folder ani-hop can't write to stopped it with Rust's debug output. It says "ani-hop: can't write to ~/.cache/ani-hop: Permission denied" now.
- A filter for how a title starts or ends with a capital letter, like `^Attack` or `Titan$`, found nothing unless it was the whole title. A word filter like `'Horizons` or `!Horizons` missed the word at the very end of a title or episode summary with an accent in it, like "Pokémon Horizons".
- A question too long for a small terminal, like "Mark episode 6 of … as watched?" with what saving also changes, was cut after six lines. It now uses the lines it needs, and the whole width when it doesn't fit beside the preview.

## 0.7.0 – 2026-10-26

### Changed

- "Remove from list" asks first, starting on No: it takes the show's progress and score off AniList, and a stray enter or → did that at once.
- Downloads shows each show's progress as AniList has it now, not as it was when the show was downloaded, and it lists the shows downloaded before the download folder changed. A show keeps its folder, also for new episodes, and the download folder setting says so.
- Deleting the downloads of several shows lists the files that go, under each show's folder.
- A queued download whose source ani-hop knows checks it with one ani-cli search, as playing does, instead of up to five.
- At 80 columns the settings show whether skipping openings and endings is on, and a hint too long for its row is left out instead of ending mid-word ("not l…", "loses pr…").
- A show that isn't out yet doesn't search ani-cli anymore and doesn't end in a red "no match" error. Its page leaves out "Play episode 1" (also when AniList has no date for it yet) and says "not out yet" on "Episodes…", its episode lists open at once and say "Not out yet", and downloading several shows leaves it out. A list cached before a show started airing says "Airing" for it. When ani-cli finds nothing for a show with only its first episode out, the error says new shows can take a day.
- The code behind episode progress, downloads, sources, lists and posters is reorganized into one part each, with tests through it. Nothing else changes on screen.

### Removed

- The Nix flake is for Linux only, NixOS and Nix on other distros. On macOS, Homebrew installs ani-hop.

### Fixed

- "Download unwatched episodes" of a season AniList splits in two, like Attack on Titan Season 3, downloaded the episodes watched in its second part again.
- Two shows with one name, like a remake, shared a download folder, so one's episodes counted as the other's. The second one's folder gets its AniList id now.
- Two ani-hops could download the same episode into the same files at once and spoil both. The second one says "another ani-hop is downloading it".
- A download the queue couldn't save (a full disk, a folder without write access) still started and was gone after quitting. It says why and isn't queued. Resuming the downloads of an ani-hop that's gone saves them in this one's queue before the old queue goes, so a crash in between loses nothing.
- A source picked with "Change source…" while ani-hop was still looking the show up was replaced by what that lookup found, and picking the second of two equal titles (Hunter x Hunter, 1999 and 2011) picked the first.
- Searching for something like "-D" ran one of ani-cli's options, which clears its history, instead of searching.
- "Episode 5 airs in" from a list cached a while ago could be in the past ("0h -30m"), and "Continue with" left out an episode that aired since. Air times are kept as a date now; a list cached before shows them once it's refreshed.
- Logging out while the login was still being checked logged back in, and a check of an old login could log out a new one.
- A cover cut short on disk (a crash while it was saved) never showed. It's fetched again.
- Two ani-hops saving the same file at once could lose one of the saves, and a crash while saving the login lost it. Each save now writes a file of its own and swaps it in.
- A removal from the list that failed said "not saved", and its answer closed another show's List status screen when that was open.

## 0.6.16 – 2026-10-26

### Added

- An episode's preview says when it first aired, like "aired 15 Dec 1999". The date comes from ani.zip, like the episode's title and summary. Episode info cached before this is fetched again once, and still shows while ani.zip can't be reached.

### Changed

- The show information is shorter:
  - Only the main studio, like "Fall 2023 · MADHOUSE". Shows from your lists and a season's second part listed every company involved, seven for Frieren.
  - On an episode list, the selected episode comes right under the show's titles and list status; score, season, studio and genres are on the show's page. At 80×24 the episode's summary was below the fold.
  - An unwatched episode shows "Episode 13" and when it aired, instead of "not watched yet, so no summary".
  - A season split in two says "Episodes 13–22 are Part 2", as the episode list's line calls it.
  - The ani-cli line leaves out the source's title when it's the show's own ("ani-cli: 28 episodes"), so a different one, like a wrong match, stands out.
  - "1 episode", not "1 episodes", and a movie's count is left out. A wrapped line no longer starts with "·", and a genre like "Slice of Life" stays on one line.
- The progress question says what else Yes changes in the question itself, like "Watched up to episode 5 of Attack on Titan Season 3? Unmarks 6–12 and moves it to Watching.", as the hint beside Yes was cut off. Yes says what it saves.
- Without a connection, errors read "Can't reach AniList" and "ani-cli: can't reach hianime.at" instead of a system error and a cut-off address. A show ani-cli doesn't find says "no match for this show".
- List hints drop whole parts instead of cutting a number ("0/24", not "0/2…"), fixed menus have no item count, and the footer says "enter play" on an episode list and has no "esc back" on the main menu. Settings says "Default audio…", a save says what it saved ("Saved Frieren · Completed · 28/28"), and the start screen says where downloads go.
- On the start screen, → is the select key, with enter and ^l beside it, as ← is the back key.

## 0.6.15 – 2026-10-26

### Changed

- The keys on the start screen are grouped by what they're for, one action per row: moving, selecting and going back come first (← or ^h goes back, enter, → or ^l selects), then filtering, where esc clears the filter before it goes back, then marking and the rest. The descriptions are easier to read, shift ↑↓ for scrolling the preview is listed, and ^f reads "load more shows". On a short terminal with an update notice, the logo makes room so no key is cut off.

## 0.6.14 – 2026-10-26

### Added

- "Watched up to…" on a show's page lists its episodes, and picking one saves your AniList progress up to it. That marks many episodes watched at once, or unmarks them when you pick an earlier one. AniList keeps one count per show, so an episode always counts as watched together with every one before it.

### Changed

- The question after an episode says when Yes also counts earlier episodes as watched, and then starts on No. Watching the latest One Piece episode on a show not on your list said only "saves 1180/?", and Yes marked all 1180 episodes watched; it now says "saves 1180/? · counts 1–1179 too". In "Watched up to…", an earlier episode says what it unmarks, like "unmarks 6–12", and starts on No too.

## 0.6.13 – 2026-10-26

### Fixed

- A show whose Japanese title finds only other shows at ani-cli got one of those: Fullmetal Alchemist and Fullmetal Alchemist: Brotherhood (Hagane no Renkinjutsushi) got Arpeggio of Blue Steel and its single episode, Neon Genesis Evangelion got Sonic Soldier Borgman 2. When none of a search's results has the show's title, ani-hop now searches its English title too and takes the result that has it. A show picked this way before gets its source again the next time it's opened.

## 0.6.12 – 2026-10-26

### Added

- A Nix flake, for NixOS and for Nix on Linux and Apple-silicon Macs: `nix profile add github:ReynkeDeVos/ani-hop` installs ani-hop with ani-cli, mpv, yt-dlp, ffmpeg and ani-skip, and NixOS or Home Manager can take it as a flake input. When a new release is out, ani-hop installed this way shows `nix profile upgrade ani-hop`, `nix flake update ani-hop` and a rebuild, or `nix run --refresh`, instead of a `cargo install` line.

## 0.6.11 – 2026-10-26

### Changed

- On an episode of a season's second part, like episode 13 of Attack on Titan Season 3, the preview shows that part: its poster, title, facts, list status and description. Its episodes' summaries still stay hidden until watched. The line saying which episodes are the second part's is only on the show's page now, since the episode list's "Part 2" line says it there.

## 0.6.10 – 2026-10-26

### Changed

- In a season AniList splits in two, like Attack on Titan Season 3, the preview's "Episodes 13–22 are … Part 2" stays a plain line, and the second part's list status gets its own line under it, shown on that part's episodes and on the show's page. The first part's status stays on every episode. In the episode lists, a faint line saying "Part 2" sits between episodes 12 and 13.
- The question after an episode says what Yes saves, like "saves 3/12", "saves 12/12 · moves it to Completed" or "saves 1/10 to Part 2 · adds it to Watching".

### Fixed

- The second half of a special that ani-cli splits in two, like the Attack on Titan Final Chapters specials (one episode each on AniList, two at ani-cli), didn't ask to be marked watched, so answering No after the first half left no way to mark it in ani-hop. An episode past what AniList counts for a show now asks too, and saves the show's last episode (1/1 here), never more.

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
