"""PreToolUse hook: refuse an unfiltered `cargo test` / `cargo test --release`.

`.claude/skills/filing-findings/SKILL.md` section 5 is binding on every agent
in this repo: "Never run bare `cargo test` or `cargo test --release`. Run the
tests you touched." Several agents build in parallel on this machine, and an
unbounded, unfiltered test run has frozen it by exhausting RAM. Until this
hook existed, nothing enforced that rule; `.claude/settings.json` even
pre-authorized the bare command (issue #153).

WHAT IT BLOCKS
--------------
`cargo test` (or `cargo test --release`, `-- --nocapture`, `--all`,
`--workspace`, any combination) with NO test-name filter: no bare positional
token before a top-level `--`, and no `--test`/`--bin`/`--example`/`--bench`
narrowing the run to one target. Also `cargo nextest run` with no filter
(nextest's equivalent full-suite run).

`cargo` (or `cargo nextest run`) is recognised even when it is not the very
first thing on the line, as long as reaching it does not require crossing
another *executed* command's own argument boundary:
- behind a transparent wrapper (`time`, `nice`, `nohup`, `ionice`, `exec`,
  `command`) and/or leading `VAR=value` environment assignments, e.g.
  `time cargo test`, `env FOO=1 cargo test`.
- inside a quoted string handed to a shell for execution: `bash -c "..."`,
  `sh -c "..."`, `zsh -c "..."`, `powershell -Command "..."`,
  `pwsh -Command "..."`. The quoted string is scanned recursively with the
  same rule, so a filter inside the quotes (`bash -c "cargo test config"`)
  is still allowed.

#279 (P1, OBSERVED): an audit of seven payloads found `time cargo test`,
`nice cargo test`, `env FOO=1 cargo test`, `cargo nextest run`,
`powershell -Command "cargo test"` and `bash -c "cargo test"` all passed
through the original strict-command-position, `test`-subcommand-only regex.

WHAT IT DOES NOT BLOCK
-----------------------
- `cargo test <name>`, `cargo test --lib <module_path>`,
  `cargo test --test <name>`, `cargo nextest run <name>`: all narrow the run.
- `cargo nextest list` (and any other cargo/nextest subcommand besides
  `test`/`nextest run`): does not execute the suite.
- Any other cargo subcommand (`build`, `clippy`, `fmt`, `deny`, ...), with or
  without a wrapper or quoting layer in front of it.
- The whole hook is bypassed when the command contains the explicit
  orchestrator override `WINGMAN_FULL_SUITE=1` (Bash env-assignment form or
  PowerShell `$env:WINGMAN_FULL_SUITE=1`), including when stacked with a
  wrapper (`WINGMAN_FULL_SUITE=1 time cargo test`).
- Prose that merely mentions the string, e.g. `grep -rn "cargo test" .` or a
  commit message quoting it: reaching `cargo` still requires the
  wrapper/quoting rule above, and a bare `grep`/`git commit` is neither a
  transparent wrapper nor a shell `-c`/`-Command` invocation, so the
  contents of an arbitrary quoted string argument are never scanned merely
  because they contain the word "cargo". `cat <<EOF` heredoc BODIES are also
  blanked before scanning, matching block_recursive_delete.py's own scoping.

CONTRACT
--------
Reads the PreToolUse payload on stdin, writes a JSON decision on stdout.
Silence plus exit 0 means "no opinion". It never blocks on its own failure: a
malformed payload or an unexpected exception exits 0 quietly, because a hook
that breaks the session when IT has a bug is worse than the bug it guards.
"""

from __future__ import annotations

import json
import re
import sys

# `cat <<DELIM ... DELIM` bodies are data, not shell. Same scoping rationale
# as block_recursive_delete.py: only `cat` heredocs are stripped, since a
# heredoc fed to an interpreter really is executed.
_CAT_HEREDOC_OPENER = re.compile(r"\bcat\b[^\n]*<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")

# Commands that pass their own argv through to the next command unchanged
# (no forking of the RAM cost, no argument-boundary crossing) -- reaching
# `cargo` behind one of these is still reaching it in command position.
_TRANSPARENT_WRAPPERS = r"(?:time|nice|nohup|ionice|exec|command|env)"

# Command position: start of line, or after a shell separator, optionally
# behind a run of inline environment assignments and/or transparent wrapper
# commands, in any order (`time cargo test`, `env FOO=1 cargo test`,
# `env FOO=1 time cargo test`, ...).
_CMD_POS = (
    r"(?:^|[;&|\n(]|&&|\|\|)\s*"
    r"(?:(?:[A-Za-z_][A-Za-z0-9_]*=\S*|" + _TRANSPARENT_WRAPPERS + r")\s+)*"
)

_CARGO = re.compile(_CMD_POS + r"cargo\b(?P<args>[^;&|\n]*)", re.IGNORECASE)

