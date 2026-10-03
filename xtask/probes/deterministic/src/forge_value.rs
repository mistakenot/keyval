//! Compile-fail fixture: a value is built only by `Value::parse`, so DM-2 cannot be bypassed (CD-3).

/// Deliberate violation: a 70,000-byte value, built without `Value::parse`.
#[must_use]
pub fn forged() -> types::Value {
    types::Value("v".repeat(70_000))
}
