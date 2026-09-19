use rust_decimal::Decimal;
use thiserror::Error;

pub const DATA_SCALE: u32 = 8;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseFixedError {
    #[error("expected a decimal number")]
    Syntax,
    #[error("value cannot be represented exactly at scale 8")]
    Precision,
    #[error("scaled value exceeds the i64 range")]
    Overflow,
}

/// Parses a decimal field without rounding or passing through floating point.
pub fn parse_fixed(input: &[u8]) -> Result<i64, ParseFixedError> {
    let (negative, unsigned) = match input.first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    };
    let (mantissa, exponent) =
        if let Some(index) = unsigned.iter().position(|&b| b == b'e' || b == b'E') {
            let exponent = &unsigned[index + 1..];
            let (negative, digits) = match exponent.first() {
                Some(b'-') => (true, &exponent[1..]),
                Some(b'+') => (false, &exponent[1..]),
                _ => (false, exponent),
            };
            if digits.is_empty() {
                return Err(ParseFixedError::Syntax);
            }
            let mut value = 0_i128;
            for &byte in digits {
                if !byte.is_ascii_digit() {
                    return Err(ParseFixedError::Syntax);
                }
                value = value
                    .saturating_mul(10)
                    .saturating_add(i128::from(byte - b'0'));
            }
            (&unsigned[..index], if negative { -value } else { value })
        } else {
            (unsigned, 0)
        };

    let mut integer_digits = 0_usize;
    let mut fractional_digits = 0_usize;
    let mut digit_count = 0_usize;
    let mut decimal_seen = false;
    let mut first_nonzero = None;
    let mut last_nonzero = 0_usize;
    for &byte in mantissa {
        if byte == b'.' && !decimal_seen && integer_digits > 0 {
            decimal_seen = true;
            continue;
        }
        if !byte.is_ascii_digit() {
            return Err(ParseFixedError::Syntax);
        }
        if decimal_seen {
            fractional_digits += 1;
        } else {
            integer_digits += 1;
        }
        if byte != b'0' {
            first_nonzero.get_or_insert(digit_count);
            last_nonzero = digit_count;
        }
        digit_count += 1;
    }
    if integer_digits == 0 || (decimal_seen && fractional_digits == 0) {
        return Err(ParseFixedError::Syntax);
    }
    let Some(first_nonzero) = first_nonzero else {
        return Ok(0);
    };
    let significant_digits = last_nonzero - first_nonzero + 1;
    let trailing_zeros = digit_count - last_nonzero - 1;
    let shift = exponent
        .saturating_add(i128::from(DATA_SCALE))
        .saturating_sub(fractional_digits as i128)
        .saturating_add(trailing_zeros as i128);
    if shift < 0 {
        return Err(ParseFixedError::Precision);
    }
    if significant_digits > 19 || shift > (19 - significant_digits) as i128 {
        return Err(ParseFixedError::Overflow);
    }

    let mut value = 0_i64;
    let mut digit_index = 0_usize;
    for &byte in mantissa {
        if byte == b'.' {
            continue;
        }
        if digit_index >= first_nonzero && digit_index <= last_nonzero {
            // Accumulating negatively also permits i64::MIN without an intermediate overflow.
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_sub(i64::from(byte - b'0')))
                .ok_or(ParseFixedError::Overflow)?;
        }
        digit_index += 1;
    }
    for _ in 0..shift as usize {
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
            ("1e-7", 10),
            ("2.5E-7", 25),
            ("1.234567890e1", 1_234_567_890),
            ("+1e+2", 10_000_000_000),
            ("0e-999999999999999999999", 0),
            ("92233720368.54775807", i64::MAX),
            ("9223372036854775807e-8", i64::MAX),
            ("-92233720368.54775808", i64::MIN),
            ("-9223372036854775808e-8", i64::MIN),
        ] {
            assert_eq!(parse_fixed(input.as_bytes()), Ok(expected), "{input}");
        }
    }

    #[test]
    fn rejects_precision_loss_overflow_and_non_decimal_syntax() {
        for input in [
            "0.000000001",
            "1.000000001",
            "-0.000000001",
            "1e-9",
            "1.234567891e-1",
        ] {
            assert_eq!(
                parse_fixed(input.as_bytes()),
                Err(ParseFixedError::Precision)
            );
        }
        for input in [
            "92233720368.54775808",
            "-92233720368.54775809",
            "999999999999999999999",
            "9223372036854775808e-8",
            "1e999999999999999999999",
        ] {
            assert_eq!(
                parse_fixed(input.as_bytes()),
                Err(ParseFixedError::Overflow)
            );
        }
        for input in [
            "", "+", "-", ".1", "1.", "1e", "1e+", "1e--7", "1e7x", "NaN", "inf", " 1", "1 ",
            "1.2.3",
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
