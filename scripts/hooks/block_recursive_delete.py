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
- Prose inside a quoted string (e.g. a `gh issue create --body "..."`
  argument) that parenthetically quotes a dangerous command as an example: a
  `;`, `&`, `|`, newline or `(` only counts as command position OUTSIDE
  quotes (issue #148). A real command's own quoted target argument is not
  affected by this.

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


def _neutralize_quoted_command_separators(command: str) -> str:
    """A shell separator character (`;`, `&`, `|`, newline, `(`) has no
    special meaning to the shell when it sits inside a quoted string: it is
    ordinary prose punctuation, e.g. inside a `gh issue create --body "..."`
    argument that parenthetically quotes a dangerous command as an example
    (issue #148). Neutralise ONLY those separator characters, and ONLY while
    inside a quoted span (single or double quotes tracked independently,
    honouring a backslash escape inside double quotes the way a real shell
    would). Everything else inside the quoted span is left untouched -- in
    particular the text of a REAL command's own quoted target argument
    (`rm -rf "/c/Users/x"`) survives byte-for-byte, so this cannot reopen
    #146 by letting a target hide inside quotes. A real, unquoted `(` (an
    actual subshell) is never touched, so a real `(rm -rf ...)` is still
    caught."""
    out: list[str] = []
    quote: str | None = None
    escaped = False
    for ch in command:
        if quote is not None:
            if escaped:
                out.append(ch)
                escaped = False
                continue
            if ch == "\\" and quote == '"':
                out.append(ch)
                escaped = True
                continue
            if ch == quote:
                quote = None
                out.append(ch)
                continue
            if ch in ";&|(\n":
                out.append(" ")
                continue
            out.append(ch)
            continue
        if ch in "'\"":
            quote = ch
        out.append(ch)
    return "".join(out)


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


# Git Bash / MSYS absolute paths: `/c/Users/...` or `/cygdrive/c/Users/...`.
# A single drive letter between two slashes (or after `/cygdrive/`) at the
# start of the string is the MSYS spelling of a Windows drive root.
_CYGDRIVE_ABS = re.compile(r"^/cygdrive/([A-Za-z])(/.*)?$")
_MSYS_ABS = re.compile(r"^/([A-Za-z])(/.*)?$")


def _translate_msys_path(target: str) -> str:
    """`pathlib.Path.resolve()` on Windows does not understand either MSYS
    spelling: it treats the leading `/` as the root of the CURRENT drive and
    the drive letter becomes a literal directory name, so `/c/Users/raaif/x`
    resolves to `C:\\c\\Users\\raaif\\x`, never under `REPO_ROOT`
    (MEASURED 2026-09-16: `pathlib.Path('/c/Users/raaif/copilot-ask/target').resolve()`
    == `WindowsPath('C:/c/Users/raaif/copilot-ask/target')`). Translate both
    MSYS spellings to a drive-rooted Windows path first. POSIX-only, since on
    a real POSIX filesystem `/c/...` is an ordinary absolute path and must be
    left alone."""
    if os.name != "nt":
        return target
    m = _CYGDRIVE_ABS.match(target)
    if m:
        drive, rest = m.group(1), m.group(2) or "/"
        return f"{drive}:{rest}"
    m = _MSYS_ABS.match(target)
    if m:
        drive, rest = m.group(1), m.group(2) or "/"
        return f"{drive}:{rest}"
    return target


def _is_inside_repo(target: str) -> bool:
    """A bare relative path counts as inside: the hook cannot know the shell's
    cwd and failing toward 'inside' is the protective direction. An absolute
    path is resolved and compared honestly, so the scratchpad stays allowed."""
    if not target or target.startswith("$") or "*" in target or "?" in target:
        return "*" not in target or not os.path.isabs(target)
    expanded = os.path.expandvars(os.path.expanduser(target))
    expanded = _translate_msys_path(expanded)
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
    scanned = _neutralize_quoted_command_separators(scanned)
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
