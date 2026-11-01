# Set a reminder

Status: **proposed, 2026-11-01** (AGENTS.md rule 12: written before any code,
waits for the owner). Covers #74. Builds on the
[executor spec](2026-09-17-executor-design.md), the date resolver
(`src/actions/dates.rs`, `relative_dates.rs`, #336), the `calendar_add` executor
(`src/executors/calendar_add.rs`, `connectors/ics.rs`) and the single "Undo last
action" slot (PR #1310, not merged when this was written).

## The problem

"Remind me to call Dana tomorrow at 9" is a screen-to-action chore: read the text,
work out the moment, set something that fires later. Wingman already does the
first two for calendar events. A reminder is lighter than an event: no file, no
calendar app, one notification at one time.

## Where it is today

- `calendar_add` turns a proposal into an `.ics` file and opens it with the shell's
  handler. Its undo removes the file. Nothing in Wingman posts a Windows
  notification: `Windows.UI.Notifications` is not used anywhere in `src/`.
- `dates::find_at(text, today, now_minute, week)` resolves "next Tuesday", "in 2
  hours", "EOD" in code, with `Found { phrase, resolution, clamped, time }`. A time
  not written is never invented except "EOD" (17:00, flagged `assumed`).
- The package is a **sparse** MSIX (`packaging/AppxManifest.xml.in`, application id
  `Wingman`, identity `RaaifYousuf.Wingman`). A direct-path launch of the exe has no
  package identity unless started through the package (`ocr::has_package_identity`).
- Rule 5: nothing runs while idle. No polling timers.

## Decisions

- **Let Windows hold the schedule.** The executor creates a
  `ScheduledToastNotification` and adds it to the OS schedule
  (`ToastNotifier::AddToSchedule`). Windows fires it at the time even if Wingman has
  exited. Wingman runs no timer, no thread and no watcher for it, which is rule 5 met
  by construction, and a restart or crash cannot lose the reminder.
- **New executor `reminder`, `Effect::Writes`.** It changes OS state (a pending
  notification), so it needs the user's Enter (`auto_confirm_read_only` refuses it).
- **One catalogue action, `set-reminder`** ("Set a reminder"), proposal kind
  `reminder`, options-card `effect` = `preview`.
- **The model never does the date arithmetic.** The model reads the text and returns
  the phrase and the message. Code resolves the moment. See "Time".
- **Local time, one moment, no repeats.** No recurring reminders, no snooze, no
  actions on the toast in v1. The toast shows text only and activating it does nothing.

## The identity question (the one risk)

A toast needs an app identity (an AUMID) to belong to. What is known and unknown:

- THEORY (unverified): from a process launched **through the sparse package** (the
  Start menu, the Run-key autostart that uses the package path), the AUMID is the
  package family name plus the application id (`<PackageFamilyName>!Wingman`) and
  `ToastNotificationManager::CreateToastNotifierWithId(aumid)` works with no shortcut.
  The picker route already launches by AUMID (`single_instance.rs`).
- THEORY (unverified): from a **direct-path launch** (`cargo run`, a dev build, a
  portable copy with no identity), an unpackaged process needs a Start menu shortcut
  carrying `System.AppUserModel.ID`, and `CreateToastNotifierWithId` for an id with no
  registration may succeed but show nothing. Whether a scheduled toast differs from an
  immediate one is not known.
- THEORY (unverified): a process with no package identity can still schedule under the
  **installed package's** AUMID string, because the AUMID is only a name the shell
  resolves. Nothing is claimed about whether Windows accepts it.

**How to measure it (slice 1, before any executor is written).** A throwaway spike
behind an `#[ignore]`d test, like `ocr_live_recognizes_gdi_rendered_text`, in three
launch modes: (a) the plain test binary, no identity; (b) the installed exe started
through the package; (c) the installed exe started by direct path while the sparse
package is installed. In each, build a toast with a unique `Id`, call `AddToSchedule`
for now plus 30 seconds, and record: the `HRESULT` of each call, the `Setting()` value
(`NotificationSetting::Enabled` or a disabled reason), the count from
`GetScheduledToastNotifications()`, and whether a toast actually appeared. The result
goes into this spec as `MEASURED <date>:` and replaces the theories in place. If (c)
fails, a direct-path launch refuses with a card ("Reminders work when Wingman is
started from the Start menu") and nothing is scheduled; it never pretends.

Also measured in the spike: the minimum lead time `AddToSchedule` accepts (THEORY,
unverified: a few seconds), whether Focus Assist or Do Not Disturb defers the toast,
and the schedule cap (THEORY, unverified: very large, far above any use here).

## Time

- The model fills `{ phrase, message, at }` in that order (AGENTS.md rule 3: what the
  user wrote, then what to say, then its own guess last). `phrase` is the wording from
  the text ("tomorrow at 9", "in 2 hours"). `at` is a local ISO date and time, treated
  as a hint only.
- Code calls `dates::find_at` on `phrase` with today's local date and the current minute
  (read once on the main thread, as `resolve_dates` does). **When the resolver gives a
  date and a time, it wins over `at`.** If they disagree, the card shows the resolver's
  moment. If the phrase resolves to a date but no time was written, the time is not
  invented: the preview asks by showing a time field the user must fill (Edit), with
  09:00 offered as a visible default that still needs Enter. "EOD" is 17:00 and says so.
- A phrase the resolver cannot read falls back to `at`, and the card says "Time worked
  out by the model. Check it." A moment that is in the past, or less than the measured
  minimum lead time from now, is refused with a card, never scheduled for a wrong day.
  A moment more than 366 days away needs the user to edit it (THEORY, unverified: no
  hard OS limit assumed).
- Conversion from local time to the toast's UTC `DateTime` uses the Windows time zone
  API (`TzSpecificLocalTimeToSystemTime`). A time that does not exist (spring-forward
  gap) is moved forward to the first valid minute and the card says so; a repeated time
  (fall-back) uses the first occurrence and says so.

## The preview card

A `Preview` card, Do it and Edit (not key-pickable, not dismissed by click-away).

- Headline: "Remind you at 09:00 on Tue 3 Nov 2026?" The **exact local date and time,
  including the weekday and the year**, always, even for "in 2 hours".
- Detail: the message text (editable), then "in 17 hours 20 minutes" and, when a note
  applies, the note ("end of day taken as 17:00").
- Edit changes the message and the time (one field each). The edited time is re-parsed by
  the same resolver; an unparseable time keeps the card open with the reason.
- Enter schedules it. Esc or the Wingman key dismisses and nothing is scheduled.

## The executor

`src/executors/reminder.rs`: `execute(Confirmed<Value>)` parses `{ message, at_local,
id }`, and behind an injectable `Scheduler` trait (like `connectors/ics.rs`'s `Opener`
and `ClipboardAccess`) does the three Win32 steps, so no test touches the OS schedule:

1. `ToastNotificationManager::CreateToastNotifierWithId(aumid)`; refuse if `Setting()`
   is not enabled, with the reason as the card text (rule 7).
2. Build the toast XML from a fixed template with the message **escaped** (a model or
   page string must never inject XML), cut to 200 characters on a char boundary, no
   hyperlinks, no actions, no image, `scenario` left default.
3. `ScheduledToastNotification::CreateScheduledToastNotification(xml, at_utc)`, set `Id`
   to `wingman-<uuid>` (short, ASCII), `AddToSchedule`.

It runs on the `spawn_executor` worker, since WinRT and the shell can be slow. The
outcome card says "Reminder set for 09:00 on Tue 3 Nov 2026" and names Undo.

## Undo

`Undo::recording` through the single "Undo last action" slot (`Undo::remember`, PR
#1310). Its restore lists `GetScheduledToastNotifications()`, finds the toast with this
`Id` and calls `RemoveFromSchedule`. If it is not in the list (it already fired, or the
user cleared it), the undo says so ("That reminder already went off") and is not an error
card with a stack of detail. Only the latest action is undoable through the slot; an older
reminder is removed from Windows' own notification settings, and the card says nothing
about it.

## Privacy and egress

The message text comes from the screen, so it goes through the existing text redaction
before a cloud model sees it, and a returned message containing a redaction marker is
refused. Nothing is sent when the reminder fires or when it is scheduled. A toast text
appears on the lock screen under some settings (THEORY, unverified); the preview says
nothing about that, but Settings help text names it. The message is never written to the
diagnostics log, only "reminder scheduled".

## Failure cards

| Case | Result |
|---|---|
| No time found, or a time in the past | "I could not tell when. Edit the time." (an editable preview, never a silent guess) |
| Notifications turned off for Wingman | The `Setting()` reason as card text, nothing scheduled |
| Direct-path launch with no working identity | "Reminders work when Wingman is started from the Start menu." |
| `AddToSchedule` fails | Error card with the shell's reason |
| Undo after the toast fired | "That reminder already went off" |
| Paused, or a password manager in front | The existing gate cards |

## Testing

Pure logic is unit-tested (rule 8); tests come first.

- Time: the resolver wins over a disagreeing `at`; "tomorrow" with no time asks for one;
  "EOD" is 17:00 flagged `assumed`; a past moment and a too-near moment refuse; a spring
  gap and a fall-back repeat are handled and noted (a pure local-to-UTC planner over
  injected zone rules); a 31 Jan "in 1 month" clamp note is carried.
- Toast XML: a message with `<`, `&`, quotes and `]]>` is escaped; a multi-byte message is
  cut on a char boundary; the template has no `launch`, `activationType` or action
  elements (asserted by a string test).
- Executor with a fake `Scheduler`: a confirmed proposal schedules once with the planned
  UTC time and id; an unreadable proposal is a named error; disabled notifications refuse;
  `resolve("reminder")` is `Effect::Writes`; `auto_confirm_read_only` refuses it.
- Undo: removes by id; reports "already went off" when the id is absent; never removes a
  toast with a different id.
- Source-scan guard (CRLF-normalised, file set asserted non-empty): the reminder module
  has no `SetTimer`, no thread sleep loop, no `ureq`, and no `provider` import.
- The schema's property order (`phrase`, `message`, `at`) is asserted.
- Tests use no real notifier and no named kernel objects (rule 9).

### Manual checks (each names the observable that differs if wired to nothing)

1. **The identity spike** above, in all three launch modes. Observable: a toast appears
   at the scheduled time, and the recorded `HRESULT`s and counts replace the theories.
2. **A toast fires.** "Remind me in 2 minutes." Observable: the toast appears at the
   time shown on the card, with Wingman closed in between.
3. **Idle.** Observable: no Wingman timer or thread is alive between scheduling and the
   toast (Task Manager shows no CPU, and the tray app is idle, rule 5).
4. **Undo.** Observable: after Undo the toast does not appear, and
   `GetScheduledToastNotifications` no longer holds the id.
5. **Do Not Disturb.** Observable: with Focus on, record whether the toast is shown,
   deferred or dropped, and put it in Settings help text.
6. **DST.** On a test clock near a change, the card's time and the toast agree.

## Slices

1. **The spike and the measurement.** An `#[ignore]`d test, results into this spec. No
   product code.
2. **Pure core.** Time planning, toast XML builder, tests.
3. **Executor, action, cards, Undo.** Done when Manual checks 2 to 4 pass.
4. **Settings help text** for notifications and Do Not Disturb.

Size: needs the `Windows_UI_Notifications` and `Data_Xml_Dom` `windows` features
(check whether they are already enabled). THEORY (unverified): about 15 KB; the gate
is `perf-budgets.json`, and the PR records the measured size.

## Open questions for the owner

1. If a direct-path launch cannot schedule (measured), is refusing with a card acceptable, or should Wingman register a Start menu shortcut with an AUMID?
2. Should a reminder with no written time ask for one (recommended), or take 09:00 for you?
3. Message only, or also a link back to the page where it was set (the toast would need an activation, which this spec leaves out)?
4. Is local-time-only acceptable, with no repeats or snooze in v1?
5. Should "Set a reminder" instead add a calendar event with an alarm through `calendar_add`, and is that a better default?
6. Depends on PR #1310 for the Undo slot: merge first, or ship without tray Undo?
