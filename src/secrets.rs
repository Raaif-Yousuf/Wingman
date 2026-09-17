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

use anyhow::{bail, Result};
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
    /// `Ok(None)` when no credential exists under `target`. Never returns
    /// the secret in an `Err` -- a read failure carries only the target name
    /// and the underlying error code.
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

/// The real store: Windows Credential Manager generic credentials via
/// `CredWriteW`/`CredReadW`/`CredDeleteW`. Zero-sized -- there is no state
/// to hold, every call goes straight to the OS.
#[derive(Default, Clone, Copy)]
pub struct CredManagerStore;

impl SecretStore for CredManagerStore {
    fn get(&self, target: &str) -> Result<Option<String>> {
        let target_w = wide_z(target);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        let result =
            unsafe { CredReadW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) };
        match result {
            Ok(()) => {
                // SAFETY: CredReadW just reported success, so `cred` is a
                // valid, non-null pointer that CredFree must release.
                let secret = unsafe {
                    let c = &*cred;
                    let bytes =
                        std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
                    let owned = String::from_utf8(bytes.to_vec()).ok();
                    CredFree(cred as *const _);
                    owned
                };
                Ok(secret)
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
        assert!(!debug_output.contains("sk-should-not-appear"), "{debug_output}");
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
        assert_eq!(store.get(&target).unwrap().as_deref(), Some("replacement-value"));

        store.delete(&target).expect("CredDeleteW should succeed");
        assert_eq!(store.get(&target).unwrap(), None, "deleted target must read back absent");

        // Deleting an already-absent credential is not an error.
        store
            .delete(&target)
            .expect("deleting an absent credential must not error");
    }
}
