#!/bin/sh
# Installs or updates ani-hop and what it needs on Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/ReynkeDeVos/ani-hop/main/install.sh | sh
#
# System packages come from the distro, with sudo, and so do ani-cli and ani-skip where it has
# them new enough (on Arch the AUR, through yay or paru). Otherwise they, ani-hop and, where the
# distro's is too old (Debian, Ubuntu), yt-dlp go into ~/.local/bin, so `ani-cli -U` needs no sudo.
# Run it again to update them. ANI_HOP_RELEASE_URL picks another release, for example
# https://github.com/ReynkeDeVos/ani-hop/releases/download/v0.6.0
set -eu

BIN=$HOME/.local/bin
RELEASE=${ANI_HOP_RELEASE_URL:-https://github.com/ReynkeDeVos/ani-hop/releases/latest/download}
ANI_CLI=https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli
ANI_SKIP=https://raw.githubusercontent.com/synacktraa/ani-skip/master
YT_DLP=https://github.com/yt-dlp/yt-dlp/releases/latest/download

say() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

# With `curl | sh` stdin is the script itself, so questions go to the terminal.
tty_ok() { (: </dev/tty) 2>/dev/null; }
ask() {
	printf '%s [Y/n] ' "$1"
	read -r answer </dev/tty || answer=
	case $answer in [Nn]*) return 1 ;; esac
}

# Only system packages need root. Without sudo, show the command and carry on.
as_root() {
	if have sudo; then
		sudo "$@"
	else
		warn "sudo not found, run this as root: $*"
	fi
}

# pacman and the AUR helpers ask before they upgrade; with `curl | sh` the answer has to come
# from the terminal, and without one there's no one to ask.
arch_install() {
	if tty_ok; then "$@" </dev/tty; else "$@" --noconfirm; fi
}

get() { curl -fsSL --retry 3 -o "$tmp/$2" "$1" || die "could not download $1"; }

# Whether version $1 is at least 5.1, which ani-hop needs.
new_enough() {
	[ -n "$1" ] && [ "$(printf '%s\n5.1\n' "$1" | sort -V 2>/dev/null | head -n 1)" = 5.1 ]
}

SKIP_LUA=${XDG_CONFIG_HOME:-$HOME/.config}/mpv/scripts/skip.lua

# ani-skip's mpv script is the one that reads the "skip" script-opts ani-skip hands to mpv.
SKIP_MARK='read_options(.*"skip")'

