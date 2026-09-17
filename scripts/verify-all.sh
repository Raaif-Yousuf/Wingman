#!/usr/bin/env bash
# The full local gate: everything CI runs plus the checks CI cannot run
# (Pester, PSScriptAnalyzer, the hook tests). One line per step, so an
# agent or orchestrator reading the output spends a few hundred tokens,
# not a few thousand.
#
# Runs the FULL test suite. Only one of these may run on the machine at a
# time (parallel full runs have exhausted RAM here); worktree agents run
# filtered tests instead. See .claude/skills/filing-findings/SKILL.md.
#
# Usage: scripts/verify-all.sh [--no-fmt]
#   --no-fmt  report `cargo fmt --check` drift without failing on it

set -u
cd "$(dirname "$0")/.."

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
FMT_FATAL=1
[ "${1:-}" = "--no-fmt" ] && FMT_FATAL=0

LOG_DIR="$(mktemp -d)"
FAILED=0

step() {
    local name="$1" fatal="$2"
    shift 2
    local log="$LOG_DIR/$name.log"
    local start=$SECONDS
    if "$@" >"$log" 2>&1; then
        printf 'ok    %-10s %3ss\n' "$name" "$((SECONDS - start))"
    else
        if [ "$fatal" = 1 ]; then
            printf 'FAIL  %-10s %3ss  (%s)\n' "$name" "$((SECONDS - start))" "$log"
            FAILED=1
        else
            printf 'warn  %-10s %3ss  (%s)\n' "$name" "$((SECONDS - start))" "$log"
        fi
        grep -E 'error|FAILED|panicked|failed|Diff in' "$log" | head -8 | sed 's/^/      /'
    fi
}

pester() {
    pwsh -NoProfile -Command '
        $r = Invoke-Pester -Path packaging -PassThru -Output None
        "Passed=$($r.PassedCount) Failed=$($r.FailedCount)"
        if ($r.FailedCount -gt 0) { $r.Failed | ForEach-Object { "failed: $($_.ExpandedName)" }; exit 1 }'
}

pssa() {
    pwsh -NoProfile -Command '
        $f = @()
        foreach ($p in "install.ps1", "uninstall.ps1") { $f += Invoke-ScriptAnalyzer -Path $p -Severity Warning,Error }
        $f += Invoke-ScriptAnalyzer -Path packaging -Recurse -Severity Warning,Error
        $f | ForEach-Object { "$($_.ScriptName):$($_.Line) $($_.RuleName)" }
        if ($f.Count -gt 0) { exit 1 }'
}

step fmt "$FMT_FATAL" cargo fmt --all -- --check
step clippy 1 cargo clippy --all-targets -- -D warnings
step test 1 env WINGMAN_FULL_SUITE=1 cargo test
step deny 1 cargo deny check
step hooks 1 python -m unittest discover -s scripts/hooks/tests -t .
step pester 1 pester
step pssa 1 pssa

grep -h 'test result' "$LOG_DIR/test.log" | sed 's/^/      /'
exit $FAILED
