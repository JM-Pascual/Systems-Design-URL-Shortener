//! Base62 encoding: `u64` <-> short string.
//!
//! Unchanged from Tier 1 — this tier's new idea is persistence, not encoding.
//! See `tier-1-naive/src/base62.rs` for the full write-up.

#![allow(dead_code)]

const ALPHABET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

const BASE: u64 = 62;

#[derive(Debug, PartialEq, Eq)]
pub enum Base62Error {
    InvalidCharacter(char),
    Overflow,
}

pub fn encode(mut n: u64) -> String {
    if n == 0 {
        return String::from("0");
    }

    let mut encoded_chars: Vec<u8> = Vec::new();

    while n > 0 {
        let next_char: u8 = ALPHABET[n as usize % 62];
        encoded_chars.push(next_char);
        n /= BASE;
    }

    encoded_chars.reverse();
    String::from_utf8(encoded_chars).expect("alphabet is ASCII")
}

pub fn decode(s: &str) -> Result<u64, Base62Error> {
    let mut n: u64 = 0;
    for b in s.bytes() {
        let value: u64 = match ALPHABET.iter().position(|&a| a == b) {
            Some(idx) => idx as u64,
            None => return Err(Base62Error::InvalidCharacter(b as char)),
        };
        n = n
            .checked_mul(BASE)
            .and_then(|x| x.checked_add(value))
            .ok_or(Base62Error::Overflow)?;
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_values() {
        assert_eq!(encode(0), "0");
        assert_eq!(encode(1), "1");
        assert_eq!(encode(9), "9");
        assert_eq!(encode(10), "a");
        assert_eq!(encode(35), "z");
        assert_eq!(encode(36), "A");
        assert_eq!(encode(61), "Z");
        assert_eq!(encode(62), "10");
        assert_eq!(encode(12345), "3d7");
    }

    #[test]
    fn decodes_known_values() {
        assert_eq!(decode("0"), Ok(0));
        assert_eq!(decode("z"), Ok(35));
        assert_eq!(decode("Z"), Ok(61));
        assert_eq!(decode("10"), Ok(62));
        assert_eq!(decode("3d7"), Ok(12345));
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(decode("ab-cd"), Err(Base62Error::InvalidCharacter('-')));
        assert_eq!(decode("hello!"), Err(Base62Error::InvalidCharacter('!')));
    }

    #[test]
    fn detects_overflow() {
        assert_eq!(decode("ZZZZZZZZZZZ"), Err(Base62Error::Overflow));
    }

    #[test]
    fn round_trips() {
        for n in [0u64, 1, 61, 62, 3843, 3844, 999_999, u64::MAX] {
            assert_eq!(decode(&encode(n)), Ok(n), "round trip failed for {n}");
        }
    }

    #[test]
    fn codes_are_short() {
        assert_eq!(encode(u64::MAX).len(), 11);
        assert!(encode(3_500_000_000_000).len() <= 7);
    }
}
