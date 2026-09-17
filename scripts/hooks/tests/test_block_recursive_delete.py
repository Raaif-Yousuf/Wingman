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
    """CLAUDE.md rule 15: the Bash tool here IS Git Bash, so `/c/...` is the
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
