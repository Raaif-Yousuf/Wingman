"""Tests for scripts/hooks/block_unfiltered_cargo_test.py.

Issue #153: .claude/settings.json auto-allowed a bare `cargo test` (no
module/test-name filter), contradicting the shared-machine RAM rule in
`.claude/skills/filing-findings/SKILL.md` section 5 ("Never run bare
`cargo test`... several agents building in parallel exhaust RAM"). This
hook is the enforcement layer for that rule, the same way
block_recursive_delete.py and block_git_stash.py enforce theirs.

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
HOOK_PATH = HOOKS_DIR / "block_unfiltered_cargo_test.py"

_spec = importlib.util.spec_from_file_location(
    "block_unfiltered_cargo_test", HOOK_PATH
)
m = importlib.util.module_from_spec(_spec)
assert _spec.loader is not None
_spec.loader.exec_module(m)


class BlockedUnfilteredTests(unittest.TestCase):
    def test_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test"))

    def test_cargo_test_release_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test --release"))

    def test_cargo_test_dashdash_nocapture_no_filter_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test -- --nocapture"))

    def test_cargo_test_all_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test --all"))

    def test_cargo_test_workspace_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test --workspace"))

    def test_cargo_test_all_and_release_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test --all --release"))

    def test_chained_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo build && cargo test"))

    def test_powershell_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo test; Write-Host done"))


class AllowedFilteredTests(unittest.TestCase):
    def test_cargo_test_with_name_filter_is_allowed(self):
        self.assertIsNone(m.verdict("cargo test config"))

    def test_cargo_test_lib_with_module_path_filter_is_allowed(self):
        self.assertIsNone(m.verdict("cargo test --lib provider::"))

    def test_cargo_test_specific_integration_test_is_allowed(self):
        self.assertIsNone(m.verdict("cargo test --test foo"))

    def test_cargo_test_release_with_filter_is_allowed(self):
        self.assertIsNone(m.verdict("cargo test --release config"))

    def test_cargo_build_is_not_this_hooks_business(self):
        self.assertIsNone(m.verdict("cargo build"))

    def test_cargo_clippy_is_not_this_hooks_business(self):
        self.assertIsNone(m.verdict("cargo clippy --all-targets -- -D warnings"))

    def test_non_cargo_command_is_untouched(self):
        self.assertIsNone(m.verdict("git status --short"))


class OverrideTests(unittest.TestCase):
    """The orchestrator's explicit override for running the full suite."""

    def test_bare_cargo_test_with_env_override_is_allowed(self):
        self.assertIsNone(
            m.verdict("WINGMAN_FULL_SUITE=1 cargo test")
        )

    def test_cargo_test_all_with_env_override_is_allowed(self):
        self.assertIsNone(
            m.verdict("WINGMAN_FULL_SUITE=1 cargo test --all")
        )

    def test_export_form_override_is_allowed(self):
        self.assertIsNone(
            m.verdict("export WINGMAN_FULL_SUITE=1 && cargo test")
        )

    def test_powershell_env_override_is_allowed(self):
        self.assertIsNone(
            m.verdict('$env:WINGMAN_FULL_SUITE=1; cargo test')
        )

    def test_override_with_wrong_value_still_blocked(self):
        # WINGMAN_FULL_SUITE=0 is not the override; only exactly "1" is.
        self.assertIsNotNone(m.verdict("WINGMAN_FULL_SUITE=0 cargo test"))

    def test_unrelated_env_var_does_not_grant_override(self):
        self.assertIsNotNone(m.verdict("SOME_OTHER_VAR=1 cargo test"))


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

    def test_bare_cargo_test_is_denied_end_to_end(self):
        result = self._run("cargo test")
        self.assertEqual(result.returncode, 0, result.stderr)
        out = json.loads(result.stdout)
        self.assertEqual(
            out["hookSpecificOutput"]["permissionDecision"],
            "deny",
            result.stdout,
        )
        # The block message must tell the agent how to run a filtered test.
        reason = out["hookSpecificOutput"]["permissionDecisionReason"]
        self.assertIn("cargo test", reason)

    def test_filtered_cargo_test_is_silent_and_exits_zero(self):
        result = self._run("cargo test config")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "", result.stdout)

    def test_malformed_payload_exits_zero_quietly(self):
        result = subprocess.run(
            [sys.executable, str(HOOK_PATH)],
            input="not json",
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
