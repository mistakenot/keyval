//! Raft positions and key versions: [`Term`], [`LogIndex`], [`EntryIndex`] and
//! [`Version`] (CD-4).
//!
//! A [`Version`] is the log index at which the write that set it executed (DM-3). It is
//! still a separate type: comparing one with a [`LogIndex`] needs an explicit conversion.

use core::fmt;

use crate::range::{MAX_SAFE_INTEGER, OutOfRange, Quantity};

/// A Raft term, the election epoch.
///
/// Invariant: at most [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER). Terms grow only
/// with elections, so the adapter treats a larger one as unreachable (PLAN.md §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Term(u64);

impl Term {
    /// Term 0, before any election.
    pub const ZERO: Self = Self(0);

    /// Parses a raw term.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if `raw` is above [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    pub const fn new(raw: u64) -> Result<Self, OutOfRange> {
        match Quantity::Term.check(raw) {
            Ok(value) => Ok(Self(value)),
            Err(err) => Err(err),
        }
    }

    /// The raw term.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next term, or `None` past [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        if self.0 < MAX_SAFE_INTEGER {
            Some(Self(self.0 + 1))
        } else {
            None
        }
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A position in the Raft log. Index 0 means "before the first entry": it is what a
/// node reports as applied, committed or durable before anything has reached that state.
///
/// Invariant: at most [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER) (D-77). History
/// admission (DM-7) keeps real indices far below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogIndex(u64);

impl LogIndex {
    /// Index 0, before the first entry.
    pub const ZERO: Self = Self(0);

    /// Parses a raw log index.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if `raw` is above [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    pub const fn new(raw: u64) -> Result<Self, OutOfRange> {
        match Quantity::LogIndex.check(raw) {
            Ok(value) => Ok(Self(value)),
            Err(err) => Err(err),
        }
    }

    /// The raw index.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next index, or `None` past [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        if self.0 < MAX_SAFE_INTEGER {
            Some(Self(self.0 + 1))
        } else {
            None
        }
    }

    /// The previous index, or `None` at index 0.
    #[must_use]
    pub const fn checked_prev(self) -> Option<Self> {
        match self.0.checked_sub(1) {
            Some(prev) => Some(Self(prev)),
            None => None,
        }
    }
}

