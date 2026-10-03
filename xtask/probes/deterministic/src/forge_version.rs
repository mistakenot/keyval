//! Compile-fail fixture: a version, and so a CAS expectation, is built only by `Version::new` (DM-3, D-77).

/// Deliberate violation: a CAS expecting version 0, built without `Version::new`.
#[must_use]
pub const fn forged() -> types::ExpectedVersion {
    types::ExpectedVersion::Version(types::Version(0))
}
