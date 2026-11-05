//! Base62 encoding: `u64` -> short string.
//!
//! Same `encode` as Tier 1; see `tier-1-naive/src/base62.rs` for the full
//! write-up and for `decode`, which nothing here needs.

const ALPHABET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

const BASE: u64 = 62;

pub fn encode(mut n: u64) -> String {
    if n == 0 {
        return String::from("0");
    }

    let mut digits = Vec::new();
    while n > 0 {
        digits.push(ALPHABET[(n % BASE) as usize]);
        n /= BASE;
    }

    digits.reverse();
    String::from_utf8(digits).expect("alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_values() {
        assert_eq!(encode(0), "0");
        assert_eq!(encode(61), "Z");
        assert_eq!(encode(62), "10");
        assert_eq!(encode(12345), "3d7");
    }
}
