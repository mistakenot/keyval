//! Probe: `unsafe` code is rejected (CD-8, `unsafe_code` forbidden).

/// Deliberate violation.
#[must_use]
pub const fn probe() -> u8 {
    // SAFETY: none; this probe exists to be rejected.
    unsafe { core::mem::transmute::<i8, u8>(-1) }
}
