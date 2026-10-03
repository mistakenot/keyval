//! Compile-fail fixture: a key is built only by `Key::parse`, so DM-1 cannot be bypassed (CD-3).

/// Deliberate violation: an empty key, built without `Key::parse`.
#[must_use]
pub fn forged() -> types::Key {
    types::Key(String::new())
}
