"""PreToolUse hook: refuse a recursive force-delete aimed inside the repository.

Ported from the sibling CLAIR repo, where four agents in one night ran
`rm -rf` on a tracked directory after the ban had been written into their own
briefs. A rule broken by people who read it is not a documentation problem;
this hook is the enforcement, and the message it prints is the replacement.

WHAT IT BLOCKS
--------------
A delete that is BOTH recursive and forced, aimed at a path that resolves
inside this repository. Covers POSIX `rm` (`-rf`, `-fr`, `-r -f`,
`--recursive --force`) and PowerShell `Remove-Item -Recurse -Force` with its
aliases (`ri`, `rd`, `rmdir`, `del`, `erase`) and prefix-matched parameters.

WHAT IT DOES NOT BLOCK
----------------------
- Deleting specific files by name.
- A recursive delete aimed OUTSIDE the repository: the session scratchpad
  under `AppData/Local/Temp/claude/...`, `/tmp`, a system temp dir.
- `git clean`, which respects `.gitignore`.
- Prose that merely mentions the string: the pattern requires the command in
  command position, and `cat <<EOF` heredoc BODIES are blanked before scanning.

CONTRACT
--------
Reads the PreToolUse payload on stdin, writes a JSON decision on stdout.
Silence plus exit 0 means "no opinion". It never blocks on its own failure: a
malformed payload or an unexpected exception exits 0 quietly, because a hook
that breaks the session when IT has a bug is worse than the bug it guards.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent

# `cat <<DELIM ... DELIM` bodies are data, not shell. Scoped to `cat`
# specifically: a heredoc fed to an interpreter (`bash <<EOF`, `python <<EOF`)
# really is executed and blanking THAT body would let a real delete through.
_CAT_HEREDOC_OPENER = re.compile(r"\bcat\b[^\n]*<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")

# Command position: start of line, or after a shell separator, optionally
# behind a run of inline environment assignments.
_CMD_POS = r"(?:^|[;&|\n(]|&&|\|\|)\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*"

_RM = re.compile(_CMD_POS + r"(?:/usr/bin/)?rm\b(?P<args>[^;&|\n]*)", re.IGNORECASE)

_REMOVE_ITEM = re.compile(
    _CMD_POS + r"(?:Remove-Item|ri|rd|rmdir|del|erase)\b(?P<args>[^;&|\n]*)",
    re.IGNORECASE,
)

MESSAGE = """A recursive force-delete aimed inside this repository is blocked.

Delete the specific files you created, by name, then prove the tree is clean:

    rm <the exact files you wrote>
    git status --short          # must show nothing you did not intend

If you want a scratch directory that is genuinely yours to destroy, use the
session scratchpad your environment names (under AppData/Local/Temp/claude/...).
A recursive delete there is not blocked.

To discard uncommitted changes to tracked files, `git checkout -- <paths>` is
the right tool and is not blocked. To remove untracked files with .gitignore
respected, `git clean` is not blocked either. `cargo clean` is not blocked."""


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


def _split_args(args: str) -> list[str]:
    return [t.strip("'\"") for t in args.split() if t.strip("'\"")]


def _posix_rm_is_recursive_force(tokens: list[str]) -> bool:
    recursive = force = False
    for tok in tokens:
        if tok == "--":
            break
        if tok.startswith("--"):
            if tok == "--recursive":
                recursive = True
            elif tok == "--force":
                force = True
        elif tok.startswith("-") and len(tok) > 1:
            for ch in tok[1:]:
                if ch in "rR":
                    recursive = True
                elif ch == "f":
                    force = True
    return recursive and force


def _powershell_is_recursive_force(tokens: list[str]) -> bool:
    recursive = force = False
    for tok in tokens:
        if not tok.startswith("-"):
            continue
        name = tok.lstrip("-").split(":")[0].lower()
        if name and "recurse".startswith(name):
            recursive = True
        elif name and "force".startswith(name):
            force = True
    return recursive and force


def _targets(tokens: list[str]) -> list[str]:
    out: list[str] = []
    seen_ddash = False
    for tok in tokens:
        if tok == "--":
            seen_ddash = True
            continue
        if not seen_ddash and tok.startswith("-"):
            continue
        out.append(tok)
    return out


def _is_inside_repo(target: str) -> bool:
    """A bare relative path counts as inside: the hook cannot know the shell's
    cwd and failing toward 'inside' is the protective direction. An absolute
    path is resolved and compared honestly, so the scratchpad stays allowed."""
    if not target or target.startswith("$") or "*" in target or "?" in target:
        return "*" not in target or not os.path.isabs(target)
    expanded = os.path.expandvars(os.path.expanduser(target))
    path = pathlib.Path(expanded)
    posix_rooted = expanded.startswith("/") or expanded.startswith("\\")
    if not path.is_absolute() and not posix_rooted:
        return True
    try:
        resolved = path.resolve()
    except (OSError, ValueError):
        return False
    try:
        resolved.relative_to(REPO_ROOT)
        return True
    except ValueError:
        return False


def verdict(command: str) -> str | None:
    scanned = _strip_cat_heredoc_bodies(command or "")
    for pattern, is_rf in ((_RM, _posix_rm_is_recursive_force),
                           (_REMOVE_ITEM, _powershell_is_recursive_force)):
        for match in pattern.finditer(scanned):
            tokens = _split_args(match.group("args"))
            if not is_rf(tokens):
                continue
            targets = _targets(tokens)
            if not targets:
                continue
            if any(_is_inside_repo(t) for t in targets):
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
                "systemMessage": "Blocked a recursive force-delete inside the repo.",
            },
            sys.stdout,
        )
    except Exception:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
