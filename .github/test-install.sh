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
		ani-cli -V | sed "s/^/ani-cli /" | grep "^ani-cli [0-9]"
		yt-dlp --version | sed "s/^/yt-dlp /"; command -v yt-dlp
		mpv --version | sed -n 1p
		ffmpeg -version | sed -n 1p
		for c in mpv ffmpeg yt-dlp patch diff xdg-open xz; do command -v $c >/dev/null || { echo "missing $c"; exit 1; }; done' ||
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

step "ani-cli is its latest release, not master"
curl -fsSL -o /tmp/released https://github.com/pystardust/ani-cli/releases/latest/download/ani-cli
cmp /tmp/released ~tester/.local/bin/ani-cli || fail "ani-cli isn't its latest release"

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
! grep -q 'Installing ani-cli' /tmp/second.log || fail "second run installed ani-cli again"
check "second run"

if grep -q '^ID=fedora' /etc/os-release; then
	step "fedora with a terminal: yes to RPM Fusion"
	echo y | as_tester 'script -qec "sh /install.sh" /dev/null' || fail "RPM Fusion run"
	rpm -q ffmpeg-libs || fail "no ffmpeg from RPM Fusion"
	check "RPM Fusion run"
fi

step "an ani-cli that's there stays, whatever its version: ours"
printf '#!/bin/sh\necho 4.10\n' >~tester/.local/bin/ani-cli
as_tester 'sh /install.sh' >/tmp/ours.log 2>&1 || { cat /tmp/ours.log; fail "run with our old one"; }
cat /tmp/ours.log
grep -q 'ani-cli 4.10$' /tmp/ours.log || fail "replaced the ani-cli in ~/.local/bin"

step "an ani-cli that's there stays, whatever its version: the system's"
rm ~tester/.local/bin/ani-cli
printf '#!/bin/sh\necho 4.10\n' >/usr/bin/ani-cli
chmod 755 /usr/bin/ani-cli
as_tester 'sh /install.sh' >/tmp/system.log 2>&1 || { cat /tmp/system.log; fail "run with the system's"; }
cat /tmp/system.log
[ ! -e ~tester/.local/bin/ani-cli ] || fail "installed a second ani-cli"
grep -q 'ani-cli 4.10$' /tmp/system.log || fail "the system's ani-cli isn't the one reported"

if grep -q '^ID=arch' /etc/os-release; then
	step "arch: ani-hop-bin and ani-cli from the AUR"
	# a stand-in yay: ani-hop-bin "exists", and installing either puts a real one into /usr/bin
	printf '#!/bin/sh\necho 5.1.0\n' >/tmp/aur-ani-cli
	cat >/usr/local/bin/yay <<-'EOF'
		#!/bin/sh
		case "$*" in *-Si*) exit 0 ;; esac
		for p; do
			[ "$p" != ani-hop-bin ] || sudo install -m 755 "$HOME/.local/bin/ani-hop" /usr/bin/ani-hop
			[ "$p" != ani-cli ] || sudo install -m 755 /tmp/aur-ani-cli /usr/bin/ani-cli
		done
	EOF
	chmod 755 /usr/local/bin/yay
	rm /usr/bin/ani-cli
	as_tester 'sh /install.sh' >/tmp/aur.log 2>&1 || { cat /tmp/aur.log; fail "AUR run"; }
	cat /tmp/aur.log
	[ -x /usr/bin/ani-hop ] || fail "the stand-in yay installed no ani-hop"
	[ ! -e ~tester/.local/bin/ani-hop ] || fail "the release's ani-hop should be gone"
	! grep -q 'Installing ani-hop into' /tmp/aur.log || fail "downloaded the release despite ani-hop-bin"
	grep -q 'ani-cli 5.1.0$' /tmp/aur.log || fail "the AUR's ani-cli isn't the one reported"
	[ ! -e ~tester/.local/bin/ani-cli ] || fail "installed a second ani-cli next to the AUR's"

	step "arch: a failed AUR install falls back to the release"
	printf '#!/bin/sh\ncase "$*" in *-Si*) exit 0 ;; esac\nexit 1\n' >/usr/local/bin/yay
	rm /usr/bin/ani-cli
	as_tester 'sh /install.sh' >/tmp/aurfail.log 2>&1 || { cat /tmp/aurfail.log; fail "failed-AUR run"; }
	cat /tmp/aurfail.log
	grep -q 'Installing ani-hop into' /tmp/aurfail.log || fail "no release download after the AUR failed"
	[ -x ~tester/.local/bin/ani-hop ] || fail "no ani-hop of ours after the AUR failed"
	cmp /tmp/released ~tester/.local/bin/ani-cli || fail "no ani-cli of ours after the AUR failed"
fi

step "ANI_HOP_KEEP leaves an ani-hop from elsewhere alone"
printf '#!/bin/sh\necho ani-hop kept\n' >/usr/local/bin/ani-hop
chmod 755 /usr/local/bin/ani-hop
rm -f ~tester/.local/bin/ani-hop
as_tester 'ANI_HOP_KEEP=1 sh /install.sh' >/tmp/keep.log 2>&1 || { cat /tmp/keep.log; fail "keep run"; }
cat /tmp/keep.log
[ ! -e ~tester/.local/bin/ani-hop ] || fail "installed a second ani-hop"
grep -q 'Done: ani-hop kept' /tmp/keep.log || fail "didn't report the kept ani-hop"
# ~/.local/bin can hold it too, as a link to where it really lives
ln -s /usr/local/bin/ani-hop ~tester/.local/bin/ani-hop
as_tester 'PATH=$HOME/.local/bin:$PATH ANI_HOP_KEEP=1 sh /install.sh' >/tmp/link.log 2>&1 || { cat /tmp/link.log; fail "keep run with a link"; }
[ -L ~tester/.local/bin/ani-hop ] || fail "replaced the link to the kept ani-hop"

step "PASS"
