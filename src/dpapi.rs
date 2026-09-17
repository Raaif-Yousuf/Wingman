//! DPAPI (`CryptProtectData`/`CryptUnprotectData`) encryption at rest
//! (issue #62). User-scoped: no `CRYPTPROTECT_LOCAL_MACHINE` flag is ever
//! passed, so the ciphertext this module produces is only unprotectable by
//! the same Windows user account that protected it -- exactly what issue
//! #62's acceptance criterion ("the database file read from another account
//! yields no plaintext") requires. [`crate::profile`] is the first caller;
//! any future on-disk secret (the awareness store, issue #62's other named
//! consumer) goes through the same two functions.
//!
//! [`protect`] and [`unprotect`] always pass `CRYPTPROTECT_UI_FORBIDDEN`.
//! Without it, a corrupted key store or an unusual account configuration
//! can make DPAPI pop a credential prompt -- exactly the dialog box
//! CLAUDE.md rule 7 promises this app never shows. Forbidding it turns that
//! case into an ordinary `Err` a caller can turn into a card instead.
//!
//! # Envelope format
//!
//! `protect` never returns a raw `CryptProtectData` blob -- it wraps it in a
//! small versioned envelope so the on-disk format can change shape later
//! without breaking every file already encrypted under the old one:
//!
//! ```text
//! [0..4)   magic     b"WMDP" ("Wingman DPAPI")
//! [4]      version   u8, currently 1
//! [5..)    ciphertext, the raw CryptProtectData output
//! ```
//!
//! `unprotect` checks the magic and version before touching Win32 at all, so
//! a file that is not one of this module's envelopes -- or is a future
//! version this build does not understand -- fails with a clear, specific
//! error instead of an opaque `CryptUnprotectData` failure or a panic.
//!
//! # Zeroizing
//!
//! Two buffers this module owns carry plaintext and are zeroized before
//! they are freed or dropped, via the [`zeroize`] helper below:
//!
//! - [`protect`]'s `plaintext` parameter is taken by value (not borrowed)
//!   exactly so this function can zero the caller's bytes once
//!   `CryptProtectData` has copied them into its own ciphertext, rather
//!   than leaving that to the caller (or to `Vec`'s ordinary drop, which
//!   does not zero).
//! - [`unprotect`]'s raw `CryptUnprotectData` output buffer is zeroed in
//!   place immediately after being copied into the `Vec<u8>` this function
//!   returns, before `LocalFree` releases it -- so the plaintext never
//!   lingers in freed-but-unzeroed heap memory.
//!
//! No new crate: nine lines with `std::ptr::write_volatile` covers it, a
//! smaller footprint than adding a dependency for something this small.

// Issue #62's scope is this module: the DPAPI primitive itself, not wiring
// it into the awareness SQLite store (a later issue) -- `crate::profile`
// (issue #36) is this module's first real caller. Until a caller exists in
// `app.rs`'s own reachability graph, this binary crate's dead-code analysis
// would otherwise flag the whole public surface below as unused, the same
// reason `ocr.rs` and `capture.rs::pick_compression_level` carry this.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Envelope magic: "Wingman DPAPI".
const MAGIC: [u8; 4] = *b"WMDP";
/// The only envelope version this build writes, and the newest one it reads.
const VERSION: u8 = 1;
/// `MAGIC` (4 bytes) + version (1 byte).
const HEADER_LEN: usize = 5;

/// Per-purpose entropy for [`crate::profile`]'s DPAPI envelope. Optional
/// additional entropy DPAPI mixes into protection, so a profile envelope
/// cannot be unprotected by a different purpose's code path even under the
/// same Windows account and even if that code also forgot to pass entropy
/// of its own -- `unprotect(profile_bytes, None)` still fails.
pub const PROFILE_ENTROPY: &[u8] = b"Wingman/profile/v1";

