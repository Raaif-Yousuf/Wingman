"""PreToolUse hook: refuse the mutating `git stash` subcommands.

Ported from the sibling CLAIR repo. The stash stack is shared across every
worktree and every concurrent Claude session of a repository, because they all
share one `.git`. Two agents' working sets swapped that way once, and the ban
was then broken five more times by agents who had it in their own brief. Every
one was a revert check, which is exactly what `fixing-a-bug` step 7 asks for,
and stash is the reflex answer. The message below is the replacement.

WHAT IT DOES NOT BLOCK
----------------------
`git stash list` and `git stash show` are read-only and allowed. Prose that
merely mentions the string is not matched: the pattern requires `git` in
command position, and `cat <<EOF` heredoc bodies are blanked first.

CONTRACT
--------
Reads the PreToolUse payload on stdin, writes a JSON decision on stdout.
Silence plus exit 0 means "no opinion". Never blocks on its own failure.
"""

from __future__ import annotations

import json
import re
import sys

MUTATING = ("push", "save", "pop", "apply", "drop", "clear", "create", "store", "branch")

# `git` in COMMAND position, optionally behind inline env assignments
# (`GIT_PAGER=cat git stash` walked through the first version of this pattern).
_STASH = re.compile(
    r"(?:^|[;&|\n]|&&|\|\|)\s*"
    r"(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*"
    r"git\b[^;&|\n]*?\bstash\b(?P<rest>[^;&|\n]*)",
    re.IGNORECASE,
)

_CAT_HEREDOC_OPENER = re.compile(r"\bcat\b[^\n]*<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")


def _strip_cat_heredoc_bodies(command: str) -> str:
    lines = command.split("\n")
    out: list[str] = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        opener = _CAT_HEREDOC_OPENER.search(line)
        out.append(line)
        i += 1
        if not opener:
            continue
        delim = opener.group(2)
        while i < n and lines[i].strip() != delim:
            out.append("")
            i += 1
        if i < n:
            out.append(lines[i])
            i += 1
    return "\n".join(out)


MESSAGE = """`git stash` is blocked in this repository. Use a patch file instead.

The stash stack is SHARED across every worktree and every concurrent Claude
session, because they all share one .git.

For a revert check, which is almost always why this comes up:

    git diff > "$SCRATCH/fix.patch"            # keep the fix (session scratchpad)
    git checkout -- <the SOURCE files only>    # keep your new tests
    cargo test <name>                          # must go RED
    git apply "$SCRATCH/fix.patch"             # restore
    git diff --stat                            # prove byte-exact

To park work and switch context, `git checkout -b <branch>` preserves
uncommitted changes in place. To keep a copy, copy the files.

`git stash list` and `git stash show` are read-only and are not blocked."""


def verdict(command: str) -> str | None:
    scanned = _strip_cat_heredoc_bodies(command or "")
    for match in _STASH.finditer(scanned):
        rest = match.group("rest").strip()
        if not rest:
            return MESSAGE
        word = rest.split()[0].lstrip("-")
        if word in MUTATING:
            return MESSAGE
        if word in ("list", "show"):
            continue
        # Unknown spelling after `stash`: fail toward refusing.
        return MESSAGE
    return None


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return 0
    try:
        command = (payload.get("tool_input") or {}).get("command") or ""
        reason = verdict(command)
        if reason is None:
            return 0
        json.dump(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                },
                "systemMessage": "Blocked a `git stash`; the shared stash stack is not safe here.",
            },
            sys.stdout,
        )
    except Exception:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
