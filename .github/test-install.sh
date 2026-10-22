#!/bin/sh
# Runs install.sh in a fresh container as a normal user with passwordless sudo, then checks
# what it installed. Used locally and by .github/workflows/install.yml.
#
#   .github/test-install.sh IMAGE RELEASE_DIR    e.g. debian:13 /tmp/release
#
# RELEASE_DIR holds the release archives and their .sha256 files. ENGINE=docker or
# ENGINE=podman picks the container tool; by default whichever is installed.
set -eu

if [ "${1-}" != --inside ]; then
	[ $# -eq 2 ] || { echo "usage: $0 IMAGE RELEASE_DIR" >&2; exit 2; }
	engine=${ENGINE:-$(command -v podman || command -v docker || echo none)}
	[ "$engine" != none ] || { echo "needs podman or docker" >&2; exit 1; }
	here=$(cd "$(dirname "$0")" && pwd)
	# The script mounts itself and runs again inside, as root, with --inside.
	exec "$engine" run --rm \
		-v "$here/../install.sh:/install.sh:ro" \
		-v "$here/test-install.sh:/test.sh:ro" \
		-v "$(cd "$2" && pwd):/release:ro" \
		"$1" sh /test.sh --inside
fi

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo "### $*"; }

step "setup: sudo, curl and a user"
if command -v apt-get >/dev/null; then
	apt-get update -q >/dev/null
	apt-get install -qy --no-install-recommends sudo curl ca-certificates >/dev/null
elif command -v dnf >/dev/null; then
	dnf install -qy sudo util-linux util-linux-script >/dev/null # su, and script for a fake terminal
elif command -v pacman >/dev/null; then
	pacman -Syu --noconfirm --needed sudo >/dev/null
fi
useradd -m -s /bin/bash tester
echo 'tester ALL=(ALL) NOPASSWD: ALL' >/etc/sudoers.d/tester

as_tester() { su - tester -c "export ANI_HOP_RELEASE_URL=file:///release; $1"; }

check() {
	# shellcheck disable=SC2016 # expanded by the tester's shell
	as_tester 'set -eo pipefail; PATH=$HOME/.local/bin:$PATH
		ani-hop --version | grep "^ani-hop [0-9]"
		ani-cli -V | sed "s/^/ani-cli /" | grep "^ani-cli 5\."
		ani-skip -V | sed "s/^/ani-skip /"
		yt-dlp --version | sed "s/^/yt-dlp /"; command -v yt-dlp
		mpv --version | sed -n 1p
		ffmpeg -version | sed -n 1p
		test -s ~/.config/mpv/scripts/skip.lua
		for c in mpv ffmpeg yt-dlp ani-skip patch diff xdg-open xz; do command -v $c >/dev/null || { echo "missing $c"; exit 1; }; done' ||
		fail "check after $1"
}

step "root is refused"
if sh /install.sh >/tmp/root.log 2>&1; then fail "install.sh ran as root"; fi
grep -q "root" /tmp/root.log || fail "no reason given for refusing root"
cat /tmp/root.log

step "first run: cat install.sh | sh"
start=$(date +%s)
as_tester 'cat /install.sh | sh' >/tmp/first.log 2>&1 || { cat /tmp/first.log; fail "first run"; }
cat /tmp/first.log
echo "first run took $(($(date +%s) - start))s"
check "first run"

if grep -q '^ID=fedora' /etc/os-release; then
	step "fedora without a terminal prints the RPM Fusion commands"
	grep -q 'dnf swap -y ffmpeg-free ffmpeg --allowerasing' /tmp/first.log || fail "no RPM Fusion hint"
fi

step "second run (update): sh install.sh"
start=$(date +%s)
as_tester 'sh /install.sh' >/tmp/second.log 2>&1 || { cat /tmp/second.log; fail "second run"; }
cat /tmp/second.log
echo "second run took $(($(date +%s) - start))s"
# everything is there now, so pacman must not install or upgrade anything
! grep -q 'resolving dependencies' /tmp/second.log || fail "second run ran a pacman transaction"
check "second run"