/// Zeroizes a buffer this module owns, in place. `write_volatile` (rather
/// than a plain `*b = 0` loop or `slice::fill`) stops the compiler from
/// proving the writes are dead -- because nothing reads the buffer again
/// before it is freed -- and eliding them, which is exactly the bug a plain
/// assignment risks right before the buffer is freed or dropped.
fn zeroize(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // SAFETY: `byte` is a valid `&mut u8` yielded by `iter_mut`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Borrows `bytes` as a `CRYPT_INTEGER_BLOB`. The blob's `pbData` pointer is
/// only valid for as long as `bytes` is; callers must not let the blob
/// outlive it.
fn blob_of(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

/// Copies a `CryptProtectData`/`CryptUnprotectData` output blob into a
/// `Vec<u8>`, zeroizes the original buffer (it may hold plaintext), and
/// frees it with `LocalFree` -- on every path, including the zero-length
/// case (Windows can still hand back a valid pointer for an empty
/// allocation, and it still owns a real allocation that must be released).
///
/// # Safety
/// `blob` must be a genuine, successful output of one of those two Win32
/// calls: `pbData` either null (only ever paired with `cbData == 0`) or a
/// valid `LocalAlloc`'d buffer of at least `cbData` bytes that this call is
/// the sole owner of.
unsafe fn take_and_free(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if blob.pbData.is_null() {
        return Vec::new();
    }
    let len = blob.cbData as usize;
    let slice = std::slice::from_raw_parts_mut(blob.pbData, len);
    let bytes = slice.to_vec();
    zeroize(slice);
    LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
    bytes
}

/// Encrypts `plaintext` with `CryptProtectData` (user-scoped) and wraps the
/// result in a versioned envelope. Takes ownership of `plaintext` so it can
/// be zeroized once `CryptProtectData` has copied it into ciphertext (see
/// the module docs' "Zeroizing" section). `entropy` is optional additional
/// entropy -- pass [`PROFILE_ENTROPY`] or `None`; [`unprotect`] must be
/// called with the same choice to succeed.
pub fn protect(mut plaintext: Vec<u8>, entropy: Option<&[u8]>) -> Result<Vec<u8>> {
    let in_blob = blob_of(&plaintext);
    let entropy_blob = entropy.map(blob_of);
    let mut out_blob = CRYPT_INTEGER_BLOB::default();

    let result = unsafe {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            entropy_blob.as_ref().map(|b| b as *const _),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };

    // Zeroize the caller's plaintext regardless of outcome: CryptProtectData
    // has either already copied it into `out_blob`'s ciphertext, or failed
    // and copied nothing -- either way this function is done reading it.
    zeroize(&mut plaintext);
    result.context("CryptProtectData failed")?;

    // SAFETY: CryptProtectData just reported success, so out_blob is a
    // genuine output blob this call owns.
    let ciphertext = unsafe { take_and_free(out_blob) };

    let mut envelope = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    envelope.extend_from_slice(&MAGIC);
    envelope.push(VERSION);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

/// Reverses [`protect`]: validates the envelope's magic and version, then
/// decrypts with `CryptUnprotectData`. `entropy` must match whatever was
/// passed to `protect` (both `None`, or the same bytes) -- a mismatch,
/// including passing entropy where none was used or the reverse, is
/// reported by DPAPI as an ordinary decryption failure and surfaced here as
/// `Err`, never a panic and never silently wrong plaintext. Tampering with
/// any ciphertext byte fails the same way, since DPAPI's own integrity
/// check rejects it before this function sees any bytes back.
pub fn unprotect(envelope: &[u8], entropy: Option<&[u8]>) -> Result<Vec<u8>> {
    if envelope.len() < HEADER_LEN {
        bail!(
            "DPAPI envelope is truncated: {} byte(s), need at least {HEADER_LEN} for the header",
            envelope.len()
        );
    }
    let (header, ciphertext) = envelope.split_at(HEADER_LEN);
    let (magic, version_byte) = header.split_at(4);
    if magic != MAGIC {
        bail!("DPAPI envelope has an unrecognized magic; this is not a Wingman DPAPI envelope");
    }
    let version = version_byte[0];
    if version != VERSION {
        bail!(
            "DPAPI envelope version {version} is not supported by this build (expected {VERSION})"
        );
    }

    let in_blob = blob_of(ciphertext);
    let entropy_blob = entropy.map(blob_of);
    let mut out_blob = CRYPT_INTEGER_BLOB::default();

    let result = unsafe {
        CryptUnprotectData(
            &in_blob,
            None,
            entropy_blob.as_ref().map(|b| b as *const _),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    result.context(
        "CryptUnprotectData failed (wrong entropy, tampered ciphertext, \
         or data protected under a different Windows account)",
    )?;

    // SAFETY: CryptUnprotectData just reported success, so out_blob is a
    // genuine output blob this call owns.
    let plaintext = unsafe { take_and_free(out_blob) };
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- round trip -----------------------------------------------------

    #[test]
    fn round_trips_without_entropy() {
        let plaintext = b"the quick brown fox".to_vec();
        let envelope = protect(plaintext.clone(), None).expect("protect should succeed");
        let decrypted = unprotect(&envelope, None).expect("unprotect should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn round_trips_with_entropy() {
        let plaintext = b"jumps over the lazy dog".to_vec();
        let envelope =
            protect(plaintext.clone(), Some(PROFILE_ENTROPY)).expect("protect should succeed");
        let decrypted =
            unprotect(&envelope, Some(PROFILE_ENTROPY)).expect("unprotect should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn round_trips_binary_data_with_nul_and_high_bytes() {
        let plaintext: Vec<u8> = vec![0x00, 0xFF, 0x01, 0xFE, 0x00, 0x80, 0x7F];
        let envelope = protect(plaintext.clone(), None).expect("protect should succeed");
        let decrypted = unprotect(&envelope, None).expect("unprotect should succeed");
        assert_eq!(decrypted, plaintext);
    }

    // -- empty input ------------------------------------------------------

    #[test]
    fn round_trips_empty_input() {
        let envelope = protect(Vec::new(), None).expect("protect should succeed on empty input");
        let decrypted = unprotect(&envelope, None).expect("unprotect should succeed");
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    #[test]
    fn round_trips_empty_input_with_entropy() {
        let envelope = protect(Vec::new(), Some(PROFILE_ENTROPY))
            .expect("protect should succeed on empty input");
        let decrypted =
            unprotect(&envelope, Some(PROFILE_ENTROPY)).expect("unprotect should succeed");
        assert_eq!(decrypted, Vec::<u8>::new());
    }

    // -- wrong entropy ------------------------------------------------------

    #[test]
    fn wrong_entropy_fails() {
        let plaintext = b"secret".to_vec();
        let envelope = protect(plaintext, Some(PROFILE_ENTROPY)).expect("protect should succeed");
        let result = unprotect(&envelope, Some(b"not-the-real-entropy"));
        assert!(result.is_err(), "wrong entropy must not decrypt");
    }

    #[test]
    fn missing_entropy_fails_when_protect_used_some() {
        let plaintext = b"secret".to_vec();
        let envelope = protect(plaintext, Some(PROFILE_ENTROPY)).expect("protect should succeed");
        let result = unprotect(&envelope, None);
        assert!(
            result.is_err(),
            "unprotect without entropy must not decrypt a blob protected with entropy"
        );
    }

    #[test]
    fn unexpected_entropy_fails_when_protect_used_none() {
        let plaintext = b"secret".to_vec();
        let envelope = protect(plaintext, None).expect("protect should succeed");
        let result = unprotect(&envelope, Some(PROFILE_ENTROPY));
        assert!(
            result.is_err(),
            "unprotect with entropy must not decrypt a blob protected without entropy"
        );
    }

    // -- tampered ciphertext ------------------------------------------------

    #[test]
    fn tampered_ciphertext_fails_with_a_clear_error() {
        let plaintext = b"do not touch me".to_vec();
        let mut envelope = protect(plaintext, None).expect("protect should succeed");
        let last = envelope.len() - 1;
        envelope[last] ^= 0xFF;

        let result = unprotect(&envelope, None);
        assert!(result.is_err(), "tampered ciphertext must not decrypt");
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("CryptUnprotectData failed"),
            "error should name the failing call: {message}"
        );
    }

    #[test]
    fn tampered_ciphertext_byte_in_the_middle_fails() {
        let plaintext = b"a somewhat longer plaintext to tamper in the middle of".to_vec();
        let mut envelope = protect(plaintext, None).expect("protect should succeed");
        let mid = HEADER_LEN + (envelope.len() - HEADER_LEN) / 2;
        envelope[mid] ^= 0x01;

        assert!(unprotect(&envelope, None).is_err());
    }

    // -- envelope version handling --------------------------------------

    #[test]
    fn unknown_version_is_rejected_with_a_clear_error() {
        let plaintext = b"secret".to_vec();
        let mut envelope = protect(plaintext, None).expect("protect should succeed");
        envelope[4] = 99;

        let result = unprotect(&envelope, None);
        assert!(result.is_err());
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("version 99") && message.contains("not supported"),
            "error should name the offending version: {message}"
        );
    }

    #[test]
    fn wrong_magic_is_rejected_with_a_clear_error() {
        let plaintext = b"secret".to_vec();
        let mut envelope = protect(plaintext, None).expect("protect should succeed");
        envelope[0..4].copy_from_slice(b"NOPE");

        let result = unprotect(&envelope, None);
        assert!(result.is_err());
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("unrecognized magic"),
            "error should name the problem: {message}"
        );
    }

    #[test]
    fn truncated_envelope_is_rejected_without_panicking() {
        for len in 0..HEADER_LEN {
            let envelope = vec![0u8; len];
            let result = unprotect(&envelope, None);
            assert!(
                result.is_err(),
                "a {len}-byte envelope must be rejected, not panic"
            );
        }
    }

    #[test]
    fn envelope_with_header_but_no_ciphertext_is_rejected_without_panicking() {
        let mut envelope = Vec::with_capacity(HEADER_LEN);
        envelope.extend_from_slice(&MAGIC);
        envelope.push(VERSION);
        // No ciphertext bytes at all: CryptUnprotectData should fail on
        // this cleanly rather than this function panicking beforehand.
        let result = unprotect(&envelope, None);
        assert!(result.is_err());
    }

    // -- output shape -------------------------------------------------------

    #[test]
    fn protect_output_starts_with_the_envelope_header() {
        let envelope = protect(b"x".to_vec(), None).expect("protect should succeed");
        assert_eq!(&envelope[0..4], &MAGIC);
        assert_eq!(envelope[4], VERSION);
    }

    #[test]
    fn ciphertext_never_contains_the_plaintext_verbatim() {
        // Not a cryptographic property test -- just a sanity check that
        // protect() is not accidentally a no-op passthrough.
        let plaintext = b"this exact phrase must not appear in the envelope".to_vec();
        let envelope = protect(plaintext.clone(), None).expect("protect should succeed");
        let envelope_str = envelope
            .windows(plaintext.len())
            .any(|w| w == plaintext.as_slice());
        assert!(
            !envelope_str,
            "ciphertext must not contain the plaintext verbatim"
        );
    }
}
