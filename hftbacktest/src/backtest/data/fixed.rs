use rust_decimal::Decimal;
use thiserror::Error;

pub const DATA_SCALE: u32 = 8;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseFixedError {
    #[error("expected a plain decimal number")]
    Syntax,
    #[error("value cannot be represented exactly at scale 8")]
    Precision,
    #[error("scaled value exceeds the i64 range")]
    Overflow,
}

/// Parses a plain decimal field without rounding or passing through floating point.
pub fn parse_fixed(input: &[u8]) -> Result<i64, ParseFixedError> {
    let (negative, digits) = match input.first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    };
    let mut value = 0_i64;
    let mut fractional = None;
    let mut integer_digits = 0;
    let mut fractional_digits = 0;
    for &byte in digits {
        if byte == b'.' && fractional.is_none() && integer_digits > 0 {
            fractional = Some(0_u32);
            continue;
        }
        if !byte.is_ascii_digit() {
            return Err(ParseFixedError::Syntax);
        }
        if let Some(count) = fractional.as_mut() {
            fractional_digits += 1;
            if *count == DATA_SCALE {
                if byte != b'0' {
                    return Err(ParseFixedError::Precision);
                }
                continue;
            }
            *count += 1;
        } else {
            integer_digits += 1;
        }
        // Accumulating negatively also permits i64::MIN without an intermediate overflow.
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_sub(i64::from(byte - b'0')))
            .ok_or(ParseFixedError::Overflow)?;
    }
    if integer_digits == 0 || (fractional.is_some() && fractional_digits == 0) {
        return Err(ParseFixedError::Syntax);
    }
    for _ in fractional.unwrap_or(0)..DATA_SCALE {
        value = value.checked_mul(10).ok_or(ParseFixedError::Overflow)?;
    }
    if negative {
        Ok(value)
    } else {
        value.checked_neg().ok_or(ParseFixedError::Overflow)
    }
}

pub fn decode_fixed(value: i64) -> Decimal {
    Decimal::new(value, DATA_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_values_and_full_signed_range() {
        for (input, expected) in [
            ("0", 0),
            ("-0.000000000", 0),
            ("+1.23", 123_000_000),
            ("1.230000000000", 123_000_000),
            ("0.00000001", 1),
            ("92233720368.54775807", i64::MAX),
            ("-92233720368.54775808", i64::MIN),
        ] {
            assert_eq!(parse_fixed(input.as_bytes()), Ok(expected), "{input}");
        }
    }

    #[test]
    fn rejects_precision_loss_overflow_and_non_decimal_syntax() {
        for input in ["0.000000001", "1.000000001", "-0.000000001"] {
            assert_eq!(
                parse_fixed(input.as_bytes()),
                Err(ParseFixedError::Precision)
            );
        }
        for input in [
            "92233720368.54775808",
            "-92233720368.54775809",
            "999999999999999999999",
        ] {
            assert_eq!(
                parse_fixed(input.as_bytes()),
                Err(ParseFixedError::Overflow)
            );
        }
        for input in [
            "", "+", "-", ".1", "1.", "1e-8", "NaN", "inf", " 1", "1 ", "1.2.3",
        ] {
            assert_eq!(
                parse_fixed(input.as_bytes()),
                Err(ParseFixedError::Syntax),
                "{input}"
            );
        }
    }

    #[test]
    fn decoding_preserves_lowest_unit() {
        assert_eq!(decode_fixed(123_000_001).to_string(), "1.23000001");
        assert_eq!(decode_fixed(i64::MIN).to_string(), "-92233720368.54775808");
    }
}
