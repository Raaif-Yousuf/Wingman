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


class Issue279BypassTests(unittest.TestCase):
    """#279 (P1, OBSERVED): the auditor ran seven payloads through the hook.
    Bare `cargo test` was blocked, but all seven of these passed through
    because the regex required `cargo` in strict shell command-position and
    the subcommand to be exactly `test`. Each must now be blocked."""

    def test_time_prefixed_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("time cargo test"))

    def test_nice_prefixed_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("nice cargo test"))

    def test_env_assignment_prefixed_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("env FOO=1 cargo test"))

    def test_cargo_nextest_run_with_no_filter_is_blocked(self):
        self.assertIsNotNone(m.verdict("cargo nextest run"))

    def test_powershell_command_wrapped_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(
            m.verdict('powershell -Command "cargo test"')
        )

    def test_bash_c_wrapped_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict('bash -c "cargo test"'))

    # A couple of neighbouring shapes the auditor did not list but which
    # share the same bypass mechanism.
    def test_nohup_prefixed_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict("nohup cargo test"))

    def test_pwsh_command_wrapped_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict('pwsh -Command "cargo test"'))

    def test_sh_c_wrapped_bare_cargo_test_is_blocked(self):
        self.assertIsNotNone(m.verdict('sh -c "cargo test"'))

    def test_env_and_time_stacked_is_blocked(self):
        self.assertIsNotNone(m.verdict("env FOO=1 time cargo test"))


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

    # -- #279: the fix for the bypasses above must not swallow legitimate
    # filtered/wrapped/quoted-mention commands.

    def test_time_prefixed_filtered_cargo_test_is_allowed(self):
        self.assertIsNone(m.verdict("time cargo test config"))

    def test_nice_prefixed_cargo_build_is_allowed(self):
        self.assertIsNone(m.verdict("nice cargo build"))

    def test_env_assignment_prefixed_filtered_cargo_test_is_allowed(self):
        self.assertIsNone(m.verdict("env FOO=1 cargo test config"))

    def test_cargo_nextest_run_with_filter_is_allowed(self):
        self.assertIsNone(m.verdict("cargo nextest run some_filter"))

    def test_cargo_nextest_list_is_allowed(self):
        # `nextest list` does not execute the suite.
        self.assertIsNone(m.verdict("cargo nextest list"))

    def test_powershell_command_wrapped_filtered_cargo_test_is_allowed(self):
        self.assertIsNone(
            m.verdict('powershell -Command "cargo test config"')
        )

    def test_bash_c_wrapped_cargo_build_is_allowed(self):
        self.assertIsNone(m.verdict('bash -c "cargo build"'))

    def test_grep_for_the_string_cargo_test_is_allowed(self):
        # A false positive here would block ordinary code search.
        self.assertIsNone(m.verdict('grep -rn "cargo test" scripts/hooks/'))

    def test_commit_message_mentioning_cargo_test_is_allowed(self):
        self.assertIsNone(
            m.verdict(
                'git commit -m "Document why bare cargo test is blocked"'
            )
        )

    def test_commit_message_mentioning_cargo_test_bypass_is_allowed(self):
        self.assertIsNone(
            m.verdict(
                'git commit -m "Fix #279: cargo test bypass via time/nice/env"'
            )
        )


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

    def test_override_still_works_stacked_with_a_time_wrapper(self):
        # The #279 fix must not make the override harder to use for the
        # orchestrator's own legitimate full-suite run.
        self.assertIsNone(m.verdict("WINGMAN_FULL_SUITE=1 time cargo test"))

    def test_override_still_works_for_nextest(self):
        self.assertIsNone(
            m.verdict("WINGMAN_FULL_SUITE=1 cargo nextest run")
        )


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

    def test_bash_c_wrapped_bare_cargo_test_is_denied_end_to_end(self):
        result = self._run('bash -c "cargo test"')
        self.assertEqual(result.returncode, 0, result.stderr)
        out = json.loads(result.stdout)
        self.assertEqual(
            out["hookSpecificOutput"]["permissionDecision"],
            "deny",
            result.stdout,
        )

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
