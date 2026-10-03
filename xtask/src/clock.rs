//! UTC timestamps for receipts and run IDs. xtask is tooling, so it may read the wall
//! clock; deterministic crates never do (§7.14).

use std::time::{SystemTime, UNIX_EPOCH};

/// A UTC calendar time, to the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utc {
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
}

impl Utc {
    /// The current time. A clock set before 1970 reads as the epoch.
    pub fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self::from_unix(secs)
    }

    /// The calendar time `secs` seconds after 1970-01-01T00:00:00Z.
    pub const fn from_unix(secs: u64) -> Self {
        let days = secs / 86_400;
        let rest = secs % 86_400;
        // Civil-from-days (H. Hinnant), for days since 1970-01-01, in unsigned arithmetic.
        let z = days + 719_468;
        let era = z / 146_097;
        let day_of_era = z % 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
        Self {
            year,
            month,
            day,
            hour: rest / 3600,
            minute: rest % 3600 / 60,
            second: rest % 60,
        }
    }

    /// RFC 3339, e.g. `2026-10-03T11:54:05Z`.
    pub fn rfc3339(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// Compact and filename-safe, e.g. `20261003T115405Z`.
    pub fn compact(self) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        let cases = [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (1_791_028_445, "2026-10-03T11:54:05Z"),
            (4_102_444_799, "2099-12-31T23:59:59Z"),
        ];
        for (secs, expected) in cases {
            assert_eq!(
                Utc::from_unix(secs).rfc3339(),
                expected,
                "unix seconds={secs}"
            );
        }
    }

    #[test]
    fn compact_form_is_filename_safe() {
        assert_eq!(Utc::from_unix(1_791_028_445).compact(), "20261003T115405Z");
    }

    #[test]
    fn now_is_after_the_project_started() {
        let now = Utc::now();
        assert!(now.year >= 2026, "observed={now:?}");
    }
}
