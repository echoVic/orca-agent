#!/usr/bin/env bash
# Orca TUI smoke test — driven by tui-test (microsoft/tui-test, stable 0.1.x).
#
# Usage: ./orca-smoke.sh [workspace-dir]
#   With no argument a fresh temp workspace is used, so the first-run
#   Welcome/trust screen always appears (Orca records onboarding per folder).
#   Set SNAPSHOT=1 to also write/compare a golden snapshot of the help overlay.
#
# Requires: orca and tui-test on PATH. Exits non-zero if any check fails.
set -uo pipefail

ORCA=${ORCA_BIN:-orca}
command -v tui-test >/dev/null 2>&1 || { echo "tui-test not found; install from https://github.com/microsoft/tui-test" >&2; exit 2; }
command -v "$ORCA" >/dev/null 2>&1 || { echo "orca not found (set ORCA_BIN or put it on PATH)" >&2; exit 2; }

# Isolated Orca state: never touch the developer's (or CI runner's) ~/.orca.
if [ -z "${ORCA_HOME:-}" ]; then
  ORCA_HOME=$(mktemp -d "${TMPDIR:-/tmp}/orca-e2e-home.XXXXXX")
  export ORCA_HOME
  ORCA_E2E_HOME_CREATED=1
fi
# A placeholder key keeps the first-run Setup screen out of the way; the default
# suites never call the model. LIVE=1 needs a real ORCA_API_KEY/DEEPSEEK_API_KEY.
if [ -z "${ORCA_API_KEY:-}" ] && [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  export ORCA_API_KEY=sk-tui-e2e-placeholder
fi
cleanup_orca_home() {
  [ "${ORCA_E2E_HOME_CREATED:-0}" = 1 ] && rm -rf "$ORCA_HOME"
}
trap cleanup_orca_home EXIT

S=orca-smoke
WS="${1:-}"
if [ -z "$WS" ]; then
  WS=$(mktemp -d /tmp/orca-smoke.XXXXXX)
  CLEANUP=1
fi
WSREAL=$(cd "$WS" && pwd -P)
SHOT="$(cd "$(dirname "$0")" && pwd)/shots"
mkdir -p "$WS" "$SHOT"

pass=0 fail=0
ok() { printf 'PASS  %s\n' "$1"; pass=$((pass + 1)); }
no() { printf 'FAIL  %s\n' "$1"; fail=$((fail + 1)); }
check() { # check <desc> <cmd...>
  local d=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$d"; else no "$d"; fi
}
tt() { tui-test --session "$S" "$@"; }

tt close --all >/dev/null 2>&1 || true

echo "workspace: $WSREAL"

# 1. launch: first-run welcome for an untrusted workspace
tt run --cols 100 --rows 30 --cwd "$WS" "$ORCA" >/dev/null 2>&1 || no "launch orca"
tt wait idle --timeout 15000 >/dev/null 2>&1
check "welcome screen renders"        tt expect text "Welcome" --timeout 15000
check "workspace path shown"          tt expect text "$WSREAL"
check "untrusted banner"              tt expect text "Folder trust: untrusted"
check "doctor summary shown"          tt expect text "Local doctor checks"
check "window title is Orca"          tt expect title "Orca"
check "alternate screen enabled"      tt expect mode alternate_screen
tt screenshot -o "$SHOT/01-welcome.png" >/dev/null 2>&1

# 2. choose "Continue untrusted" -> main chat UI
tt key press u >/dev/null 2>&1
tt key press Enter >/dev/null 2>&1
tt wait idle --timeout 15000 >/dev/null 2>&1
check "version in splash"             tt expect text "v[0-9]+\.[0-9]+\.[0-9]+" --regex
check "model in splash"               tt expect text "deepseek-flash"
check "cwd in splash"                 tt expect text "$WSREAL"
check "composer visible"              tt expect text "Message Orca"
check "composer cursor visible"       tt expect cursor --visible
tt screenshot -o "$SHOT/02-chat.png" >/dev/null 2>&1

# 3. help overlay (?)
tt type '?' >/dev/null 2>&1
check "help overlay opens"            tt expect text "cancel and quit"
check "help lists editor keys"        tt expect text "ctrl+w/d/k/u"
tt screenshot -o "$SHOT/03-help.png" >/dev/null 2>&1
[ "${SNAPSHOT:-0}" = "1" ] && check "help overlay snapshot" tt expect snapshot help-overlay
tt key press Escape >/dev/null 2>&1

# 4. command palette (/)
tt type '/' >/dev/null 2>&1
check "palette lists /new"            tt expect text "/new"
check "palette lists /status"         tt expect text "/status"
tt screenshot -o "$SHOT/04-palette.png" >/dev/null 2>&1
tt key press Escape >/dev/null 2>&1
tt key press Ctrl+u >/dev/null 2>&1

# 5. /status card and expansion
tt type '/status' >/dev/null 2>&1
tt key press Enter >/dev/null 2>&1
check "status card renders"           tt expect text "Session status"
tt key press Ctrl+o >/dev/null 2>&1
check "status details expand"         tt expect text "mode: full-auto"
check "status shows cwd"              tt expect text "$WSREAL"
tt screenshot -o "$SHOT/05-status.png" >/dev/null 2>&1

# 6. resize down and back
tt resize 60 20 >/dev/null 2>&1
check "narrow layout renders"         tt expect text "deepseek-flash"
tt resize 100 30 >/dev/null 2>&1
check "restored layout renders"       tt expect text "Message Orca"

# 7. quit: orca needs two Ctrl+C presses
tt key press Ctrl+c >/dev/null 2>&1
sleep 0.4
tt key press Ctrl+c >/dev/null 2>&1
sleep 1.2
if tt state 2>/dev/null | grep -qE '^exited: [0-9]+'; then
  ok "clean exit after ctrl+c ctrl+c"
else
  no "clean exit after ctrl+c ctrl+c"
fi

tt close --all >/dev/null 2>&1
[ "${CLEANUP:-0}" = "1" ] && rm -rf "$WS"

echo
echo "result: $pass passed, $fail failed"
exit $((fail > 0 ? 1 : 0))
