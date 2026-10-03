//! The bounded integer quantities and their shared range error.

use core::fmt;

/// The largest integer every JSON client represents exactly: 2^53 − 1
/// (`9007199254740991`, PLAN.md §8.1 and D-77).
///
/// Node IDs, terms, log indices and versions all appear in the HTTP API, so their types
/// refuse anything larger at construction. Every value the core can report is then a
/// safe JSON integer by type.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// A bounded integer quantity, as named in an [`OutOfRange`] error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantity {
    /// [`NodeId`](crate::NodeId): 1 to [`MAX_SAFE_INTEGER`].
    NodeId,
    /// [`Term`](crate::Term): 0 to [`MAX_SAFE_INTEGER`].
    Term,
    /// [`LogIndex`](crate::LogIndex): 0 to [`MAX_SAFE_INTEGER`].
    LogIndex,
    /// [`Version`](crate::Version): 1 to [`MAX_SAFE_INTEGER`].
    Version,
    /// [`PageSize`](crate::PageSize): 1 to 128 (F-4).
    PageSize,
}

impl Quantity {
    /// The smallest valid value.
    #[must_use]
    pub const fn min(self) -> u64 {
        match self {
            Self::NodeId | Self::Version | Self::PageSize => 1,
            Self::Term | Self::LogIndex => 0,
        }
    }

    /// The largest valid value.
    #[must_use]
    pub const fn max(self) -> u64 {
        match self {
            Self::NodeId | Self::Term | Self::LogIndex | Self::Version => MAX_SAFE_INTEGER,
            Self::PageSize => crate::PageSize::MAX,
        }
    }

    /// The value if it lies in this quantity's range.
    pub(crate) const fn check(self, value: u64) -> Result<u64, OutOfRange> {
        if value < self.min() || value > self.max() {
            Err(OutOfRange {
                quantity: self,
                value,
            })
        } else {
            Ok(value)
        }
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NodeId => "node ID",
            Self::Term => "term",
            Self::LogIndex => "log index",
            Self::Version => "version",
            Self::PageSize => "page size",
        })
    }
}

/// A value outside its quantity's valid range.
///
/// Invariant: `value` lies outside `quantity.min()..=quantity.max()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange {
    /// Which quantity was being constructed.
    pub quantity: Quantity,
    /// The rejected value.
    pub value: u64,
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} is outside {}..={}",
            self.quantity,
            self.value,
            self.quantity.min(),
            self.quantity.max()
        )
    }
}

impl std::error::Error for OutOfRange {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_safe_integer_is_2_pow_53_minus_1() {
        assert_eq!(
            MAX_SAFE_INTEGER, 9_007_199_254_740_991,
            "rule: D-77 fixes the ceiling at 2^53 - 1"
        );
    }

    #[test]
    fn range_errors_name_quantity_value_and_bounds() {
        let err = OutOfRange {
            quantity: Quantity::PageSize,
            value: 129,
        };
        assert_eq!(
            err.to_string(),
            "page size 129 is outside 1..=128",
            "rule: errors are diagnosable (CD-6)"
        );
        let bounds: Vec<(u64, u64)> = [
            Quantity::NodeId,
            Quantity::Term,
            Quantity::LogIndex,
            Quantity::Version,
            Quantity::PageSize,
        ]
        .iter()
        .map(|quantity| (quantity.min(), quantity.max()))
        .collect();
        assert_eq!(
            bounds,
            vec![
                (1, MAX_SAFE_INTEGER),
                (0, MAX_SAFE_INTEGER),
                (0, MAX_SAFE_INTEGER),
                (1, MAX_SAFE_INTEGER),
                (1, 128)
            ],
            "rule: each quantity's range (§7.10, §8.1, DM-3, F-4, D-77)"
        );
    }
}
