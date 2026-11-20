#!/usr/bin/env bash
# Rebuilds the release binary if the source has changed, then runs it. The
# desktop shortcut runs this, so code changes show up the next time it's
# clicked. With no arguments it opens the window; anything else goes to the
# CLI instead (e.g. `scripts/launch.sh status`).
#
# If the build fails, the last good build runs and a popup says so. The
# compiler output is in target/launch-build.log.
set -u

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1
bin=target/release/protonfilter
log=target/launch-build.log
mkdir -p target

# Desktop sessions often don't have ~/.cargo/bin on PATH.
cargo=${CARGO:-cargo}
command -v "$cargo" >/dev/null || cargo=$HOME/.cargo/bin/cargo

# popup <message> <seconds>: a desktop notification, or stderr in a terminal.
popup() {
    if [ -t 2 ] || ! command -v kdialog >/dev/null; then
        echo "protonfilter: $1" >&2
    else
        kdialog --title ProtonFilter --icon protonfilter --passivepopup "$1" "$2" &
    fi
}

# Nothing to build takes about 0.1 s, so only mention a build that's slow
# enough to notice.
"$cargo" build --release --quiet >"$log" 2>&1 &
build=$!
(sleep 1.5 && kill -0 "$build" 2>/dev/null && popup "Rebuilding with your latest changes…" 10) &
watcher=$!
wait "$build"
status=$?
kill "$watcher" 2>/dev/null

if [ "$status" -ne 0 ]; then
    if [ -x "$bin" ]; then
        popup "The build failed, so this is the previous version. Compiler output: $PWD/$log" 15
    else
        popup "The build failed and there's no earlier build to run. Compiler output: $PWD/$log" 30
        exit 1
    fi
fi

exec "$bin" "${@:-gui}"
