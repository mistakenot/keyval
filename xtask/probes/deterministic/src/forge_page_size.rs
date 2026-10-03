//! Compile-fail fixture: a page size is built only by `PageSize::new`, so 0 and 129 are unrepresentable (F-4).

/// Deliberate violation: a page of 0 items, built without `PageSize::new`.
#[must_use]
pub const fn forged() -> types::PageSize {
    types::PageSize(0)
}
