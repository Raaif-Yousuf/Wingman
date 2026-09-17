//! Windows Credential Manager access for provider API keys (issue #2).
//!
//! Generic credentials are named `Wingman/<provider>` (e.g. `Wingman/openai`,
//! `Wingman/anthropic`). Everything outside this module reaches a key
//! through [`crate::config::Config`] -- `config.rs`'s load/save path is the
//! only caller of [`SecretStore`], per the expansion plan's §4 `secrets.rs`
//! row ("providers read keys from the store via config loading, not by each
//! provider hitting Win32").
//!
//! The real store ([`CredManagerStore`]) wraps `CredWriteW`/`CredReadW`/
//! `CredDeleteW` behind the [`SecretStore`] trait so `config.rs`'s
//! import/hydrate logic is unit-testable against an in-memory stub
//! ([`InMemoryStore`], test-only) instead of the real Credential Manager.
//!
//! No error path here ever includes a secret value -- only the target name
//! (e.g. `"Wingman/openai"`, never secret) and the Win32 error code.
//!
//! # Blob encoding (issue #175)
//!
//! [`CredManagerStore::set`] always writes the secret as UTF-8 bytes
//! (`secret.as_bytes()`). [`CredManagerStore::get`] decodes UTF-8 first and,
//! if that fails, falls back to UTF-16LE (the encoding `CredWriteW`'s own
//! documentation examples use for a generic credential's blob) before giving
//! up -- see [`decode_blob`]. A blob that is neither is reported as `Err`,
//! never as `Ok(None)`: before this fix a non-UTF-8 blob under a
//! `Wingman/<provider>` target name (written by something other than this
//! module) was silently treated as "no credential", and the next `save()`
//! deleted it.

use anyhow::{bail, Context, Result};
use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
};

/// A secret store keyed by an opaque target name. Implemented for the real
/// Windows Credential Manager ([`CredManagerStore`]) and, under
/// `#[cfg(test)]`, an in-memory stub ([`InMemoryStore`]).
pub trait SecretStore {
    /// Tri-state per issue #175: `Ok(None)` when no credential exists under
    /// `target`; `Ok(Some(secret))` when one was read cleanly; `Err` when a
    /// credential exists but could not be read (a transient `CredReadW`
    /// failure, or a blob [`decode_blob`] cannot parse). Callers must never
    /// treat `Err` the same as `Ok(None)` -- doing so is exactly what let a
    /// save delete a credential the caller never got to see (#175). Never
    /// returns the secret in an `Err` -- a read failure carries only the
    /// target name and the underlying error code.
    fn get(&self, target: &str) -> Result<Option<String>>;
    /// Writes (overwriting) the secret under `target`.
    fn set(&self, target: &str, secret: &str) -> Result<()>;
    /// Deletes the credential under `target`. Not an error if it is already
    /// absent. Not yet called from production code (nothing removes a
    /// provider's key today), but is part of the CredWriteW/CredReadW/
    /// CredDeleteW trio issue #2 asks for and is exercised directly by this
    /// module's own tests.
    #[allow(dead_code)]
    fn delete(&self, target: &str) -> Result<()>;
}

/// `Wingman/<provider>` -- the generic-credential naming convention every
/// provider's key is stored under.
pub fn target_name(provider: &str) -> String {
    format!("Wingman/{provider}")
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a Credential Manager blob into a secret string. Tries UTF-8
/// first (what this module's own `set` always writes) and accepts it
/// outright *unless* it contains an embedded NUL: plain ASCII text written
/// as UTF-16LE (the `CredWriteW` convention, in case something else wrote
/// this blob) also happens to parse as "valid" UTF-8, one NUL byte between
/// every character, which is never what a real secret looks like. In that
/// case -- or if UTF-8 parsing failed outright -- UTF-16LE is tried next; a
/// NUL-laden UTF-8 reading is kept only as the last resort, if UTF-16LE
/// itself does not decode cleanly either. Pure and Win32-free on purpose so
/// the decode logic -- the part issue #175 actually needed fixed -- is
/// unit-testable without touching the real store (CLAUDE.md rule 8).
fn decode_blob(bytes: &[u8]) -> Result<String> {
    let utf8: Option<String> = std::str::from_utf8(bytes).ok().map(|s| s.to_string());
    if let Some(s) = &utf8 {
        if !s.contains('\u{0}') {
            return Ok(s.clone());
        }
    }
    if bytes.len() % 2 == 0 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if let Ok(s) = String::from_utf16(&units) {
            return Ok(s);
        }
    }
    if let Some(s) = utf8 {
        return Ok(s);
    }
    bail!("credential blob is neither valid UTF-8 nor valid UTF-16LE")
}

/// The real store: Windows Credential Manager generic credentials via
/// `CredWriteW`/`CredReadW`/`CredDeleteW`. Zero-sized -- there is no state
/// to hold, every call goes straight to the OS.
#[derive(Default, Clone, Copy)]
pub struct CredManagerStore;

