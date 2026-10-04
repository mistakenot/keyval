//! Strict query-string decoding (PLAN.md §8.1 "Query decoding"; api/openapi.yaml
//! conventions): parameters are percent-decoded per RFC 3986; an unescaped `+`, an invalid
//! percent escape, invalid UTF-8 after decoding, a raw byte outside printable ASCII, a
//! parameter without `=`, a repeated parameter and a parameter the operation does not take
//! are all refused, so a key can never be silently corrupted.

use std::collections::BTreeMap;
use std::fmt;

/// Why a query string was refused (each is `400 invalid_request`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// A parameter has no `=`.
    NoEquals(String),
    /// A parameter the operation does not take.
    Unknown(String),
    /// A parameter given twice.
    Repeated(String),
    /// An unescaped `+`, which form encoders send for a space (§8.1).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn one(raw: &str) -> Result<String, QueryError> {
        parse(Some(raw), &["key"]).map(|parameters| parameters["key"].clone())
    }

    #[test]
    fn keys_decode_per_rfc_3986_with_slash_question_mark_and_ampersand() {
        let cases = [
            ("key=a", "a"),
            ("key=a/b?c", "a/b?c"),
            ("key=k%26v", "k&v"),
            ("key=%2B", "+"),
            ("key=a%20b", "a b"),
            ("key=%C3%A9", "\u{e9}"),
            ("key=%c3%a9", "\u{e9}"),
            ("key=", ""),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                one(raw),
                Ok(expected.to_owned()),
                "rule: query values are percent-decoded per RFC 3986 (§8.1); raw={raw:?}"
            );
        }
    }

    #[test]
    fn malformed_queries_are_refused() {
        let cases = [
            ("key=a+b", QueryError::Plus("key".to_owned())),
            ("key=%2", QueryError::BadEscape("key".to_owned())),
            ("key=%zz", QueryError::BadEscape("key".to_owned())),
            ("key=%C3", QueryError::NotUtf8("key".to_owned())),
            ("key=a b", QueryError::RawByte("key".to_owned())),
            ("key", QueryError::NoEquals("key".to_owned())),
            ("key=a&key=b", QueryError::Repeated("key".to_owned())),
            ("key=a&other=b", QueryError::Unknown("other".to_owned())),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                parse(Some(raw), &["key"]),
                Err(expected),
                "rule: an unescaped '+', a bad escape, invalid UTF-8, a raw space, a missing '=', \
                 a repeated or an unknown parameter is refused (§8.1); raw={raw:?}"
            );
        }
    }

    #[test]
    fn an_absent_or_empty_query_has_no_parameters() {
        for raw in [None, Some("")] {
            assert_eq!(
                parse(raw, &["key"]),
                Ok(BTreeMap::new()),
                "rule: no query string means no parameters; raw={raw:?}"
            );
        }
    }
}
