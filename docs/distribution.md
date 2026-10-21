# Distributing ani-hop on Linux, macOS and Windows

Researched on 2026-10-21 and updated the same day with the maintainer's decisions: Linux comes first (install script, AUR, Homebrew), the licence is GPL, and Windows only gets a WSL guide. Package versions and store policies change often, so every claim links to its source. Steps marked *(unconfirmed)* were not run and have no source that confirms them. The first version, with the full Windows analysis, is commit `b09665e`.

## 1. Decisions and order

| Platform | Channel | Why |
|---|---|---|
| **Debian/Ubuntu, Fedora, Arch** | **Install script** (section 4) | It installs mpv, ffmpeg, curl, patch and xdg-utils from the distro, plus yt-dlp where the distro's is current. Upstream ani-cli goes in `~/.local/bin`, where `ani-cli -U` keeps it fresh without sudo, and the ani-hop binary goes there too. Running the script again updates them. |
| **Arch** | **AUR `ani-hop-bin`** | The AUR has a current ani-cli ([5.1-1](https://aur.archlinux.org/packages/ani-cli)), yay/paru [resolve AUR dependencies](https://wiki.archlinux.org/title/AUR_helpers), and `yay -Syu` updates everything. The install script hands over to it when yay or paru is installed. |
| **macOS, and Linux with Homebrew** | **Formula in your own tap** | It pulls in mpv, yt-dlp and ffmpeg, and `brew upgrade` keeps them current. ani-cli comes from the upstream tap, whose formula has no macOS-only parts ([formula](https://github.com/pystardust/ani-cli/blob/master/Formula/ani-cli.rb)). mpv, ffmpeg and yt-dlp have x86_64 and arm64 Linux bottles ([mpv](https://formulae.brew.sh/api/formula/mpv.json), [ffmpeg](https://formulae.brew.sh/api/formula/ffmpeg.json), [yt-dlp](https://formulae.brew.sh/api/formula/yt-dlp.json)). No DMG: it installs nothing else and needs a [$99/year Developer ID](https://developer.apple.com/programs/). |
| **Windows** | **WSL guide only** | No native build, no MSI, EXE, winget or Scoop. WSL needs no code, gets the Linux channels and their updates, and is the only way to get posters on Windows (section 4). |
| **Own auto-updater** | **No** | ani-cli is the only dependency that has to stay fresh. Every channel above either updates it with the rest (AUR, Homebrew) or puts the upstream script where `ani-cli -U` works (install script, WSL). ani-hop should only show the right update command (section 5). |
| **Licence** | **GPL-3.0-or-later** (added on this branch) | ani-cli is GPL-3.0 too ([formula](https://github.com/pystardust/ani-cli/blob/master/Formula/ani-cli.rb)). Homebrew core accepts any licence compatible with the Debian Free Software Guidelines ([Licence Guidelines](https://docs.brew.sh/Licence-Guidelines)), and your own tap has no rule. It has to be v3, not v2, because `ring` (used by rustls) is Apache-2.0, which [works with GPLv3 but not GPLv2](https://www.gnu.org/licenses/license-list.html#apache2). The other dependencies are MIT, Apache, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0 (nucleo-matcher, not marked "Incompatible With Secondary Licenses") and CDLA-Permissive-2.0 (webpki-roots). |

In this order:

1. ~~LICENSE~~: done on this branch, with `license` in Cargo.toml and a line in the README.
2. A tag-triggered release with [dist](https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.0), **Linux only at first** (section 6). It builds static x86_64 and aarch64 binaries, creates the GitHub release, pushes the Homebrew formula (usable on Linux right away) and pushes the AUR package. You keep tagging by hand.
3. `install.sh` for Debian/Ubuntu, Fedora and Arch, which installs those binaries (section 4). The README's install section then lists the script, the AUR and brew.
4. The macOS fixes, about 50 lines (section 8), together with `ANI_CLI_PLAYER=mpv`, which also fixes WSL. After that, add the Apple silicon target to dist, so the same formula covers Macs.
5. A short WSL section in the README pointing to the guide here. No further Windows work.

## 2. Starting point

ani-hop checks PATH for ani-cli, mpv, ani-skip, yt-dlp, ffmpeg, curl and xdg-open ([`src/ani.rs:144`](../src/ani.rs#L144)); yt-dlp, ffmpeg and curl are only used through ani-cli. It reads ani-cli's menus through a fake `rofi` script ([`src/ani.rs:24`](../src/ani.rs#L24)), so ani-cli needs no fzf. It runs mpv itself for downloaded episodes ([`src/ani.rs:576`](../src/ani.rs#L576)) and reads yt-dlp's `[download]` lines ([`src/ani.rs:810`](../src/ani.rs#L810)). Besides these, ani-cli itself needs sed, grep and, for `-U`, patch ([ani-cli L554-L578](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L554-L578)).

**Linux** works today on every distro that has the dependencies.

**macOS** compiles, but four things break at runtime:
- The AniList login runs `xdg-open` ([`src/main.rs:334`](../src/main.rs#L334)), which macOS does not have.
- `running()` ([`src/ani.rs:853`](../src/ani.rs#L853)) and the poster cleanup ([`src/kitty.rs:115`](../src/kitty.rs#L115)) read `/proc`, so every other ani-hop looks dead. A second ani-hop then takes over the first one's download queue ([`src/main.rs:420`](../src/main.rs#L420)) and deletes its poster files.
- On macOS ani-cli picks IINA before mpv ([ani-cli L461](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L461)), and ani-cli's README tells Mac users to install IINA ([README](https://github.com/pystardust/ani-cli/blob/master/README.md)). ani-hop assumes mpv: it passes mpv flags ([`src/ani.rs:549`](../src/ani.rs#L549)) and waits for mpv's `VO:` line ([`src/ani.rs:455`](../src/ani.rs#L455)).
- The install offer only knows yay, paru and pacman ([`src/ani.rs:165`](../src/ani.rs#L165)).

Posters should work in Ghostty and kitty, because the poster file falls back from `/dev/shm` to `$TMPDIR` ([`src/kitty.rs:90`](../src/kitty.rs#L90)).

**Windows** does not compile (`std::os::unix` imports), and no Windows terminal draws the kitty Unicode placeholders that posters use ([`src/kitty.rs:145`](../src/kitty.rs#L145)). Commit `b09665e` lists the roughly 250 lines a native port would need. That work is not planned.

## 3. Dependencies per channel

Latest upstream versions are in brackets. **Bold** means stale or missing.

| | Arch + AUR | Homebrew (macOS, Linux) | Debian 13 / Ubuntu 26.04 | Fedora 44 | nixpkgs unstable |
|---|---|---|---|---|---|
| **ani-cli** [5.1, master 5.1.5] | AUR 5.1, `ani-cli-git` | upstream tap 5.1, **not in core** | **4.10 / 4.10** | **none** (COPR **4.12.0**) | 5.1 |
| **ani-skip** [1.2.1] | `ani-skip-git` | **none** | **none** | **none** | 1.2.1 |
| mpv [0.41.0] | 0.41.0 | 0.41.0 | **0.40.0** / 0.41.0 | 0.41.0, but **H.264/HEVC need RPM Fusion's ffmpeg** | 0.41.0 |
| yt-dlp [2026.08.19] | current | current | **2025.04.30 / 2026.03.17** (backports current) | current | current |
| ffmpeg [9.0.2] | current | current | **7.1.5 / 8.0.1** | **ffmpeg-free 8.1.3** | current |
| curl-impersonate (optional) | extra | **none** | **none** | **none** | in the ani-cli wrapper |

Sources: Repology for [ani-cli](https://repology.org/project/ani-cli/versions), [ani-skip](https://repology.org/project/ani-skip/versions), [mpv](https://repology.org/project/mpv/versions), [yt-dlp](https://repology.org/project/yt-dlp/versions), [ffmpeg](https://repology.org/project/ffmpeg/versions) and [curl-impersonate](https://repology.org/project/curl-impersonate/versions). Also [AUR ani-cli](https://aur.archlinux.org/packages/ani-cli), the [upstream tap formula](https://github.com/pystardust/ani-cli/blob/master/Formula/ani-cli.rb), [brew core 404](https://formulae.brew.sh/api/formula/ani-cli.json), [Debian](https://sources.debian.org/src/yt-dlp/), [Ubuntu](https://launchpad.net/ubuntu/+source/ani-cli), [Fedora](https://mdapi.fedoraproject.org/f44/pkg/mpv), [COPR](https://copr.fedorainfracloud.org/coprs/derisis13/ani-cli/), [Fedora decoders](https://src.fedoraproject.org/rpms/ffmpeg/raw/f44/f/enable_decoders) and [nixpkgs ani-cli](https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/an/ani-cli/package.nix).

Takeaway:
- **Only ani-cli has to stay fresh.** It changed its streaming source twice this year ([v5.0](https://github.com/pystardust/ani-cli/releases/tag/v5.0), 2026-08-01; [v5.1](https://github.com/pystardust/ani-cli/releases/tag/v5.1), 2026-09-11), so the 4.x packages are probably broken (not tested). It is current only in the AUR, the upstream Homebrew tap, MacPorts, Scoop and nixpkgs-unstable. nixpkgs 26.05 still has 5.0. That is why the install script never takes ani-cli from apt or dnf.
- **yt-dlp can be a few months old.** ani-cli only hands it a direct link it has already found ([ani-cli L349](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L349)). Debian 13's is 18 months old, though, so the script uses the upstream binary there. For ani-hop yt-dlp is still a hard dependency: ani-hop reads its progress lines, and it stops downloads the way it does so that ani-cli doesn't fall back to ffmpeg, which leaves a partial file that looks finished ([`src/ani.rs:838`](../src/ani.rs#L838)). AUR ani-cli doesn't depend on yt-dlp ([AUR](https://aur.archlinux.org/packages/ani-cli)), so ani-hop's packages must declare it.
- **mpv and ffmpeg are fine from any package manager,** apart from Fedora's codecs.
- **ani-skip is optional** and only packaged in the AUR and nixpkgs.

## 4. Per OS

### Linux

| Option | Installs deps | Updates deps | Updates ani-hop | Maintainer effort | Fits a terminal app |
|---|---|---|---|---|---|
| **Install script** | yes: distro packages, upstream ani-cli | distro update + `ani-cli -U`, or rerun | rerun | M once (about 150 lines of sh), then none | yes |
| **AUR `ani-hop-bin`** | yes, ani-cli included | `yay -Syu` | same | low: CI pushes it | yes |
| **Homebrew on Linux** (same tap) | yes, ani-cli from the upstream tap | `brew upgrade` | same | none beyond the formula | yes; [glibc 2.39 for Tier 1](https://docs.brew.sh/Support-Tiers) |
| Nix flake | yes, if it follows nixos-unstable | channel update | same | low | yes, later |
| deb / rpm / PPA / COPR | ani-cli **stale or missing** | distro pace | yes | medium | yes, but the script covers these distros |
| Flatpak | — | — | — | — | no: [console apps and host-dependent apps are rejected](https://docs.flathub.org/docs/for-app-authors/requirements) |
| Snap | would bundle mpv and ani-cli | you | yes | high | no: [classic confinement is not granted for host-only deps](https://snapcraft.io/docs/reference/administration/reviewing-classic-confinement-snaps/) |
| AppImage | no | no | needs [AppImageUpdate](https://github.com/AppImage/AppImageUpdate/releases), still alpha | low | no gain over a static binary, and it [needs FUSE](https://docs.appimage.org/user-guide/troubleshooting/fuse.html) |

#### The install script

One POSIX sh file, `install.sh`, at the repo root, run straight from the main branch:

```sh
curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh
```

dist's own `ani-hop-installer.sh` only installs the binary ([template](https://github.com/axodotdev/cargo-dist/blob/main/cargo-dist/templates/installer/installer.sh.j2)), and the dependencies are the point here. So dist builds no shell installer, and this script downloads dist's archives itself.

It reads `ID` and `ID_LIKE` from `/etc/os-release`, so Ubuntu, Mint and Pop!_OS count as Debian, and Manjaro and EndeavourOS as Arch:

| Step | Arch | Debian / Ubuntu | Fedora | anything else |
|---|---|---|---|---|
| yay or paru installed | `yay -S --needed ani-hop-bin`, then stop | | | |
| system packages (sudo) | `pacman -S --needed mpv yt-dlp ffmpeg curl patch xdg-utils` | `apt-get install -y mpv ffmpeg curl patch xdg-utils` | `dnf install -y mpv yt-dlp ffmpeg-free curl patch xdg-utils` | print the list and carry on |
| yt-dlp | from pacman | upstream binary into `~/.local/bin` | from dnf | upstream binary |
| codecs | | | asks before enabling RPM Fusion free and running `dnf swap ffmpeg-free ffmpeg --allowerasing` | |
| ani-cli, ani-skip | upstream into `~/.local/bin` | same | same | same |
| ani-hop | release archive for `uname -m` (x86_64, aarch64), checksum checked, into `~/.local/bin` | same | same | same |

Sources: the Arch, Debian and Fedora package names and versions are in section 3. The RPM Fusion commands come from [Configuration](https://rpmfusion.org/Configuration) and [Howto/Multimedia](https://rpmfusion.org/Howto/Multimedia). Fedora's ffmpeg has no H.264 or HEVC decoder of its own, only openh264 ([enable_decoders](https://src.fedoraproject.org/rpms/ffmpeg/raw/f44/f/enable_decoders)). The yt-dlp files are `yt-dlp_linux` and `yt-dlp_linux_aarch64` ([release files](https://github.com/yt-dlp/yt-dlp#release-files)).

Details:
- **ani-skip:** the script downloads `ani-skip` and its mpv script to `~/.config/mpv/scripts/skip.lua`, the same two files as in the WSL guide below. It doesn't use ani-skip's installer, which insists on Windows mpv.exe under WSL ([install](https://raw.githubusercontent.com/synacktraa/ani-skip/master/install)).
- **Prompts:** the RPM Fusion question reads from `/dev/tty`, because with `curl | sh` the script itself is on stdin. Without a terminal it skips the step and prints the two commands.
- **sudo** is used only for system packages. Everything else goes in `$HOME`. The script refuses to run as root, because the user-level files would then land in `/root`.
- **PATH:** at the end it checks whether `~/.local/bin` is on PATH, and if not, prints the line to add to the shell's rc file.
- **Updating:** run the same line again. It fetches the newest ani-hop and fresh copies of ani-cli, ani-skip and the upstream yt-dlp. System packages update with the distro.
- **Needs:** curl, tar, sha256sum and uname, which is everything that `curl | sh` already implies on these distros.
- **Testing:** a CI job runs it in `debian:13`, `ubuntu:24.04`, `fedora:44` and `archlinux` containers, as a normal user with sudo, and checks `ani-hop --version` and `ani-cli -V`. Ubuntu 24.04 is in the list because it is still supported and its mpv is 0.37 ([Launchpad](https://launchpad.net/ubuntu/noble/+source/mpv)), which ani-hop hasn't been tried with.

#### Homebrew on Linux

Same tap, same commands as on macOS (below). dist puts Linux and macOS archives in one formula *(check with `dist plan`)*. This is the way for distros the script doesn't know, and for users who already use brew.

### macOS

| Option | Installs deps | Updates deps | Updates ani-hop | Maintainer effort | Fits a terminal app |
|---|---|---|---|---|---|
| **Formula in own tap** | mpv, yt-dlp, ffmpeg; ani-cli from the upstream tap | `brew upgrade` | same | low: dist pushes it | yes |
| homebrew-core | — | — | — | — | not eligible: a self-submission needs [225 stars (or 90 forks or watchers) and a repo at least 30 days old](https://docs.brew.sh/Package-Acceptance-Policy); ani-hop has [1](https://api.github.com/repos/ReynkeDeVos/ani-hop) |
| Cask / DMG / pkg | no | no | yes | high: notarization, $99/year | no: [casks are quarantined](https://docs.brew.sh/Homebrew-Security-and-Supply-Chain), and [CLI tools belong in formulae](https://docs.brew.sh/Acceptable-Casks) |
| MacPorts | yes ([ani-cli 5.1](https://ports.macports.org/port/ani-cli/), no ani-skip) | `port upgrade` | needs a port | medium | later, if asked |

**Pick:** a formula in `ReynkeDeVos/homebrew-tap`, generated by dist from the prebuilt binary. It depends on the core formulae `mpv` and `yt-dlp`. Homebrew's mpv already pulls in ffmpeg and yt-dlp ([formula](https://formulae.brew.sh/formula/mpv)). User steps, the same on Linux *(unconfirmed, not run)*:

```sh
brew tap pystardust/ani-cli https://github.com/pystardust/ani-cli.git
brew trust pystardust/ani-cli
brew install ani-cli reynkedevos/tap/ani-hop
brew install --cask ghostty    # macOS only, or kitty; posters only show there
```

Updates: `brew upgrade`.

Don't make the formula depend on `pystardust/ani-cli/ani-cli`. Since Homebrew 6.0, non-official taps [need explicit trust](https://docs.brew.sh/Tap-Trust), and only a name typed on the command line is trusted automatically. The docs don't say that this covers dependencies, and Homebrew's [loader checks every formula](https://github.com/Homebrew/brew/blob/main/Library/Homebrew/trust.rb). pystardust's tap also needs the explicit URL above. If ani-cli is missing, ani-hop's own dependency check reports it.

Limits:
- **Apple silicon on macOS 15 or newer only.** Homebrew [stopped building Intel bottles](https://docs.brew.sh/Support-Tiers), so mpv and ffmpeg would compile from source on an Intel Mac.
- **Posters need Ghostty or kitty.** Those are the only terminals ani-hop detects ([`src/kitty.rs:51`](../src/kitty.rs#L51)).
- **Use the mpv formula, not IINA** (see section 2). The mpv cask is [disabled](https://github.com/Homebrew/homebrew-cask/blob/main/Casks/s/stolendata-mpv.rb).

### Windows: only through WSL

Windows is not a focus. ani-hop runs there in WSL, which uses the Linux channels above. A native build would install no dependencies (MSI/EXE), lack ani-cli (winget, Chocolatey), or need the same port plus PowerShell underneath (Scoop). It would also show no posters, and couldn't be tested on Arch. Commit `b09665e` has the full comparison.

#### Windows via WSL, step by step

The guide uses **Ubuntu 26.04**:
- It is Microsoft's default WSL distro, and `Ubuntu-26.04` is its pinned name in the [catalog](https://raw.githubusercontent.com/microsoft/WSL/master/distributions/DistributionInfo.json). The pinned name matters: an older `Ubuntu` install may be 24.04, whose xdg-utils (1.1.3) can't open the Windows browser ([Repology](https://repology.org/project/xdg-utils/versions)).
- Its first start creates a user with sudo ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/setup/environment)).
- Its mpv is current, and its ffmpeg, kitty (0.45) and xdg-utils (1.2.1) are new enough ([Repology](https://repology.org/project/kitty-terminal/versions)).

Nothing below was run on a real Windows machine.

You need Windows 11, or Windows 10 build 19044 or newer for [WSLg](https://learn.microsoft.com/en-us/windows/wsl/tutorials/gui-apps), which shows the mpv and kitty windows. Windows 10 Home and Pro are [out of support](https://learn.microsoft.com/en-us/lifecycle/products/windows-10-home-and-pro). Virtualization must be enabled in the BIOS/UEFI ([troubleshooting](https://learn.microsoft.com/en-us/windows/wsl/troubleshooting)).

1. **Install WSL, once.** Start menu → "PowerShell" (or "Command Prompt") → right-click → *Run as administrator* ([Microsoft](https://learn.microsoft.com/en-us/windows/wsl/install)):
   ```
   wsl --install -d Ubuntu-26.04
   ```
   Plain `wsl --install` only works if WSL isn't installed yet; `-d` works either way. Restart. Open "Ubuntu 26.04 LTS" from the Start menu and choose a user name and password.
2. **Install everything.** In the Ubuntu window:
   ```sh
   sudo apt update && sudo apt upgrade -y && sudo apt install -y kitty
   curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh
   echo 'export PATH="$HOME/.local/bin:$PATH" ANI_CLI_PLAYER=mpv' >> ~/.bashrc && . ~/.bashrc
   ```
   You need `ANI_CLI_PLAYER=mpv`. On WSL2, ani-cli otherwise looks for Windows `mpv.exe` ([L464](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L464); the WSL kernel name [ends in WSL2](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.18.y/arch/x86/configs/config-wsl)), and Play fails with "Program mpv.exe not found". The setting can go once ani-hop sets it itself (section 8).

   **Until `install.sh` exists**, do by hand what it will do:
   ```sh
   sudo apt install -y mpv ffmpeg curl patch xdg-utils cargo build-essential
   mkdir -p ~/.local/bin ~/.config/mpv/scripts && cd ~/.local/bin
   curl -fsSLO https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli
   curl -fsSL -o yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux
   curl -fsSLO https://raw.githubusercontent.com/synacktraa/ani-skip/master/ani-skip
   curl -fsSL -o ~/.config/mpv/scripts/skip.lua https://raw.githubusercontent.com/synacktraa/ani-skip/master/integrations/mpv.lua
   chmod +x ani-cli yt-dlp ani-skip && cd
   cargo install --locked --root ~/.local --git https://github.com/ReynkeDeVos/ani-hop
   ```
   - On an ARM PC, use `yt-dlp_linux_aarch64`.
   - Ubuntu 26.04's Rust is newer than the 1.89 that ani-hop's locked dependencies need ([Repology](https://repology.org/project/rust/versions), [Cargo.lock](../Cargo.lock)), but this build was not tried *(unconfirmed)*.
3. **Start it.**
   - **With posters:** type `kitty &` in the Ubuntu window, then `ani-hop` in the kitty window *(unconfirmed)*. Only a terminal inside the same WSL distro can show posters, because ani-hop hands each one over as a Linux file ([`src/kitty.rs:145`](../src/kitty.rs#L145)). If kitty says "Wayland: Failed to connect to display", run `mkdir -p ~/.config/kitty && echo 'linux_display_server x11' >> ~/.config/kitty/kitty.conf` ([kitty #8463](https://github.com/kovidgoyal/kitty/issues/8463), [wslg #1032](https://github.com/microsoft/wslg/issues/1032)). Whether current WSL still needs this is *(unconfirmed)*.
   - **Without posters:** run `ani-hop` in the "Ubuntu 26.04 LTS" tab of Windows Terminal.
4. **AniList login:** nothing to do. xdg-utils 1.2 detects WSL and opens links in the Windows default browser ([xdg-utils-common](https://gitlab.freedesktop.org/xdg/xdg-utils/-/blob/master/scripts/xdg-utils-common.in), [xdg-open](https://gitlab.freedesktop.org/xdg/xdg-utils/-/blob/master/scripts/xdg-open.in)).
5. **Downloads** go to `~/Videos/Anime` inside Linux ([`src/ani.rs:95`](../src/ani.rs#L95)). In Explorer that is `\\wsl.localhost\Ubuntu-26.04\home\<user>\Videos\Anime`, or run `cd ~/Videos/Anime && explorer.exe .`. You can set "Download folder…" to `/mnt/c/Users/<you>/Videos`, but Microsoft says crossing file systems [is slower](https://learn.microsoft.com/en-us/windows/wsl/filesystems).
6. **Updates:** `sudo apt update && sudo apt upgrade`, then the `install.sh` line again (or `ani-cli -U; yt-dlp -U; ani-skip -U`). Run `wsl --update` in PowerShell now and then ([basic commands](https://learn.microsoft.com/en-us/windows/wsl/basic-commands)).

**Arch variant**, with commands from the [Arch wiki](https://wiki.archlinux.org/title/Install_Arch_Linux_on_WSL). Arch starts as root with no user and no sudo, and is x86_64 only:

```sh
wsl --install -d archlinux                         # admin PowerShell, as above
# in archlinux, as root:
pacman -Syu --needed sudo base-devel git kitty ttf-dejavu
useradd -m -G wheel -s /bin/bash NAME && passwd NAME
echo '%wheel ALL=(ALL:ALL) ALL' > /etc/sudoers.d/10-wheel
# back in PowerShell:
wsl --manage archlinux --set-default-user NAME; wsl --terminate archlinux
# in archlinux, as NAME:
git clone https://aur.archlinux.org/yay-bin.git && (cd yay-bin && makepkg -si)
yay -S ani-hop-bin
echo 'export ANI_CLI_PLAYER=mpv' >> ~/.bashrc
```

Until `ani-hop-bin` exists, replace the `yay -S` line with `yay -S ani-cli ani-skip-git mpv yt-dlp ffmpeg patch xdg-utils rust && cargo install --locked --git https://github.com/ReynkeDeVos/ani-hop`, and add `$HOME/.cargo/bin` to PATH.

Then start kitty as in step 3. Updates: `yay -Syu` and `wsl --update`. The image ships no fonts, so `ttf-dejavu` is a precaution *(unconfirmed)*.

## 5. Updates

The package manager should own updates, for ani-hop and its dependencies alike:
- **It owns the files.** `sudo ani-cli -U` patches its own file in place ([update_script](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L110-L127)), so on pacman or apt installs it edits a file the package manager owns. On Nix it can't work at all ([package.nix](https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/an/ani-cli/package.nix)).
- **yt-dlp won't update itself there.** `yt-dlp -U` refuses for package-manager builds and points to the manager ([update.py](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/update.py)).
- **Homebrew core forbids it.** Core requires self-updating to be disabled ([Acceptable Formulae](https://docs.brew.sh/Acceptable-Formulae)). A tap isn't bound by that, but the reason is the same.
- **Ready-made updaters are a poor fit.** dist's updater is experimental and only covers its own installers ([updater](https://axodotdev.github.io/cargo-dist/book/installers/updater.html)), and its library pulls in reqwest and tokio ([axoupdater](https://github.com/axodotdev/axoupdater/blob/main/axoupdater/Cargo.toml)).

What ani-hop can still do, small and hint-only:
1. **ani-cli version.** Run `ani-cli -V` along with the existing dependency check. If it is older than a `TESTED_ANI_CLI` constant (5.1 today), say so on the dependency screen with the right command:
   - the script is user-writable: `ani-cli -U`
   - `/usr/bin` with yay or paru: `yay -Syu`
   - under `brew --prefix`: `brew upgrade ani-cli`
   - under `/nix/store`: update the channel

   Never suggest `sudo ani-cli -U`.
2. **New ani-hop release.** At most once a day, request `https://github.com/ReynkeDeVos/ani-hop/releases/latest` without following the redirect. The `Location` header names the newest tag, and this needs no API call or rate limit ([GitHub](https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases)). If the tag is newer than `CARGO_PKG_VERSION`, show one line with the command for the install channel, judged by where the running binary lives:
   - `~/.local/bin`: the `install.sh` line
   - under `brew --prefix`: `brew upgrade`
   - `/usr/bin`: `yay -Syu`
   - otherwise: `cargo install --locked --force --git …`
3. Not worth doing: running updates for the user, and checking yt-dlp, which [warns by itself after 90 days](https://github.com/yt-dlp/yt-dlp#update).

## 6. Release pipeline

**Tooling: dist, plus one custom job for the AUR.**

| | dist 0.33.0 | GoReleaser | Hand-written (taiki-e actions) |
|---|---|---|---|
| Rust | native; [in Arch extra](https://archlinux.org/packages/extra/x86_64/cargo-dist/) | [partial](https://goreleaser.com/customization/builds/builders/rust/) (zigbuild, workspace caveats) | [yes](https://github.com/taiki-e/upload-rust-binary-action) |
| Homebrew | formula with `depends_on` ([docs](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html)) | formula publisher [deprecated in favour of casks](https://goreleaser.com/customization/publish/homebrew_formulas/) | write it yourself |
| AUR | custom job ([docs](https://github.com/axodotdev/cargo-dist/blob/main/book/src/ci/customizing.md)) | built in | write it yourself |
| Release notes from CHANGELOG.md | yes, via [parse-changelog](https://github.com/taiki-e/parse-changelog); the `## 0.5.0 – 2026-10-20` headings work | not checked | yes, [same parser](https://github.com/taiki-e/create-gh-release-action) |
| Upkeep | generated workflow, regenerate on upgrade | config file | all the YAML is yours |

dist is community-maintained since 2025 ([announcement](https://blog.mistys-internet.website/post/2025/07/20/dist-0-dot-28-dot-1-is-out)) and [still releases](https://github.com/axodotdev/cargo-dist/releases).

**One-time setup:**
1. Add `description`, `repository` and `homepage` to Cargo.toml (the licence is already there). `dist init` stops without `repository`.
2. Create an empty repo, `ReynkeDeVos/homebrew-tap`.
3. Register on the AUR and add an SSH key.
4. Run `sudo pacman -S cargo-dist`, then `dist init`. Edit `dist-workspace.toml` roughly like this, and check the result with `dist plan`:
   ```toml
   [dist]
   targets = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]   # add "aarch64-apple-darwin" after the macOS fixes
   installers = ["homebrew"]             # install.sh replaces dist's shell installer
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
   license=('GPL-3.0-or-later')
   depends=(ani-cli mpv yt-dlp ffmpeg curl xdg-utils)
   optdepends=('ani-skip: skips openings and endings' 'curl-impersonate: gets ani-cli past Cloudflare'
               'ghostty: posters' 'kitty: posters')
   ```
   Keep the dependencies unversioned. `ani-cli-git` [provides an unversioned `ani-cli`](https://aur.archlinux.org/packages/ani-cli-git), which a versioned dependency would [not accept](https://wiki.archlinux.org/title/PKGBUILD#provides).
6. Ship the dependencies' licence notices in the archives. MIT, BSD and Apache ask for them in binary copies too. [cargo-about](https://github.com/EmbarkStudios/cargo-about) generates the file, and dist's `include` setting adds it ([config](https://github.com/axodotdev/cargo-dist/blob/main/book/src/reference/config.md)).

**Every release afterwards:**
1. Open a PR with the version bump and the CHANGELOG entry, and squash-merge it, as now.
2. Run `git tag vX.Y.Z && git push origin vX.Y.Z`. v0.5.0 is not tagged yet.
3. `release.yml` runs plan, then builds the targets. It then creates the GitHub release with the archives, checksums and notes from CHANGELOG.md, pushes `ani-hop.rb` to the tap, and pushes the PKGBUILD to the AUR. `install.sh` picks up the new archives by itself, through `releases/latest/download/…`.

Stop writing GitHub releases by hand. Release titles change to the CHANGELOG heading (`0.5.0 – 2026-10-20`). Every PR also gets a `dist plan` check.

**Secrets:**

| Secret | For | Notes |
|---|---|---|
| `HOMEBREW_TAP_TOKEN` | pushing the formula | PAT with write access to the tap repo. dist's docs say a classic PAT with `repo` scope; a fine-grained PAT with Contents write should work, since the job only checks out and pushes (inferred) |
| `AUR_SSH_PRIVATE_KEY` | pushing the PKGBUILD | passwordless key registered on the AUR account |
| `GITHUB_TOKEN` | creating the release | automatic |

Traps:
- **Runners being retired.** dist's defaults include `macos-14` ([retired 2026-11-02](https://github.com/actions/runner-images/issues/13518)) and `ubuntu-22.04` ([retired 2027-04-17](https://github.com/actions/runner-images/issues/14254)), hence the custom runners above. `global` [only covers the non-build jobs](https://github.com/axodotdev/cargo-dist/blob/main/book/src/reference/config.md#github-custom-runners), so each target needs its own line.
- **No partial releases.** dist [refuses to release](https://github.com/axodotdev/cargo-dist/blob/main/book/src/reference/config.md#fail-fast) if any target fails, so add the Mac target only once it builds and the macOS fixes are in.
- **`on: release` workflows won't fire.** A release created with `GITHUB_TOKEN` does not [trigger other workflows](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow). That is why the AUR job runs as a dist publish job.
- **Archive names.** `install.sh` depends on dist's archive and checksum file names. Take them from `dist plan` and keep them in one place in the script.

## 7. Rejected options

- **Any native Windows build** (MSI, EXE, winget, Scoop, Chocolatey): decided against, and WSL is the answer instead. In short: an MSI or EXE installs no dependencies ([dist MSI](https://github.com/axodotdev/cargo-dist/blob/main/book/src/installers/msi.md)). winget and Chocolatey have no ani-cli ([Repology](https://repology.org/project/ani-cli/versions)). Scoop has it, but runs PowerShell underneath. A port is about 250 lines with no posters. Details are in commit `b09665e`.
- **Windows mpv.exe under WSL**, as ani-cli's README suggests: `have("mpv")` and `play_file` need Linux mpv ([`src/ani.rs:154`](../src/ani.rs#L154), [`src/ani.rs:576`](../src/ani.rs#L576)), and Windows programs get Linux paths [unconverted](https://learn.microsoft.com/en-us/windows/wsl/filesystems).
- **DMG, pkg, cask, notarization:** install no dependencies, [cost $99/year](https://developer.apple.com/programs/), and [CLI tools belong in formulae](https://docs.brew.sh/Acceptable-Casks).
- **homebrew-core:** [notability and repo age](https://docs.brew.sh/Package-Acceptance-Policy), and ani-cli isn't in core. GPL would be fine there.
- **Intel macOS builds:** Homebrew is [Tier 3 on Intel with no new bottles](https://docs.brew.sh/Support-Tiers).
- **Flatpak:** [Flathub rejects console apps and apps that rely on host components](https://docs.flathub.org/docs/for-app-authors/requirements).
- **Snap:** [no classic confinement for host-only deps](https://snapcraft.io/docs/reference/administration/reviewing-classic-confinement-snaps/), so you would bundle and refresh mpv and ani-cli yourself.
- **AppImage:** not sandboxed, but installs no dependencies, [needs FUSE](https://docs.appimage.org/user-guide/troubleshooting/fuse.html), and adds nothing to a static binary.
- **deb/rpm/PPA/COPR:** they would depend on ani-cli 4.x or on a stale COPR (section 3), and the install script already covers those distros.
- **dist's shell installer:** installs only the binary; `install.sh` does that and the dependencies.
- **Bundling mpv** (as [Curd's Windows installer](https://github.com/Wraient/curd/blob/main/Build/curd-windows-build.iss) does): freezes the versions you then have to update.
- **ani-hop downloading and managing its own ani-cli copy:** works the same everywhere, but it is an updater with a trust boundary, and the install script already puts a self-updating ani-cli in place.
- **Own self-updater** (axoupdater, self_update): the package managers own the files (section 5).
- **GoReleaser:** its Homebrew formulas are [deprecated in favour of casks](https://goreleaser.com/customization/publish/homebrew_formulas/), and its Rust support is partial.

## 8. Code changes needed

**All platforms**
- [x] LICENSE file and `license` in [Cargo.toml](../Cargo.toml).
- [ ] `description`, `repository` and `homepage` in Cargo.toml, for dist. XS.
- [ ] `install.sh` as in section 4, plus its container test job. M.
- [ ] [`src/ani.rs:549`](../src/ani.rs#L549) `stream()`: pass `ANI_CLI_PLAYER=mpv` when the user hasn't set it. ani-hop already requires mpv ([`src/ani.rs:146`](../src/ani.rs#L146)). This fixes WSL ([ani-cli L464](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L464)) and macOS with IINA ([L461](https://github.com/pystardust/ani-cli/blob/master/ani-cli#L461)). XS.
- [ ] [`src/ani.rs:165`](../src/ani.rs#L165) `install_command`: add apt, dnf and brew branches, with their package names in DEPS and an updated hint at [`src/ui.rs:521`](../src/ui.rs#L521). For apt and dnf, offer only the stable dependencies, never ani-cli; for those, point to `install.sh`. S.
- [ ] The update hints from section 5, on the dependency screen ([`src/ui.rs:491`](../src/ui.rs#L491)). S.

**macOS** (S, about 50 lines, plus one run on a Mac or a `macos-15` runner)
- [ ] [`src/main.rs:333`](../src/main.rs#L333) `open_url`: use `open` on macOS, and drop the `xdg-open` row from DEPS there ([`src/ani.rs:151`](../src/ani.rs#L151)). XS.
- [ ] [`src/ani.rs:852`](../src/ani.rs#L852) `running()` and [`src/kitty.rs:115`](../src/kitty.rs#L115): replace `/proc` with a per-process `queue/<pid>.lock` that is held for the whole run and probed with [`File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html). This also survives pid reuse. S.
- [ ] Optional: default downloads to `~/Movies` on macOS ([`src/ani.rs:61`](../src/ani.rs#L61)). XS.

**Windows:** nothing. WSL runs the Linux build.

## 9. Open questions

1. **Nothing was run on a Mac or on Windows.** On a Mac: the tap install sequence, and whether kitty accepts poster files from `$TMPDIR` (Ghostty's code does, [graphics_image.zig](https://github.com/ghostty-org/ghostty/blob/v1.3.1/src/terminal/kitty/graphics_image.zig)). Under WSL: the guide, kitty under WSLg, and the Wayland workaround.
2. **Should the script enable RPM Fusion by default on Fedora?** Without it most streams probably won't play (H.264), but it adds a third-party repo. The plan above asks first.
3. **Should ani-hop be tested against ani-cli master or tags?** `-U` and the install script jump to master (5.1.5 today), while packages ship tags (5.1). ani-hop depends on undocumented ani-cli behaviour (rofi menus, debug player output, file names), and master changed download file naming after 5.1 ([compare](https://github.com/pystardust/ani-cli/compare/v5.1...master)). The script could fetch the latest tag instead of master; `ani-cli -U` would still move to master later.
4. **Does ani-cli 4.10 or 4.14 still play anything?** If not, should ani-hop refuse versions below 5.1 rather than just warn?
5. **Does dist build the musl targets with these custom runners, and does it write one formula for Linux and macOS?** Check with `dist plan` and a test tag.