impl SecretStore for CredManagerStore {
    fn get(&self, target: &str) -> Result<Option<String>> {
        let target_w = wide_z(target);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        let result = unsafe {
            CredReadW(
                PCWSTR(target_w.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut cred,
            )
        };
        match result {
            Ok(()) => {
                // SAFETY: CredReadW just reported success, so `cred` is a
                // valid, non-null pointer that CredFree must release.
                let bytes = unsafe {
                    let c = &*cred;
                    let bytes =
                        std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)
                            .to_vec();
                    CredFree(cred as *const _);
                    bytes
                };
                // #175: a blob this build cannot decode is a real
                // credential that could not be read, never "no credential".
                // Confusing the two is exactly what let a later save delete
                // it -- never fold this back into `Ok(None)`.
                decode_blob(&bytes)
                    .map(Some)
                    .with_context(|| format!("CredReadW returned an unreadable blob for {target}"))
            }
            Err(e) if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(None),
            Err(e) => bail!("CredReadW failed for {target}: {}", e.code()),
        }
    }

    fn set(&self, target: &str, secret: &str) -> Result<()> {
        let mut target_w = wide_z(target);
        let mut blob = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        unsafe { CredWriteW(&cred, 0) }
            .map_err(|e| anyhow::anyhow!("CredWriteW failed for {target}: {}", e.code()))
    }

    fn delete(&self, target: &str) -> Result<()> {
        let target_w = wide_z(target);
        match unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(()),
            Err(e) => bail!("CredDeleteW failed for {target}: {}", e.code()),
        }
    }
}

/// In-memory [`SecretStore`] stub for tests. Never touches the real
/// Credential Manager, so `config.rs`'s import/hydrate logic can be
/// exercised without the "tests never touch production names" risk (rule 9)
/// a real store would carry. `Debug` is hand-rolled to redact stored values,
/// matching the real key-never-in-Debug rule (#157) even though nothing in
/// this crate currently formats a store with `{:?}`.
#[cfg(test)]
#[derive(Default)]
pub struct InMemoryStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Test-only, issue #175: targets `get` must report `Err` for, to
    /// simulate a Credential Manager read failure independently of whether
    /// `set`/`delete` still work against `entries`.
    poisoned: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[cfg(test)]
impl InMemoryStore {
    /// Makes every future `get(target)` call return `Err`, without touching
    /// `entries` -- so a credential can be seeded via `set` first and then
    /// made unreadable, exactly like a real credential that exists but
    /// whose `CredReadW` fails or whose blob cannot be decoded.
    pub fn poison(&self, target: &str) {
        self.poisoned.lock().unwrap().insert(target.to_string());
    }
}

#[cfg(test)]
impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("InMemoryStore")
            .field("entries", &format!("<{count} redacted>"))
            .finish()
    }
}

#[cfg(test)]
impl SecretStore for InMemoryStore {
    fn get(&self, target: &str) -> Result<Option<String>> {
        if self.poisoned.lock().unwrap().contains(target) {
            bail!("simulated unreadable credential for {target}");
        }
        Ok(self.entries.lock().unwrap().get(target).cloned())
    }

