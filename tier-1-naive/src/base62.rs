//! Base62 encoding: `u64` <-> short string.
//!
//! # Why base62?
//!
//! We need to turn a number (our counter, see `store.rs`) into a short string
//! that is safe to put in a URL path. Base62 uses the alphabet
//! `0-9 a-z A-Z` -- 62 symbols, every one of which is unreserved in a URL, so
//! no percent-encoding is ever needed.
//!
//! * base10 (`"1234567"`)  -> 10 symbols/char, long codes.
//! * base16 (`"12D687"`)   -> 16 symbols/char.
//! * base62 (`"5BAA"`)     -> 62 symbols/char, ~1.7x shorter than base16.
//! * base64 -> 64 symbols, but includes `+` and `/`, which are *not* URL-safe.
//!   (There is a "URL-safe base64" variant using `-` and `_`; we avoid it
//!   because both are easy to mangle when a link is read aloud or line-wrapped
//!   in an email.)
//!
//! # base62 is an ENCODING, not a HASH
//!
//! This distinction is the whole point of the module, and it comes back in
//! Tier 2:
//!
//! | | base62 encoding | hash function (MD5/SHA-256) |
//! |---|---|---|
//! | Direction | **reversible** — `decode(encode(n)) == n` | **one-way** |
//! | Collisions | **impossible** — it is a bijection | possible (truncated) |
//! | Output size | grows with input | fixed |
//! | Input | a number | any bytes |
//!
//! Because `encode` is a bijection from `u64` to strings, distinct counter
//! values *cannot* produce the same code. Uniqueness is guaranteed by
//! construction, not by luck. That is why Tier 2 calls this "Path A".

// `decode` is not called by `main.rs` in this tier -- only by the tests and,
// from Tier 2 onward, by code that needs to reverse a code back to its
// counter value. Without this the compiler rightly warns it is dead.
#![allow(dead_code)]

/// The base62 alphabet. Index `i` in this slice is the digit with value `i`.
///
/// Order matters: `0..9` then `a..z` then `A..Z`. Any consistent ordering
/// works as long as `encode` and `decode` agree, but this one is conventional
/// and makes codes sort in a vaguely sensible way.
const ALPHABET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

const BASE: u64 = 62;

/// The error returned when a string is not valid base62.
///
/// We define a real error type rather than returning `Option` so the HTTP
/// layer can tell the two failure modes apart in a 400 response.
#[derive(Debug, PartialEq, Eq)]
pub enum Base62Error {
    /// A character outside the base62 alphabet, e.g. `'-'` or `'!'`.
    /// Carries the offending character so the caller can report it.
    InvalidCharacter(char),
    /// The string decodes to a number larger than `u64::MAX`.
    Overflow,
}

/// Encode a `u64` into its base62 representation.
///
/// ```text
/// encode(0)     == "0"
/// encode(61)    == "Z"
/// encode(62)    == "10"
/// encode(12345) == "3d7"
/// ```
pub fn encode(mut n: u64) -> String {
    if n == 0 {
        return String::from("0")
    }

    let mut encoded_chars: Vec<u8> = Vec::new();
    
    while n > 0 {
        let next_char: u8 = ALPHABET[n as usize % 62];
        encoded_chars.push(next_char);
        n = n / BASE
    }

    encoded_chars.reverse();
    return String::from_utf8(encoded_chars).expect("Alphabet is ASCII"); 
}

/// Decode a base62 string back into a `u64`.
///
/// This is the exact inverse of [`encode`]: `decode(&encode(n)) == Ok(n)` for
/// every `n`. That round-trip property is what the property test at the bottom
/// of this file checks.
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
        // 62 is the first two-digit number, exactly like 10 in base 10.
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
        // 11 base62 digits of 'Z' is comfortably larger than u64::MAX
        // (62^11 ≈ 5.2e19 > 1.8e19).
        assert_eq!(decode("ZZZZZZZZZZZ"), Err(Base62Error::Overflow));
    }

    /// The property that makes base62 an *encoding* rather than a hash:
    /// it round-trips. A hash function could never pass this test.
    #[test]
    fn round_trips() {
        for n in [0u64, 1, 61, 62, 3843, 3844, 999_999, u64::MAX] {
            assert_eq!(decode(&encode(n)), Ok(n), "round trip failed for {n}");
        }
    }

    /// Codes stay short: the whole u64 range fits in 11 characters, and the
    /// first ~3.5 trillion codes fit in 7 — the number we computed in Tier 0.
    #[test]
    fn codes_are_short() {
        assert_eq!(encode(u64::MAX).len(), 11);
        assert!(encode(3_500_000_000_000).len() <= 7);
    }
}
