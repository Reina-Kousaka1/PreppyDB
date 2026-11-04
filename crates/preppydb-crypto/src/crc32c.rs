//! CRC-32C (Castagnoli) used for write-ahead log record and page checksums.

/// Table for the reflected Castagnoli polynomial 0x82F63B78.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x82F6_3B78;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Compute CRC-32C over `data`, starting from `seed` (use 0 for a fresh checksum).
///
/// The result is post-inverted exactly once, so `crc32c(data, crc32c(part1, 0))` is
/// **not** the same as the checksum of the concatenation. Use [`Crc32c`] for streaming.
pub fn crc32c(data: &[u8], seed: u32) -> u32 {
    let mut crc = !seed;
    for byte in data {
        crc = TABLE[((crc ^ *byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Streaming CRC-32C helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    pub fn new() -> Self {
        Crc32c { state: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut crc = !self.state;
        for byte in data {
            crc = TABLE[((crc ^ *byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.state = !crc;
    }

    pub fn finish(&self) -> u32 {
        self.state
    }
}

/// Checksum rendered as 8 hex characters.
pub fn crc32c_hex(data: &[u8]) -> String {
    format!("{:08x}", crc32c(data, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_check_value() {
        // The CRC-32C catalogue "check" value.
        assert_eq!(crc32c(b"123456789", 0), 0xe306_9283);
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"", 0), 0);
        assert_eq!(crc32c(b"a", 0), 0xc1d0_4330);
        assert_eq!(crc32c(b"abc", 0), 0x364b_3fb7);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 256) as u8).collect();
        let expected = crc32c(&data, 0);
        let mut hasher = Crc32c::new();
        for chunk in data.chunks(37) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finish(), expected);
    }

    #[test]
    fn detects_bit_flips() {
        let mut data = b"the quick brown fox".to_vec();
        let clean = crc32c(&data, 0);
        data[3] ^= 0x01;
        assert_ne!(crc32c(&data, 0), clean);
    }
}