# What the system provides, however it got there: an ani-cli ani-hop can use, ani-skip, and
# ani-skip's mpv script where mpv loads it for everyone.
packaged() {
	pkg_cli=$(new_enough "$(/usr/bin/ani-cli -V 2>/dev/null || true)" && echo 1 || true)
	pkg_skip=$([ -x /usr/bin/ani-skip ] && echo 1 || true)
	sys_lua=$(grep -l "$SKIP_MARK" /etc/mpv/scripts/*.lua 2>/dev/null | head -n 1 || true)
}

drop() {
	[ -e "$1" ] || return 0
	rm -f "$1"
	say "Removed $1, the system's copy is used instead"
}

# Removes an earlier run's copies the system's now replace: ours would come first on PATH,
# and mpv would load the skip script twice.
drop_copies() {
	[ -z "$pkg_cli" ] || drop "$BIN/ani-cli"
	[ -z "$pkg_skip" ] || drop "$BIN/ani-skip"
	if [ -n "$sys_lua" ] && grep -q "$SKIP_MARK" "$SKIP_LUA" 2>/dev/null; then drop "$SKIP_LUA"; fi
}

# The last step is a rename inside the target dir, so a failed download never leaves half
# a file behind and a running ani-hop can be replaced.
place() {
	mkdir -p "$(dirname "$2")"
	chmod "$3" "$tmp/$1"
	mv -f "$tmp/$1" "$2.new"
	mv -f "$2.new" "$2"
}

# Fedora's own ffmpeg can't decode H.264 or HEVC, which most streams use; RPM Fusion's can.
# Choosing before the install means ffmpeg-free is never pulled in just to be swapped out.
fedora_packages() {
	set -- dnf install -y mpv yt-dlp curl patch diffutils xdg-utils
	fusion=https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm
	if rpm -q ffmpeg-libs >/dev/null 2>&1; then
		as_root "$@" ffmpeg
	elif tty_ok && ask "Enable RPM Fusion for an ffmpeg that plays H.264 and HEVC?"; then
		as_root dnf install -y "$fusion"
		as_root "$@" ffmpeg --allowerasing
	else
		as_root "$@" ffmpeg-free
		say "Most streams need H.264. To get it, enable RPM Fusion and swap ffmpeg:"
		printf '  %s\n' "sudo dnf install -y $fusion" "sudo dnf swap -y ffmpeg-free ffmpeg --allowerasing"
	fi
}

main() {
	[ "$(id -u)" -ne 0 ] || die "don't run this as root: ani-hop and ani-cli go into ~/.local/bin of the user who runs it, and sudo is only needed for system packages"
	[ "$(uname -s)" = Linux ] || die "this script is for Linux; on macOS install ani-hop with Homebrew, see the README"

	case $(uname -m) in
	x86_64) target=x86_64-unknown-linux-musl yt=yt-dlp_linux ;;
	aarch64) target=aarch64-unknown-linux-musl yt=yt-dlp_linux_aarch64 ;;
	*) die "there is no ani-hop build for $(uname -m), only for x86_64 and aarch64" ;;
	esac
	archive=ani-hop-$target.tar.xz

	# shellcheck source=/dev/null
	os=$([ -r /etc/os-release ] && . /etc/os-release; echo " ${ID-} ${ID_LIKE-} ")
	case $os in
	*" arch "*) os=arch ;;
	*" debian "* | *" ubuntu "*) os=debian ;;
	# Rocky and Alma say they are like fedora too, but have none of its packages.
	*" fedora "*) case $(rpm -E %fedora 2>/dev/null) in [0-9]*) os=fedora ;; *) os=other ;; esac ;;
	*) os=other ;;
	esac
	# Image-based systems (Silverblue, Bazzite, Bluefin, Endless) can't install into /usr.
	[ ! -e /run/ostree-booted ] || os=other

	# ani-hop-bin brings ani-cli and the rest, ani-skip-git the skipping; both update with
	# the system. The steps below still run, to fill in what the packages leave out (an ani-cli
	# too old, a skip script mpv doesn't load); only the release download is skipped.
	hop=$BIN/ani-hop
	if [ "$os" = arch ]; then
		for helper in yay paru; do
			have "$helper" || continue
			"$helper" -Si ani-hop-bin >/dev/null 2>&1 || continue
			say "Installing ani-hop-bin from the AUR with $helper"
			# an older /usr/bin/ani-hop left by a failed install mustn't replace a newer copy of ours
			if arch_install "$helper" -S --needed ani-hop-bin ani-skip-git && [ -x /usr/bin/ani-hop ]; then
				hop=/usr/bin/ani-hop
				drop "$BIN/ani-hop"
			else
				warn "the AUR install failed, so ani-hop comes from the release"
			fi
			break
		done
	fi

	for cmd in curl tar sha256sum mktemp; do
		have "$cmd" || die "$cmd is needed to run this script"
	done
	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	trap 'exit 1' HUP INT TERM

	say "Installing system packages"
	case $os in
	arch)
		# Only what's missing, and no system upgrade: that's the user's, or omarchy update's,
		# and Omarchy refuses -Syu outside it. Without -y the package database isn't refreshed,
		# so this installs what fits the system as it was last upgraded.
		# pacman -T: 0 all there, 127 some missing (listed), anything else broke
		missing=$(pacman -T mpv yt-dlp ffmpeg curl patch diffutils xdg-utils) ||
			[ $? -eq 127 ] || die "pacman couldn't check which packages are installed"
		if [ -n "$missing" ]; then
			# shellcheck disable=SC2086 # one package per word
			arch_install as_root pacman -S --needed $missing ||
				die "pacman failed; if a package wasn't found or couldn't be downloaded, update the system (omarchy update or sudo pacman -Syu) and run this again"
		fi
		yt=
		;;
	debian)
		as_root apt-get update
		# mpv recommends the distro's yt-dlp, which is too old and would sit next to ours.
		as_root apt-get install -y --no-install-recommends mpv ffmpeg curl patch xdg-utils xz-utils
		;;
	fedora)
		fedora_packages
		yt=
		;;
	*)
		warn "unknown distro, install these with your package manager: mpv yt-dlp ffmpeg curl patch diffutils xdg-utils xz"
		yt= # upstream's build needs glibc, which Alpine and NixOS don't have the usual way
		;;
	esac

	# Debian's yt-dlp is too old, so it comes from upstream there.
	if [ -n "$yt" ]; then
		say "Installing yt-dlp into $BIN"
		get "$YT_DLP/$yt" yt-dlp
		place yt-dlp "$BIN/yt-dlp" 755
	fi

	# ani-cli and ani-skip: the system's packages where they're new enough, as those update with
	# the system; the AUR only has what's missing installed, without a system upgrade.
	case $os in
	arch)
		for helper in yay paru; do
			have "$helper" || continue
			want=
			pacman -T ani-cli >/dev/null || want=" ani-cli"
			pacman -T ani-skip >/dev/null || want="$want ani-skip-git"
			[ -n "$want" ] || break
			say "Installing$want from the AUR with $helper"
			# shellcheck disable=SC2086 # one package per word
			arch_install "$helper" -S --needed $want || warn "the AUR install failed, so upstream's copies are used"
			break
		done
		;;
	debian)
		v=$(LC_ALL=C apt-cache policy ani-cli 2>/dev/null | sed -n 's/^ *Candidate: //p')
		v=${v#*:}
		if new_enough "${v%%-*}"; then as_root apt-get install -y ani-cli || true; fi
		;;
	fedora)
		v=$(dnf -q repoquery --latest-limit=1 --qf '%{version}\n' ani-cli 2>/dev/null || true)
		if new_enough "$v"; then as_root dnf install -y ani-cli || true; fi
		;;
	esac
	# what's really there decides, also a package installed by hand
	packaged
	drop_copies
	if [ -z "$pkg_cli" ]; then
		say "Installing ani-cli into $BIN"
		get "$ANI_CLI" ani-cli
		place ani-cli "$BIN/ani-cli" 755
	fi
	if [ -z "$pkg_skip" ]; then
		say "Installing ani-skip into $BIN"
		get "$ANI_SKIP/ani-skip" ani-skip
		place ani-skip "$BIN/ani-skip" 755
	fi
	# its mpv script also when ani-skip is packaged but brings none mpv loads
	if [ -z "$sys_lua" ]; then
		get "$ANI_SKIP/integrations/mpv.lua" skip.lua
		place skip.lua "$SKIP_LUA" 644
	fi

	if [ "$hop" = "$BIN/ani-hop" ]; then
		say "Installing ani-hop into $BIN"
		get "$RELEASE/$archive" "$archive"
		get "$RELEASE/$archive.sha256" "$archive.sha256"
		# compared by hand: busybox's sha256sum -c trips over the empty line dist ends the file with
		[ "$(cut -d' ' -f1 "$tmp/$archive.sha256")" = "$(sha256sum <"$tmp/$archive" | cut -d' ' -f1)" ] ||
			die "checksum mismatch for $archive"
		tar -xJf "$tmp/$archive" -C "$tmp" || die "could not unpack $archive (is xz installed?)"
		place "ani-hop-$target/ani-hop" "$BIN/ani-hop" 755
	fi

	case ":$PATH:" in
	*":$BIN:"*)
		for cmd in ani-hop ani-cli ani-skip yt-dlp; do
			found=$(command -v "$cmd" || true)
			if [ -e "$BIN/$cmd" ] && [ "$found" != "$BIN/$cmd" ]; then
				warn "$found comes first on your PATH, so $BIN/$cmd won't run; remove the old one"
			fi
		done
		;;
	*)
		# shellcheck disable=SC2016 # $HOME and $PATH are meant for the rc file
		line='export PATH="$HOME/.local/bin:$PATH"'
		case ${SHELL-} in
		*/fish) fix='fish_add_path ~/.local/bin' ;;
		*/zsh) fix="echo '$line' >> ~/.zshrc" ;;
		*/bash) fix="echo '$line' >> ~/.bashrc" ;;
		*) fix="echo '$line' >> ~/.profile" ;;
		esac
		say "$BIN is not on your PATH yet. Run this, then open a new terminal:"
		echo "  $fix"
		;;
	esac

	version=$("$hop" --version) || die "$hop was installed but does not run"
	cli=$BIN/ani-cli
	[ -z "$pkg_cli" ] || cli=/usr/bin/ani-cli
	say "Done: $version, ani-cli $("$cli" -V 2>/dev/null || echo '(not working yet)')"
}

main "$@"
