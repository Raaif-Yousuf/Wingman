//! Shared `CUIAutomation8` construction (#261, #264).
//!
//! Every UIA call in this crate (`inputs::selection::com`,
//! `inputs::uia::com`, `executors::target::com`,
//! `actions::review_email::com`) used to call
//! `CoCreateInstance(&CUIAutomation8, ...)` directly, with nothing bounding
//! a subsequent blocking call (`GetFocusedElement`, `ElementFromHandle`,
//! `FindAllBuildCache`, `GetSelection`, ...) against a hung foreground
//! provider. #261 and #264 are the same root cause from two call sites:
//! `inputs::selection` wedges `App::busy` forever, and
//! `inputs::uia::com::walk`'s `budget` parameter only bounds how long the
//! *cache request* accumulates results, never the initial blocking calls
//! that hand it an element to walk in the first place.
//!
//! UIA has its own bound for this: `IUIAutomation2::SetConnectionTimeout`
//! and `SetTransactionTimeout` (Windows 8+) make a call into a hung
//! provider fail with `UIA_E_TIMEOUT` instead of blocking indefinitely.
//! `THEORY (unverified)`: this actually unblocks a call already in flight
//! against a genuinely hung provider process -- not measured against a
//! live hung UIA provider (would need a synthetic test app whose provider
//! sleeps, out of scope for this pass; see the PR's "by-hand check owed").
//!
//! [`create_automation`] is the single place that constructs the
//! automation object and applies both timeouts, so every call site above
//! now goes through it instead of calling `CoCreateInstance` itself. The
//! source-scanning test at the bottom of this file fails the build if a
//! new `CoCreateInstance(&CUIAutomation8` appears anywhere in `src/`
//! outside this file, so a future call site cannot silently bypass the
//! bound.

use std::time::Duration;
use windows::core::Interface;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{CUIAutomation8, IUIAutomation, IUIAutomation2};

