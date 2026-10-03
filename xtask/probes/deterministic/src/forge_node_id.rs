//! Compile-fail fixture: a node ID is built only by `NodeId::new`, so zero and 2^53 are unrepresentable (D-77).

/// Deliberate violation: node ID 0, built without `NodeId::new`.
#[must_use]
pub const fn forged() -> types::NodeId {
    types::NodeId(0)
}
