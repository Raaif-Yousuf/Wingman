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
▸ Custom ▸ copilot-ask. Shell-protected; no script can do it. Needs re-doing
once after the rename to Wingman. If already done, delete this row.

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
