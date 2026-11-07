//! Randomness: an OS-backed source for tokens/salts and a deterministic ChaCha20
//! stream generator for reproducible (seeded) pricing runs and tests.

/// Fill `buf` with bytes from the operating system's cryptographic random source.
///
/// This fails closed when the OS RNG is unavailable; secrets must never fall back to
/// time, process metadata, or other non-cryptographic sources.
pub fn os_random(buf: &mut [u8]) -> Result<(), String> {
    getrandom::fill(buf).map_err(|err| format!("failed to obtain OS randomness: {err}"))
}

/// Convenience wrapper around [`os_random`].
pub struct OsRng;

impl OsRng {
    /// 32 cryptographically random bytes, e.g. for API tokens.
    pub fn bytes32() -> Result<[u8; 32], String> {
        let mut buf = [0u8; 32];
        os_random(&mut buf)?;
        Ok(buf)
    }

    pub fn fill(buf: &mut [u8]) -> Result<(), String> {
        os_random(buf)
    }

    /// Random base64url token of `len` bytes of entropy.
    pub fn token(len: usize) -> Result<String, String> {
        let mut buf = vec![0u8; len];
        os_random(&mut buf)?;
        Ok(crate::base64_url_encode(&buf))
    }
}

/// ChaCha20-based deterministic generator (RFC 7539 block function).
///
/// This is *not* used for secrets: it exists so that "random" price changes can be
/// reproduced exactly from a stored seed (auditing, regression tests).
pub struct ChaCha20Rng {
    key: [u32; 8],
    nonce: [u32; 3],
    counter: u32,
    buffer: [u8; 64],
    buffered: usize,
}

