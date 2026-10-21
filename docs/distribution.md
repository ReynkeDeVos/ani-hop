# Distributing ani-hop on Linux, macOS and Windows

Researched on 2026-10-21. Package versions and store policies change often, so every claim links to its source. Steps marked *(unconfirmed)* were not run and have no source that confirms them.

## 1. Recommendation

| Question | Answer | Why |
|---|---|---|
| Windows: MSI, EXE or winget? | None of them for now. Use **WSL** ([guide](#windows-via-wsl-step-by-step)). | An MSI or EXE installs no dependencies ([dist MSI docs](https://github.com/axodotdev/cargo-dist/blob/main/book/src/installers/msi.md)). winget has no ani-cli ([winget-pkgs](https://github.com/microsoft/winget-pkgs/tree/master/manifests), [Repology](https://repology.org/project/ani-cli/versions)), and its dependencies [must come from the same source](https://github.com/microsoft/winget-pkgs/blob/master/doc/manifest/schema/1.12.0/installer.md). A native port is about 250 lines, can't be tested on Arch, and still shows no posters. |
| macOS: DMG or Homebrew? | **Homebrew**: a formula in your own tap. | A formula pulls in mpv and yt-dlp, and `brew upgrade` keeps them current. A DMG installs nothing else and needs a [$99/year Developer ID](https://developer.apple.com/programs/) to [get past Gatekeeper](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution). Homebrew puts command-line tools in [formulae, not casks](https://docs.brew.sh/Acceptable-Casks). |
| Linux: AppImage, AUR, Flatpak, deb/rpm? | **AUR** (`ani-hop-bin`) first. Everyone else gets the release tarball or the `curl \| sh` installer. | The AUR has a current ani-cli ([5.1-1](https://aur.archlinux.org/packages/ani-cli)), and yay/paru [resolve AUR dependencies](https://wiki.archlinux.org/title/AUR_helpers). Flathub [does not accept console software](https://docs.flathub.org/docs/for-app-authors/requirements). An AppImage adds nothing to a static binary. deb and rpm would depend on a stale or missing ani-cli (section 3). |
| Do we need our own auto-updater for ani-hop and its dependencies? | **No.** | ani-cli is the only dependency that has to stay fresh. Every channel picked here either carries a current ani-cli and updates it with the rest (AUR, upstream Homebrew tap), or puts the upstream script in a user-owned folder where `ani-cli -U` updates it (other distros, WSL Ubuntu). ani-hop should only show the right update command (section 5). |

In this order:

1. Add a LICENSE ([there is none](https://api.github.com/repos/ReynkeDeVos/ani-hop); [Cargo.toml](../Cargo.toml) has no `license` either).
2. macOS fixes, about 50 lines (section 8), so the AniList login and a second running ani-hop work on a Mac before the first Mac build ships. `ANI_CLI_PLAYER=mpv` in the same change also covers WSL.
3. Set up one tag-triggered release with [dist](https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.0). It builds Linux (x86_64, aarch64, static musl) and Apple silicon binaries, creates the GitHub release, and pushes the Homebrew formula and the AUR package. You keep tagging by hand (section 6).
4. Add a WSL section to the README.
5. Native Windows: only if Windows users ask for it, and only after CI exists.

## 2. Starting point

ani-hop checks PATH for ani-cli, mpv, ani-skip, yt-dlp, ffmpeg, curl and xdg-open ([`src/ani.rs:144`](../src/ani.rs#L144)); yt-dlp, ffmpeg and curl are only used through ani-cli. It reads ani-cli's menus through a fake `rofi` script ([`src/ani.rs:24`](../src/ani.rs#L24)), runs mpv itself for downloaded episodes ([`src/ani.rs:576`](../src/ani.rs#L576)) and reads yt-dlp's `[download]` lines ([`src/ani.rs:810`](../src/ani.rs#L810)).

**macOS** compiles today, but four things break at runtime:
- The AniList login runs `xdg-open` ([`src/main.rs:334`](../src/main.rs#L334)), which macOS does not have.
- `running()` ([`src/ani.rs:853`](../src/ani.rs#L853)) and the poster cleanup ([`src/kitty.rs:115`](../src/kitty.rs#L115)) read `/proc`, so every other ani-hop looks dead. A second ani-hop then takes over the first one's download queue ([`src/main.rs:420`](../src/main.rs#L420)) and deletes its poster files.
- On macOS ani-cli picks IINA before mpv ([ani-cli L461](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L461)), and ani-cli's README tells Mac users to install IINA ([README](https://github.com/pystardust/ani-cli/blob/master/README.md)). ani-hop assumes mpv: it passes mpv flags ([`src/ani.rs:549`](../src/ani.rs#L549)) and waits for mpv's `VO:` line ([`src/ani.rs:455`](../src/ani.rs#L455)).
- The install offer only knows yay, paru and pacman ([`src/ani.rs:165`](../src/ani.rs#L165)).

Posters should work in Ghostty and kitty, because the poster file falls back from `/dev/shm` to `$TMPDIR` ([`src/kitty.rs:90`](../src/kitty.rs#L90)).

**Windows** does not compile, because of the `std::os::unix` imports ([`src/ani.rs:14`](../src/ani.rs#L14), [`src/main.rs:22`](../src/main.rs#L22), [`src/kitty.rs:11`](../src/kitty.rs#L11), test at [`src/ani.rs:1050`](../src/ani.rs#L1050)). Behind those:
- `Command::new("ani-cli")` cannot start an sh script, because Rust only adds `.exe` to bare names ([Command docs](https://doc.rust-lang.org/std/process/struct.Command.html)).
- PATH is joined with `:` ([`src/ani.rs:183`](../src/ani.rs#L183)).
- `have()` doesn't know about `.exe` ([`src/ani.rs:154`](../src/ani.rs#L154)).
- Downloads are stopped with `kill`/`pkill` ([`src/ani.rs:841`](../src/ani.rs#L841)).
- The installer runs through `sh -c` ([`src/main.rs:285`](../src/main.rs#L285)).
- Show folder names keep `:` and `?` ([`src/ani.rs:667`](../src/ani.rs#L667)), which Windows [does not allow](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file).

Posters can't show on Windows. ani-hop sends kitty Unicode placeholders ([`src/kitty.rs:145`](../src/kitty.rs#L145)), and no Windows terminal draws them:
- Windows Terminal has no kitty graphics ([#8389](https://github.com/microsoft/terminal/issues/8389)).
- WezTerm has no placeholder support ([#986](https://github.com/wezterm/wezterm/issues/986)).
- kitty and Ghostty have no Windows builds ([kitty](https://sw.kovidgoyal.net/kitty/binary/), [Ghostty](https://ghostty.org/download)).

## 3. Dependencies per channel

Latest upstream versions are in brackets. **Bold** means stale or missing.

| | Arch + AUR | Homebrew (macOS) | Debian 13 / Ubuntu 26.04 | Fedora 44 | Scoop | winget | nixpkgs unstable |
|---|---|---|---|---|---|---|---|
| **ani-cli** [5.1, master 5.1.5] | AUR 5.1, `ani-cli-git` | upstream tap 5.1, **not in core** | **4.10 / 4.10** | **none** (COPR **4.12.0**) | extras 5.1 | **none** | 5.1 |
| **ani-skip** [1.2.1] | `ani-skip-git` | **none** | **none** | **none** | **none** | **none** | 1.2.1 |
| mpv [0.41.0] | 0.41.0 | 0.41.0 | **0.40.0** / 0.41.0 | 0.41.0, but **H.264/HEVC need RPM Fusion's ffmpeg** | 0.41.0 | 0.41.0 | 0.41.0 |
| yt-dlp [2026.08.19] | current | current | **2025.04.30 / 2026.03.17** (backports current) | current | current | current | current |
| ffmpeg [9.0.2] | current | current | **7.1.5 / 8.0.1** | **ffmpeg-free 8.1.3** | current | current | current |
| curl-impersonate (optional) | extra | **none** | **none** | **none** | main | **none** | in the ani-cli wrapper |
| bash (Windows only) | | | | | `git` 2.56.0.2 | `Git.Git` **2.55.0.5** | |

Sources: Repology for [ani-cli](https://repology.org/project/ani-cli/versions), [ani-skip](https://repology.org/project/ani-skip/versions), [mpv](https://repology.org/project/mpv/versions), [yt-dlp](https://repology.org/project/yt-dlp/versions), [ffmpeg](https://repology.org/project/ffmpeg/versions) and [curl-impersonate](https://repology.org/project/curl-impersonate/versions). Also [AUR ani-cli](https://aur.archlinux.org/packages/ani-cli), the [upstream tap formula](https://github.com/pystardust/ani-cli/blob/master/Formula/ani-cli.rb), [brew core 404](https://formulae.brew.sh/api/formula/ani-cli.json), [Debian](https://qa.debian.org/madison.php?package=ani-cli), [Ubuntu](https://launchpad.net/ubuntu/+source/ani-cli), [COPR](https://copr.fedorainfracloud.org/coprs/derisis13/ani-cli/), [Fedora decoders](https://src.fedoraproject.org/rpms/ffmpeg/raw/f44/f/enable_decoders), [Scoop ani-cli](https://github.com/ScoopInstaller/Extras/blob/master/bucket/ani-cli.json), [Scoop git](https://github.com/ScoopInstaller/Main/blob/master/bucket/git.json), [winget Git.Git](https://github.com/microsoft/winget-pkgs/tree/master/manifests/g/Git/Git) and [nixpkgs ani-cli](https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/an/ani-cli/package.nix).

Takeaway:
- **Only ani-cli has to stay fresh.** It changed its streaming source twice this year ([v5.0](https://github.com/pystardust/ani-cli/releases/tag/v5.0), 2026-08-01; [v5.1](https://github.com/pystardust/ani-cli/releases/tag/v5.1), 2026-09-11), so the 4.x packages are probably broken (not tested). It is current only in the AUR, the upstream Homebrew tap, MacPorts, Scoop and nixpkgs-unstable. nixpkgs 26.05 still has 5.0.
- **yt-dlp can be a few months old.** ani-cli only hands it a direct link it has already found ([ani-cli L349](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L349)). For ani-hop it is still a hard dependency: ani-hop reads its progress lines, and it stops downloads the way it does so that ani-cli doesn't fall back to ffmpeg, which leaves a partial file that looks finished ([`src/ani.rs:838`](../src/ani.rs#L838)). AUR ani-cli doesn't depend on yt-dlp ([AUR](https://aur.archlinux.org/packages/ani-cli)), so ani-hop's packages must declare it.
- **mpv and ffmpeg are fine from any package manager,** apart from Fedora's codecs.
- **ani-skip is optional** and only packaged in the AUR and nixpkgs.

## 4. Per OS

### Linux

| Option | Installs deps | Updates deps | Updates ani-hop | Maintainer effort | Fits a terminal app |
|---|---|---|---|---|---|
| **AUR `ani-hop-bin`** | yes, ani-cli included | `yay -Syu` | same | low: CI pushes it | yes |
| **Release tarball / `curl \| sh`** | no | no | rerun the installer | none: dist makes it | yes |
| Homebrew on Linux (same tap) | mpv, yt-dlp, ffmpeg | `brew upgrade` | same | none | yes, but needs [glibc 2.39 for Tier 1](https://docs.brew.sh/Support-Tiers) |
| Nix flake | yes, if it follows nixos-unstable | channel update | same | low | yes |
| deb / rpm / PPA / COPR | ani-cli **stale or missing** | distro pace | yes | medium | yes |
| Flatpak | — | — | — | — | no: [console apps and host-dependent apps are rejected](https://docs.flathub.org/docs/for-app-authors/requirements) |
| Snap | would bundle mpv and ani-cli | you | yes | high | no: [classic confinement is not granted for host-only deps](https://snapcraft.io/docs/reference/administration/reviewing-classic-confinement-snaps/) |
| AppImage | no | no | needs [AppImageUpdate](https://github.com/AppImage/AppImageUpdate/releases), still alpha | low | no gain over a static binary, and it [needs FUSE](https://docs.appimage.org/user-guide/troubleshooting/fuse.html) |

**Pick:**
- AUR `ani-hop-bin`, pushed from CI (section 6). It is the maintainer's own platform, and `yay -Syu` updates ani-hop and ani-cli together.
- For other distros, the README says: install mpv, ffmpeg, yt-dlp, curl and patch from your distro, put upstream ani-cli in `~/.local/bin`, and run the installer. A user-owned ani-cli can update itself with `ani-cli -U` without sudo ([update_script](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L110-L127) patches its own file). ani-cli's own README uses `/usr/local/bin` and sudo instead ([README](https://github.com/pystardust/ani-cli/blob/master/README.md)).
- A `flake.nix` later, pinned to nixos-unstable.

### macOS

| Option | Installs deps | Updates deps | Updates ani-hop | Maintainer effort | Fits a terminal app |
|---|---|---|---|---|---|
| **Formula in own tap** | mpv, yt-dlp, ffmpeg; ani-cli from the upstream tap | `brew upgrade` | same | low: dist pushes it | yes |
| homebrew-core | — | — | — | — | not eligible: a self-submission needs [225 stars (or 90 forks or watchers) and a repo at least 30 days old](https://docs.brew.sh/Package-Acceptance-Policy); ani-hop has [1](https://api.github.com/repos/ReynkeDeVos/ani-hop) |
| Cask / DMG / pkg | no | no | yes | high: notarization, $99/year | no: [casks are quarantined](https://docs.brew.sh/Homebrew-Security-and-Supply-Chain), and [CLI tools belong in formulae](https://docs.brew.sh/Acceptable-Casks) |
| MacPorts | yes ([ani-cli 5.1](https://ports.macports.org/port/ani-cli/), no ani-skip) | `port upgrade` | needs a port | medium | later, if asked |
| `curl \| sh` installer | no | no | rerun | none | yes; curl [doesn't quarantine](https://developer.apple.com/forums/thread/666452) |

**Pick:** a formula in `ReynkeDeVos/homebrew-tap`, generated by dist from the prebuilt binary. It depends on the core formulae `mpv` and `yt-dlp`. Homebrew's mpv already pulls in ffmpeg and yt-dlp ([formula](https://formulae.brew.sh/formula/mpv)). User steps *(unconfirmed, not run on a Mac)*:

```sh
brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git
brew trust pystardust/ani-cli
brew install ani-cli reynkedevos/tap/ani-hop
brew install --cask ghostty    # or kitty; posters only show there
```

Updates: `brew upgrade`.

Don't make the formula depend on `pystardust/ani-cli/ani-cli`. Since Homebrew 6.0, non-official taps [need explicit trust](https://docs.brew.sh/Tap-Trust), and only a name typed on the command line is trusted automatically. The docs don't say that this covers dependencies, and Homebrew's [loader checks every formula](https://github.com/Homebrew/brew/blob/main/Library/Homebrew/trust.rb). pystardust's tap also needs the explicit URL above. If ani-cli is missing, ani-hop's own dependency check reports it.

Limits:
- **Apple silicon on macOS 15 or newer only.** Homebrew [stopped building Intel bottles](https://docs.brew.sh/Support-Tiers), so mpv and ffmpeg would compile from source on an Intel Mac.
- **Posters need Ghostty or kitty.** Those are the only terminals ani-hop detects ([`src/kitty.rs:51`](../src/kitty.rs#L51)).
- **Use the mpv formula, not IINA** (see section 2). The mpv cask is [disabled](https://github.com/Homebrew/homebrew-cask/blob/main/Casks/s/stolendata-mpv.rb).

### Windows

| Option | Installs deps | Updates deps | Updates ani-hop | Maintainer effort | Posters |
|---|---|---|---|---|---|
| **WSL (Ubuntu or Arch)** | apt/pacman, ani-cli from upstream or the AUR | `apt upgrade` + `ani-cli -U`, or `yay -Syu` | like Linux | no code (one line recommended) | in kitty under WSLg |
| Native in Git Bash | winget for mpv and yt-dlp, curl for ani-cli | `winget upgrade --all`, `ani-cli -U` | rerun the installer | **L**: about 250 lines behind `cfg(windows)`, untestable on Arch | none |
| Own Scoop bucket | all, ani-cli included ([extras](https://github.com/ScoopInstaller/Extras/blob/master/bucket/ani-cli.json)) | `scoop update *` | same | medium + the same port; runs PowerShell underneath | none |
| winget package | not ani-cli | `winget upgrade` | same | medium, with [manual review](https://learn.microsoft.com/en-us/windows/package-manager/package/repository) | none |
| MSI / EXE | no | no | ani-hop only | medium + signing | none |

What native-in-Git-Bash would mean:
- **For users** *(unconfirmed, nothing was run on Windows)*. Everything happens in Git Bash, with no admin rights and no PowerShell:
  ```sh
  winget install -e --id mpv-player.mpv-CI.MSVC --accept-source-agreements --accept-package-agreements
  winget install -e --id yt-dlp.yt-dlp --accept-source-agreements --accept-package-agreements  # also installs yt-dlp.FFmpeg and Deno
  mkdir -p ~/bin && curl -fsSL -o ~/bin/ani-cli https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli
  curl -fsSL https://github.com/ReynkeDeVos/ani-hop/releases/latest/download/ani-hop-installer.sh | sh
  # then open a new Git Bash window, because winget changed PATH
  ```
  Why this should work:
  - winget runs in Git Bash: Git for Windows removed its winpty aliases as no longer needed ([build-extra #664](https://github.com/git-for-windows/build-extra/pull/664), shipped in 2.54.0).
  - `~/bin` is first on Git Bash's PATH ([env.sh](https://github.com/git-for-windows/build-extra/blob/main/git-extra/env.sh)).
  - dist's `installer.sh` detects MINGW/MSYS and picks the Windows build ([template](https://github.com/axodotdev/cargo-dist/blob/main/cargo-dist/templates/installer/installer.sh.j2)).
  - winget's [PATH change needs a new shell](https://github.com/microsoft/winget-cli/blob/master/src/AppInstallerCLIPackage/Shared/Strings/en-us/winget.resw).
- **Scoop is left out** under the no-PowerShell rule, even though it is the only Windows manager with ani-cli and `scoop update *` would refresh everything. Its [installer is a PowerShell script](https://github.com/ScoopInstaller/Install), and even typed in Git Bash, every `scoop` command goes through a shim that runs `powershell.exe` ([core.ps1](https://github.com/ScoopInstaller/Scoop/blob/master/lib/core.ps1), `shim()`). If PowerShell running in the background is acceptable, an own bucket beats winget + curl.
- **For the code:** see the Windows list in section 8.
- **Posters:** none. Windows Terminal only does [Sixel since 1.22](https://github.com/microsoft/terminal/releases/tag/v1.22.10352.0). A Sixel path would be extra work.
- **ani-skip:** leave it off. ani-cli's README says it ["does not seem to work under Windows"](https://github.com/pystardust/ani-cli/blob/master/README.md).

**Pick: WSL now.** It needs no code, it gets the Linux channels and their updates, and it is the only way to get posters on Windows. The cost for users is admin rights and one restart ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/install)). Do the native port only once dist and a Windows CI build exist and someone asks for it.

#### Windows via WSL, step by step

The guide uses **Ubuntu 26.04**:
- It is Microsoft's default WSL distro, and `Ubuntu-26.04` is its pinned name in the [catalog](https://raw.githubusercontent.com/microsoft/WSL/master/distributions/DistributionInfo.json). The pinned name matters: an older `Ubuntu` install may be 24.04, whose default Rust (1.75) can't build edition 2024 and whose xdg-utils (1.1.3) can't open the Windows browser (Repology: [rust](https://repology.org/project/rust/versions), [xdg-utils](https://repology.org/project/xdg-utils/versions)).
- Its first start creates a user with sudo ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/setup/environment)).
- Its mpv is current, and its ffmpeg, kitty (0.45) and xdg-utils (1.2.1) are new enough ([Repology](https://repology.org/project/kitty-terminal/versions)).

ani-cli, which must stay fresh, goes in `~/.local/bin`, where it updates itself without sudo. yt-dlp goes there too: Ubuntu's is five months behind, and the release binary updates itself. **Arch** keeps everything fresh with one `yay -Syu`, but it starts as root with no user and no sudo, needs yay bootstrapped, and is x86_64 only ([Arch wiki](https://wiki.archlinux.org/title/Install_Arch_Linux_on_WSL), [catalog](https://raw.githubusercontent.com/microsoft/WSL/master/distributions/DistributionInfo.json)). It is listed at the end for Arch users.

Nothing below was run on a real Windows machine.

You need Windows 11, or Windows 10 build 19044 or newer for [WSLg](https://learn.microsoft.com/en-us/windows/wsl/tutorials/gui-apps), which shows the mpv and kitty windows. Windows 10 Home and Pro are [out of support](https://learn.microsoft.com/en-us/lifecycle/products/windows-10-home-and-pro). Virtualization must be enabled in the BIOS/UEFI ([troubleshooting](https://learn.microsoft.com/en-us/windows/wsl/troubleshooting)).

The only Windows-side commands are `wsl …`. They take no file paths, so Git Bash's path conversion doesn't touch them, and none needs PowerShell.

1. **Install WSL, once.** Start menu → "Git Bash" (or "Command Prompt") → right-click → *Run as administrator*. Microsoft documents [PowerShell or Command Prompt](https://learn.microsoft.com/en-us/windows/wsl/setup/environment); Git Bash runs the same `wsl.exe` but is *(unconfirmed)*.
   ```
   wsl --install -d Ubuntu-26.04
   ```
   Plain `wsl --install` only works if WSL isn't installed yet ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/install)); `-d` works either way. Restart. Open "Ubuntu 26.04 LTS" from the Start menu and choose a user name and password.
2. **Tools from Ubuntu.** In the Ubuntu window:
   ```sh
   sudo apt update && sudo apt upgrade -y
   sudo apt install -y mpv ffmpeg curl patch fzf xdg-utils kitty cargo build-essential
   ```
   Don't install apt's `ani-cli` (4.10) or `yt-dlp` (2026.03.17).
3. **ani-cli, yt-dlp and ani-skip from upstream:**
   ```sh
   mkdir -p ~/.local/bin ~/.config/mpv/scripts && cd ~/.local/bin
   curl -fsSLO https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli
   curl -fsSL -o yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux
   curl -fsSLO https://raw.githubusercontent.com/synacktraa/ani-skip/master/ani-skip
   curl -fsSL -o ~/.config/mpv/scripts/skip.lua https://raw.githubusercontent.com/synacktraa/ani-skip/master/integrations/mpv.lua
   chmod +x ani-cli yt-dlp ani-skip && cd
   ```
   On an ARM PC, use `yt-dlp_linux_aarch64` ([yt-dlp](https://github.com/yt-dlp/yt-dlp#release-files)). Don't use ani-skip's install script: on WSL it insists on Windows mpv.exe ([install](https://raw.githubusercontent.com/synacktraa/ani-skip/master/install)).
4. **ani-hop.** Ubuntu 26.04's Rust is newer than the 1.89 that ani-hop's locked dependencies need ([Repology](https://repology.org/project/rust/versions), [Cargo.lock](../Cargo.lock)), but this build was not tried *(unconfirmed)*:
   ```sh
   cargo install --locked --root ~/.local --git https://github.com/ReynkeDeVos/ani-hop
   ```
   Once releases exist, this step becomes one `curl … | sh` line.
5. **Two settings:**
   ```sh
   echo 'export PATH="$HOME/.local/bin:$PATH" ANI_CLI_PLAYER=mpv' >> ~/.bashrc && . ~/.bashrc
   ```
   You need `ANI_CLI_PLAYER=mpv`. On WSL2, ani-cli otherwise looks for Windows `mpv.exe` ([L464](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L464); the WSL kernel name [ends in WSL2](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.18.y/arch/x86/configs/config-wsl)), and Play fails with "Program mpv.exe not found". Search and downloads work without it. The setting goes away once ani-hop sets it itself (section 8).
6. **Start it.**
   - **With posters:** type `kitty &` in the Ubuntu window, then `ani-hop` in the kitty window *(unconfirmed)*. Only a terminal inside the same WSL distro can show posters, because ani-hop hands each one over as a Linux file ([`src/kitty.rs:145`](../src/kitty.rs#L145)). Under WSLg, Mesa provides [OpenGL 4.6 through d3d12 or in software](https://gitlab.freedesktop.org/mesa/mesa/-/blob/main/docs/features.txt), which is more than kitty needs. If kitty says "Wayland: Failed to connect to display", run `mkdir -p ~/.config/kitty && echo 'linux_display_server x11' >> ~/.config/kitty/kitty.conf` ([kitty #8463](https://github.com/kovidgoyal/kitty/issues/8463), [wslg #1032](https://github.com/microsoft/wslg/issues/1032)); whether current WSL still needs this is *(unconfirmed)*.
   - **Without posters:** run `ani-hop` in the "Ubuntu 26.04 LTS" tab of Windows Terminal.
7. **AniList login:** nothing to do. xdg-utils 1.2 detects WSL and opens links in the Windows default browser ([xdg-utils-common](https://gitlab.freedesktop.org/xdg/xdg-utils/-/blob/master/scripts/xdg-utils-common.in), [xdg-open](https://gitlab.freedesktop.org/xdg/xdg-utils/-/blob/master/scripts/xdg-open.in)). Ubuntu 26.04 and Arch ship 1.2.1.
8. **Downloads** go to `~/Videos/Anime` inside Linux ([`src/ani.rs:95`](../src/ani.rs#L95)). In Explorer that is `\\wsl.localhost\Ubuntu-26.04\home\<user>\Videos\Anime`, or run `cd ~/Videos/Anime && explorer.exe .` ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/setup/environment)). You can set "Download folder…" to `/mnt/c/Users/<you>/Videos`, but Microsoft says crossing file systems [is slower](https://learn.microsoft.com/en-us/windows/wsl/filesystems).
9. **Updates:**
   ```sh
   sudo apt update && sudo apt upgrade
   ani-cli -U; yt-dlp -U; ani-skip -U
   cargo install --locked --force --root ~/.local --git https://github.com/ReynkeDeVos/ani-hop
   ```
   Also run `wsl --update` in Git Bash or Command Prompt now and then ([basic commands](https://learn.microsoft.com/en-us/windows/wsl/basic-commands)).
   - `ani-cli -U` pulls ani-cli's master branch, not the latest tag ([update_script](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L110-L127)).
   - `yt-dlp -U` works because this is the official release binary ([yt-dlp](https://github.com/yt-dlp/yt-dlp#update)).
   - `ani-skip -U` also updates `~/.config/mpv/scripts/skip.lua` ([update script](https://raw.githubusercontent.com/synacktraa/ani-skip/master/integrations/update)).

**Arch variant.** Commands from the [Arch wiki](https://wiki.archlinux.org/title/Install_Arch_Linux_on_WSL):

```sh
wsl --install -d archlinux                         # admin Git Bash or Command Prompt, as above
# in archlinux, as root:
pacman -Syu --needed sudo base-devel git rust mpv yt-dlp ffmpeg curl patch xdg-utils kitty ttf-dejavu
useradd -m -G wheel -s /bin/bash NAME && passwd NAME
echo '%wheel ALL=(ALL:ALL) ALL' > /etc/sudoers.d/10-wheel
# back in Git Bash or Command Prompt:
wsl --manage archlinux --set-default-user NAME && wsl --terminate archlinux
# in archlinux, as NAME:
git clone https://aur.archlinux.org/yay-bin.git && (cd yay-bin && makepkg -si)
yay -S ani-cli ani-skip-git                        # later: yay -S ani-hop-bin
cargo install --locked --git https://github.com/ReynkeDeVos/ani-hop
echo 'export PATH="$HOME/.cargo/bin:$PATH" ANI_CLI_PLAYER=mpv' >> ~/.bashrc
```

Then start kitty as in step 6. Updates: `yay -Syu` and `wsl --update`.
- The image ships no fonts, so `ttf-dejavu` is a precaution *(unconfirmed)*.
- AUR `ani-cli` follows tags (5.1) and lives in `/usr/bin`, so `ani-cli -U` can't update it. For master fixes, use `ani-cli-git` and `yay -Syu --devel`.

## 5. Updates

The package manager should own updates, for ani-hop and its dependencies alike:
- **It owns the files.** `sudo ani-cli -U` patches its own file in place ([update_script](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L110-L127)), so on pacman or apt installs it edits a file the package manager owns. On Nix it can't work at all ([package.nix](https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/an/ani-cli/package.nix)).
- **yt-dlp won't update itself there.** `yt-dlp -U` refuses for package-manager builds and points to the manager ([update.py](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/update.py)).
- **Homebrew core forbids it.** Core requires self-updating to be disabled ([Acceptable Formulae](https://docs.brew.sh/Acceptable-Formulae)). A tap isn't bound by that, but the reason is the same.
- **Ready-made updaters are a poor fit.** dist's updater is experimental and only covers its shell and PowerShell installers ([updater](https://axodotdev.github.io/cargo-dist/book/installers/updater.html)), and its library pulls in reqwest and tokio ([axoupdater](https://github.com/axodotdev/axoupdater/blob/main/axoupdater/Cargo.toml)).

What ani-hop can still do, small and hint-only:
1. **ani-cli version.** Run `ani-cli -V` along with the existing dependency check. If it is older than a `TESTED_ANI_CLI` constant (5.1 today), say so on the dependency screen with the right command:
   - the script is user-writable: `ani-cli -U`
   - `/usr/bin` with yay or paru: `yay -Syu`
   - under `brew --prefix`: `brew upgrade ani-cli`
   - under `/nix/store`: update the channel

   Never suggest `sudo ani-cli -U`.
2. **New ani-hop release.** At most once a day, request `https://github.com/ReynkeDeVos/ani-hop/releases/latest` without following the redirect. The `Location` header names the newest tag, and this needs no API call or rate limit ([GitHub](https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases)). If the tag is newer than `CARGO_PKG_VERSION`, show one line with the command for the install channel:
   - a dist receipt in `~/.config/ani-hop/` ([installer](https://github.com/axodotdev/cargo-dist/blob/main/cargo-dist/templates/installer/installer.sh.j2)): rerun the installer line
   - an executable under `brew --prefix`: `brew upgrade`
   - `/usr/bin`: `yay -Syu`
   - otherwise: `cargo install --locked --force --git …`

   Check the receipt first, because dist's installer also defaults to `~/.cargo/bin`.
3. Not worth doing: running updates for the user, and checking yt-dlp, which [warns by itself after 90 days](https://github.com/yt-dlp/yt-dlp#update).

## 6. Release pipeline

**Tooling: dist, plus one custom job for the AUR.**

| | dist 0.33.0 | GoReleaser | Hand-written (taiki-e actions) |
|---|---|---|---|
| Rust | native; [in Arch extra](https://archlinux.org/packages/extra/x86_64/cargo-dist/) | [partial](https://goreleaser.com/customization/builds/builders/rust/) (zigbuild, workspace caveats) | [yes](https://github.com/taiki-e/upload-rust-binary-action) |
| Homebrew | formula with `depends_on` ([docs](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html)) | formula publisher [deprecated in favour of casks](https://goreleaser.com/customization/publish/homebrew_formulas/) | write it yourself |
| AUR / Scoop / winget | custom jobs ([docs](https://github.com/axodotdev/cargo-dist/blob/main/book/src/ci/customizing.md)) | built in | write it yourself |
| Release notes from CHANGELOG.md | yes, via [parse-changelog](https://github.com/taiki-e/parse-changelog); the `## 0.5.0 – 2026-10-20` headings work | not checked | yes, [same parser](https://github.com/taiki-e/create-gh-release-action) |
| Upkeep | generated workflow, regenerate on upgrade | config file | all the YAML is yours |

dist is community-maintained since 2025 ([announcement](https://blog.mistys-internet.website/post/2025/07/20/dist-0-dot-28-dot-1-is-out)) and [still releases](https://github.com/axodotdev/cargo-dist/releases).

**One-time setup:**
1. Add a LICENSE, plus `license`, `description`, `repository` and `homepage` in Cargo.toml. `dist init` stops without `repository`.
2. Create an empty repo, `ReynkeDeVos/homebrew-tap`.
3. Register on the AUR and add an SSH key.
4. Run `sudo pacman -S cargo-dist`, then `dist init`. Edit `dist-workspace.toml` roughly like this, and check the result with `dist plan`:
   ```toml
   [dist]
   targets = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "aarch64-apple-darwin"]
   installers = ["shell", "homebrew"]
   tap = "ReynkeDeVos/homebrew-tap"
   publish-jobs = ["homebrew", "./publish-aur"]

   [dist.github-custom-runners]          # dist's defaults are being retired
   global = "ubuntu-24.04"               # plan/host/publish jobs only, not builds
   x86_64-unknown-linux-musl = "ubuntu-24.04"
   aarch64-unknown-linux-musl = "ubuntu-24.04-arm"
   aarch64-apple-darwin = "macos-15"

   [dist.dependencies.homebrew]
   mpv = { stage = ["run"] }
   yt-dlp = { stage = ["run"] }
   ```
5. Add `.github/workflows/publish-aur.yml`, a `workflow_call` job using [KSXGitHub/github-actions-deploy-aur](https://github.com/KSXGitHub/github-actions-deploy-aur). It pushes `ani-hop-bin` with:
   ```
   depends=(ani-cli mpv yt-dlp ffmpeg curl xdg-utils)
   optdepends=('ani-skip: skips openings and endings' 'curl-impersonate: gets ani-cli past Cloudflare'
               'ghostty: posters' 'kitty: posters')
   ```
   - Keep the dependencies unversioned. `ani-cli-git` [provides an unversioned `ani-cli`](https://aur.archlinux.org/packages/ani-cli-git), which a versioned dependency would [not accept](https://wiki.archlinux.org/title/PKGBUILD#provides).
   - Put a LICENSE in the AUR repo too; the [guidelines ask for one](https://wiki.archlinux.org/title/AUR_submission_guidelines).

**Every release afterwards:**
1. Open a PR with the version bump and the CHANGELOG entry, and squash-merge it, as now.
2. Run `git tag vX.Y.Z && git push origin vX.Y.Z`. v0.5.0 is not tagged yet.
3. `release.yml` runs plan, then builds the three targets. It then creates the GitHub release with the archives, checksums, `ani-hop-installer.sh` and notes from CHANGELOG.md, pushes `ani-hop.rb` to the tap, and pushes the PKGBUILD to the AUR.

Stop writing GitHub releases by hand. Release titles change to the CHANGELOG heading (`0.5.0 – 2026-10-20`). Every PR also gets a `dist plan` check.

**Secrets:**

| Secret | For | Notes |
|---|---|---|
| `HOMEBREW_TAP_TOKEN` | pushing the formula | PAT with write access to the tap repo. dist's docs say a classic PAT with `repo` scope; a fine-grained PAT with Contents write should work, since the job only checks out and pushes (inferred) |
| `AUR_SSH_PRIVATE_KEY` | pushing the PKGBUILD | passwordless key registered on the AUR account |
| `GITHUB_TOKEN` | creating the release | automatic |

Traps:
- **Runners being retired.** dist's defaults include `macos-14` ([retired 2026-11-02](https://github.com/actions/runner-images/issues/13518)) and `ubuntu-22.04` ([retired 2027-04-17](https://github.com/actions/runner-images/issues/14254)), hence the custom runners above. `global` [only covers the non-build jobs](https://github.com/axodotdev/cargo-dist/blob/main/book/src/reference/config.md#github-custom-runners), so each target needs its own line.
- **No partial releases.** dist [refuses to release](https://github.com/axodotdev/cargo-dist/blob/main/book/src/reference/config.md#fail-fast) if any target fails, so leave Windows out until it compiles.
- **`on: release` workflows won't fire.** A release created with `GITHUB_TOKEN` does not [trigger other workflows](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow). That is why the AUR job runs as a dist publish job.
- Optional later: `cargo binstall --git https://github.com/ReynkeDeVos/ani-hop ani-hop` already works with these archives without crates.io ([cargo-binstall](https://github.com/cargo-bins/cargo-binstall)). This was not tested.

## 7. Rejected options

- **MSI/EXE installer:** installs no dependencies, and its updater only updates ani-hop ([dist MSI](https://github.com/axodotdev/cargo-dist/blob/main/book/src/installers/msi.md)). Azure signing for individuals is [US/Canada only](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart).
- **winget package:** ani-cli isn't in winget, and dependencies [must come from the same source](https://github.com/microsoft/winget-pkgs/blob/master/doc/manifest/schema/1.12.0/installer.md).
- **Chocolatey:** has no ani-cli ([Repology](https://repology.org/project/ani-cli/versions)).
- **Own Scoop bucket:** would keep everything fresh, but Scoop's installer and every `scoop` command run PowerShell, even from Git Bash (section 4). Revisit if that is acceptable.
- **Native Windows now:** about 250 lines, no posters, no local testing, and ani-cli itself rates Windows [Tier 2](https://github.com/pystardust/ani-cli/blob/master/README.md).
- **Windows mpv.exe under WSL**, as ani-cli's README suggests: `have("mpv")` and `play_file` need Linux mpv ([`src/ani.rs:154`](../src/ani.rs#L154), [`src/ani.rs:576`](../src/ani.rs#L576)), and Windows programs get Linux paths [unconverted](https://learn.microsoft.com/en-us/windows/wsl/filesystems).
- **DMG, pkg, cask, notarization:** install no dependencies, [cost $99/year](https://developer.apple.com/programs/), and [CLI tools belong in formulae](https://docs.brew.sh/Acceptable-Casks).
- **homebrew-core:** [notability and repo age](https://docs.brew.sh/Package-Acceptance-Policy), and ani-cli isn't in core.
- **Intel macOS builds:** Homebrew is [Tier 3 on Intel with no new bottles](https://docs.brew.sh/Support-Tiers).
- **Flatpak:** [Flathub rejects console apps and apps that rely on host components](https://docs.flathub.org/docs/for-app-authors/requirements).
- **Snap:** [no classic confinement for host-only deps](https://snapcraft.io/docs/reference/administration/reviewing-classic-confinement-snaps/), so you would bundle and refresh mpv and ani-cli yourself.
- **AppImage:** not sandboxed, but installs no dependencies, [needs FUSE](https://docs.appimage.org/user-guide/troubleshooting/fuse.html), and adds nothing to a static binary.
- **deb/rpm/PPA/COPR:** they would depend on ani-cli 4.x or on a stale COPR (section 3), and the tarball plus upstream ani-cli already covers those distros.
- **Bundling mpv** (as [Curd's Windows installer](https://github.com/Wraient/curd/blob/main/Build/curd-windows-build.iss) does): freezes the versions you then have to update.
- **ani-hop downloading and managing its own ani-cli copy:** works the same everywhere, but it is an updater with a trust boundary, and the chosen channels already carry a current ani-cli. Reconsider if Debian/Ubuntu users ask.
- **Own self-updater** (axoupdater, self_update): the package managers own the files (section 5).
- **GoReleaser:** its Homebrew formulas are [deprecated in favour of casks](https://goreleaser.com/customization/publish/homebrew_formulas/), and its Rust support is partial.

## 8. Code changes needed

**All platforms**
- [ ] LICENSE file, plus `license`, `description`, `repository` and `homepage` in [Cargo.toml](../Cargo.toml). XS.
- [ ] [`src/ani.rs:549`](../src/ani.rs#L549) `stream()`: pass `ANI_CLI_PLAYER=mpv` when the user hasn't set it. ani-hop already requires mpv ([`src/ani.rs:146`](../src/ani.rs#L146)). This fixes WSL ([ani-cli L464](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L464)) and macOS with IINA ([L461](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L461)). XS.
- [ ] The update hints from section 5, on the dependency screen ([`src/ui.rs:491`](../src/ui.rs#L491)). S.

**macOS** (S, about 50 lines, plus one run on a Mac or a `macos-15` runner)
- [ ] [`src/main.rs:333`](../src/main.rs#L333) `open_url`: use `open` on macOS, and drop the `xdg-open` row from DEPS there ([`src/ani.rs:151`](../src/ani.rs#L151)). XS.
- [ ] [`src/ani.rs:852`](../src/ani.rs#L852) `running()` and [`src/kitty.rs:115`](../src/kitty.rs#L115): replace `/proc` with a per-process `queue/<pid>.lock` that is held for the whole run and probed with [`File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html). This also works on Windows and survives pid reuse. S.
- [ ] [`src/ani.rs:165`](../src/ani.rs#L165) `install_command`: add a brew branch, with brew package names in DEPS and an updated hint at [`src/ui.rs:521`](../src/ui.rs#L521). For apt and dnf, only offer the stable deps, never ani-cli. S.
- [ ] Optional: default downloads to `~/Movies` on macOS ([`src/ani.rs:61`](../src/ani.rs#L61)). XS.

**Windows native** (L, about 250 lines; later, only with a Windows CI job)
- [ ] Compile: put `#[cfg(unix)]` on [`src/ani.rs:14`](../src/ani.rs#L14), [`:179`](../src/ani.rs#L179) and [`:1050`](../src/ani.rs#L1050), and on [`src/main.rs:22`](../src/main.rs#L22) and [`:321`](../src/main.rs#L321). [`src/kitty.rs:142`](../src/kitty.rs#L142): use `as_encoded_bytes()` instead of `OsStrExt`. S.
- [ ] [`src/ani.rs:182`](../src/ani.rs#L182) `ani_cli()`: run Git's `bash.exe` by full path (`GIT_INSTALL_ROOT`, or `InstallPath` under `HKCU/HKLM\Software\GitForWindows`), never a bare `bash`. Rust searches System32 before the inherited PATH, and System32 holds WSL's `bash.exe` when WSL is installed ([Command docs](https://doc.rust-lang.org/std/process/struct.Command.html)). Build PATH with `std::env::join_paths`. M.
- [ ] [`src/ani.rs:586`](../src/ani.rs#L586): ani-skip through bash too, or switch it off on Windows. S.
- [ ] [`src/ani.rs:154`](../src/ani.rs#L154) `have()`: accept `.exe`, and check ani-cli through bash's `command -v`. S.
- [ ] [`src/ani.rs:834`](../src/ani.rs#L834) `stop_download`: a [Job Object](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects), or `taskkill /PID <bash> /T /F`. M.
- [ ] [`src/ani.rs:667`](../src/ani.rs#L667): map `<>:"\|?*` in show folder names to look-alikes, as `/` is today. S.
- [ ] [`src/main.rs:334`](../src/main.rs#L334): `explorer.exe <url>`. Avoid `cmd /c start`, because [`&` splits the command](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/cmd) and the login URL contains one ([`src/main.rs:1379`](../src/main.rs#L1379)). XS.
- [ ] [`src/main.rs:285`](../src/main.rs#L285): run winget directly instead of `sh -c`. XS.
- [ ] [`src/main.rs:232`](../src/main.rs#L232): ignore an `EnableBracketedPaste` error, which crossterm [returns where VT is unavailable](https://github.com/crossterm-rs/crossterm/blob/master/src/event/commands.rs). XS.
- [ ] Posters: none needed. `kitty::supported()` is false there ([`src/kitty.rs:51`](../src/kitty.rs#L51)), so they switch off.

## 9. Open questions

1. **Which license?** It blocks nothing technically for your own tap and AUR package, but third parties can't legally repackage ani-hop without one.
2. **Nothing was run on Windows or a Mac.**
   - On Windows: the WSL guide, kitty under WSLg, the Arch Wayland workaround, and `wsl --install` from an admin Git Bash.
   - On a Mac: the tap install sequence, and whether kitty accepts poster files from `$TMPDIR`. Ghostty's code does accept them ([graphics_image.zig](https://github.com/ghostty-org/ghostty/blob/v1.3.1/src/terminal/kitty/graphics_image.zig)).
3. **Should ani-hop be tested against ani-cli master or tags?** `-U` jumps to master (5.1.5 today), while packages ship tags (5.1). ani-hop depends on undocumented ani-cli behaviour (rofi menus, debug player output, file names), and master changed download file naming after 5.1 ([compare](https://github.com/pystardust/ani-cli/compare/v5.1...master)).
4. **Does ani-cli 4.10 or 4.14 still play anything?** If not, should ani-hop refuse versions below 5.1 rather than just warn?
5. **Does dist build the musl targets with these custom runners?** Check with `dist plan` and a test tag.
6. **Native Windows, if ever:**
   - Does `mpv.exe` still print `VO:` into a pipe? The [source suggests yes](https://github.com/mpv-player/mpv/blob/master/osdep/main-fn-win.c).
   - Does the fake rofi work under Git Bash?
   - Does ani-skip work there at all?
7. **Is there Windows demand at all?** That decides whether the native port is ever worth it.
