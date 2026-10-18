# ani-hop

Anime-TUI in Rust ([ratatui](https://ratatui.rs) + [ratatui-image](https://github.com/ratatui/ratatui-image)), inspiriert von [viu](https://github.com/viu-media/viu), nur deutlich simpler.
AniList liefert Listen, Infos und Poster; Streamen und Herunterladen übernehmen `ani-cli` und `ani-skip`.

- Trending / Top rated / Popular / Suche
- AniList-Login: Watching, Planning, Completed, … ansehen, Status setzen, Fortschritt nach dem Schauen eintragen
- Episoden streamen (mit ani-skip) oder heruntergeladene lokal abspielen
- Episoden herunterladen und löschen, auch in Bulk
- fzf-Bedienung in jedem Menü, Poster + Infos in der Vorschau (Kitty-Grafikprotokoll)
- Schnell zuerst: Listen, Login und ani-cli-Quellen sind gecacht und erscheinen sofort, aktualisiert wird im Hintergrund
- Effekte, die nie Inhalt verstecken: kurze ░▒▓█-Start-Animation, Glanz über dem Titel, Skeleton beim ersten Laden

## Voraussetzungen

`ani-cli`, `ani-skip`, `mpv`, `yt-dlp`, Terminal: **Ghostty** oder **Kitty**.

```sh
cargo install --path .
ani-hop
```

Beim ersten Start (und immer, wenn etwas fehlt) prüft ani-hop alle Tools und das Kitty-Grafikprotokoll,
bietet die Installation fehlender Pakete per `yay`/`paru` an und schlägt den AniList-Login vor.

## Bedienung

| Taste | Aktion |
|---|---|
| tippen | filtern, fzf-Syntax: `'exakt ^anfang ende$ !nicht` |
| `↑↓` `^j/^k` `^n/^p` | bewegen |
| `tab` / `shift-tab` | markieren (Bulk) |
| `^a` | alle markieren |
| `enter` | auswählen |
| `esc` | Filter leeren / zurück |
| `^w` `^u` | Wort / Filter löschen |
| `^c` | beenden |

Mehrere Anime markieren + `enter` → alle (ungeschauten) Episoden laden oder Downloads löschen.

## AniList verbinden

Einmalig: unter <https://anilist.co/settings/developer> einen Client anlegen
(Redirect URL `https://anilist.co/api/v2/oauth/pin`), dann in ani-hop „Log in to AniList“
wählen, Client-ID und danach das angezeigte Token einfügen.

## Downloads

Landen in `~/Videos/Anime/<Titel>/` (oder `$ANI_CLI_DOWNLOAD_DIR`), eine `.media.json` verknüpft den Ordner mit AniList.
ani-cli-Variablen wirken wie gewohnt, z. B. `ANI_CLI_QUALITY=720`, `ANI_CLI_MODE=dub`.

## Wie ani-cli angebunden ist

ani-cli hat keine maschinenlesbare Ausgabe. Ohne TTY wechselt es auf rofi als Menü. ani-hop legt
ein eigenes `rofi`-Skript vorne in dessen `PATH`, das das Menü in eine Datei schreibt statt es anzuzeigen.
So bleibt das Scraping komplett bei ani-cli: Wechselt der Provider, reicht `ani-cli -U`.

Falls die automatische Zuordnung AniList → Provider danebenliegt: „Change source…“ im Anime-Menü,
die Wahl wird in `~/.config/ani-hop/sources.json` gespeichert.

Dateien: `~/.config/ani-hop/` (Token, Client-ID, Nutzer, Quellen-Overrides), `~/.cache/ani-hop/` (Poster, Listen, aufgelöste Quellen).
