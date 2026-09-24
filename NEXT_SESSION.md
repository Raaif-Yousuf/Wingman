# Handoff: 2026-09-24

Written 2026-09-24. Re-check `git log -1` before trusting any sha here.

## Housekeeping done

All 62 `status:unpushed` issues were confirmed present on `origin/master`
(their commits reached GitHub via PR #384). The label was removed from each
with a comment naming the commit that carries it. None have been reopened.

## Open PRs awaiting the owner's merge

CI is green on all seven (fmt, clippy, test, release build; cargo deny,
gitleaks; Python hook tests; Pester; PSScriptAnalyzer). Suggested merge
order: #386, #387, #396, then #389, #391, #388; #394 last, after the
Settings-fits decision below.

- **#386**, AGENTS.md as the single agent instructions file. Docs only.
- **#387**, fixes #322: `SHA256SUMS.txt` published on releases. The real
  check is the next tagged release: confirm the asset is present, has LF
  endings, and `Get-FileHash` matches.
- **#388**, fixes #347: the first-run card opens Settings. Verified only by
  a PostMessage round-trip test, not seen on screen, because `config.rs`
  resolves the config path with `SHGetKnownFolderPath` and has no env
  override, so a no-provider run needs the real config moved aside first.
  Owner check: move `%APPDATA%\Wingman\config.toml` aside, press the key,
  confirm the card reads "No AI model set up yet", click it, confirm
  Settings opens, then restore the config.
- **#389**, fixes #357: palette mouse hover and click. Seen on screen
  2026-09-24 when merged locally onto #396: hover highlights rows, clicking
  "Copy text from screen" runs it. Depends on #396 to be visible at all;
  expect a trivial `use`-list conflict in `src/ui/palette.rs` when the
  second of the two merges.
- **#391**, fixes #350: preview edit fields dark in dark mode. Seen on
  screen (screenshot) 2026-09-24. The palette's search box has the same
  white-edit bug (noted on #350) and is not fixed by this PR.
- **#394**, fixes #340: Settings Save/Cancel no longer overlap the Prompt
  box. Caution: on the owner's display (1280x800 logical at 250%) the
  content needs about 640dp but only about 300dp is available, so the
  footer is now clipped below the window instead of overlapping, and Save
  may be unreachable. Needs a scrollable or resizable Settings (or #342)
  before or with merge. Owner decision.
- **#396**, fixes #395 (new P1 filed today): the Quick Ask palette had
  opened at 0x0 since it shipped (`SetWindowPos` without
  `SWP_NOMOVE|SWP_NOSIZE` in `PaletteInner::show`). Seen on screen after
  the fix.

## New issues filed today

- **#390**: calendar conflict check. Card names the clash, offers Book
  anyway / Edit. Depends on #54/#55.
- **#392**: owner UI rule, no Cancel button on a card where nothing has
  been done.
- **#393**: Wingman-key picks options. Dynamic default: a card with options
  takes a single press as option 1, a double press as option 2; a card
  without options dismisses on a press; no card at all makes a single press
  look and a double press repeat the last action. Settings gets Dynamic /
  Always pick / Always close. Conflicts with #324's click-away rule; needs
  settling in the spec.
- **#395**: the palette 0x0 bug (fixed by #396 above).

## Lesson (MEASURED 2026-09-24)

Screenshots taken from agent sessions do capture the real desktop
(PowerShell `CopyFromScreen`, with the process made DPI aware). Earlier
"windows invisible to screenshots" reports were wrong: the owner was
clicking the windows away. GDI `GetDIBits` captures can come back with
alpha 0 and look blank until made opaque. Wingman is single-instance, so
agents must check `tasklist` before launching a debug exe.

## Next up

- Owner merges the seven PRs above, in the suggested order.
- The palette search box's dark-mode follow-up (noted under #391) is still
  open and unfiled against a PR.
- Bigger items need a spec first (rule 12): #242, #305-#308, with #392 and
  #393 folded into the options-card spec.
