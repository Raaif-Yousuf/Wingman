//! HMAC-SHA-256 and random bytes through Windows CNG (no crate), for the
//! memory table's dedupe hash. Memory text is encrypted per value, so equal
//! texts cannot be found by SQL; a keyed hash of the normalised text can.

use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCryptHash, BCRYPT_HMAC_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

/// CNG failed. Callers refuse the write: a memory item is never stored
/// without its dedupe hash.
#[derive(Debug, PartialEq, Eq)]
pub struct HmacError;

/// Raw HMAC-SHA-256 of `data` under `key`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<[u8; 32], HmacError> {
    let mut out = [0u8; 32];
    // SAFETY: the slices are valid for the whole call; the algorithm handle is
    // a pseudo-handle owned by the OS.
    unsafe { BCryptHash(BCRYPT_HMAC_SHA256_ALG_HANDLE, Some(key), data, &mut out) }
        .ok()
        .map_err(|_| HmacError)?;
    Ok(out)
}

/// 32 random bytes from the system generator.
pub fn random_key() -> Result<[u8; 32], HmacError> {
    let mut buf = [0u8; 32];
    // SAFETY: `buf` is a valid writable slice for the whole call.
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|_| HmacError)?;
    Ok(buf)
}

pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// The bytes of a hex string, or `None` if it is not whole hex.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_vector() {
        // RFC 4231, test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?").unwrap();
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn the_key_changes_the_hash() {
        let a = hmac_sha256(b"one", b"text").unwrap();
        let b = hmac_sha256(b"two", b"text").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn random_keys_differ_and_hex_round_trips() {
        let a = random_key().unwrap();
        let b = random_key().unwrap();
        assert_ne!(a, b);
        assert_eq!(from_hex(&to_hex(&a)).unwrap(), a.to_vec());
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("é"), None);
    }
}
