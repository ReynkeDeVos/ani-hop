# ani-hop

![ani-hop demo](demo.avif)

A fast terminal UI for anime: find a show, then stream or download it through [ani-cli](https://github.com/pystardust/ani-cli).

- **No scraper of its own.** Every source comes from `ani-cli`, which a large community keeps working on. When a site changes, updating `ani-cli` is enough.
- **Posters in the terminal.** In Ghostty and kitty, even inside tmux/herdr, though slower there.
- **AniList built in.** Your lists, with watched episodes marked. After each episode it asks to save your progress, and "Watched up to…" sets it for many episodes at once.
- **Find an episode by what happens in it.** The filter searches the titles and summaries of watched episodes. Unwatched ones stay blank, so nothing spoils.
- **Downloads in the background.** Pause them or quit; unfinished ones come back after a restart.
- **Sub or dub.** Japanese or English audio, with or without subtitles, for all shows or just one.
- **Skips openings and endings.** With `ani-skip`, in streams and downloads alike.

## Install

### Debian/Ubuntu, Fedora, Arch and their derivatives

```sh
curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh
```

Installs ani-hop, ani-cli, mpv and the rest. Run it again to update; ani-hop tells you when there's a new version.

### macOS (Apple silicon)

```sh
brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git
brew install pystardust/ani-cli/ani-cli reynkedevos/tap/ani-hop
```

Update with `brew upgrade`.

### Windows

ani-hop runs in WSL. In PowerShell as admin:

```powershell
wsl --install -d Ubuntu-26.04
```

Restart, open Ubuntu and run the Linux line above. For posters, see the [WSL guide](docs/distribution.md#windows-via-wsl-step-by-step).

<details>
<summary>Other distros, Nix, or from source</summary>

On other distros the script installs ani-hop, ani-cli and ani-skip, then lists the system packages to install yourself.

With Nix, on NixOS or on Linux and macOS (Apple silicon):

```sh
nix profile add github:ReynkeDeVos/ani-hop
```

Brings its own ani-cli, mpv and the rest. Update with `nix profile upgrade ani-hop`; `nix run github:ReynkeDeVos/ani-hop` tries it without installing. Both need flakes switched on.

On NixOS or with Home Manager, add it to your flake instead:

```nix
inputs.ani-hop.url = "github:ReynkeDeVos/ani-hop";
# in a module (Home Manager: home.packages)
environment.systemPackages = [ inputs.ani-hop.packages.${pkgs.stdenv.hostPlatform.system}.default ];
```

Update with `nix flake update ani-hop` and a rebuild. Leave out `inputs.nixpkgs.follows` unless your nixpkgs is unstable: ani-hop needs ani-cli 5.1, and NixOS 26.05 has 5.0.

From source: `cargo install --locked --git https://github.com/ReynkeDeVos/ani-hop` (Rust 1.89 or newer). It needs [ani-cli](https://github.com/pystardust/ani-cli) 5.1 or newer, [mpv](https://mpv.io), [yt-dlp](https://github.com/yt-dlp/yt-dlp), ffmpeg, curl, patch and xdg-utils; [ani-skip](https://github.com/synacktraa/ani-skip) is optional. ani-hop checks for them at the start and offers to install what's missing.

</details>

## License

[GPL-3.0-or-later](LICENSE)
