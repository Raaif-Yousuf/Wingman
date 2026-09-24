#!/usr/bin/env bash
# List open pull requests with their CI and mergeable state, find every pair
# that touches the same file, and suggest a merge order (fewest overlaps
# first). For an orchestrator juggling several open PRs (agent fan-outs,
# reviewer queues) so it can land the least-contended ones first and expect
# the rest to need a rebase.
#
# Requires: gh (authenticated), jq. Everything gh returns is read with
# --jq / plain jq, so no other JSON tooling is needed.
#
# Usage:
#   scripts/pr-overlap.sh              # current repo (gh infers owner/repo)
#   scripts/pr-overlap.sh owner/repo   # a specific repo
#
# Output:
#   1. one line per open PR: number, author, CI conclusion, mergeable state,
#      changed file count
#   2. one line per pair of PRs that share at least one changed file, with
#      the shared files listed
#   3. a suggested merge order: PRs sorted by how many other open PRs they
#      overlap with, fewest first

set -euo pipefail

REPO="${1:-}"
GH_REPO_ARGS=()
if [ -n "$REPO" ]; then
    GH_REPO_ARGS=(--repo "$REPO")
fi

command -v gh >/dev/null 2>&1 || { echo "FAIL  gh is not installed" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "FAIL  jq is not installed" >&2; exit 1; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# One JSON object per open PR: number, author, mergeable state, and its own
# changed-file list. statusCheckRollup gives an overall CI conclusion without
# a second API call per PR.
gh pr list "${GH_REPO_ARGS[@]}" --state open --limit 200 \
    --json number,author,mergeable,statusCheckRollup,files \
    > "$WORKDIR/prs.json"

PR_COUNT=$(jq 'length' "$WORKDIR/prs.json")
if [ "$PR_COUNT" -eq 0 ]; then
    echo "No open pull requests."
    exit 0
fi

echo "## Open pull requests"
jq -r '
  .[] |
  [
    "#\(.number)",
    .author.login,
    ( [.statusCheckRollup[]?.conclusion // "PENDING"]
      | if any(. == "FAILURE" or . == "ERROR") then "FAIL"
        elif any(. == "PENDING" or . == "") then "PENDING"
        else "PASS" end ),
    (.mergeable // "UNKNOWN"),
    "\(.files | length) files"
  ] | @tsv
' "$WORKDIR/prs.json" | column -t -s "$(printf '\t')"

echo
echo "## Overlapping file changes"
FOUND_OVERLAP=0
while read -r a && read -r b; do
    [ -z "$a" ] && continue
    common=$(jq -n --slurpfile prs "$WORKDIR/prs.json" --arg a "$a" --arg b "$b" '
      ($prs[0][] | select(.number == ($a | tonumber)) | [.files[].path]) as $fa |
      ($prs[0][] | select(.number == ($b | tonumber)) | [.files[].path]) as $fb |
      ($fa - ($fa - $fb))
    ')
    count=$(jq 'length' <<<"$common")
    if [ "$count" -gt 0 ]; then
        FOUND_OVERLAP=1
        files=$(jq -r 'join(", ")' <<<"$common")
        echo "#$a <-> #$b ($count file(s)): $files"
    fi
done < <(
    jq -r '.[].number' "$WORKDIR/prs.json" | sort -n > "$WORKDIR/nums.txt"
    n=$(wc -l < "$WORKDIR/nums.txt")
    for ((i = 1; i <= n; i++)); do
        for ((j = i + 1; j <= n; j++)); do
            sed -n "${i}p" "$WORKDIR/nums.txt"
            sed -n "${j}p" "$WORKDIR/nums.txt"
        done
    done
)
[ "$FOUND_OVERLAP" -eq 0 ] && echo "(none)"

echo
echo "## Suggested merge order (fewest overlaps first)"
jq -r '
  . as $prs |
  [ $prs[] | . as $p |
    {
      number: $p.number,
      overlaps: (
        [ $prs[] | select(.number != $p.number) |
          select( ([$p.files[].path] - ([$p.files[].path] - [.files[].path])) | length > 0 )
        ] | length
      )
    }
  ] | sort_by(.overlaps) | .[] | "#\(.number)  (\(.overlaps) overlapping PR(s))"
' "$WORKDIR/prs.json"
