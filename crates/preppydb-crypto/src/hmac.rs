//! HMAC-SHA-256 (RFC 2104).

use crate::sha256::{sha256, Sha256};

const BLOCK: usize = 64;

/// Compute HMAC-SHA-256 over `message` with `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = sha256(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(message);
    let inner_hash = inner.finish();

    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(&inner_hash);
    outer.finish()
}

/// Verify an HMAC-SHA-256 tag in constant time.
pub fn verify_hmac_sha256(key: &[u8], message: &[u8], tag: &[u8]) -> bool {
    let expected = hmac_sha256(key, message);
    crate::constant_time_eq(&expected, tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex_encode;

    fn tag(key: &[u8], msg: &[u8]) -> String {
        hex_encode(&hmac_sha256(key, msg))
    }

    // RFC 4231 test vectors.
    #[test]
    fn rfc4231_case1() {
        assert_eq!(
            tag(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn rfc4231_case2() {
        assert_eq!(
            tag(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc4231_case3() {
        assert_eq!(
            tag(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn rfc4231_case6_long_key() {
        let key = [0xaa; 131];
        assert_eq!(
            tag(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn rfc4231_case7_long_key_and_message() {
        let key = [0xaa; 131];
        let msg = b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.";
        assert_eq!(
            tag(&key, msg),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn verify_helper() {
        let key = b"secret";
        let tag = hmac_sha256(key, b"payload");
        assert!(verify_hmac_sha256(key, b"payload", &tag));
        assert!(!verify_hmac_sha256(key, b"payloaD", &tag));
    }
}
