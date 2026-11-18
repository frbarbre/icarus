#!/usr/bin/env bash
# Runs the latency tests and prints what each flow took against its budget.
#
#   scripts/dev/perf.sh [--server | --app]
#
# Server tests (crates/icarus-server/tests/perf_e2e.rs) time requests on a repository with
# thousands of files; app tests (app/crates/icarus_ui/src/perf_tests.rs) drive the UI through
# clicks and keys until the result is drawn. App rows also show how long the flow waited on timers.
# Debug builds, so real numbers are lower; a test fails when a flow is over its budget.
set -uo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
which=${1:-all}
output=$(mktemp)
trap 'rm -f "$output"' EXIT
status=0

if [[ $which != --app ]]; then
  (
    cd "$root"
    export PATH="$HOME/.cargo/bin:$(/opt/homebrew/bin/mise bin-paths 2>/dev/null | tr '\n' ':')$PATH"
    CARGO_INCREMENTAL=0 cargo test -p icarus-server --test perf_e2e -- --nocapture --test-threads=1
  ) 2>&1 | tee -a "$output" | grep -E '^test |panicked|error'
  [[ ${PIPESTATUS[0]} -eq 0 ]] || status=1
fi

if [[ $which != --server ]]; then
  (
    cd "$root/app"
    export PATH="$HOME/.cargo/bin:$PATH" DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
      CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=1.98.1
    /opt/homebrew/bin/mise exec cmake@latest -- cargo test -p icarus_ui perf_tests -- --nocapture
  ) 2>&1 | tee -a "$output" | grep -E '^test |panicked|error'
  [[ ${PIPESTATUS[0]} -eq 0 ]] || status=1
  (
    cd "$root/app"
    export PATH="$HOME/.cargo/bin:$PATH" DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
      CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=1.98.1
    /opt/homebrew/bin/mise exec cmake@latest -- cargo test -p agent_ui redraw_budget -- --nocapture
  ) 2>&1 | tee -a "$output" | grep -E '^test |panicked|error'
  [[ ${PIPESTATUS[0]} -eq 0 ]] || status=1
fi

echo
grep -ao $'PERF\t.*' "$output" | awk -F'\t' '
  BEGIN { printf "%-7s %-58s %10s %10s %10s  %s\n", "side", "flow", "ms", "budget", "timers ms", ""; }
  {
    timers = ($6 == "") ? "-" : $6
    flag = ($4 + 0 > $5 + 0) ? "OVER" : ""
    printf "%-7s %-58s %10.1f %10d %10s  %s\n", $2, $3, $4, $5, timers, flag
  }'

# Redraw budgets: the most notifies any watched view got over 5 s of frames, and files written.
redraws=$(grep -ao $'REDRAWS\t.*' "$output")
if [[ -n $redraws ]]; then
  echo
  echo "$redraws" | awk -F'\t' '
    BEGIN { printf "%-7s %-58s %10s %10s %10s  %s\n", "side", "state", "redraws", "budget", "writes", ""; }
    {
      flag = ($4 + 0 > $5 + 0 || $6 + 0 > 0) ? "OVER" : ""
      printf "%-7s %-58s %10d %10d %10d  %s\n", $2, $3, $4, $5, $6, flag
    }'
  echo "$redraws" | awk -F'\t' '$4 + 0 > $5 + 0 || $6 + 0 > 0 { over = 1 } END { exit over }' || status=1
elif [[ $which != --server ]]; then
  echo "no redraw budgets ran" >&2
  status=1
fi
exit $status
