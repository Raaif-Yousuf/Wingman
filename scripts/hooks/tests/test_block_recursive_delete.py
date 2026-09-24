"""Tests for scripts/hooks/block_recursive_delete.py.

Issue #146: MSYS/Git-Bash-style absolute paths (`/c/Users/...`,
`/cygdrive/c/Users/...`) are not translated before `pathlib.Path.resolve()`,
so a recursive force-delete aimed inside the repo silently passes when the
target is spelled the Git-Bash way instead of the Windows way.

Run: python -m unittest discover scripts/hooks/tests
"""

from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys
import unittest

HOOKS_DIR = pathlib.Path(__file__).resolve().parent.parent
HOOK_PATH = HOOKS_DIR / "block_recursive_delete.py"

_spec = importlib.util.spec_from_file_location("block_recursive_delete", HOOK_PATH)
m = importlib.util.module_from_spec(_spec)
assert _spec.loader is not None
_spec.loader.exec_module(m)

# A real path inside THIS worktree's repo root, used for every "inside" case.
INSIDE_TARGET = m.REPO_ROOT / "target"
# A real path that is definitely not under this worktree's repo root.
OUTSIDE_TARGET = pathlib.Path(m.REPO_ROOT.drive + "/") / "not-this-repo-at-all"


def _to_msys(path: pathlib.Path) -> str:
    """`C:/Users/raaif/...` -> `/c/Users/raaif/...`, the way Git Bash spells it."""
    posix = path.as_posix()
    drive, rest = posix.split(":", 1)
    return f"/{drive.lower()}{rest}"


def _to_cygdrive(path: pathlib.Path) -> str:
    posix = path.as_posix()
    drive, rest = posix.split(":", 1)
    return f"/cygdrive/{drive.lower()}{rest}"


def _to_backslash(path: pathlib.Path) -> str:
    return str(path)


def _to_forward(path: pathlib.Path) -> str:
    return path.as_posix()


class MsysPathTranslationTests(unittest.TestCase):
    """AGENTS.md rule 14: the Bash tool here IS Git Bash, so `/c/...` is the
    idiomatic way an agent writes an absolute path, not an edge case."""

    def test_msys_style_path_is_blocked(self):
        target = _to_msys(INSIDE_TARGET)
        cmd = f"rm -rf {target}"
        self.assertIsNotNone(m.verdict(cmd), f"MSYS path not blocked: {cmd!r}")

    def test_cygdrive_style_path_is_blocked(self):
        target = _to_cygdrive(INSIDE_TARGET)
        cmd = f"rm -rf {target}"
        self.assertIsNotNone(m.verdict(cmd), f"cygdrive path not blocked: {cmd!r}")

    def test_windows_backslash_path_is_blocked(self):
        target = _to_backslash(INSIDE_TARGET)
        cmd = f"rm -rf {target}"
        self.assertIsNotNone(m.verdict(cmd), f"backslash path not blocked: {cmd!r}")

    def test_windows_forward_slash_path_is_blocked(self):
        target = _to_forward(INSIDE_TARGET)
        cmd = f"rm -rf {target}"
        self.assertIsNotNone(m.verdict(cmd), f"forward-slash path not blocked: {cmd!r}")

    def test_mixed_slashes_path_is_blocked(self):
        forward = _to_forward(INSIDE_TARGET)
        # Flip every other slash to a backslash to build a mixed-separator path.
        mixed_chars = list(forward)
        for i, ch in enumerate(mixed_chars):
            if ch == "/" and i % 4 == 0:
                mixed_chars[i] = "\\"
        mixed = "".join(mixed_chars)
        cmd = f"rm -rf {mixed}"
        self.assertIsNotNone(m.verdict(cmd), f"mixed-slash path not blocked: {cmd!r}")

    def test_quoted_msys_path_is_blocked(self):
        target = _to_msys(INSIDE_TARGET)
        cmd = f'rm -rf "{target}"'
        self.assertIsNotNone(m.verdict(cmd), f"quoted MSYS path not blocked: {cmd!r}")

    def test_tilde_relative_msys_path_is_blocked(self):
        # ~/copilot-ask-style path under the user's home directory: build one
        # that is genuinely inside REPO_ROOT via the home dir if REPO_ROOT is
        # itself under the home dir, otherwise this is skipped, it's still a
        # real-world spelling worth covering when the repro machine allows it.
        home = pathlib.Path.home()
        try:
            rel = m.REPO_ROOT.relative_to(home)
        except ValueError:
            self.skipTest("REPO_ROOT is not under the home directory on this machine")
            return
        cmd = f"rm -rf ~/{rel.as_posix()}/target"
        self.assertIsNotNone(m.verdict(cmd), f"~-relative path not blocked: {cmd!r}")

    def test_msys_style_path_OUTSIDE_repo_is_not_blocked(self):
        target = _to_msys(OUTSIDE_TARGET)
        cmd = f"rm -rf {target}"
        self.assertIsNone(m.verdict(cmd), f"outside-repo MSYS path wrongly blocked: {cmd!r}")

    def test_msys_style_path_all_flag_spellings(self):
        target = _to_msys(INSIDE_TARGET)
        for flags in ("-rf", "-fr", "-r -f", "--recursive --force"):
            with self.subTest(flags=flags):
                cmd = f"rm {flags} {target}"
                self.assertIsNotNone(m.verdict(cmd), f"not blocked: {cmd!r}")

    def test_powershell_remove_item_msys_style_path_is_blocked(self):
        # Remove-Item wouldn't really be given an MSYS path in practice, but
        # the translation must not be rm-specific; prove it applies to the
        # PowerShell branch too.
        target = _to_msys(INSIDE_TARGET)
        cmd = f"Remove-Item -Recurse -Force {target}"
        self.assertIsNotNone(m.verdict(cmd), f"not blocked: {cmd!r}")


