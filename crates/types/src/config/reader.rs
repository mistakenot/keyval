//! A strict reader for exactly the config file's grammar: a small subset of TOML, read
//! in-crate with no dependency (PLAN.md §7.10, D-79).
//!
//! It accepts, line by line (spaces and tabs around tokens; `\n` or `\r\n` line ends):
//! - blank lines and `#` comments, including a comment after a value or header;
//! - `key = value`, where the key is bare (letters, digits, `_`, `-`) and the value is a
//!   decimal integer (no sign, underscores, exponent or leading zero) or a basic string
//!   (`"…"`, on one line, without escape sequences);
//! - the header `[[nodes]]`, which starts a new node table. As in TOML, every key after
//!   it belongs to that node.
//!
//! Anything else is refused with its line number: other tables, quoted or dotted keys,
//! booleans, floats, dates, arrays, inline tables, literal or multi-line strings, escape
//! sequences, control characters, and a key defined twice in one table. So every file it
//! accepts is valid TOML with the same meaning.

use super::error::{ConfigError, ConfigSyntax};

/// A value as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Raw {
    /// A decimal integer.
    Integer(u64),
    /// A basic string's contents.
    String(String),
}

/// One `key = value` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    /// The bare key.
    pub(super) key: String,
    /// The value.
    pub(super) value: Raw,
    /// The line number, counted from 1.
    pub(super) line: usize,
}

/// A table's entries, in file order, with no key twice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Table {
    /// The entries.
    pub(super) entries: Vec<Entry>,
}

impl Table {
    /// The entry for `key`, if present.
    pub(super) fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.key == key)
    }
}

/// A read config file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Document {
    /// The top-level table.
    pub(super) top: Table,
    /// Each `[[nodes]]` table in file order, with its header's line.
    pub(super) nodes: Vec<(usize, Table)>,
}

/// Reads a config file.
///
/// # Errors
///
/// [`ConfigError::Syntax`] for the first line outside the grammar, or
/// [`ConfigError::DuplicateKey`] for a key defined twice in one table.
pub(super) fn read(text: &str) -> Result<Document, ConfigError> {
    let mut document = Document::default();
    for (index, raw_line) in text.split('\n').enumerate() {
        let line = index + 1;
        let content = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        read_line(&mut document, content, line)?;
    }
    Ok(document)
}

/// Reads one line into `document`.
fn read_line(document: &mut Document, content: &str, line: usize) -> Result<(), ConfigError> {
    let syntax = |problem| ConfigError::Syntax { line, problem };
    let rest = skip_space(content);
    if rest.is_empty() {
        return Ok(());
    }
    if let Some(comment) = rest.strip_prefix('#') {
        return check_comment(comment).map_err(syntax);
    }
    if rest.starts_with('[') {
        header(rest).map_err(syntax)?;
        document.nodes.push((line, Table::default()));
        return Ok(());
    }
    let (key, after_key) = bare_key(rest).map_err(syntax)?;
    let after_equals = skip_space(after_key)
        .strip_prefix('=')
        .ok_or_else(|| syntax(ConfigSyntax::ExpectedKeyValue))?;
    let (value, after_value) = value(skip_space(after_equals)).map_err(syntax)?;
    end_of_line(after_value).map_err(syntax)?;
    let table = match document.nodes.last_mut() {
        Some((_, node)) => node,
        None => &mut document.top,
    };
    if let Some(first) = table.get(key) {
        return Err(ConfigError::DuplicateKey {
            line,
            key: key.to_owned(),
            first_line: first.line,
        });
    }
    table.entries.push(Entry {
        key: key.to_owned(),
        value,
        line,
    });
    Ok(())
}

/// `text` without its leading spaces and tabs.
fn skip_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t'])
}

/// A comment's text, after the `#`: anything but control characters other than tab.
fn check_comment(comment: &str) -> Result<(), ConfigSyntax> {
    if comment.chars().any(|c| c.is_control() && c != '\t') {
        Err(ConfigSyntax::ControlCharacter)
    } else {
        Ok(())
    }
}

/// The rest of a line after a value or header: spaces, then nothing or a comment.
fn end_of_line(rest: &str) -> Result<(), ConfigSyntax> {
    let rest = skip_space(rest);
    if rest.is_empty() {
        Ok(())
    } else if let Some(comment) = rest.strip_prefix('#') {
        check_comment(comment)
    } else {
        Err(ConfigSyntax::TrailingCharacters)
    }
}

/// The header `[[nodes]]`, with optional spaces inside the brackets, then the line's end.
fn header(rest: &str) -> Result<(), ConfigSyntax> {
    let inner = rest
        .strip_prefix("[[")
        .ok_or(ConfigSyntax::UnsupportedTable)?;
    let after_name = skip_space(inner)
        .strip_prefix("nodes")
        .ok_or(ConfigSyntax::UnsupportedTable)?;
    let after = skip_space(after_name)
        .strip_prefix("]]")
        .ok_or(ConfigSyntax::UnsupportedTable)?;
    end_of_line(after)
}

/// A bare key at the start of `rest`, and what follows it.
fn bare_key(rest: &str) -> Result<(&str, &str), ConfigSyntax> {
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    let (key, after) = rest.split_at(end);
    if key.is_empty() {
        if rest.starts_with(['"', '\'']) {
            Err(ConfigSyntax::UnsupportedKey)
        } else {
            Err(ConfigSyntax::ExpectedKeyValue)
        }
    } else if skip_space(after).starts_with('.') {
        Err(ConfigSyntax::UnsupportedKey)
    } else {
        Ok((key, after))
    }
}

/// A value at the start of `rest`, and what follows it.
fn value(rest: &str) -> Result<(Raw, &str), ConfigSyntax> {
    match rest.chars().next() {
        None | Some('#') => Err(ConfigSyntax::MissingValue),
        Some('"') => basic_string(rest),
        Some(first) if first.is_ascii_digit() => integer(rest),
        Some(_) => Err(ConfigSyntax::UnsupportedValue),
    }
}

/// A basic string at the start of `rest` (which starts with `"`), and what follows it.
fn basic_string(rest: &str) -> Result<(Raw, &str), ConfigSyntax> {
    if rest.starts_with("\"\"\"") {
        return Err(ConfigSyntax::UnsupportedValue);
    }
    let body = rest
        .strip_prefix('"')
        .ok_or(ConfigSyntax::UnsupportedValue)?;
    for (offset, c) in body.char_indices() {
        if c == '"' {
            let (contents, after) = body.split_at(offset);
            let after = after.strip_prefix('"').unwrap_or(after);
            return Ok((Raw::String(contents.to_owned()), after));
        }
        if c == '\\' {
            return Err(ConfigSyntax::EscapeSequence);
        }
        if c.is_control() && c != '\t' {
            return Err(ConfigSyntax::ControlCharacter);
        }
    }
    Err(ConfigSyntax::UnterminatedString)
}

/// A decimal integer at the start of `rest` (which starts with a digit), and what follows
/// it.
fn integer(rest: &str) -> Result<(Raw, &str), ConfigSyntax> {
    let end = rest.find([' ', '\t', '#']).unwrap_or(rest.len());
    let (token, after) = rest.split_at(end);
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ConfigSyntax::UnsupportedValue);
    }
    if token.len() > 1 && token.starts_with('0') {
        return Err(ConfigSyntax::LeadingZero);
    }
    // TOML's integers are 64-bit signed, so 2^63 − 1 is the largest.
    let value: i64 = token.parse().map_err(|_| ConfigSyntax::IntegerTooLarge)?;
    Ok((Raw::Integer(value.unsigned_abs()), after))
}
