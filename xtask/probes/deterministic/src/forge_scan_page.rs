//! Compile-fail fixture: a SCAN page is built only by `ScanPage::new`, which checks order, range, limits and the continuation (§8.5, §7.11).

/// Deliberate violation: a page whose continuation is not its last key, built without `ScanPage::new`.
#[must_use]
pub const fn forged(
    items: Vec<types::ScanItem>,
    next: types::Key,
    served_by: types::ServedBy,
) -> types::ScanPage {
    types::ScanPage {
        items,
        continuation: Some(next),
        served_by,
    }
}