/// How long a connection to a UIA provider process may take to establish
/// before a call fails with `UIA_E_TIMEOUT` rather than blocking. Chosen to
/// be well above any real provider's normal response time (typically low
/// milliseconds) but short enough that a hung foreground app still ends in
/// a card within a few seconds, per AGENTS.md rule 7.
pub(crate) const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a single UIA method call (`GetFocusedElement`,
/// `ElementFromHandle`, `GetSelection`, ...) may block before failing with
/// `UIA_E_TIMEOUT`.
pub(crate) const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// The `UIA_E_TIMEOUT` HRESULT, duplicated as a plain constant rather than
/// depending on the `windows` crate's own item of the same name existing
/// at the exact same value forever: this is the value UIA calls fail with
/// once [`CONNECTION_TIMEOUT`]/[`TRANSACTION_TIMEOUT`] trip against a hung
/// provider (`uiautomationcore.h`'s `UIA_E_TIMEOUT`, `0x80131505`).
pub(crate) const UIA_E_TIMEOUT_HRESULT: i32 = 0x8013_1505u32 as i32;

/// Constructs the shared `CUIAutomation8` object with both timeouts
/// applied, so a blocking call made through the returned `IUIAutomation`
/// fails with `UIA_E_TIMEOUT` instead of hanging forever against an
/// unresponsive provider. The single construction site every `com` module
/// in this crate must call instead of `CoCreateInstance` directly.
///
/// # Safety
/// Caller must already hold an initialized COM apartment on this thread
/// (i.e. call this from inside a `ComApartment::enter()` guard's scope),
/// same requirement `CoCreateInstance` itself has.
pub(crate) unsafe fn create_automation() -> windows::core::Result<IUIAutomation> {
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;
    // IUIAutomation2 is a Windows 8+ extension of IUIAutomation; the cast
    // fails only on an ancient OS this crate does not otherwise support,
    // so a failed cast is tolerated (falls back to the unbounded object)
    // rather than turned into a hard error.
    if let Ok(automation2) = automation.cast::<IUIAutomation2>() {
        unsafe {
            let _ = automation2.SetConnectionTimeout(CONNECTION_TIMEOUT.as_millis() as u32);
            let _ = automation2.SetTransactionTimeout(TRANSACTION_TIMEOUT.as_millis() as u32);
        }
    }
    Ok(automation)
}

/// True if `err` is UIA's own timeout HRESULT (`UIA_E_TIMEOUT`), i.e. a
/// call bounded by [`create_automation`]'s timeouts actually tripped that
/// bound rather than failing for some other reason.
pub(crate) fn is_uia_timeout(err: &windows::core::Error) -> bool {
    err.code().0 == UIA_E_TIMEOUT_HRESULT
}

/// The human-readable fragment `app.rs`'s `KNOWN_TECHNICAL_DETAILS` maps
/// to a card-visible sentence. Every call site that turns a UIA timeout
/// into an `anyhow::Error` should include this exact fragment (e.g. via
/// [`timeout_context`]) so the card never shows a raw HRESULT.
pub(crate) const TIMEOUT_MESSAGE_FRAGMENT: &str = "UI Automation timed out";

/// Wraps a UIA call's `windows::core::Result` so a `UIA_E_TIMEOUT` failure
/// carries [`TIMEOUT_MESSAGE_FRAGMENT`] in its message (any other failure
/// passes through unchanged, converted to `anyhow::Error` as usual via
/// `?`'s `From` impl).
pub(crate) fn describe_timeout<T>(
    result: windows::core::Result<T>,
    what: &str,
) -> anyhow::Result<T> {
    match result {
        Ok(v) => Ok(v),
        Err(e) if is_uia_timeout(&e) => {
            Err(anyhow::anyhow!("{} ({what})", TIMEOUT_MESSAGE_FRAGMENT))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_uia_timeout_matches_only_the_uia_timeout_hresult() {
        let timeout =
            windows::core::Error::from_hresult(windows::core::HRESULT(UIA_E_TIMEOUT_HRESULT));
        assert!(is_uia_timeout(&timeout));

        let other = windows::core::Error::from_hresult(windows::core::HRESULT(
            windows::Win32::Foundation::E_FAIL.0,
        ));
        assert!(!is_uia_timeout(&other));
    }

    #[test]
    fn describe_timeout_adds_the_readable_fragment_on_timeout() {
        let timeout: windows::core::Result<()> = Err(windows::core::Error::from_hresult(
            windows::core::HRESULT(UIA_E_TIMEOUT_HRESULT),
        ));
        let err = describe_timeout(timeout, "GetFocusedElement").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains(TIMEOUT_MESSAGE_FRAGMENT),
            "expected {msg:?} to contain {TIMEOUT_MESSAGE_FRAGMENT:?}"
        );
        assert!(msg.contains("GetFocusedElement"));
    }

    #[test]
    fn describe_timeout_passes_through_a_non_timeout_error() {
        let other: windows::core::Result<()> = Err(windows::core::Error::from_hresult(
            windows::core::HRESULT(windows::Win32::Foundation::E_FAIL.0),
        ));
        let err = describe_timeout(other, "GetFocusedElement").unwrap_err();
        assert!(!format!("{err}").contains(TIMEOUT_MESSAGE_FRAGMENT));
    }

    #[test]
    fn connection_and_transaction_timeouts_are_bounded_and_nonzero() {
        // A regression guard on the constants themselves: zero would mean
        // "no timeout" (UIA treats 0 as infinite for these properties),
        // and anything over a minute would defeat rule 7 ("every failure
        // ends in a card" -- promptly, not eventually).
        assert!(CONNECTION_TIMEOUT.as_millis() > 0);
        assert!(TRANSACTION_TIMEOUT.as_millis() > 0);
        assert!(CONNECTION_TIMEOUT <= Duration::from_secs(60));
        assert!(TRANSACTION_TIMEOUT <= Duration::from_secs(60));
    }

    /// #261/#264's "no site can be missed" guard: every
    /// `CoCreateInstance(&CUIAutomation8` construction in `src/` must go
    /// through this file's [`create_automation`], not call
    /// `CoCreateInstance` directly. Scans `src/` source normalized to `\n`
    /// line endings (a CRLF checkout must not change what this test sees,
    /// per the repo's own source-scanning tests) and fails if that literal
    /// call shape appears in any file other than this one.
    #[test]
    fn every_cuiautomation_construction_goes_through_create_automation() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let needle = "CoCreateInstance(&CUIAutomation8";
        let mut offenders = Vec::new();
        visit_rs_files(&src_dir, &mut |path, contents| {
            if path.file_name().and_then(|n| n.to_str()) == Some("uia_automation.rs") {
                return;
            }
            let normalized = contents.replace("\r\n", "\n");
            for (i, line) in normalized.lines().enumerate() {
                if line.contains(needle) {
                    offenders.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        });
        assert!(
            offenders.is_empty(),
            "found a CUIAutomation8 construction bypassing \
             inputs::uia_automation::create_automation: {offenders:?}"
        );
    }

    fn visit_rs_files(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit_rs_files(&path, f);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    f(&path, &contents);
                }
            }
        }
    }
}