class QuotedProseFalsePositiveTests(unittest.TestCase):
    """Issue #148: a bare `(` in prose, inside a quoted string such as a
    `gh issue create --body "..."` argument, satisfies `_CMD_POS`'s
    `[;&|\\n(]` alternation and is wrongly treated as command position, so
    the whole sentence is blocked as if it were a real destructive command.
    Fix must make quoted-string contents not count as command position
    without reopening #146 (a real subshell-wrapped `rm`, or a real command
    whose own target argument happens to be quoted, must still be caught)."""

    def test_prose_paren_inside_quoted_gh_body_is_not_blocked(self):
        target = _to_msys(INSIDE_TARGET)
        cmd = (
            'gh issue create --title "x" --body '
            f'"the docstring explains (rm -rf {target} as an example) '
            'of what not to do"'
        )
        self.assertIsNone(m.verdict(cmd), f"prose false-positive still blocked: {cmd!r}")

    def test_prose_semicolon_inside_quoted_body_is_not_blocked(self):
        # The separator sits BEFORE `rm` here (not after), so it is the same
        # command-position false positive as the `(` case, just spelled with
        # `;` instead.
        target = _to_msys(INSIDE_TARGET)
        cmd = (
            'gh issue create --title "x" --body '
            f'"first clean it up; rm -rf {target} was suggested and rejected"'
        )
        self.assertIsNone(m.verdict(cmd), f"prose false-positive still blocked: {cmd!r}")

    def test_real_subshell_wrapped_rm_still_blocked(self):
        # No quotes anywhere: the leading `(` really is a subshell open, not
        # prose punctuation, and must still be caught (guards #146).
        target = _to_msys(INSIDE_TARGET)
        cmd = f"true && (rm -rf {target})"
        self.assertIsNotNone(m.verdict(cmd), f"real subshell rm not blocked: {cmd!r}")

    def test_real_rm_with_quoted_target_still_blocked(self):
        # The DANGEROUS command's own target is quoted (not prose describing
        # it). Neutralising separators inside quotes must not blank out the
        # target text itself, or this would reopen #146 via quoting.
        target = _to_msys(INSIDE_TARGET)
        cmd = f'rm -rf "{target}"'
        self.assertIsNotNone(m.verdict(cmd), f"quoted-target rm not blocked: {cmd!r}")

    def test_prose_mentioning_command_outside_any_quotes_is_still_blocked(self):
        # #148 is scoped to QUOTED prose. A bare, unquoted mention with a
        # real command-position `(` in front of it is unchanged behaviour
        # (still a known, documented false-positive shape outside the scope
        # of this fix) -- this test just pins today's behaviour so a future
        # change to this exact case is a deliberate decision, not a surprise.
        target = _to_msys(INSIDE_TARGET)
        cmd = f"notes: explains (rm -rf {target} as an example) done"
        self.assertIsNotNone(m.verdict(cmd))


class EndToEndStdinStdoutProtocolTests(unittest.TestCase):
    """Pipe realistic PreToolUse JSON through the script exactly the way
    Claude Code invokes it, and check the real exit code and stdout."""

    def _run(self, command: str) -> subprocess.CompletedProcess:
        payload = {
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        }
        return subprocess.run(
            [sys.executable, str(HOOK_PATH)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_msys_recursive_delete_is_denied_end_to_end(self):
        target = _to_msys(INSIDE_TARGET)
        result = self._run(f"rm -rf {target}")
        self.assertEqual(result.returncode, 0, result.stderr)
        out = json.loads(result.stdout)
        self.assertEqual(
            out["hookSpecificOutput"]["permissionDecision"],
            "deny",
            result.stdout,
        )

    def test_safe_command_is_silent_and_exits_zero(self):
        result = self._run("git status --short")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "", result.stdout)


if __name__ == "__main__":
    unittest.main()
