//! Compile-fail fixture: a version cannot be compared with a log index without an explicit conversion (CD-4).

/// Deliberate violation: comparing two different quantities.
#[must_use]
pub fn same(version: types::Version, index: types::LogIndex) -> bool {
    version == index
}
