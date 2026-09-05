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
//! * base64                -> 64 symbols, but includes `+` and `/`, which are
//!                            *not* URL-safe. (There is a "URL-safe base64"
//!                            variant using `-` and `_`; we avoid it because
//!                            `-` and `_` are easy to mangle when a link is
//!                            read aloud or line-wrapped in an email.)
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
///
/// # TODO(you): implement this
///
/// The algorithm is repeated division, the same one you would use to convert
/// to binary by hand:
///
/// 1. **Special-case `n == 0`.** The loop below runs zero times for `n == 0`,
///    which would produce an empty string. Return `"0"` instead.
/// 2. Create an empty `Vec<u8>` to collect digit bytes.
/// 3. While `n > 0`:
///    - take `remainder = n % BASE` — this is the value of the *least*
///      significant digit;
///    - push `ALPHABET[remainder as usize]` onto the vec;
///    - set `n = n / BASE`.
/// 4. The digits came out least-significant-first, so **reverse** the vec.
/// 5. Turn the bytes into a `String`.
///
/// # Rust notes for step 5
///
/// You have a `Vec<u8>` and you want a `String`. Three options, in increasing
/// order of "should I really?":
///
/// * `String::from_utf8(v).expect("alphabet is ASCII")` — allocates nothing
///   extra, validates the bytes are UTF-8. The `expect` can never fire because
///   `ALPHABET` is pure ASCII, and ASCII is always valid UTF-8.
/// * `v.iter().map(|&b| b as char).collect::<String>()` — also fine, and
///   avoids the `expect`, at the cost of one extra pass.
/// * `unsafe { String::from_utf8_unchecked(v) }` — don't. The safe version is
///   not measurably slower here and `unsafe` in a teaching repo is a smell.
///
/// Note the parameter is `mut n`: taking the parameter by value and mutating
/// the local copy is idiomatic Rust and avoids a separate `let mut` binding.
/// `u64` is `Copy`, so the caller's variable is untouched.
pub fn encode(mut n: u64) -> String {
    let _ = &mut n; // remove this line once you start using `n`
    todo!("implement base62 encoding — see the steps above")
}

/// Decode a base62 string back into a `u64`.
///
/// This is the exact inverse of [`encode`]: `decode(&encode(n)) == Ok(n)` for
/// every `n`. That round-trip property is what the property test at the bottom
/// of this file checks.
///
/// # TODO(you): implement this
///
/// The algorithm is Horner's method — accumulate left to right:
///
/// 1. Start with `let mut n: u64 = 0;`
/// 2. For each byte `b` of the input (`s.bytes()`):
///    - find its digit value: the index of `b` in `ALPHABET`;
///    - if it is not in the alphabet, return
///      `Err(Base62Error::InvalidCharacter(b as char))`;
///    - `n = n * BASE + value`.
/// 3. Return `Ok(n)`.
///
/// # Handling overflow (step 2)
///
/// `n * BASE + value` will panic in debug builds and silently wrap in release
/// builds if it exceeds `u64::MAX`. Silently wrapping means a bogus 20-character
/// code would resolve to some *valid* short code's URL — a real bug. Use the
/// checked arithmetic methods, which return `Option`:
///
/// ```ignore
/// n = n.checked_mul(BASE)
///      .and_then(|x| x.checked_add(value))
///      .ok_or(Base62Error::Overflow)?;
/// ```
///
/// Read that as: try to multiply; if that worked, try to add; if either step
/// overflowed we have `None`, so turn it into our error and bail out with `?`.
///
/// # Rust notes
///
/// * To find a byte's index in the alphabet:
///   `ALPHABET.iter().position(|&a| a == b)` returns `Option<usize>`.
///   (This is a linear scan over 62 bytes. Fine here. If you want to make it
///   O(1), build a 256-entry reverse-lookup table as a `const` — a good
///   optional exercise.)
/// * Prefer `s.bytes()` over `s.chars()`: our alphabet is ASCII, and iterating
///   bytes sidesteps the "a `char` is a Unicode scalar value, not a byte"
///   subtlety entirely. A multi-byte UTF-8 character will simply fail the
///   alphabet lookup on its first byte.
/// * What should an *empty* string decode to? Under the algorithm above the
///   loop never runs and you get `Ok(0)`. Decide whether you are happy with
///   that; the HTTP layer never passes an empty code, so either answer is
///   defensible — but write a test for whichever you choose.
pub fn decode(s: &str) -> Result<u64, Base62Error> {
    let _ = s; // remove this line once you start using `s`
    todo!("implement base62 decoding — see the steps above")
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
