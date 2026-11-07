#!/usr/bin/env bash
# Orca TUI extended test — tui-test (stable 0.1.x) driven.
#
# Covers interaction semantics, pickers, approval-mode cycling, workspace trust,
# mentions, narrow layout, unicode, and (opt-in) live model round trips.
#
# Usage: ./orca-extended.sh        # UI-only, no model calls
#        LIVE=1 ./orca-extended.sh # also send prompts (uses DeepSeek API)
set -uo pipefail

ORCA=${ORCA_BIN:-orca}
command -v tui-test >/dev/null 2>&1 || { echo "tui-test not found; install from https://github.com/microsoft/tui-test" >&2; exit 2; }
command -v "$ORCA" >/dev/null 2>&1 || { echo "orca not found (set ORCA_BIN or put it on PATH)" >&2; exit 2; }

# tui-test changes to --cwd before spawning, so a relative ORCA_BIN would
# resolve against the workspace. Normalize it to an absolute path.
case "$ORCA" in
  */*) ORCA=$(cd "$(dirname "$ORCA")" && pwd -P)/$(basename "$ORCA") ;;
esac

# Isolated Orca state: never touch the developer's (or CI runner's) ~/.orca.
# LIVE runs use the real ~/.orca (or an explicit ORCA_HOME) so a logged-in
# configuration can reach the provider.
if [ "${LIVE:-0}" != "1" ] && [ -z "${ORCA_HOME:-}" ]; then
  ORCA_HOME=$(mktemp -d "${TMPDIR:-/tmp}/orca-e2e-home.XXXXXX")
  export ORCA_HOME
  ORCA_E2E_HOME_CREATED=1
fi
# A placeholder key keeps the first-run Setup screen out of the way; the default
# suites never call the model. LIVE=1 needs a real ORCA_API_KEY/DEEPSEEK_API_KEY.
if [ "${LIVE:-0}" != "1" ] && [ -z "${ORCA_API_KEY:-}" ] && [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  export ORCA_API_KEY=sk-tui-e2e-placeholder
fi
cleanup_orca_home() {
  [ "${ORCA_E2E_HOME_CREATED:-0}" = 1 ] && rm -rf "$ORCA_HOME"
}
trap cleanup_orca_home EXIT

BASE="$(cd "$(dirname "$0")" && pwd)"
SHOT="$BASE/shots"
mkdir -p "$SHOT"
pass=0 fail=0
ok() { printf 'PASS  %s\n' "$1"; pass=$((pass + 1)); }
no() { printf 'FAIL  %s\n' "$1"; fail=$((fail + 1)); }
check() { local d=$1; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else no "$d"; fi; }
tt() { tui-test --session "$1" "${@:2}"; }

tui-test close --all >/dev/null 2>&1 || true

# ---------------------------------------------------------------- part A: UI
A=$(mktemp -d /tmp/orca-ext-a.XXXXXX)
tt a run --cols 100 --rows 30 --cwd "$A" "$ORCA" >/dev/null 2>&1; sleep 2
check "welcome: T option"        tt a expect text "Trust workspace"
check "welcome: U option"        tt a expect text "Continue untrusted"
check "welcome: E option"        tt a expect text "Exit"
check "welcome: doctor line"     tt a expect text "Local doctor checks"
tt a key press u >/dev/null 2>&1; tt a key press Enter >/dev/null 2>&1; sleep 1.2
check "splash version"           tt a expect text "v[0-9]+\.[0-9]+\.[0-9]+" --regex
check "splash model"             tt a expect text "deepseek-flash"
tt a screenshot -o "$SHOT/10-splash.png" >/dev/null 2>&1

# help only opens on an empty composer; otherwise '?' is literal
tt a type 'abc' >/dev/null 2>&1; sleep 0.3
tt a key press '?' >/dev/null 2>&1; sleep 0.5
check "? with text is literal"   tt a expect text 'abc?'
tt a key press Ctrl+u >/dev/null 2>&1; sleep 0.4
check "draft-clear hint shown"   tt a expect text "Draft cleared"

tt a key press Ctrl+k >/dev/null 2>&1; sleep 0.6
check "ctrl+k opens help"        tt a expect text "find in transcript"
tt a key press Escape >/dev/null 2>&1; sleep 0.3
tt a key press F1 >/dev/null 2>&1; sleep 0.6
check "F1 opens help"            tt a expect text "cycle approval mode"
tt a key press Escape >/dev/null 2>&1; sleep 0.3

# approval mode cycle
check "mode starts full-auto"    tt a expect text "⇧Tab full-auto"
tt a key press Shift+Tab >/dev/null 2>&1; sleep 0.4
check "shift+tab -> plan"        tt a expect text "⇧Tab plan"
tt a key press Shift+Tab >/dev/null 2>&1; sleep 0.4
check "shift+tab -> suggest"     tt a expect text "⇧Tab suggest"
tt a key press Shift+Tab >/dev/null 2>&1; sleep 0.4
check "shift+tab -> auto-edit"   tt a expect text "⇧Tab auto-edit"
tt a key press Shift+Tab >/dev/null 2>&1; sleep 0.5
check "full-access modal"        tt a expect text "Continue with Full Access"
check "full-access warning"      tt a expect text "removes command approvals"
tt a screenshot -o "$SHOT/11-full-access.png" >/dev/null 2>&1
tt a key press Escape >/dev/null 2>&1; sleep 0.4
check "cancel keeps auto-edit"   tt a expect text "⇧Tab auto-edit"

# ctrl+l clears the screen, composer survives
tt a key press Ctrl+l >/dev/null 2>&1; sleep 1.2
check "ctrl+l keeps status bar"  tt a expect text "? help"

# pickers
tt a type '/model' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 0.8
check "/model picker options"    tt a expect text "deepseek-v4-pro"
check "/model auto option"       tt a expect text "auto (pro + flash for aux)"
tt a key press Escape >/dev/null 2>&1; sleep 0.3

tt a type '/base-url' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 0.8
check "/base-url shows endpoint" tt a expect text "api.deepseek.com"
tt a key press Escape >/dev/null 2>&1; sleep 0.3

tt a type '/resume' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 1.2
if tt a text 2>/dev/null | grep -qE 'No saved conversations|type to filter'; then
  ok "/resume shows list or empty state"
else
  no "/resume shows list or empty state"
fi
tt a key press Escape >/dev/null 2>&1; tt a key press Ctrl+u >/dev/null 2>&1; sleep 0.3

tt a type '/side' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 1.0
check "side before conversation errors" tt a expect text "start the main conversation"
tt a type '/new' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 1.2
tt a type '/side' >/dev/null 2>&1; sleep 0.5; tt a key press Enter >/dev/null 2>&1; sleep 1.2
check "/side opens after /new"   tt a expect text "Side · main idle"
tt a screenshot -o "$SHOT/15-side.png" >/dev/null 2>&1
tt a key press Ctrl+/ >/dev/null 2>&1; sleep 1.0
check "ctrl+/ back to main"      tt a expect text "Message Orca"

tt a type '/trust' >/dev/null 2>&1; sleep 0.6
check "/trust palette entry"     tt a expect text "Manage folder trust"
tt a key press Escape >/dev/null 2>&1; tt a key press Ctrl+u >/dev/null 2>&1; sleep 0.3

# unicode + narrow layout
tt a type '你好世界 🐋' >/dev/null 2>&1; sleep 0.5
check "unicode input renders"    tt a expect text "你好世界"
tt a key press Ctrl+u >/dev/null 2>&1
tt a resize 40 12 >/dev/null 2>&1; sleep 1.2
check "narrow 40x12 renders"     tt a expect text "auto-edit"
tt a screenshot -o "$SHOT/12-narrow.png" >/dev/null 2>&1
tt a resize 100 30 >/dev/null 2>&1; sleep 1.2
check "back to 100x30"           tt a expect text "deepseek-flash"
tt a key press Ctrl+c >/dev/null 2>&1; sleep 1.0
check "ctrl+c arms quit (hint)"  tt a expect text "Press Ctrl+C again to quit"
tt a key press Ctrl+c >/dev/null 2>&1; sleep 1.2
if tt a state 2>/dev/null | grep -qE '^exited: [0-9]+'; then ok "second ctrl+c quits"; else no "second ctrl+c quits"; fi
rm -rf "$A"

# ------------------------------------------------- part C: trusted vs untrusted mentions
T=$(mktemp -d /tmp/orca-ext-t.XXXXXX); mkdir -p "$T/src"
echo hello > "$T/README.md"; echo x > "$T/src/main.ts"
orca trust add -C "$T" >/dev/null 2>&1
tt t run --cols 100 --rows 30 --cwd "$T" "$ORCA" >/dev/null 2>&1; sleep 2
check "trusted welcome label"    tt t expect text "Folder trust: trusted"
tt t key press t >/dev/null 2>&1; tt t key press Enter >/dev/null 2>&1; sleep 1.0
tt t type '@' >/dev/null 2>&1; sleep 1.2
check "trusted @ lists files"    tt t expect text "README.md"
check "trusted @ lists dirs"     tt t expect text "src/"
tt t screenshot -o "$SHOT/13-mentions-trusted.png" >/dev/null 2>&1
tt t key press Escape >/dev/null 2>&1; tt t close >/dev/null 2>&1

U=$(mktemp -d /tmp/orca-ext-u.XXXXXX); echo hello > "$U/README.md"
tt u run --cols 100 --rows 30 --cwd "$U" "$ORCA" >/dev/null 2>&1; sleep 2
tt u key press u >/dev/null 2>&1; tt u key press Enter >/dev/null 2>&1; sleep 1.0
tt u type '@' >/dev/null 2>&1; sleep 1.2
check "untrusted @ lists skills" tt u expect text "agent-browser"
check "untrusted @ still reads files" tt u expect text "README.md"
tt u key press Escape >/dev/null 2>&1; tt u close >/dev/null 2>&1
rm -rf "$U"; orca trust remove -C "$T" >/dev/null 2>&1; rm -rf "$T"

# ------------------------------------------------------- part E: welcome exit paths
W=$(mktemp -d /tmp/orca-ext-w.XXXXXX)
tt w run --cols 100 --rows 30 --cwd "$W" "$ORCA" >/dev/null 2>&1; sleep 2
check "welcome before esc"       tt w expect text "Welcome"
tt w key press Escape >/dev/null 2>&1; sleep 1.2
if tt w state 2>/dev/null | grep -qE '^exited: [0-9]+'; then ok "esc exits from welcome"; else no "esc exits from welcome"; fi
rm -rf "$W"

# ------------------------------------------------------------ part G: startup flags
P=$(mktemp -d /tmp/orca-ext-p.XXXXXX)
tt p run --cols 100 --rows 30 --cwd "$P" "$ORCA" --plan >/dev/null 2>&1; sleep 2
check "--plan keeps welcome"     tt p expect text "Welcome"
tt p key press u >/dev/null 2>&1; tt p key press Enter >/dev/null 2>&1; sleep 1.2
check "--plan starts in plan"    tt p expect text "⇧Tab plan"
tt p close >/dev/null 2>&1; rm -rf "$P"

# ------------------------------------------------------------- part F: live (opt-in)
if [ "${LIVE:-0}" = "1" ] && [ -z "${ORCA_API_KEY:-}" ] && [ -z "${DEEPSEEK_API_KEY:-}" ] \
   && [ ! -f "${ORCA_HOME:-$HOME/.orca}/auth.json" ]; then
  echo "LIVE=1 needs a configured key: ORCA_API_KEY, DEEPSEEK_API_KEY or ~/.orca/auth.json" >&2
  exit 2
fi

if [ "${LIVE:-0}" = "1" ]; then
  L=$(mktemp -d /tmp/orca-ext-l.XXXXXX)
  tt l run --cols 100 --rows 30 --cwd "$L" "$ORCA" >/dev/null 2>&1; sleep 2
  tt l key press u >/dev/null 2>&1; tt l key press Enter >/dev/null 2>&1; sleep 1.0
  tt l type 'Reply with the four-letter ping response' >/dev/null 2>&1
  tt l key press Enter >/dev/null 2>&1
  check "live: model answers pong"  tt l expect text "pong" --timeout 60000
  tt l screenshot -o "$SHOT/14-live.png" >/dev/null 2>&1
  # /copy puts the assistant response on the clipboard
  tt l type '/copy' >/dev/null 2>&1; sleep 0.5; tt l key press Enter >/dev/null 2>&1; sleep 1.2
  check "live: /copy toast"         tt l expect text "copied"
  if tt l get clipboard 2>/dev/null | grep -qi 'pong'; then ok "live: /copy fills clipboard"; else no "live: /copy fills clipboard"; fi
  # Escape on an empty composer backtracks to the previous prompt
  tt l key press Escape >/dev/null 2>&1; sleep 1.0
  check "live: esc backtracks prompt" tt l expect text "Backtracked to previous prompt"
  tt l key press Ctrl+u >/dev/null 2>&1; sleep 0.4
  tt l type '/status' >/dev/null 2>&1; sleep 0.5; tt l key press Enter >/dev/null 2>&1; sleep 1.0
  tt l key press Ctrl+o >/dev/null 2>&1; sleep 0.6
  check "live: conversation title"  tt l expect text "title: Reply with the four-letter"
  check "live: status shows usage"  tt l expect text "usage:"
  # interrupt a running turn
  tt l type 'Count slowly from 1 to 200, one number per line' >/dev/null 2>&1
  tt l key press Enter >/dev/null 2>&1; sleep 2.5
  check "live: turn is running"     tt l expect text "Running" --timeout 15000
  tt l key press Ctrl+c >/dev/null 2>&1; sleep 1.5
  check "live: ctrl+c cancels turn" tt l expect text "cancelled" --timeout 15000
  tt l close >/dev/null 2>&1
  rm -rf "$L"
fi

echo
echo "result: $pass passed, $fail failed"
exit $((fail > 0 ? 1 : 0))