impl ChaCha20Rng {
    /// Build a generator from an arbitrary seed by hashing it to 32 bytes.
    pub fn from_seed(seed: &[u8]) -> Self {
        let digest = crate::sha256::sha256(seed);
        let mut key_words = [0u32; 8];
        for (i, word) in key_words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                digest[i * 4],
                digest[i * 4 + 1],
                digest[i * 4 + 2],
                digest[i * 4 + 3],
            ]);
        }
        let nonce_digest = crate::sha256::sha256(&[digest.as_slice(), b"nonce"].concat());
        let nonce = [
            u32::from_le_bytes([
                nonce_digest[0],
                nonce_digest[1],
                nonce_digest[2],
                nonce_digest[3],
            ]),
            u32::from_le_bytes([
                nonce_digest[4],
                nonce_digest[5],
                nonce_digest[6],
                nonce_digest[7],
            ]),
            u32::from_le_bytes([
                nonce_digest[8],
                nonce_digest[9],
                nonce_digest[10],
                nonce_digest[11],
            ]),
        ];
        ChaCha20Rng {
            key: key_words,
            nonce,
            counter: 0,
            buffer: [0u8; 64],
            buffered: 64,
        }
    }

    /// Build from the OS entropy source.
    pub fn from_entropy() -> Result<Self, String> {
        let mut seed = [0u8; 32];
        os_random(&mut seed)?;
        Ok(ChaCha20Rng::from_seed(&seed))
    }

    fn refill(&mut self) {
        let mut state = [0u32; 16];
        state[0] = 0x6170_7865;
        state[1] = 0x3320_646e;
        state[2] = 0x7962_2d32;
        state[3] = 0x6b20_6574;
        state[4..12].copy_from_slice(&self.key);
        state[12] = self.counter;
        state[13] = self.nonce[0];
        state[14] = self.nonce[1];
        state[15] = self.nonce[2];

        let mut working = state;
        for _ in 0..10 {
            quarter_round(&mut working, 0, 4, 8, 12);
            quarter_round(&mut working, 1, 5, 9, 13);
            quarter_round(&mut working, 2, 6, 10, 14);
            quarter_round(&mut working, 3, 7, 11, 15);
            quarter_round(&mut working, 0, 5, 10, 15);
            quarter_round(&mut working, 1, 6, 11, 12);
            quarter_round(&mut working, 2, 7, 8, 13);
            quarter_round(&mut working, 3, 4, 9, 14);
        }
        for (i, word) in working.iter().enumerate() {
            let mixed = word.wrapping_add(state[i]);
            self.buffer[i * 4..i * 4 + 4].copy_from_slice(&mixed.to_le_bytes());
        }
        self.counter = self.counter.wrapping_add(1);
        self.buffered = 0;
    }

    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        let mut written = 0;
        while written < out.len() {
            if self.buffered == 64 {
                self.refill();
            }
            let take = (64 - self.buffered).min(out.len() - written);
            out[written..written + take]
                .copy_from_slice(&self.buffer[self.buffered..self.buffered + take]);
            self.buffered += take;
            written += take;
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.fill_bytes(&mut buf);
        u64::from_le_bytes(buf)
    }

    /// Uniform value in `[0, bound)`. Rejection sampling avoids modulo bias.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "bound must be positive");
        let zone = u64::MAX - (u64::MAX % bound);
        loop {
            let value = self.next_u64();
            if value < zone {
                return value % bound;
            }
        }
    }

    /// Uniform value in `[low, high]` (inclusive).
    pub fn range_i64(&mut self, low: i64, high: i64) -> i64 {
        assert!(low <= high, "low must not exceed high");
        let span = (high as i128 - low as i128 + 1) as u128;
        let mut buf = [0u8; 16];
        self.fill_bytes(&mut buf);
        let raw = u128::from_le_bytes(buf);
        let offset = (raw % span) as i64;
        low.saturating_add(offset)
    }

    /// Uniform float in `[0, 1)`.
    pub fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chacha20_rfc7539_block() {
        // RFC 7539 §2.3.2 test vector: key and nonce with counter 1.
        let key_bytes: Vec<u8> = (0x00u8..0x20).collect();
        let mut rng = ChaCha20Rng::from_seed(b"placeholder");
        let mut key_words = [0u32; 8];
        for (i, word) in key_words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                key_bytes[i * 4],
                key_bytes[i * 4 + 1],
                key_bytes[i * 4 + 2],
                key_bytes[i * 4 + 3],
            ]);
        }
        rng.key = key_words;
        rng.nonce = [
            u32::from_le_bytes([0x00, 0x00, 0x00, 0x09]),
            u32::from_le_bytes([0x00, 0x00, 0x00, 0x4a]),
            u32::from_le_bytes([0x00, 0x00, 0x00, 0x00]),
        ];
        rng.counter = 1;
        rng.refill();
        // RFC 8439, section 2.3.2, serialized 64-byte block.
        let expected = concat!(
            "10f1e7e4d13b5915500fdd1fa32071c4",
            "c7d1f4c733c068030422aa9ac3d46c4e",
            "d2826446079faa0914c2d705d98b02a2",
            "b5129cd1de164eb9cbd083e8a2503c4e"
        );
        assert_eq!(crate::hex_encode(&rng.buffer), expected);
    }

    #[test]
    fn deterministic_from_seed() {
        let mut a = ChaCha20Rng::from_seed(b"seed-1");
        let mut b = ChaCha20Rng::from_seed(b"seed-1");
        let mut c = ChaCha20Rng::from_seed(b"seed-2");
        let left: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let right: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        let other: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
        assert_eq!(left, right);
        assert_ne!(left, other);
    }

    #[test]
    fn ranges_are_inclusive_and_unbiased_shape() {
        let mut rng = ChaCha20Rng::from_seed(b"range");
        let mut buckets = [0usize; 5];
        for _ in 0..5000 {
            let value = rng.range_i64(-2, 2);
            assert!((-2..=2).contains(&value));
            buckets[(value + 2) as usize] += 1;
        }
        for count in buckets {
            assert!(count > 700, "distribution too skewed: {buckets:?}");
        }
    }

    #[test]
    fn below_respects_bound() {
        let mut rng = ChaCha20Rng::from_seed(b"below");
        for _ in 0..1000 {
            assert!(rng.below(7) < 7);
        }
    }

    #[test]
    fn unit_f64_in_range() {
        let mut rng = ChaCha20Rng::from_seed(b"unit");
        for _ in 0..1000 {
            let v = rng.unit_f64();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn os_random_fills_and_varies() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        os_random(&mut a).unwrap();
        os_random(&mut b).unwrap();
        assert_ne!(a, b);
        assert!(a.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn tokens_are_url_safe_and_unique() {
        let a = OsRng::token(24).unwrap();
        let b = OsRng::token(24).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32); // 24 bytes -> 32 base64 chars
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
