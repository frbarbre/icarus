#!/usr/bin/env bash
# Runs the real app against a throwaway Icarus server and checks its main thread goes quiet:
# idle, with an agent thinking in an open tab, and after opening workspaces that weren't open yet.
#
#   scripts/dev/live-smoke.sh [--app <binary>] [--keep] [--no-build]
#
# Everything (HOME, server home, config, app data) lives in a temporary directory, so the real
# ~/.icarus, its server and the user's settings are never touched. Agents run icarus-fake-agent.
# Prints CPU averages and exits non-zero when the app stays busy or a window never finishes loading.
set -uo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
app_bin="$root/app/target/debug/zed"
keep=0
build=1
while [[ $# -gt 0 ]]; do
  case $1 in
    --app) app_bin=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    --no-build) build=0; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
idle_budget=${IDLE_CPU_BUDGET:-5}
working_budget=${WORKING_CPU_BUDGET:-30}
bins="$root/target/debug"

if [[ $build -eq 1 ]]; then
  (
    cd "$root"
    export PATH="$HOME/.cargo/bin:$(/opt/homebrew/bin/mise bin-paths 2>/dev/null | tr '\n' ':')$PATH"
    CARGO_INCREMENTAL=0 cargo build -q -p icarus-cli -p icarus-server
  ) || exit 1
  if [[ $app_bin == "$root/app/target/debug/zed" ]]; then
    (
      cd "$root/app"
      export PATH="$HOME/.cargo/bin:$PATH" DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
        CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=1.98.1
      /opt/homebrew/bin/mise exec cmake@latest -- cargo build -q -p zed --bin zed
    ) || exit 1
  fi
fi

# Resolved, as the server stores real paths (/private/tmp) and the app must match them.
tmp=$(cd "$(mktemp -d /tmp/icarus-smoke.XXXXXX)" && pwd -P)
export HOME="$tmp/home"
export ICARUS_HOME="$HOME/.icarus" ICARUS_CONFIG_DIR="$HOME/.config/icarus"
export ICARUS_WORKSPACE_ROOT="$tmp/workspaces" ICARUS_GH_FIXTURES="$tmp/gh-fixtures.json"
export SHELL=/bin/sh GIT_CONFIG_NOSYSTEM=1
mkdir -p "$ICARUS_HOME" "$ICARUS_CONFIG_DIR" "$ICARUS_WORKSPACE_ROOT"
echo '{}' >"$ICARUS_GH_FIXTURES"
server_pid=""
app_pid=""
cleanup() {
  [[ -n $app_pid ]] && kill "$app_pid" 2>/dev/null
  [[ -n $server_pid ]] && kill "$server_pid" 2>/dev/null
  pkill -f "$bins/icarus-fake-agent" 2>/dev/null
  if [[ $keep -eq 1 ]]; then echo "kept $tmp"; else rm -rf "$tmp"; fi
}
trap cleanup EXIT
icarus() { "$bins/icarus" "$@"; }
fail() { echo "FAIL: $*"; exit 1; }

repo="$tmp/widgets"
git init -q --bare -b main "$tmp/origin.git"
git init -q -b main "$repo"
git -C "$repo" config user.email smoke@example.com
git -C "$repo" config user.name Smoke
git -C "$repo" config commit.gpgsign false
printf 'icarus.local.json\n' >"$repo/.gitignore"
printf 'hello\n' >"$repo/readme.md"
git -C "$repo" add .
git -C "$repo" commit -q -m init
git -C "$repo" remote add origin "$tmp/origin.git"
git -C "$repo" push -q origin main
# User settings, so worktrees (which don't get the ignored icarus.local.json) use the fake agent too.
printf '{"agents":{"commands":{"claude":["%s"]}}}\n' "$bins/icarus-fake-agent" >"$ICARUS_CONFIG_DIR/icarus.json"

"$bins/icarus-server" --home "$ICARUS_HOME" serve >"$tmp/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 100); do
  [[ -S $ICARUS_HOME/server.sock && -f $ICARUS_HOME/server.token ]] && break
  sleep 0.1
done
[[ -S $ICARUS_HOME/server.sock ]] || fail "the server did not start ($tmp/server.log)"

project=$(icarus --json project add "$repo" --name widgets | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])') ||
  fail "project add"
workspace_id() { python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }
thinking=$(icarus --json workspace create --project "$project" thinking --prompt "stream:600000 thinking" | workspace_id) ||
  fail "workspace create"
quiet=$(icarus --json workspace create --project "$project" quiet | workspace_id) || fail "workspace create"
# Athena's 0.2.12 froze reopening a review workspace (agent pane beside a browser pane).
review=$(icarus --json workspace create --project "$project" review --layout review \
  --pr-url "about:blank" --prompt "stream:600000 reviewing" | workspace_id) || fail "workspace create --layout review"

"$app_bin" --user-data-dir "$tmp/app-data" "$repo" >"$tmp/app.log" 2>&1 &
app_pid=$!
for _ in $(seq 240); do
  icarus app tree >/dev/null 2>&1 && break
  kill -0 "$app_pid" 2>/dev/null || fail "the app exited ($tmp/app.log)"
  sleep 0.5
done
icarus app tree >/dev/null 2>&1 || fail "the app never answered the driver"
sleep 5

# Average %CPU of the app over $1 seconds.
cpu() {
  local total=0
  for _ in $(seq "$1"); do
    total=$(echo "$total + $(ps -o %cpu= -p "$app_pid" | tr -d ' ')" | bc)
    sleep 1
  done
  echo "scale=1; $total / $1" | bc
}

idle=$(cpu 15)

status=0
check() {
  if ! icarus app "$@" >/dev/null; then
    echo "FAIL: icarus app $*"
    status=1
  fi
}
open() {
  [[ $status -eq 0 ]] && check wait "card:$1" --timeout 30000
  [[ $status -eq 0 ]] && check click "card:$1" --wait 'text:"Loading…"' --gone --timeout 30000
  [[ $status -eq 0 ]] && check wait "card:$1" --timeout 30000
}
# Opening workspaces that aren't open yet gives each its own sidebar panel holding the same busy
# agent; 0.2.12 hung here, its panels overwriting each other's copy of the agent forever.
open "$quiet"
open "$thinking"
# A turn started from outside the app (as a next action or a notification's agent does).
icarus agent create --workspace "$quiet" --prompt "stream:600000 thinking" >/dev/null ||
  fail "agent create"
[[ $status -eq 0 ]] && check wait "agent-tab:*" --timeout 30000
open "$review"
open "$thinking"
open "$quiet"
[[ $status -eq 0 ]] && check wait 'text:"Thinking…"' --timeout 30000
sleep 3
working=$(cpu 20)
sample "$app_pid" 3 -file "$tmp/sample.txt" >/dev/null 2>&1

printf '\n%-44s %8s %8s\n' "state" "cpu %" "budget"
row() {
  local flag=""
  if (($(echo "$2 > $3" | bc))); then flag="OVER"; status=1; fi
  printf '%-44s %8s %8s  %s\n' "$1" "$2" "$3" "$flag"
}
row "idle, 15 s" "$idle" "$idle_budget"
row "agents thinking, 3 workspaces incl. review, 20 s" "$working" "$working_budget"
if [[ $status -ne 0 ]]; then
  echo
  echo "Heaviest main-thread frames ($tmp/sample.txt):"
  grep -A25 "Thread_.*DispatchQueue_1: com.apple.main-thread" "$tmp/sample.txt" | head -40
  keep=1
  echo "FAIL"
else
  echo "PASS"
fi
exit $status
