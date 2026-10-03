//! Compile-fail fixture: a `503 recovering` body can never claim the node is ready (CD-2, O-1).

/// Deliberate violation: a recovering refusal whose lifecycle is `Ready`.
#[must_use]
pub const fn forged() -> types::Refusal {
    types::Refusal::Recovering {
        lifecycle: types::NotReadyLifecycle::Ready,
    }
}