if grep -q '^ID=fedora' /etc/os-release; then
	step "fedora with a terminal: yes to RPM Fusion"
	echo y | as_tester 'script -qec "sh /install.sh" /dev/null' || fail "RPM Fusion run"
	rpm -q ffmpeg-libs || fail "no ffmpeg from RPM Fusion"
	check "RPM Fusion run"
fi

step "a packaged ani-cli and ani-skip replace ours"
# stand-ins for the distro's packages, which only the AUR has new enough today
printf '#!/bin/sh\necho 5.1.0\n' >/usr/bin/ani-cli
printf '#!/bin/sh\necho 1.0.1\n' >/usr/bin/ani-skip
chmod 755 /usr/bin/ani-cli /usr/bin/ani-skip
mkdir -p /etc/mpv/scripts
cp ~tester/.config/mpv/scripts/skip.lua /etc/mpv/scripts/mpv.lua
as_tester 'sh /install.sh' >/tmp/packaged.log 2>&1 || { cat /tmp/packaged.log; fail "packaged run"; }
cat /tmp/packaged.log
for f in .local/bin/ani-cli .local/bin/ani-skip .config/mpv/scripts/skip.lua; do
	[ ! -e ~tester/$f ] || fail "$f should be gone, the package's copy replaces it"
done
grep -q 'ani-cli 5.1.0$' /tmp/packaged.log || fail "the packaged ani-cli isn't the one reported"

step "a packaged ani-skip without its mpv script: ours"
rm /etc/mpv/scripts/mpv.lua
as_tester 'sh /install.sh' >/tmp/nolua.log 2>&1 || { cat /tmp/nolua.log; fail "no-script run"; }
grep -q "read_options" ~tester/.config/mpv/scripts/skip.lua || fail "no skip.lua although mpv has no skip script"
[ ! -e ~tester/.local/bin/ani-skip ] || fail "ani-skip should still be the package's"

step "a packaged ani-cli that's too old: ours again"
printf '#!/bin/sh\necho 4.10\n' >/usr/bin/ani-cli
as_tester 'sh /install.sh' >/tmp/old.log 2>&1 || { cat /tmp/old.log; fail "too-old run"; }
[ -x ~tester/.local/bin/ani-cli ] || fail "no ani-cli of ours next to a too old one"
[ ! -e ~tester/.local/bin/ani-skip ] || fail "ani-skip should still be the package's"

if grep -q '^ID=arch' /etc/os-release; then
	step "arch: ani-hop-bin from the AUR, the rest filled in"
	# a stand-in yay: ani-hop-bin "exists", and installing it puts a real ani-hop into /usr/bin;
	# the system's ani-cli stays the too old one and ani-skip brings no mpv script
	cat >/usr/local/bin/yay <<-'EOF'
		#!/bin/sh
		case "$*" in *-Si*) exit 0 ;; esac
		for p; do
			[ "$p" != ani-hop-bin ] || sudo install -m 755 "$HOME/.local/bin/ani-hop" /usr/bin/ani-hop
		done
	EOF
	chmod 755 /usr/local/bin/yay
	as_tester 'sh /install.sh' >/tmp/aur.log 2>&1 || { cat /tmp/aur.log; fail "AUR run"; }
	cat /tmp/aur.log
	[ ! -e ~tester/.local/bin/ani-hop ] || fail "the release's ani-hop should be gone"
	grep -q 'Done: ani-hop ' /tmp/aur.log || fail "/usr/bin/ani-hop isn't the one reported"
	[ -x ~tester/.local/bin/ani-cli ] || fail "no ani-cli of ours next to a too old one"
	[ -s ~tester/.config/mpv/scripts/skip.lua ] || fail "no skip.lua although mpv has no skip script"
	! grep -q 'Installing ani-hop into' /tmp/aur.log || fail "downloaded the release despite ani-hop-bin"
fi

step "PASS"
