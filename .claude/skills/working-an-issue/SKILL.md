---
name: working-an-issue
description: Use before starting OR closing any GitHub issue in this repo. The sibling CLAIR repo repeatedly rebuilt work that had already shipped and repeatedly closed issues that were never done; both came from not asking the repository what it already knew. Also use when an issue cannot be closed because nobody wrote down what "done" means, or when an issue reads as an epic rather than a task.
---

# Working an issue

Two failures, both from the same missing step:

- Work rebuilt that had already shipped (six of nine issues in one sweep were
  already fixed and never marked).
- Issues closed on "the diff looks right" that were disproven the next day on a
  real build.

**Nobody asked the repository what it already knew.** It always knew.

## 1. Before you start: ask the commit graph, then the thread

```
git log --oneline --all -i --grep '#<n>\b'
git log --oneline --all -S'<a literal from the issue>'
gh issue view <n> --comments
```

Read the log before the issue body. The body describes what somebody wanted;
the log describes what happened; when they disagree the log is right. Then read
the comment thread, **newest first**: the thread is what has been learned since
the body was written, and it routinely supersedes it.

If the issue has a checklist, precheck each item separately. An issue can be
open at the issue level while item 3 shipped weeks ago.

## 2. Does the issue say what "done" means?

Most stale issues are not hard, they are unclosable. If there are no acceptance
criteria, supplying them is the work: do that first, in the body, and often you
will find they are already met.

A criterion is usable only if it is falsifiable: one command or one screen,
yes or no.

| Not a criterion | A criterion |
|---|---|
| "Ollama support works" | pressing the key with `mode = local` and Ollama stopped shows the card `Ollama is not running`; with it running, `ollama ps` shows the model loaded and the card carries the answer |
| "Offline mode is safe" | with `mode = offline` and a valid OpenAI key configured, a Wireshark capture during ten key presses shows zero packets to any non-loopback address |
| "The palette is fast" | palette visible within 100 ms of keydown, measured with the `--trace-timing` flag over 20 presses |

For an epic or container issue: enumerate what it contains, check each against
the code, then split into concrete children and close the container pointing at
them, or rescope the body to the single remaining item. A rescoped issue comes
out smaller and sharper than it went in.

## 3. Before you close: name the observable

Closing records that the code is believed done. Never close on "the diff looks
right". State three things in the closing comment, always:

1. **The commit sha(s)** that did it.
2. **The acceptance criteria** you are closing against.
3. **The one observable that would differ if the change were wired to
   nothing**, and that you checked it. See `wired-to-nothing`.

```
gh issue close <n> --reason completed --comment "$(cat "$SCRATCH/close.md")"
```

`gh issue close` has no `--comment-file`, and backticks inside a double-quoted
`--comment` get shell-substituted: write the body to a file first.

## 4. If the behaviour is unproven on a real build

Closing the issue and proving the behaviour are different statements. If the
proof needs a packaged install, a real Copilot key press, or a live model, put
the exact manual check and what passing looks like in the closing comment,
and add the `needs-manual-check` label. That label must drain: remove it when
verified, reopen the issue when it fails.

## Red flags

| Thought | What it actually means |
|---|---|
| "This looks like nobody started it" | Run the git log searches. Six of nine were already done. |
| "The diff is obviously right, close it" | Two issues were closed exactly that way and disproven the next day. |
| "I'll write the criteria after I build it" | Then nobody can tell whether you are finished. |
| "The tests pass, so it works" | No test has ever caught a wired-to-nothing bug. Name the observable. |
