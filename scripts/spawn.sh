#!/bin/bash
# spawn.sh — open a pre-configured llm-terminal session in a new macOS Terminal
# window (or tab) from a manifest file or URL.
#
# This is the window-spawning layer: it reuses the macOS `osascript` "do script"
# + custom-title mechanism to open a titled Terminal, then runs
# `mock-terminal open <file-or-url>` inside it. The spawned session shows its
# session card and asks for confirmation in the new window before running, so a
# spec — even one fetched from a URL — is never executed unreviewed.
#
# Usage:
#   spawn.sh <manifest.json | https://…> [--name <title>] [--tab] [--bin <path>]
#
# Arguments:
#   --name   window/tab title (default: derived from the manifest file name)
#   --tab    open as a TAB in the front Terminal window instead of a new one
#   --bin    path to the mock-terminal binary (default: ./target/<…>, then $PATH)
#
# macOS only (uses osascript). On Linux, swap the osascript block for
# `gnome-terminal --` or equivalent.

set -euo pipefail

SOURCE=""
NAME=""
TAB=false
BIN=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) NAME="$2"; shift 2;;
    --tab) TAB=true; shift;;
    --bin) BIN="$2"; shift 2;;
    -*) echo "Unknown arg: $1" >&2; exit 1;;
    *) SOURCE="$1"; shift;;
  esac
done

if [ -z "$SOURCE" ]; then
  echo "Usage: spawn.sh <manifest.json | https://…> [--name <title>] [--tab] [--bin <path>]" >&2
  exit 1
fi

# Resolve the binary: explicit --bin, else release/debug build, else $PATH.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [ -z "$BIN" ]; then
  if [ -x "$REPO_ROOT/target/release/mock-terminal" ]; then
    BIN="$REPO_ROOT/target/release/mock-terminal"
  elif [ -x "$REPO_ROOT/target/debug/mock-terminal" ]; then
    BIN="$REPO_ROOT/target/debug/mock-terminal"
  else
    BIN="mock-terminal" # rely on $PATH
  fi
elif [ -e "$BIN" ] && [[ "$BIN" != /* ]]; then
  # A relative --bin resolves against OUR cwd, but the spawned shell starts
  # in $HOME — where it would die with "command not found" before the
  # session ever registers. Absolutize it here.
  BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
fi

# A local path is made absolute (the spawned shell starts in $HOME); a URL is
# passed through untouched.
case "$SOURCE" in
  http://*|https://*)
    ARG="$SOURCE"
    [ -z "$NAME" ] && NAME="manifest-url"
    ;;
  *)
    if [ ! -f "$SOURCE" ]; then echo "Manifest not found: $SOURCE" >&2; exit 1; fi
    ARG="$(cd "$(dirname "$SOURCE")" && pwd)/$(basename "$SOURCE")"
    [ -z "$NAME" ] && NAME="$(basename "$SOURCE" .json)"
    ;;
esac

# Forward MT_* tuning vars into the spawned window: osascript's "do script"
# opens a fresh login shell that does NOT inherit this shell's environment,
# so e.g. `MT_AUTO_ADVANCE_IDLE_MS=15000 spawn.sh …` would silently apply
# only to the parent. Prefix the command with explicit assignments instead.
ENV_PREFIX=""
for var in MT_AUTO_ADVANCE MT_AUTO_ADVANCE_IDLE_MS MT_AUTO_APPROVE MT_INTERACTIVE MT_TIMEOUT_SECS MT_TRAJECTORY_DIR MT_TIL_DIR; do
  if [ -n "${!var:-}" ]; then
    ENV_PREFIX="${ENV_PREFIX}${var}='${!var}' "
  fi
done

RUN_CMD="${ENV_PREFIX}$BIN open $ARG"
TITLE="🤖 ${NAME}"

# AppleScript: open a NEW WINDOW (default) or a TAB in the front window.
if [ "$TAB" = true ]; then
  osascript \
    -e "tell application \"Terminal\" to do script \"${RUN_CMD}\" in front window" \
    -e "delay 0.2" \
    -e "tell application \"Terminal\" to set custom title of selected tab of front window to \"${TITLE}\"" \
    -e 'tell application "Terminal" to activate' >/dev/null
else
  osascript \
    -e "tell application \"Terminal\" to do script \"${RUN_CMD}\"" \
    -e "delay 0.2" \
    -e "tell application \"Terminal\" to set custom title of front window to \"${TITLE}\"" \
    -e 'tell application "Terminal" to activate' >/dev/null
fi

echo "Spawned llm-terminal session: ${NAME}"
echo "  Title:   ${TITLE}"
echo "  Layout:  $([ "$TAB" = true ] && echo tab || echo "new window")"
echo "  Command: ${RUN_CMD}"
echo ""
echo "The spawned window shows a session card and confirms before running."
echo "It logs its trajectory under ~/.til/trajectories/ (JSONL)."
