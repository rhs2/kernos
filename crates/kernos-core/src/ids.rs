//! Typed identifiers: a prefix plus a 26-character lowercase Crockford base32 ULID.

use rand::RngCore;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Generates a ULID (48-bit millisecond timestamp, 80 random bits) rendered in
/// lowercase Crockford base32.
pub fn ulid(now_ms: i64) -> String {
    let mut random = [0u8; 10];
    rand::thread_rng().fill_bytes(&mut random);
    let timestamp = (now_ms.max(0) as u128) & ((1u128 << 48) - 1);
    let mut value: u128 = timestamp << 80;
    for (i, byte) in random.iter().enumerate() {
        value |= (*byte as u128) << (8 * (9 - i));
    }
    let mut out = String::with_capacity(26);
    for i in (0..26).rev() {
        let index = ((value >> (i * 5)) & 31) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

/// A new identifier such as `run_01j6zq5v9k3m8x2w4y7a0b1c2d`.
pub fn new_id(prefix: &str, now_ms: i64) -> String {
    format!("{prefix}_{}", ulid(now_ms))
}

/// True when the text is a well-formed identifier with the given prefix.
pub fn is_id(prefix: &str, text: &str) -> bool {
    match text
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    {
        Some(body) => body.len() == 26 && body.bytes().all(|b| ALPHABET.contains(&b)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_well_formed_and_unique() {
        let a = new_id("run", 1_788_609_600_000);
        let b = new_id("run", 1_788_609_600_000);
        assert!(is_id("run", &a));
        assert!(is_id("run", &b));
        assert_ne!(a, b);
        assert!(!is_id("stp", &a));
        assert!(!is_id("run", "run_short"));
        assert_eq!(a.len(), 4 + 26);
    }

    #[test]
    fn later_timestamps_sort_later() {
        let a = ulid(1_000);
        let b = ulid(2_000_000);
        assert!(a < b);
    }
}
