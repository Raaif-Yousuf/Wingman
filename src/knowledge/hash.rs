//! SHA-256 for the content-hash check, through Windows CNG (no crate).
//!
//! The hash of a file's bytes decides whether it is parsed again, and the hash
//! of a chunk's text decides whether an edit keeps that chunk's row.

use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

/// The hash could not be computed. Callers treat it as "changed" (parse the
/// file again), never as "unchanged": the safe outcome is more work.
#[derive(Debug, PartialEq, Eq)]
pub struct HashError;

/// Raw SHA-256 of `data`. Also used by the OAuth PKCE challenge.
pub fn sha256(data: &[u8]) -> Result<[u8; 32], HashError> {
    let mut out = [0u8; 32];
    // SAFETY: both slices are valid for the whole call; the algorithm handle
    // is a pseudo-handle owned by the OS.
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) }
        .ok()
        .map_err(|_| HashError)?;
    Ok(out)
}

/// Lower-case hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> Result<String, HashError> {
    Ok(hex(&sha256(data)?))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_vectors() {
        // FIPS 180-2 test vectors.
        assert_eq!(
            sha256_hex(b"").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").unwrap(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_large_input_hashes() {
        let data = vec![b'a'; 3 * 1024 * 1024];
        assert_eq!(sha256_hex(&data).unwrap().len(), 64);
    }

    #[test]
    fn one_changed_byte_changes_the_hash() {
        assert_ne!(sha256_hex(b"hello").unwrap(), sha256_hex(b"hellp").unwrap());
    }
}