    fn set(&self, target: &str, secret: &str) -> Result<()> {
        self.entries
            .lock()
            .unwrap()
            .insert(target.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<()> {
        self.entries.lock().unwrap().remove(target);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- target_name ------------------------------------------------------

    #[test]
    fn target_name_is_wingman_slash_provider() {
        assert_eq!(target_name("openai"), "Wingman/openai");
        assert_eq!(target_name("anthropic"), "Wingman/anthropic");
        assert_eq!(target_name("gemini"), "Wingman/gemini");
    }

    // -- InMemoryStore ------------------------------------------------------

    #[test]
    fn in_memory_store_round_trips() {
        let store = InMemoryStore::default();
        assert_eq!(store.get("Wingman/openai").unwrap(), None);

        store.set("Wingman/openai", "sk-test-value").unwrap();
        assert_eq!(
            store.get("Wingman/openai").unwrap().as_deref(),
            Some("sk-test-value")
        );

        store.delete("Wingman/openai").unwrap();
        assert_eq!(store.get("Wingman/openai").unwrap(), None);
    }

    #[test]
    fn in_memory_store_delete_of_absent_key_is_not_an_error() {
        let store = InMemoryStore::default();
        assert!(store.delete("Wingman/never-set").is_ok());
    }

    #[test]
    fn in_memory_store_debug_never_contains_the_secret() {
        let store = InMemoryStore::default();
        store.set("Wingman/openai", "sk-should-not-appear").unwrap();
        let debug_output = format!("{store:?}");
        assert!(
            !debug_output.contains("sk-should-not-appear"),
            "{debug_output}"
        );
    }

    // -- InMemoryStore::poison (#175 test scaffolding) -----------------------

    #[test]
    fn poisoning_a_target_makes_get_fail_without_touching_entries() {
        let store = InMemoryStore::default();
        store.set("Wingman/openai", "sk-still-here").unwrap();
        store.poison("Wingman/openai");

        assert!(
            store.get("Wingman/openai").is_err(),
            "a poisoned target must report Err, not Ok(None) and not the old value"
        );

        // set/delete must still work normally -- poison only affects get.
        store.set("Wingman/openai", "sk-overwritten").unwrap();
    }

    #[test]
    fn poisoning_one_target_leaves_another_readable() {
        let store = InMemoryStore::default();
        store.set("Wingman/anthropic", "sk-ant-fine").unwrap();
        store.poison("Wingman/openai");

        assert_eq!(
            store.get("Wingman/anthropic").unwrap().as_deref(),
            Some("sk-ant-fine")
        );
    }

    // -- decode_blob (#175: pure, Win32-free) --------------------------------

    #[test]
    fn decode_blob_accepts_valid_utf8() {
        let bytes = "sk-real-secret".as_bytes();
        assert_eq!(decode_blob(bytes).unwrap(), "sk-real-secret");
    }

    #[test]
    fn decode_blob_accepts_valid_utf16le() {
        // The CredWriteW-convention encoding, in case something other than
        // this module's own `set` (always UTF-8) wrote the blob.
        let text = "sk-utf16-secret";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_blob(&bytes).unwrap(), text);
    }

    #[test]
    fn decode_blob_rejects_bytes_that_are_neither_utf8_nor_utf16le() {
        // [0x00, 0xD8] as UTF-8: NUL, then a lead byte (0xD8 = 110xxxxx)
        // with no continuation byte -- truncated, invalid. As UTF-16LE: one
        // code unit, 0xD800, a lone (unpaired) high surrogate -- also
        // invalid. Neither fallback accepts it.
        let bytes: [u8; 2] = [0x00, 0xD8];
        assert!(
            decode_blob(&bytes).is_err(),
            "a lone UTF-16 surrogate must not decode as either encoding"
        );
    }

    #[test]
    fn decode_blob_rejects_odd_length_bytes_that_are_not_utf8() {
        // Not valid UTF-8 (0xFF is never a valid lead byte), and odd-length
        // so the UTF-16LE fallback cannot even be attempted.
        let bytes: [u8; 3] = [0xFF, 0xFF, 0xFF];
        assert!(decode_blob(&bytes).is_err());
    }

    #[test]
    fn decode_blob_prefers_utf16le_over_a_nul_laden_utf8_reading() {
        // Two-byte "ab" written as UTF-16LE ('a', 0x00, 'b', 0x00) is *also*
        // technically valid UTF-8 ("a\0b\0"), which is never what a real
        // secret looks like -- the UTF-16LE reading must win.
        let bytes: [u8; 4] = [b'a', 0x00, b'b', 0x00];
        assert_eq!(decode_blob(&bytes).unwrap(), "ab");
    }

    #[test]
    fn decode_blob_of_empty_bytes_is_empty_string() {
        // An empty blob is valid (empty) UTF-8; nothing here needs to treat
        // it as unreadable.
        assert_eq!(decode_blob(&[]).unwrap(), "");
    }

    // -- CredManagerStore: real Win32 round trip -----------------------------
    //
    // Uses a TEST-ONLY target name prefix ("Wingman.Test.<pid>/...", a dot
    // where the production convention uses a slash) so this can never
    // collide with a real `Wingman/<provider>` credential (rule 9). Cleans
    // up via both an explicit delete and a Drop guard, so a mid-test panic
    // still removes the credential (the crate's dev/test profile unwinds on
    // panic; only `[profile.release]` sets `panic = "abort"`).
    #[test]
    fn win32_round_trip_uses_a_test_only_target_and_always_cleans_up() {
        let target = format!("Wingman.Test.{}/secrets-roundtrip", std::process::id());
        let store = CredManagerStore;

        struct Cleanup<'a> {
            store: &'a CredManagerStore,
            target: &'a str,
        }
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.store.delete(self.target);
            }
        }
        let _cleanup = Cleanup {
            store: &store,
            target: &target,
        };

        assert_eq!(
            store.get(&target).unwrap(),
            None,
            "a fresh test-only target must start absent"
        );

        store
            .set(&target, "test-only-secret-value")
            .expect("CredWriteW should succeed for a generic credential");
        assert_eq!(
            store.get(&target).unwrap().as_deref(),
            Some("test-only-secret-value"),
            "CredReadW should return exactly what CredWriteW stored"
        );

        // Overwrite: set must replace, not fail or duplicate.
        store.set(&target, "replacement-value").unwrap();
        assert_eq!(
            store.get(&target).unwrap().as_deref(),
            Some("replacement-value")
        );

        store.delete(&target).expect("CredDeleteW should succeed");
        assert_eq!(
            store.get(&target).unwrap(),
            None,
            "deleted target must read back absent"
        );

        // Deleting an already-absent credential is not an error.
        store
            .delete(&target)
            .expect("deleting an absent credential must not error");
    }
}
