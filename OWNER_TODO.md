# OWNER TODO: things only you can do

Everything here is blocked on you: a click Windows will not let a script make,
an account, or a decision. Nothing in this file can be done by an agent. Rows
are deleted when done.

## 1. Push the repo

`git push -u origin master` once you are happy with the first commit. The
issues filed on 2026-09-16 link to `docs/superpowers/specs/…` and those links
are dead until the push lands.

## 2. OAuth app registrations (needed in Phase 3, not before)

- Google Cloud: an OAuth client of type "Desktop app" for the Calendar API.
  The client id is public by design (PKCE); it goes in `config.example.toml`.
- Microsoft Entra: an app registration, "Mobile and desktop applications"
  platform, redirect `http://127.0.0.1`, delegated `Calendars.ReadWrite` and
  `Mail.ReadWrite`. Same: client id is public.

## 3. Form-fill sensitivity default (plan § 15)

Recommended: name, email, phone and address fill without a per-field tick;
date of birth and anything the model marks sensitive need a tick in the
preview. Say yes or adjust.

## 4. The Copilot key picker click (carried over from 2026-09-15)

Settings ▸ Bluetooth & devices ▸ Keyboard ▸ Customize Copilot key on keyboard
▸ Custom ▸ Wingman. Shell-protected; no script can do it. Needs re-doing
once after the rename. If already done, delete this row.

## 7. Approve or reject the activation trust spec (added 2026-09-19)

`docs/superpowers/specs/2026-09-19-activation-trust-design.md` covers issues
#236 and #237, both P1, and no code was written for either because rule 12
says an architectural change gets an approved spec first. Its § 7 asks four
questions, one of which has a real downside you should weigh rather than
wave through: whether Wingman should **start anyway, without the lock**, when
the single-instance name is held but no owner window answers after two
seconds. That is what stops a squatter blocking every launch forever,
including at login, and the cost is a second instance if a genuine one is ever
alive that long with no window.

The spec's other conclusion is worth a minute even if you reject the rest:
sender authentication cannot close either hole against a process running as
you, so the named-pipe fix both issues suggest would be a week spent on a
property the threat model cannot have.

## 8. Four manual desktop checks (added 2026-09-19, tracked on #166)

Tonight's round produced four findings that cannot be closed without a live
desktop, two of them against fixes that have now landed:

- **#271**: open the region overlay over two overlapping real windows, click
  without dragging on the front one, and check the size label matches that
  window rather than the whole desktop. Also click bare desktop background:
  the fixing agent flagged, as an unverified theory, that Progman/WorkerW may
  appear in the window snapshot and stage a whole monitor.
- **#272**: open the overlay, Alt-Tab away, and check it disappears rather
  than sticking on screen unresponsive to Escape.
- **#261**: needs a genuinely hung UIA provider to observe `busy` wedging.
- **#255**: needs a real focused preview control destroyed with
  `DestroyWindow`, to see what `WM_KILLFOCUS` Windows actually delivers.

## 5. GitHub repo settings (one-time, in the browser)

Enable Discussions; add the MIT license file via the first push (the repo
currently reports no license); pin the roadmap issue once filed.

## 6. Release signing secrets (optional, needed for a signed release)

`.github/workflows/release.yml` (issue #8) signs the exe and msix only if
`WINGMAN_MSIX_PFX_BASE64` (a PKCS#12 code-signing certificate, base64-encoded)
and `WINGMAN_MSIX_PFX_PASSWORD` are set as repository secrets under Settings ▸
Secrets and variables ▸ Actions. Without them the workflow still runs and
still produces a release; the exe and msix are just unsigned, with a note to
that effect in the release body. No script can generate and upload a
certificate a CI runner should hold on your behalf, so this is a manual step
if you want signed release artifacts. Delete this row once decided (skip
signing for now, or the secrets are set).
