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
	dnf install -qy sudo util-linux >/dev/null # util-linux has su
elif command -v pacman >/dev/null; then
	pacman -Syu --noconfirm --needed sudo >/dev/null
fi
useradd -m -s /bin/bash tester
echo 'tester ALL=(ALL) NOPASSWD: ALL' >/etc/sudoers.d/tester

as_tester() { su - tester -c "export ANI_HOP_RELEASE_URL=file:///release; $1"; }

check() {
	as_tester 'set -e; PATH=$HOME/.local/bin:$PATH
		ani-hop --version | grep "^ani-hop [0-9]"
		echo "ani-cli $(ani-cli -V)"; ani-cli -V | grep -q "^5\."
		echo "ani-skip $(ani-skip -V)"
		echo "yt-dlp $(yt-dlp --version) at $(command -v yt-dlp)"
		mpv --version | head -n1
		ffmpeg -version | head -n1
		test -s ~/.config/mpv/scripts/skip.lua
		for c in patch diff xdg-open xz; do command -v $c >/dev/null || { echo "missing $c"; exit 1; }; done' ||
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
as_tester 'sh /install.sh' || fail "second run"
echo "second run took $(($(date +%s) - start))s"
check "second run"

step "PASS"
