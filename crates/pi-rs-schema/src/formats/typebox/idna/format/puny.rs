// Punycode (RFC 3492), ported from `packages/schema/src/formats/typebox/idna/format/puny.ts`.
//
// The encoder and decoder here follow the upstream implementation, including its error cases: the decoder
// rejects a non-basic character before the delimiter, a character that is not a punycode digit, and an input
// that ends in the middle of a sequence.

/// The punycode base.
const PUNYCODE_BASE: u64 = 36;
/// The minimum code point threshold.
const PUNYCODE_TMIN: u64 = 1;
/// The maximum code point threshold.
const PUNYCODE_TMAX: u64 = 26;
/// The skew.
const PUNYCODE_SKEW: u64 = 38;
/// The damp.
const PUNYCODE_DAMP: u64 = 700;
/// The initial bias.
const PUNYCODE_INITIAL_BIAS: u64 = 72;
/// The initial code point.
const PUNYCODE_INITIAL_N: u64 = 128;

/// The prefix that marks an ACE (ASCII Compatible Encoding) label.
const ACE_PREFIX: &str = "xn--";

/// The reason a punycode payload could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PunycodeError {
    /// A non-basic (non-ASCII) character appeared before the delimiter.
    NonBasicCharacter,
    /// A character that is not a punycode digit appeared in the payload.
    BadDigitCharacter,
    /// The payload ended in the middle of a punycode sequence.
    UnexpectedEndOfInput,
    /// The decoded code point is not a Unicode scalar value.
    InvalidCodePoint,
}

impl std::fmt::Display for PunycodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            PunycodeError::NonBasicCharacter => "invalid punycode: non-basic before delimiter",
            PunycodeError::BadDigitCharacter => "invalid punycode: bad digit character",
            PunycodeError::UnexpectedEndOfInput => "invalid punycode: unexpected end of input",
            PunycodeError::InvalidCodePoint => "invalid punycode: invalid code point",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PunycodeError {}

/// Whether the value is prefixed with the ACE prefix (`xn--`), compared case-insensitively.
pub fn is_ace_prefixed(value: &str) -> bool {
    value
        .get(..ACE_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ACE_PREFIX))
}

/// JavaScript's `delta >> 1`: the value is truncated to a 32-bit signed integer and shifted right.
fn shift_right_one(value: u64) -> i64 {
    ((value as u32) as i32 >> 1) as i64
}

/// The punycode bias adaptation (RFC 3492 §6.1).
fn adapt(delta: u64, num_points: u64, first_time: bool) -> u64 {
    let mut delta = if first_time {
        delta / PUNYCODE_DAMP
    } else {
        let shifted = shift_right_one(delta);
        if shifted < 0 { 0 } else { shifted as u64 }
    };
    delta = delta.saturating_add(delta / num_points.max(1));
    let mut k: u64 = 0;
    while delta > ((PUNYCODE_BASE - PUNYCODE_TMIN) * PUNYCODE_TMAX) >> 1 {
        delta /= PUNYCODE_BASE - PUNYCODE_TMIN;
        k = k.saturating_add(PUNYCODE_BASE);
    }
    let numerator = (PUNYCODE_BASE - PUNYCODE_TMIN + 1).saturating_mul(delta);
    let denominator = delta.saturating_add(PUNYCODE_SKEW).max(1);
    k.saturating_add(numerator / denominator)
}

