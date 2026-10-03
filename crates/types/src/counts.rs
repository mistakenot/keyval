//! Counts: [`Tick`], [`ByteCount`] and [`PageSize`] (CD-4). Arithmetic is checked.

use core::fmt;

use crate::range::{OutOfRange, Quantity};

/// A number of logical clock steps (PLAN.md §7.2, §7.10).
///
/// Time reaches the deterministic core only as ticks: a node's logical time is the ticks
/// since it started, and a deadline is a tick count from admission.
///
/// Invariant: none beyond its range; arithmetic never wraps (`checked_*` returns `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick(u64);

impl Tick {
    /// No ticks.
    pub const ZERO: Self = Self(0);

    /// A tick count.
    #[must_use]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// The raw count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - other`, or `None` if `other` is larger.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(difference) => Some(Self(difference)),
            None => None,
        }
    }
}

impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ticks", self.0)
    }
}

/// A number of bytes: a key's or value's decoded length, a page's size, a byte quota
/// (DM-6, DM-7, §7.11).
///
/// Invariant: none beyond its range; arithmetic never wraps (`checked_*` returns `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteCount(u64);

impl ByteCount {
    /// No bytes.
    pub const ZERO: Self = Self(0);

    /// A byte count.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The byte length of `text` in UTF-8, or `None` if it does not fit in 64 bits.
    #[must_use]
    pub fn of(text: &str) -> Option<Self> {
        u64::try_from(text.len()).ok().map(Self)
    }

    /// The raw count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - other`, or `None` if `other` is larger.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(difference) => Some(Self(difference)),
            None => None,
        }
    }
}

impl fmt::Display for ByteCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// The most items one SCAN page may hold (F-4).
///
/// Invariant: 1 to [`PageSize::MAX`] (128).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageSize(u8);

impl PageSize {
    /// The largest page, in items (F-4).
    pub const MAX: u64 = 128;

    /// The page size when a SCAN gives no `limit` (F-4).
    pub const DEFAULT: Self = Self(32);

    /// Parses a SCAN `limit`.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] unless `raw` is 1 to 128.
    pub fn new(raw: u64) -> Result<Self, OutOfRange> {
        Quantity::PageSize.check(raw).and_then(|value| {
            u8::try_from(value).map(Self).map_err(|_| OutOfRange {
                quantity: Quantity::PageSize,
                value,
            })
        })
    }

    /// The number of items.
    #[must_use]
    pub fn get(self) -> u64 {
        u64::from(self.0)
    }
}

impl fmt::Display for PageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} items", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_and_byte_arithmetic_is_checked() {
        let max_tick = Tick::new(u64::MAX);
        assert_eq!(
            max_tick.checked_add(Tick::new(1)),
            None,
            "rule: tick arithmetic never wraps (CD-4)"
        );
        assert_eq!(
            Tick::ZERO.checked_sub(Tick::new(1)),
            None,
            "rule: no negative tick span"
        );
        assert_eq!(
            Tick::new(2).checked_add(Tick::new(3)),
            Some(Tick::new(5)),
            "rule: in-range sums are exact"
        );
        assert_eq!(
            Tick::new(5).checked_sub(Tick::new(3)),
            Some(Tick::new(2)),
            "rule: in-range differences are exact"
        );
        let max_bytes = ByteCount::new(u64::MAX);
        assert_eq!(
            max_bytes.checked_add(ByteCount::new(1)),
            None,
            "rule: byte arithmetic never wraps (CD-4)"
        );
        assert_eq!(
            ByteCount::ZERO.checked_sub(ByteCount::new(1)),
            None,
            "rule: no negative byte count"
        );
        assert_eq!(
            ByteCount::new(64).checked_add(ByteCount::new(65_536)),
            Some(ByteCount::new(65_600)),
            "rule: in-range sums are exact"
        );
    }

    #[test]
    fn byte_counts_measure_utf8_bytes_not_characters() {
        for (text, bytes) in [
            ("", 0),
            ("a", 1),
            ("é", 2),
            ("€", 3),
            ("😀", 4),
            ("e\u{301}", 3),
        ] {
            assert_eq!(
                ByteCount::of(text),
                Some(ByteCount::new(bytes)),
                "rule: lengths are UTF-8 bytes (DM-1); text={text:?}"
            );
        }
    }

    #[test]
    fn page_sizes_are_1_to_128_defaulting_to_32() {
        for (raw, ok) in [
            (0, false),
            (1, true),
            (32, true),
            (128, true),
            (129, false),
            (256, false),
            (u64::MAX, false),
        ] {
            let parsed = PageSize::new(raw);
            assert_eq!(
                parsed.is_ok(),
                ok,
                "rule: a SCAN limit is 1..=128 (F-4); raw={raw} parsed={parsed:?}"
            );
            if let Ok(size) = parsed {
                assert_eq!(
                    size.get(),
                    raw,
                    "rule: a page size keeps its value; raw={raw}"
                );
            }
        }
        assert_eq!(
            PageSize::DEFAULT.get(),
            32,
            "rule: the default limit is 32 (F-4)"
        );
        assert_eq!(
            PageSize::new(129),
            Err(OutOfRange {
                quantity: Quantity::PageSize,
                value: 129
            }),
            "rule: the error names the quantity and the value"
        );
    }
}
