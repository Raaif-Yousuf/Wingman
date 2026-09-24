#!/usr/bin/env bash
# Merge the branches a fan-out of worktree agents left behind, one at a time,
# in an order that puts the least-contended branches first.
#
# Why a script: after a 10-agent night there are ten branches, most of which
# touch two or three of the same files (app.rs, config.rs, ui/tray.rs). Merging
# them by hand loses track of which ones went in clean, which conflicted, and
# which were merged but never re-verified. This keeps that bookkeeping and
# leaves the repo on a known state whatever happens.
#
# It is deliberately conservative:
#   - a conflicting merge is aborted, not resolved, and reported for a human
#   - every successful merge is followed by a build + clippy + fmt check
#   - the FULL test suite runs once at the end, never per merge, because
#     parallel full runs have exhausted this machine's RAM before
#     (see .claude/skills/filing-findings/SKILL.md)
#
# Usage:
#   scripts/merge-agent-branches.sh                 # merge every unmerged agent branch
#   scripts/merge-agent-branches.sh --dry-run       # say what it would do, change nothing
#   scripts/merge-agent-branches.sh br1 br2 br3     # merge exactly these, in this order
#
# Exit code is 0 only if every attempted merge landed and the final gate passed.

set -u
cd "$(dirname "$0")/.."

DRY=0
if [ "${1:-}" = "--dry-run" ]; then DRY=1; shift; fi

export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

note() { printf '%s\n' "$*"; }

# Refuse to start from a dirty tree or a half-finished merge: both make the
# per-branch bookkeeping below meaningless.
if [ -e .git/MERGE_HEAD ]; then
    note "FAIL  a merge is already in progress. Finish or 'git merge --abort' it first."
    exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
    note "FAIL  working tree is not clean. Commit or deal with it first:"
    git status --short | sed 's/^/      /'
    exit 1
fi

START_SHA="$(git rev-parse --short HEAD)"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
note "on $BRANCH at $START_SHA"

# Which branches to merge.
if [ "$#" -gt 0 ]; then
    BRANCHES="$*"
else
    BRANCHES=""
    for b in $(git for-each-ref --format='%(refname:short)' 'refs/heads/*agent-*'); do
        [ "$b" = "$BRANCH" ] && continue
        # Skip branches with nothing new; a fan-out always leaves a few of those
        # behind from agents that filed issues instead of writing code.
        if [ -n "$(git log --oneline "$BRANCH..$b")" ]; then
            BRANCHES="$BRANCHES $b"
        fi
    done
fi

if [ -z "${BRANCHES// /}" ]; then
    note "nothing to merge"
    exit 0
fi

note ""
note "branches with unmerged commits, and the files each touches:"
for b in $BRANCHES; do
    n="$(git log --oneline "$BRANCH..$b" | wc -l | tr -d ' ')"
    files="$(git diff --name-only "$BRANCH...$b" | tr '\n' ' ')"
    printf '  %-40s %2s commits  %s\n' "$b" "$n" "$files"
done

if [ "$DRY" = 1 ]; then
    note ""
    note "(dry run: nothing merged)"
    exit 0
fi

MERGED=""
CONFLICTED=""
BROKEN=""

for b in $BRANCHES; do
    note ""
    note "=== $b"
    if ! git merge --no-ff --no-edit "$b" >/tmp/merge.$$.log 2>&1; then
        note "CONFLICT in:"
        git diff --name-only --diff-filter=U | sed 's/^/      /'
        git merge --abort
        CONFLICTED="$CONFLICTED $b"
        note "  aborted and left for a human. Merge it by hand, then re-run."
        continue
    fi

    # A merge that compiles is not automatically correct, but a merge that does
    # not compile is definitely wrong, and finding that out ten merges later is
    # what makes a fan-out night expensive.
    if ! cargo build >/tmp/build.$$.log 2>&1; then
        note "  merged but DOES NOT BUILD:"
        grep -E '^error' /tmp/build.$$.log | head -8 | sed 's/^/      /'
        note "  rolling back this merge."
        git reset --hard HEAD~1 >/dev/null
        BROKEN="$BROKEN $b"
        continue
    fi
    if ! cargo fmt --all -- --check >/dev/null 2>&1; then
        note "  merged; fmt drift (fixing)"
        cargo fmt --all
        git commit -aqm "cargo fmt after merging $b"
    fi
    if ! cargo clippy --all-targets -- -D warnings >/tmp/clippy.$$.log 2>&1; then
        note "  merged but CLIPPY FAILS (left in place; fix before the final gate):"
        grep -E '^(error|warning)' /tmp/clippy.$$.log | head -8 | sed 's/^/      /'
    fi
    note "  merged  $(git rev-parse --short HEAD)"
    MERGED="$MERGED $b"
done

rm -f /tmp/merge.$$.log /tmp/build.$$.log /tmp/clippy.$$.log

note ""
note "merged:     ${MERGED:-none}"
note "conflicted: ${CONFLICTED:-none}"
note "rolled back (build failure): ${BROKEN:-none}"

if [ -z "${MERGED// /}" ]; then
    note "nothing landed; skipping the full gate"
    [ -n "${CONFLICTED// /}${BROKEN// /}" ] && exit 1
    exit 0
fi

note ""
note "running the full gate once (scripts/verify-all.sh)"
scripts/verify-all.sh
GATE=$?

note ""
note "$START_SHA -> $(git rev-parse --short HEAD)"
[ -n "${CONFLICTED// /}${BROKEN// /}" ] && exit 1
exit $GATE
