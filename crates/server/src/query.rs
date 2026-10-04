//! Strict query-string decoding (PLAN.md §8.1 "Query decoding").
//!
//! Parameters are percent-decoded per RFC 3986. These are refused, so a key can never be
//! silently corrupted, and each is `400 invalid_request`:
//! - an unescaped `+`, which form encoders send for a space;
//! - an invalid percent escape, or a raw byte outside printable ASCII;
//! - invalid UTF-8 after decoding;
//! - a parameter without `=`, a repeated one, or one the operation does not take.
//!
//! [`encode`] is the inverse used for a 307 `Location`: it re-encodes exactly the decoded
//! bytes (§8.1).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

/// Why a query string was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// A parameter has no `=`.
    NoEquals(String),
    /// A parameter the operation does not take.
    Unknown(String),
    /// A parameter given twice.
    Repeated(String),
    /// An unescaped `+`.
    Plus(String),
    /// A `%` not followed by two hexadecimal digits.
    BadEscape(String),
    /// A raw byte that is not printable ASCII.
    RawByte(String),
    /// The decoded bytes are not UTF-8.
    NotUtf8(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEquals(name) => write!(f, "query parameter {name:?} has no value"),
            Self::Unknown(name) => write!(f, "unknown query parameter {name:?}"),
            Self::Repeated(name) => write!(f, "query parameter {name:?} is repeated"),
            Self::Plus(name) => write!(
                f,
                "query parameter {name:?} has an unescaped '+'; send a plus as %2B and a space \
                 as %20"
            ),
            Self::BadEscape(name) => {
                write!(f, "query parameter {name:?} has an invalid percent escape")
            }
            Self::RawByte(name) => write!(
                f,
                "query parameter {name:?} has a byte that must be percent-encoded"
            ),
            Self::NotUtf8(name) => write!(f, "query parameter {name:?} is not UTF-8 once decoded"),
        }
    }
}

/// The decoded parameters of a query string, each at most once, all of them among
/// `allowed`. An absent or empty query string has none.
///
/// # Errors
///
/// [`QueryError`] for the first parameter that breaks a rule.
pub fn parse(raw: Option<&str>, allowed: &[&str]) -> Result<BTreeMap<String, String>, QueryError> {
    let mut parameters = BTreeMap::new();
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(parameters);
    };
    for pair in raw.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(QueryError::NoEquals(pair.to_owned()));
        };
        let name = decode(name, name)?;
        if !allowed.contains(&name.as_str()) {
            return Err(QueryError::Unknown(name));
        }
        let value = decode(value, &name)?;
        if parameters.insert(name.clone(), value).is_some() {
            return Err(QueryError::Repeated(name));
        }
    }
    Ok(parameters)
}

/// Percent-decodes one name or value (`label` names it in errors).
fn decode(text: &str, label: &str) -> Result<String, QueryError> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'+' => return Err(QueryError::Plus(label.to_owned())),
            b'%' => {
                let high = bytes.get(at + 1).and_then(|digit| hex(*digit));
                let low = bytes.get(at + 2).and_then(|digit| hex(*digit));
                let (Some(high), Some(low)) = (high, low) else {
                    return Err(QueryError::BadEscape(label.to_owned()));
                };
                decoded.push(high * 16 + low);
                at += 3;
            }
            0x21..=0x7E => {
                decoded.push(byte);
                at += 1;
            }
            _ => return Err(QueryError::RawByte(label.to_owned())),
        }
    }
    String::from_utf8(decoded).map_err(|_| QueryError::NotUtf8(label.to_owned()))
}

const fn hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Percent-encodes `text` for a query value: every byte but RFC 3986's unreserved
/// characters is escaped, so decoding gives back exactly `text`.
#[must_use]
pub fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{QueryError, encode, parse};

    fn key(raw: &str) -> Result<String, QueryError> {
        parse(Some(raw), &["key"]).map(|parameters| parameters["key"].clone())
    }

    #[test]
    fn values_are_percent_decoded_and_plus_spaces_and_bad_bytes_are_refused() {
        assert_eq!(
            [
                key("key=a%2Fb%3Fc%26d"),
                key("key=%2B%20"),
                key("key=%C3%A9"),
                key("key=a+b"),
                key("key=a%2"),
                key("key=a%zz"),
                key("key=%FF"),
                key("key=a b"),
            ],
            [
                Ok("a/b?c&d".to_owned()),
                Ok("+ ".to_owned()),
                Ok("é".to_owned()),
                Err(QueryError::Plus("key".to_owned())),
                Err(QueryError::BadEscape("key".to_owned())),
                Err(QueryError::BadEscape("key".to_owned())),
                Err(QueryError::NotUtf8("key".to_owned())),
                Err(QueryError::RawByte("key".to_owned())),
            ],
            "rule: keys are RFC 3986 percent-decoded; an unescaped '+', a bad escape, a raw \
             byte or invalid UTF-8 is refused, never guessed (§8.1)"
        );
    }

    #[test]
    fn parameters_are_known_unique_and_have_values() {
        assert_eq!(
            [
                parse(None, &["key"]),
                parse(Some(""), &["key"]),
                parse(Some("key=a&key=b"), &["key"]),
                parse(Some("key=a&other=b"), &["key"]),
                parse(Some("key"), &["key"]),
            ],
            [
                Ok(BTreeMap::new()),
                Ok(BTreeMap::new()),
                Err(QueryError::Repeated("key".to_owned())),
                Err(QueryError::Unknown("other".to_owned())),
                Err(QueryError::NoEquals("key".to_owned())),
            ],
            "rule: a repeated parameter, an unknown one and one without a value are refused \
             (§8.1)"
        );
    }

    #[test]
    fn encoding_gives_back_exactly_the_decoded_bytes() {
        let keys = ["plain", "a/b?c&d=e", "+ %", "é漢字", "~-._"];
        let round_trips: Vec<Result<String, QueryError>> = keys
            .iter()
            .map(|text| key(&format!("key={}", encode(text))))
            .collect();
        assert_eq!(
            (round_trips, encode("a b+c")),
            (
                keys.iter().map(|text| Ok((*text).to_owned())).collect(),
                "a%20b%2Bc".to_owned()
            ),
            "rule: a 307 Location re-encodes exactly the same decoded bytes (§8.1)"
        );
    }
}
