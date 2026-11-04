//! Base64url (RFC 4648 §5) without padding, used for tokens and binary blobs in JSON.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode bytes using the URL-safe alphabet without padding.
pub fn base64_url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let (chunks, remainder) = data.as_chunks::<3>();
    for chunk in chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 6) as usize & 0x3f] as char);
        out.push(ALPHABET[n as usize & 0x3f] as char);
    }
    match remainder {
        [] => {}
        [a] => {
            let n = (*a as u32) << 16;
            out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        }
        [a, b] => {
            let n = ((*a as u32) << 16) | ((*b as u32) << 8);
            out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
            out.push(ALPHABET[(n >> 6) as usize & 0x3f] as char);
        }
        _ => unreachable!("chunks_exact leaves at most two bytes"),
    }
    out
}

/// Decode URL-safe base64. Padding characters are tolerated at the end.
pub fn base64_url_decode(text: &str) -> Result<Vec<u8>, String> {
    let trimmed = text.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 3);
    let mut accum: u32 = 0;
    let mut bits = 0u32;
    for (idx, byte) in bytes.iter().enumerate() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            other => {
                return Err(format!(
                    "invalid base64 character '{}' at position {}",
                    *other as char, idx
                ))
            }
        };
        accum = (accum << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accum >> bits) as u8);
            accum &= (1u32 << bits) - 1;
        }
    }
    // A single trailing character can never encode a whole byte.
    if bits >= 6 {
        return Err("truncated base64 input".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        let cases: &[(&str, &[u8])] = &[
            ("", b""),
            ("Zg", b"f"),
            ("Zm8", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg", b"foob"),
            ("Zm9vYmE", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
        ];
        for (encoded, raw) in cases {
            assert_eq!(base64_url_encode(raw), *encoded);
            assert_eq!(base64_url_decode(encoded).unwrap(), raw.to_vec());
        }
    }

    #[test]
    fn url_safe_alphabet() {
        assert_eq!(base64_url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
    }

    #[test]
    fn padding_tolerated() {
        assert_eq!(base64_url_decode("Zm9vYg==").unwrap(), b"foob".to_vec());
    }

    #[test]
    fn rejects_garbage() {
        assert!(base64_url_decode("!!!").is_err());
        assert!(base64_url_decode("Z").is_err());
    }

    #[test]
    fn roundtrip_random_bytes() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 31 % 256) as u8).collect();
        for len in 0..=data.len() {
            let slice = &data[..len];
            assert_eq!(
                base64_url_decode(&base64_url_encode(slice)).unwrap(),
                slice.to_vec()
            );
        }
    }
}
