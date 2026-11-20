//! Bit-exact port of `Utils.encodeId` / `Utils.decodeId` from `src/utils/utils.ts`.
//!
//! Algorithm (see `src/utils/utils.ts`):
//!
//! * alphabet is `0123456789abcdefghijklmnopqrstuvwxyz` (base 36);
//! * `rotationNumber = Number(String(id).slice(-2)) % 36`;
//! * the alphabet is rotated left by `rotationNumber`;
//! * the id is printed in the rotated base, then the rotation index itself
//!   (as a base-36 digit) is appended as the last character.
//!
//! Generated ids (`get_id()`) are 13 characters long, which is why the decoder
//! rejects any other length — exactly like the TypeScript implementation.

use std::fmt;

pub const CODING_BASE: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
pub const ENCODED_ID_LENGTH: usize = 13;
const RADIX: u64 = 36;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    InvalidLength(usize),
    InvalidChar(char),
    RotationMismatch,
    Overflow,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::InvalidLength(len) => write!(
                f,
                "The ID must consist of {ENCODED_ID_LENGTH} characters, got {len}."
            ),
            IdError::InvalidChar(ch) => write!(f, "The ID contains an invalid character: {ch}."),
            IdError::RotationMismatch => write!(f, "The ID is incorrect."),
            IdError::Overflow => write!(f, "The ID is out of the signed 64-bit range."),
        }
    }
}

impl std::error::Error for IdError {}

/// `Utils.getRotationNumber`: last two decimal digits modulo the alphabet size.
fn rotation_number(id: i64) -> usize {
    (id.unsigned_abs() % 100) as usize % CODING_BASE.len()
}

/// `Utils.rotate`: `array.slice(n).concat(array.slice(0, n))`.
fn rotated_base(rotation: usize) -> [u8; CODING_BASE.len()] {
    let mut base = [0u8; CODING_BASE.len()];
    for (index, slot) in base.iter_mut().enumerate() {
        *slot = CODING_BASE[(index + rotation) % CODING_BASE.len()];
    }
    base
}

/// `Utils.encodeId`. Returns an empty string for the zero id, just like the
/// original implementation (`if (bigIntId)`).
pub fn encode_id(id: i64) -> String {
    if id == 0 {
        return String::new();
    }

    let rotation = rotation_number(id);
    let base = rotated_base(rotation);

    let mut value = id.unsigned_abs();
    let mut digits = [0u8; 16];
    let mut cursor = digits.len();

    while value > 0 {
        cursor -= 1;
        digits[cursor] = base[(value % RADIX) as usize];
        value /= RADIX;
    }

    let mut encoded = String::with_capacity(digits.len() - cursor + 1);
    for digit in &digits[cursor..] {
        encoded.push(*digit as char);
    }
    encoded.push(CODING_BASE[rotation] as char);

    encoded
}

/// `Utils.encodeIdOrNull`.
pub fn encode_id_opt(id: Option<i64>) -> Option<String> {
    id.filter(|value| *value != 0).map(encode_id)
}

/// `Utils.decodeId` with the same validation rules and the same re-check.
pub fn decode_id(input: &str) -> Result<i64, IdError> {
    let bytes = input.as_bytes();

    if bytes.len() != ENCODED_ID_LENGTH {
        return Err(IdError::InvalidLength(bytes.len()));
    }

    let (suffix, long_part) = bytes.split_last().expect("length checked above");

    let rotation = CODING_BASE
        .iter()
        .position(|candidate| candidate == suffix)
        .ok_or(IdError::InvalidChar(*suffix as char))?;

    let base = rotated_base(rotation);

    let mut value: u64 = 0;
    for byte in long_part {
        let digit = base
            .iter()
            .position(|candidate| candidate == byte)
            .ok_or(IdError::InvalidChar(*byte as char))? as u64;

        value = value.checked_mul(RADIX).ok_or(IdError::Overflow)?;
        value = value.checked_add(digit).ok_or(IdError::Overflow)?;
    }

    let value = i64::try_from(value).map_err(|_| IdError::Overflow)?;

    if rotation_number(value) != rotation {
        return Err(IdError::RotationMismatch);
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors produced by the original TypeScript implementation
    /// (`power-radix@2.5.6` + `Utils.encodeId`), realistic `get_id()` range.
    const ROUND_TRIP: &[(i64, &str)] = &[
        (1_844_674_407_370_955_161, "3p837si3kczqp"),
        (2_099_999_999_999_999_999, "6p46m4jdid42r"),
        (2_100_000_000_000_000_000, "fydfvdsmrmdc0"),
        (2_200_000_000_000_000_123, "3cdpji82xpl6n"),
        (4_123_456_789_012_345_678, "1hzdlz2ksrgs6"),
        (999_999_999_999_999_999, "yc955oqovboir"),
        (4_611_686_018_427_387_903, "24flccmma7a63"),
        (1_234_567_890_123_456_789, "qu5i90kqn4wqh"),
    ];

    /// Short ids encode to short strings (the original does not zero-pad), so
    /// only the forward direction is comparable.
    const ENCODE_ONLY: &[(i64, &str)] = &[
        (1, "21"),
        (7, "e7"),
        (35, "yz"),
        (36, "100"),
        (37, "221"),
        (100, "2s0"),
        (1_234_567, "lbgev"),
        (1_234_567_890, "2xjk60i"),
        (9_007_199_254_740_991, "lz7btq8tlzej"),
        (9_223_372_036_854_775_807, "859w7pqa9lfle7"),
    ];

    #[test]
    fn encode_matches_typescript() {
        for (id, expected) in ROUND_TRIP.iter().chain(ENCODE_ONLY) {
            assert_eq!(encode_id(*id), *expected, "encode_id({id})");
        }
    }

    #[test]
    fn round_trip_matches_typescript() {
        for (id, encoded) in ROUND_TRIP {
            assert_eq!(encode_id(*id), *encoded);
            assert_eq!(decode_id(encoded).expect("decode"), *id);
        }
    }

    #[test]
    fn zero_encodes_to_empty_string() {
        assert_eq!(encode_id(0), "");
        assert_eq!(encode_id_opt(None), None);
        assert_eq!(encode_id_opt(Some(0)), None);
        assert_eq!(
            encode_id_opt(Some(1_844_674_407_370_955_161)),
            Some("3p837si3kczqp".to_string())
        );
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(decode_id(""), Err(IdError::InvalidLength(0)));
        assert_eq!(decode_id("3p837si3kczq"), Err(IdError::InvalidLength(12)));
        assert_eq!(decode_id("3p837si3kczqpa"), Err(IdError::InvalidLength(14)));
    }

    #[test]
    fn decode_rejects_broken_ids() {
        // Rotation suffix tampered with: the re-check must fail, like in TS.
        assert!(matches!(
            decode_id("3p837si3kczq0"),
            Err(IdError::RotationMismatch)
        ));
        // Character outside of the rotated alphabet.
        assert!(matches!(
            decode_id("3p837si3kcz$q"),
            Err(IdError::InvalidChar('$'))
        ));
    }
}
