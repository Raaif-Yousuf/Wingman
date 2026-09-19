# Activation trust: who may make Wingman ask

Status: **draft, owner approval owed.** Written 2026-09-19 against commit
`a55ab42` to unblock issues **#236** and **#237**, both P1. No code was written
for either; CLAUDE.md rule 12 says an architectural change gets a spec first,
and both issues propose changing how the app decides that a request to act is
legitimate. That is architecture.

Related: [#236](https://github.com/Raaif-Yousuf/Wingman/issues/236),
[#237](https://github.com/Raaif-Yousuf/Wingman/issues/237),
[#101](https://github.com/Raaif-Yousuf/Wingman/issues/101) (daily cost budget),
[#106](https://github.com/Raaif-Yousuf/Wingman/issues/106) (egress log, landed),
[#166](https://github.com/Raaif-Yousuf/Wingman/issues/166) (manual checks owed).

---

## 1. What the two issues actually say

**#236.** `Wingman.Owner.Window.4d1b62f0` is a fixed window class name in a
public repo and `WM_APP_ACTIVATE` is `WM_APP + 6`. Any process in the same
interactive session can `FindWindowW` the class and `PostMessageW` that id.
The handler at `src/app.rs:3335` calls `app.ask()` with no check of the sender:
screenshot the active monitor, then in Cloud or Auto mode a billed request to
the configured provider. UIPI does not help, because it only blocks a lower
integrity level posting to a higher one, and the attacker here is an ordinary
same-user process.

**#237.** `MUTEX_NAME` is likewise a fixed public string. Any process can hold
`Local\Wingman.SingleInstance.4d1b62f0` first. Wingman then reads
`ERROR_ALREADY_EXISTS`, treats it as a duplicate launch, calls `poke_existing`,
finds no owner window, returns from the `let Ok(hwnd) = ... else { return; }`
and exits with no tray icon, no card and no log line. Every launch after that,
including autostart at every login, does the same. CLAUDE.md rule 7 says every
failure ends in a card; this one ends in nothing.

## 2. The finding that should stop someone building the wrong fix

Both issues suggest authenticating the sender. **Sender authentication does not
close either hole, and building it would be a week spent on a property the
threat model cannot have.**

The attacker in both issues is a process running as the same user, at the same
integrity level, in the same session. Windows gives that process everything
Wingman itself can reach: every file Wingman can read, every registry value,
every handle it can open by name, and the contents of Wingman's own address
space via `OpenProcess`/`ReadProcessMemory`. So:

- **A shared secret fails.** Any nonce Wingman generates at startup and
  publishes anywhere a genuine duplicate launch could read it, an attacker
  reads the same way. There is no place to put it that is readable by one and
  not the other.
- **A named pipe with `GetNamedPipeClientProcessId` plus an image-path check
  fails too, and this is the non-obvious one.** It genuinely proves the client
  is `wingman.exe` at the expected path. But the capability it is protecting
  is "cause the running instance to ask", and any same-user process can obtain
  that capability by simply launching `wingman.exe` itself, which is exactly
  what the duplicate-launch path is for. The check raises the attacker's cost
  from one `PostMessageW` to one `CreateProcess`, and changes nothing about
  what the user loses.
- **Code signing the client does not help** for the same reason.

The conclusion is uncomfortable but it is the honest one: **on Windows, a
same-user process can always make Wingman ask.** The defensible properties are
not authentication. They are:

1. **Bound the damage.** An attacker must not be able to run up an unbounded
   bill or drain the battery.
2. **Make it visible.** The user must be able to see that it happened.
3. **Make it attributable.** The record must distinguish an ask caused by a
   key press from an ask caused by an activation message.
4. **Never let it stop the app from starting** (that is #237's half).

Everything below follows from those four.

## 3. #237: the silent exit, and the reordering that fixes it properly

There is a second, benign instance of the same bug already recorded in
`NEXT_SESSION.md`: `app::run` takes the mutex **before** the owner window
exists, so a duplicate launch that arrives during that gap finds the name held
and no window to poke, and exits silently with no adversary anywhere. The
adversarial case and the race case have one cause, and one fix.

**Proposal: create the owner window first, take the mutex second.**

```
CreateWindowExW(owner window)      // cheap, invisible, message-only
match acquire() {
    First(lock) => carry on with this window,
    Already     => poke_existing(activation); DestroyWindow(ours); exit,
}
```

Then "the name is held" implies "a window with our class exists", because the
only process that holds the name is one that created its window first. The
`let Ok(hwnd) = ... else { return; }` branch stops being reachable in the
normal case, and becomes a real signal when it does fire.

**What a duplicate must do when the name is held but no window answers.** This
is now exactly the adversarial case, so:

- Retry `FindWindowW` for a bounded window (proposal: 2 s, 50 ms apart) to
  absorb a teardown race where a genuine instance is exiting.
- If still nothing: **start anyway, without the lock.** A process squatting our
  name must not be able to prevent the app from running, and the thing the lock
  guards against (two tray icons, two hooks, two billed calls per press) is
  defined by a live instance, which by construction is not there.
- Surface it. A card on that path (CLAUDE.md rule 7) plus a diagnostics entry
  naming the condition: the name is held by a process that is not Wingman.

Tradeoff, stated plainly so the owner can reject it: if a genuine instance is
ever alive for more than 2 s with the mutex held and no owner window, this
starts a second instance. The reordering above is what makes that condition
impossible in our own code; the risk is that some future change reintroduces a
gap. A test that asserts the ordering (window created before `acquire` is
called) is cheap and is listed in § 6.

## 4. #236: bound, see, attribute

Three changes, none of which pretends to authenticate anyone.

**4.1 Separate the trusted path from the untrusted one.** Today
`WM_APP_HOTKEY` and `WM_APP_ACTIVATE` both land on a bare `app.ask()`.
`WM_APP_HOTKEY` is posted by our own low-level keyboard hook inside this
process and is trustworthy in a way `WM_APP_ACTIVATE` is not. Give `ask` a
trigger argument (`Trigger::Hotkey`, `Trigger::Activation`, `Trigger::Tray`,
`Trigger::Palette`) rather than leaving the two indistinguishable. Everything
else in this section needs that argument to exist.

**4.2 Rate-limit activations, not key presses.** A minimum interval between two
`Trigger::Activation` asks. A human double-launching the app does not need
sub-second repeats; an attacker needs exactly that. Proposal: one activation
ask per 3 s, and a further cap of N per hour, both configurable and both
applying only to `Trigger::Activation`. A suppressed activation is not silent:
it increments a counter the diagnostics report shows.

**4.3 Record the trigger in the egress log.** #106 landed the log. Adding the
trigger to each entry is what turns "my bill is higher than I expected" into
"forty of these were activations and I pressed the key twice". This is the
cheapest of the three and the one that makes the other two auditable.

**4.4 The real backstop is #101.** A daily cost budget with a hard stop bounds
the damage from *every* cause, not just this one, and it is already an open
issue. This spec should raise its priority rather than duplicate it.

**Explicitly rejected:** a confirmation prompt before an activation ask. It
would land on the Copilot key's own path the first time someone reorders the
code, and "one press, one action, one card" is the product.

## 5. Threat model, written down so it stops being re-argued

| Attacker | Can they make Wingman ask? | Mitigation |
|---|---|---|
| Remote, no code on the machine | No | Not reachable; no listening socket |
| Another user on the machine | No | `Local\` namespace and the window station are per-session |
| Lower-integrity process (sandboxed app) | No | UIPI blocks the post |
| **Same user, same integrity** | **Yes, unavoidably** | § 4: bound, see, attribute |
| Same user, elevated | Yes | Out of scope; already owns the machine |

The fourth row is the one that matters, and it is the row where "authenticate
the sender" is a category error rather than an unfinished feature.

## 6. What "done" looks like

Each is one observable, per CLAUDE.md rule 8.

1. A unit test asserts the owner window handle exists before
   `single_instance::acquire` is called in `app::run` (source-scanning in the
   style of `every_tray_cmd_id_has_a_wnd_proc_arm` if a runtime assertion is
   awkward).
2. `poke_existing` returns an outcome rather than `()`, and a test covers
   window-found, window-appears-on-retry, and never-appears.
3. A manual check on #166: hold the mutex name from a separate process, launch
   Wingman, observe that it starts and shows a card naming the condition.
4. A manual check on #166: `PostMessageW` `WM_APP_ACTIVATE` in a loop from a
   separate process, observe that at most one ask per interval fires and that
   the diagnostics report shows the suppressed count.
5. An egress-log entry carries its trigger, asserted by a unit test over the
   log's serialization.

## 7. Decisions owed from the owner

1. Approve or reject § 3's "start anyway without the lock" when the name is
   held and no window answers after 2 s. This is the only user-visible
   behaviour change with a real downside.
2. Confirm the § 4.2 numbers (3 s, and the hourly cap) or replace them.
3. Confirm that authentication is being dropped as a goal (§ 2), so that #236
   is not later reopened asking for a named pipe.
4. Say whether #101 should be pulled forward as the backstop § 4.4 assumes.
