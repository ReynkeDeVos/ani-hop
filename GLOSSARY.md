# ani-hop

A terminal app for anime: shows and the user's progress come from AniList, episodes from ani-cli's provider.

## Shows and progress

**Show**:
An anime as AniList lists it, or one ani-cli's provider has that AniList couldn't give (an ani-cli show, without details or progress sync).
_Avoid_: anime, title, media

**Entry**:
The user's AniList record of a show: the list it's on and its progress.
_Avoid_: list item

**List**:
One of the user's AniList lists (Watching, Planning, Completed, Paused, Dropped, Rewatching), or any list of shows on screen: a ranking, a search, Downloads.

**Progress**:
How many episodes of a show AniList counts as watched; every episode up to it counts as watched.
_Avoid_: watched count

**Part**:
A sequel on AniList that the provider lists as more episodes of the show before it, as with a season split in two.
_Avoid_: cour, sequel (for this meaning)

**Mark**:
What marking an episode watched saves: the entry it counts toward, its progress before and after, and the list it lands on.

## Sources

**Source**:
Where the provider has a show: the search that finds it, which result it is, and its episodes.
_Avoid_: match, provider entry

**Resolve**:
Finding a show's source by searching its titles and matching them to the provider's.

**Pick**:
A source the user chose by hand with Change source, used from then on.
_Avoid_: override

## Downloads

**Download queue**:
What one running ani-hop still has to download, kept so the next start can resume it.

**Job**:
One show's queued episodes, or every episode of it not watched yet.

**Leftovers**:
The queues of ani-hops that are gone, which the next start resumes or drops.

**Show folder**:
The folder one show's episodes download into, inside the download folder of the time.

**Show downloads**:
Where a show's episodes stand with downloads (on disk, queued, queued by another ani-hop, downloading or paused), and what enter does with the ones picked.

## Screen

**Preview**:
The right-hand pane: a show's poster and details, or an episode's.

**Poster**:
A show's cover drawn in the terminal; its cover color stands in where the terminal can't draw it.
_Avoid_: cover image, thumbnail