# A quoted command string handed to a shell for execution: `bash -c "..."`,
# `sh -c "..."`, `powershell -Command "..."`, `pwsh -Command "..."`. The
# quoted content is itself scanned with the same rules (recursively), since
# it is a real command line as far as the target shell is concerned -- not
# prose, and not a generic tool's string argument (e.g. `grep`'s pattern).
_NESTED_SHELL = re.compile(
    r"\b(?:bash|sh|zsh|dash|ksh)\s+-c\s+"
    r"|\b(?:powershell(?:\.exe)?|pwsh(?:\.exe)?)\s+-(?:Command|c)\s+"
    r"|\bcmd(?:\.exe)?\s+/[cC]\s+",
    re.IGNORECASE,
)
_QUOTED_STRING = re.compile(r"(['\"])(.*)\1")

# The explicit orchestrator override. Only the literal value "1" counts;
# WINGMAN_FULL_SUITE=0 (or any other value) is not an override.
_OVERRIDE = re.compile(r"(?:\$env:)?WINGMAN_FULL_SUITE\s*=\s*\"?1\"?\b")

# Flags that take a value and narrow the run to one target: presence alone
# (regardless of the value) counts as "filtered".
_NARROWING_VALUE_FLAGS = {"--test", "--bin", "--example", "--bench"}

# Flags that take a value but do NOT by themselves narrow to a specific
# test: skip both the flag and its value so the value token is never
# mistaken for a bare positional test-name filter.
_GENERIC_VALUE_FLAGS = {
    "--package", "-p", "--manifest-path", "--target", "--target-dir",
    "--features", "--jobs", "-j", "--color", "--message-format",
    "--profile", "--exclude", "--exclude-features",
}

MESSAGE = """A `cargo test` with no test-name filter is blocked.

Several agents build on this shared machine at once; an unfiltered run has
exhausted RAM and frozen it before (see filing-findings/SKILL.md section 5).

Run only the tests you touched instead:

    cargo test <module_or_test_name>
    cargo test --lib <module_path>::
    cargo test --test <integration_test_name>

Build with a capped job count and the shared target dir so builds queue
behind a file lock instead of running in parallel:

    export CARGO_TARGET_DIR=C:/Users/raaif/copilot-ask/target CARGO_BUILD_JOBS=4

The orchestrator's full-suite run sets WINGMAN_FULL_SUITE=1 on the command
to bypass this check; that override is not for individual agents to use."""


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


def _cargo_test_argv(args: str) -> list[str] | None:
    """Return the tokens after `test` (or after `nextest run`) if this cargo
    invocation runs the suite, skipping any leading global flags
    (`--offline`, `-q`, ...) or a toolchain selector (`+nightly`). Returns
    None for any other subcommand (including `nextest list` and friends,
    which do not execute the suite), or no subcommand at all."""
    tokens = args.split()
    i = 0
    n = len(tokens)
    while i < n:
        tok = tokens[i]
        if tok.startswith("+"):
            i += 1
            continue
        if tok.startswith("-"):
            i += 1
            continue
        if tok == "test":
            return tokens[i + 1:]
        if tok == "nextest":
            j = i + 1
            while j < n and tokens[j].startswith("-"):
                j += 1
            if j < n and tokens[j] == "run":
                return tokens[j + 1:]
            return None
        return None
    return None


def _is_filtered(tokens: list[str]) -> bool:
    i = 0
    n = len(tokens)
    while i < n:
        tok = tokens[i]
        if tok == "--":
            # Everything after this belongs to the test binary itself
            # (e.g. `-- --nocapture`), not to cargo's own filter.
            break
        if "=" in tok and tok.startswith("-"):
            name = tok.split("=", 1)[0]
            if name in _NARROWING_VALUE_FLAGS:
                return True
            i += 1
            continue
        if tok in _NARROWING_VALUE_FLAGS:
            return True
        if tok in _GENERIC_VALUE_FLAGS:
            i += 2
            continue
        if tok.startswith("-"):
            # A boolean flag that does not narrow the run on its own:
            # --release, --lib, --all, --workspace, -q, ...
            i += 1
            continue
        # A bare positional token before `--`: the test-name filter.
        return True
    return False


def _has_unfiltered_cargo_test(scanned: str) -> bool:
    for match in _CARGO.finditer(scanned):
        argv = _cargo_test_argv(match.group("args"))
        if argv is None:
            continue
        if not _is_filtered(argv):
            return True
    return False


def verdict(command: str) -> str | None:
    scanned = _strip_cat_heredoc_bodies(command or "")
    if _OVERRIDE.search(scanned):
        return None
    if _has_unfiltered_cargo_test(scanned):
        return MESSAGE
    # A command string handed to a shell for execution (`bash -c "..."`,
    # `powershell -Command "..."`) is a real command line to that shell, not
    # prose -- scan its contents the same way. Only the FIRST quoted string
    # after the wrapper is treated this way, which is exactly the argument
    # `-c`/`-Command` takes.
    for wrapper in _NESTED_SHELL.finditer(scanned):
        rest = scanned[wrapper.end():]
        quoted = _QUOTED_STRING.match(rest)
        if quoted and _has_unfiltered_cargo_test(quoted.group(2)):
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
                "systemMessage": "Blocked an unfiltered `cargo test` (shared-machine RAM rule).",
            },
            sys.stdout,
        )
    except Exception:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