impl fmt::Display for LogIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The index of an entry that exists: a [`LogIndex`] of at least 1 (the API spec's
/// `EntryIndex`, §8.1).
///
/// Index 0 holds no entry, so an entry, or a request's first execution
/// (`original_index`, F-6), can never be at 0.
///
/// Invariant: 1 to [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER) (D-77).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryIndex(u64);

impl EntryIndex {
    /// Parses a raw entry index.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if `raw` is 0 or above [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    pub const fn new(raw: u64) -> Result<Self, OutOfRange> {
        match Quantity::EntryIndex.check(raw) {
            Ok(value) => Ok(Self(value)),
            Err(err) => Err(err),
        }
    }

    /// The entry index at log position `index`, or `None` for index 0, which holds no
    /// entry.
    #[must_use]
    pub const fn at(index: LogIndex) -> Option<Self> {
        match Self::new(index.0) {
            Ok(entry) => Some(entry),
            Err(_) => None,
        }
    }

    /// The same position as a [`LogIndex`].
    #[must_use]
    pub const fn index(self) -> LogIndex {
        LogIndex(self.0)
    }

    /// The version a write executed at this entry gives its key: the same number (DM-3).
    /// Entry indices and versions share one range, so this never fails.
    #[must_use]
    pub const fn version(self) -> Version {
        Version(self.0)
    }

    /// The raw index.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EntryIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A key's version: the log index at which the PUT or compare-and-swap that set it
/// executed (DM-3). Versions strictly increase per key and have gaps.
///
/// Invariant: 1 to [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER), because log entries
/// start at index 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64);

impl Version {
    /// Parses a raw version, such as a compare-and-swap's `expected.version`.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if `raw` is 0 or above [`MAX_SAFE_INTEGER`](crate::MAX_SAFE_INTEGER).
    pub const fn new(raw: u64) -> Result<Self, OutOfRange> {
        match Quantity::Version.check(raw) {
            Ok(value) => Ok(Self(value)),
            Err(err) => Err(err),
        }
    }

    /// The version a write executed at `index` gives its key, or `None` for index 0,
    /// which holds no entry.
    #[must_use]
    pub const fn at(index: LogIndex) -> Option<Self> {
        match Self::new(index.0) {
            Ok(version) => Some(version),
            Err(_) => None,
        }
    }

    /// The log index this version was assigned at.
    #[must_use]
    pub const fn index(self) -> LogIndex {
        LogIndex(self.0)
    }

    /// The raw version.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_and_indices_accept_0_to_2_pow_53_minus_1() {
        for raw in [0, 1, MAX_SAFE_INTEGER] {
            assert_eq!(
                Term::new(raw).map(Term::get),
                Ok(raw),
                "rule: terms are 0..=2^53-1; raw={raw}"
            );
            assert_eq!(
                LogIndex::new(raw).map(LogIndex::get),
                Ok(raw),
                "rule: indices are 0..=2^53-1; raw={raw}"
            );
        }
        for raw in [MAX_SAFE_INTEGER + 1, u64::MAX] {
            assert_eq!(
                Term::new(raw),
                Err(OutOfRange {
                    quantity: Quantity::Term,
                    value: raw
                }),
                "rule: a term at or above 2^53 is refused (§8.1, D-77); raw={raw}"
            );
            assert_eq!(
                LogIndex::new(raw),
                Err(OutOfRange {
                    quantity: Quantity::LogIndex,
                    value: raw
                }),
                "rule: an index at or above 2^53 is refused (D-77); raw={raw}"
            );
        }
    }

    #[test]
    fn versions_are_1_to_2_pow_53_minus_1() {
        for (raw, ok) in [
            (0, false),
            (1, true),
            (MAX_SAFE_INTEGER, true),
            (MAX_SAFE_INTEGER + 1, false),
        ] {
            let parsed = Version::new(raw);
            assert_eq!(
                parsed.is_ok(),
                ok,
                "rule: versions are 1..=2^53-1 (DM-3, D-77); raw={raw} parsed={parsed:?}"
            );
        }
    }

    #[test]
    fn arithmetic_is_checked_at_both_ends() {
        let last = LogIndex::new(MAX_SAFE_INTEGER).unwrap();
        assert_eq!(
            last.checked_next(),
            None,
            "rule: no index past 2^53-1; index={last}"
        );
        assert_eq!(
            LogIndex::ZERO.checked_prev(),
            None,
            "rule: no index before 0"
        );
        assert_eq!(
            LogIndex::ZERO
                .checked_next()
                .and_then(LogIndex::checked_prev),
            Some(LogIndex::ZERO),
            "rule: next then prev is the identity"
        );
        assert_eq!(
            Term::new(MAX_SAFE_INTEGER).unwrap().checked_next(),
            None,
            "rule: no term past 2^53-1"
        );
        assert_eq!(
            Term::ZERO.checked_next().map(Term::get),
            Some(1),
            "rule: the first election is term 1"
        );
    }

    #[test]
    fn versions_and_indices_convert_only_explicitly() {
        assert_eq!(
            Version::at(LogIndex::ZERO),
            None,
            "rule: index 0 holds no entry, so it is no version"
        );
        let index = LogIndex::new(42).unwrap();
        let version = Version::at(index).unwrap();
        assert_eq!(
            version.get(),
            42,
            "rule: version == index of the executing write (DM-3)"
        );
        assert_eq!(version.index(), index, "rule: the conversion back is exact");
        assert_eq!(
            EntryIndex::at(index).map(EntryIndex::version),
            Some(version),
            "rule: a write at entry 42 gets version 42 (DM-3)"
        );
    }

    #[test]
    fn entry_indices_are_1_to_2_pow_53_minus_1() {
        for raw in [1, 2, MAX_SAFE_INTEGER] {
            assert_eq!(
                EntryIndex::new(raw).map(EntryIndex::get),
                Ok(raw),
                "rule: an existing entry's index is 1..=2^53-1 (§8.1 EntryIndex); raw={raw}"
            );
        }
        for raw in [0, MAX_SAFE_INTEGER + 1, u64::MAX] {
            assert_eq!(
                EntryIndex::new(raw),
                Err(OutOfRange {
                    quantity: Quantity::EntryIndex,
                    value: raw
                }),
                "rule: index 0 holds no entry, and 2^53 is out of range (D-77); raw={raw}"
            );
        }
        assert_eq!(
            EntryIndex::at(LogIndex::ZERO),
            None,
            "rule: log position 0 is no entry's index"
        );
        let index = LogIndex::new(7).unwrap();
        assert_eq!(
            EntryIndex::at(index).map(EntryIndex::index),
            Some(index),
            "rule: the conversion to and from LogIndex is exact"
        );
    }
}
