//! Reading the output of Rust's built-in test harness (libtest).
//!
//! libtest has no stable machine-readable format (its JSON output needs nightly), so
//! this module parses the stable text lines: `<name>: test` from `--list`,
//! `test result: …` summaries and `test <name> ... FAILED` result lines.

use std::fmt;

/// Case counts summed over every test binary that ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Cases that ran and passed.
    pub passed: u32,
    /// Cases that ran and failed.
    pub failed: u32,
    /// Cases that were selected but skipped by `#[ignore]`.
    pub ignored: u32,
}

/// Why libtest output could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A `test result:` line did not have the expected shape.
    MalformedSummary(String),
    /// The counts overflowed `u32`.
    Overflow,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedSummary(line) => write!(f, "unrecognised libtest summary: {line:?}"),
            Self::Overflow => write!(f, "libtest case counts overflowed"),
        }
    }
}

/// The test-case names in `cargo test -- --list` output, benchmarks excluded.
pub fn parse_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

/// Sums every `test result:` summary line in `cargo test` output.
pub fn parse_tally(stdout: &str) -> Result<Tally, ParseError> {
    let mut tally = Tally::default();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("test result: ") else {
            continue;
        };
        let malformed = || ParseError::MalformedSummary(line.to_owned());
        // "ok. 5 passed; 0 failed; 1 ignored; 0 measured; 3 filtered out; finished in 0.01s"
        let (_verdict, counts) = rest.split_once(". ").ok_or_else(malformed)?;
        let mut seen = 0;
        for part in counts.split(';') {
            let mut words = part.split_whitespace();
            let (Some(number), Some(label)) = (words.next(), words.next()) else {
                continue;
            };
            let slot = match label {
                "passed" => &mut tally.passed,
                "failed" => &mut tally.failed,
                "ignored" => &mut tally.ignored,
                _ => continue,
            };
            let n: u32 = number.parse().map_err(|_| malformed())?;
            *slot = slot.checked_add(n).ok_or(ParseError::Overflow)?;
            seen += 1;
        }
        if seen != 3 {
            return Err(malformed());
        }
    }
    Ok(tally)
}

/// Names of the cases that failed: result lines `test <name> ... FAILED`.
pub fn failed_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("test ")?.strip_suffix(" ... FAILED"))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
output::tests::usage_errors_exit_2: test
output::tests::write_line_appends_exactly_one_newline: test
benches::big: benchmark

2 tests, 1 benchmarks
cli::no_command_is_a_usage_error: test

1 tests, 0 benchmarks
";

    const RUN: &str = "
running 3 tests
test a::passes ... ok
test a::fails ... FAILED
test a::skipped ... ignored, fixture

failures:

---- a::fails stdout ----
assertion failed

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 4 filtered out; finished in 0.00s

running 2 tests
test b::one ... ok
test b::two ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";

    #[test]
    fn parse_list_keeps_tests_and_drops_benchmarks_and_summaries() {
        let observed = parse_list(LIST);
        let expected = [
            "output::tests::usage_errors_exit_2",
            "output::tests::write_line_appends_exactly_one_newline",
            "cli::no_command_is_a_usage_error",
        ];
        assert_eq!(observed, expected, "input:\n{LIST}");
    }

    #[test]
    fn parse_tally_sums_all_binaries() {
        let observed = parse_tally(RUN);
        let expected = Tally {
            passed: 3,
            failed: 1,
            ignored: 1,
        };
        assert_eq!(observed, Ok(expected), "input:\n{RUN}");
    }

    #[test]
    fn parse_tally_of_output_without_summaries_is_zero() {
        assert_eq!(parse_tally("nothing ran\n"), Ok(Tally::default()));
    }

    #[test]
    fn parse_tally_rejects_a_malformed_summary() {
        let line = "test result: ok. many passed; 0 failed; 0 ignored";
        assert_eq!(
            parse_tally(line),
            Err(ParseError::MalformedSummary(line.to_owned())),
            "rule: unreadable counts are an error, never zero cases"
        );
        let missing = "test result: ok. 1 passed; 0 failed";
        assert!(parse_tally(missing).is_err(), "input={missing:?}");
    }

    #[test]
    fn failed_names_lists_only_failures() {
        assert_eq!(failed_names(RUN), ["a::fails"], "input:\n{RUN}");
    }
}
