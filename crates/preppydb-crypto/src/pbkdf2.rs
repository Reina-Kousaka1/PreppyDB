//! PBKDF2-HMAC-SHA-256 (RFC 8018 / PKCS#5 v2.1) used for password hashing.

use crate::hmac::hmac_sha256;

/// Derive `out_len` bytes from `password` and `salt` with `iterations` rounds.
///
/// `iterations` is clamped to at least 1. Callers must choose a work factor; the
/// server uses [`crate::pbkdf2::DEFAULT_ITERATIONS`] unless a record says otherwise
/// (so that stored hashes stay verifiable after a policy change).
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out_len: usize,
) -> Vec<u8> {
    let iterations = iterations.max(1);
    let mut out = Vec::with_capacity(out_len);
    let mut block_index: u32 = 1;
    while out.len() < out_len {
        let mut message = Vec::with_capacity(salt.len() + 4);
        message.extend_from_slice(salt);
        message.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256(password, &message);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha256(password, &u);
            for i in 0..32 {
                t[i] ^= u[i];
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
        // Guard against absurd out_len values (2^32 blocks would never finish anyway).
        if block_index == u32::MAX {
            break;
        }
    }
    out.truncate(out_len);
    out
}

/// Default PBKDF2 work factor for new password hashes.
pub const DEFAULT_ITERATIONS: u32 = 120_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex_encode;

    #[test]
    fn rfc7914_vectors() {
        assert_eq!(
            hex_encode(&pbkdf2_hmac_sha256(b"passwd", b"salt", 1, 64)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
        assert_eq!(
            hex_encode(&pbkdf2_hmac_sha256(b"Password", b"NaCl", 80000, 64)),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
    }

    #[test]
    fn empty_password_and_salt() {
        let dk = pbkdf2_hmac_sha256(b"", b"", 1, 32);
        assert_eq!(dk.len(), 32);
        // Deterministic: same input, same output.
        assert_eq!(dk, pbkdf2_hmac_sha256(b"", b"", 1, 32));
    }

    #[test]
    fn truncates_to_requested_length() {
        let dk = pbkdf2_hmac_sha256(b"pw", b"salt", 2, 20);
        assert_eq!(dk.len(), 20);
        let long = pbkdf2_hmac_sha256(b"pw", b"salt", 2, 80);
        assert_eq!(&long[..20], &dk[..]);
    }
}