/// Decodes a punycode payload into its Unicode form.
pub fn decode(value: &str) -> Result<String, PunycodeError> {
    let mut output: Vec<u32> = Vec::new();
    let mut n: u64 = PUNYCODE_INITIAL_N;
    let mut i: u64 = 0;
    let mut bias: u64 = PUNYCODE_INITIAL_BIAS;
    // The delimiter is the last hyphen of the payload; the code points before it are the basic ones.
    let delimiter = value.rfind('-');
    if let Some(delimiter) = delimiter
        && delimiter > 0
    {
        for character in value[..delimiter].chars() {
            if !character.is_ascii() {
                return Err(PunycodeError::NonBasicCharacter);
            }
            output.push(character as u32);
        }
    }
    let mut index = match delimiter {
        Some(delimiter) => delimiter + 1,
        None => 0,
    };
    while index < value.len() {
        let old_i = i;
        let mut w: u64 = 1;
        let mut k: u64 = PUNYCODE_BASE;
        loop {
            if index >= value.len() {
                return Err(PunycodeError::UnexpectedEndOfInput);
            }
            let character = value[index..]
                .chars()
                .next()
                .ok_or(PunycodeError::UnexpectedEndOfInput)?;
            index += character.len_utf8();
            let digit = if character.is_ascii_lowercase() {
                character as u64 - 'a' as u64
            } else if character.is_ascii_digit() {
                character as u64 - '0' as u64 + 26
            } else {
                return Err(PunycodeError::BadDigitCharacter);
            };
            i = i
                .checked_add(
                    digit
                        .checked_mul(w)
                        .ok_or(PunycodeError::InvalidCodePoint)?,
                )
                .ok_or(PunycodeError::InvalidCodePoint)?;
            let t = if k <= bias {
                PUNYCODE_TMIN
            } else if k >= bias + PUNYCODE_TMAX {
                PUNYCODE_TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            w = w
                .checked_mul(PUNYCODE_BASE - t)
                .ok_or(PunycodeError::InvalidCodePoint)?;
            k = k.saturating_add(PUNYCODE_BASE);
        }
        let out_len = output.len() as u64 + 1;
        bias = adapt(i - old_i, out_len, old_i == 0);
        n = n
            .checked_add(i / out_len)
            .ok_or(PunycodeError::InvalidCodePoint)?;
        i %= out_len;
        let insertion = usize::try_from(i).map_err(|_| PunycodeError::InvalidCodePoint)?;
        let code_point = u32::try_from(n).map_err(|_| PunycodeError::InvalidCodePoint)?;
        output.insert(insertion, code_point);
        i = i.checked_add(1).ok_or(PunycodeError::InvalidCodePoint)?;
    }
    let mut decoded = String::with_capacity(output.len());
    for code_point in output {
        let character = char::from_u32(code_point).ok_or(PunycodeError::InvalidCodePoint)?;
        decoded.push(character);
    }
    Ok(decoded)
}

/// Maps a punycode digit to its character.
fn digit_to_char(digit: u64) -> char {
    if digit < 26 {
        (b'a' + digit as u8) as char
    } else {
        (b'0' + (digit - 26) as u8) as char
    }
}

/// Encodes a Unicode string into its punycode payload, the part that follows the `xn--` ACE prefix.
pub fn encode(input: &str) -> String {
    // The basic (ASCII) code points, and the initial state of the output.
    let basic: String = input
        .chars()
        .filter(|character| character.is_ascii())
        .collect();
    let basic_length = basic.len();
    let mut result = if basic_length > 0 {
        format!("{basic}-")
    } else {
        String::new()
    };
    let code_points: Vec<u32> = input.chars().map(u32::from).collect();
    let mut n: u64 = PUNYCODE_INITIAL_N;
    let mut delta: u64 = 0;
    let mut bias: u64 = PUNYCODE_INITIAL_BIAS;
    let mut handled = basic_length as u64;
    while handled < code_points.len() as u64 {
        let mut m = u64::MAX;
        for code_point in &code_points {
            let code_point = *code_point as u64;
            if code_point >= n && code_point < m {
                m = code_point;
            }
        }
        delta = delta.saturating_add(m.saturating_sub(n).saturating_mul(handled + 1));
        n = m;
        for code_point in &code_points {
            let code_point = *code_point as u64;
            if code_point < n {
                delta = delta.saturating_add(1);
            }
            if code_point == n {
                let mut q = delta;
                let mut k: u64 = PUNYCODE_BASE;
                loop {
                    let t = if k <= bias {
                        PUNYCODE_TMIN
                    } else if k >= bias + PUNYCODE_TMAX {
                        PUNYCODE_TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    let digit = t + ((q - t) % (PUNYCODE_BASE - t));
                    result.push(digit_to_char(digit));
                    q = (q - t) / (PUNYCODE_BASE - t);
                    k = k.saturating_add(PUNYCODE_BASE);
                }
                result.push(digit_to_char(q));
                bias = adapt(delta, handled + 1, handled == basic_length as u64);
                delta = 0;
                handled += 1;
            }
        }
        delta = delta.saturating_add(1);
        n = n.saturating_add(1);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::super::pattern::pattern::{RE_ASCII, RE_NON_ASCII};
    use super::*;

    #[test]
    fn ace_prefix_is_detected_case_insensitively() {
        assert!(is_ace_prefixed("xn--bcher-kva"));
        assert!(is_ace_prefixed("XN--bcher-kva"));
        assert!(is_ace_prefixed("Xn--"));
        assert!(!is_ace_prefixed("xn-"));
        assert!(!is_ace_prefixed("bücher"));
        assert!(!is_ace_prefixed(""));
    }

    #[test]
    fn decodes_a_payload() {
        assert_eq!(decode("bcher-kva").unwrap(), "bücher");
        assert_eq!(decode("mnchen-3ya").unwrap(), "münchen");
        assert_eq!(decode("28j2a3ar1p").unwrap(), "こんにちは");
        assert_eq!(
            decode("3B-ww4c5e180e575a65lsy2b").unwrap(),
            "3年B組金八先生"
        );
    }

    #[test]
    fn encodes_a_payload() {
        assert_eq!(encode("bücher"), "bcher-kva");
        assert_eq!(encode("münchen"), "mnchen-3ya");
        assert_eq!(encode("こんにちは"), "28j2a3ar1p");
        assert_eq!(encode("مثال"), "mgbh0fb");
        assert_eq!(encode("δοκιμή"), "jxalpdlp");
    }

    #[test]
    fn round_trips() {
        for value in ["bücher", "münchen", "こんにちは", "مثال", "δοκιμή"] {
            let payload = encode(value);
            assert_eq!(decode(&payload).unwrap(), value);
        }
    }

    #[test]
    fn rejects_a_non_basic_character_before_the_delimiter() {
        assert_eq!(decode("bücher-kva"), Err(PunycodeError::NonBasicCharacter));
    }

    #[test]
    fn rejects_a_bad_digit_character() {
        assert_eq!(decode("bcher-kv!"), Err(PunycodeError::BadDigitCharacter));
        assert_eq!(decode("bcher-kvA"), Err(PunycodeError::BadDigitCharacter));
    }

    #[test]
    fn rejects_an_unexpected_end_of_input() {
        assert_eq!(decode("bcher-kv"), Err(PunycodeError::UnexpectedEndOfInput));
    }

    #[test]
    fn an_ascii_value_encodes_to_its_basic_form() {
        assert_eq!(encode("example"), "example-");
        assert_eq!(encode(""), "");
        assert!(RE_ASCII.find("example").is_some());
        assert!(RE_NON_ASCII.find("bücher").is_some());
    }
}
