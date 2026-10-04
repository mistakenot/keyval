//! Reading the output of Rust's built-in test harness (libtest).
//!
//! libtest has no stable machine-readable format (its JSON output needs nightly), so
//! this module parses the stable text lines: `<name>: test` from `--list`,
//! `test result: …` summaries and `test <name> ... ok|FAILED|ignored` result lines.
//! [`reconcile`] trusts a run's per-case outcomes only when they add up to what libtest
//! and cargo themselves announced (br-judge-cargo-partial-run-afvw).

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

/// What happened to one case, as its result line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `test <name> ... ok`.
    Passed,
    /// `test <name> ... FAILED`.
    Failed,
    /// `test <name> ... ignored`, with or without a reason.
    Ignored,
}

/// Every case's result line in `cargo test` output, in order. A `#[should_panic]` case's
/// ` - should panic` is dropped, so names match `--list` output.
pub fn outcomes(stdout: &str) -> Vec<(String, Outcome)> {
    stdout.lines().filter_map(result_line).collect()
}

/// One case's result line: `test <name> ... ok|FAILED|ignored[, reason]`.
fn result_line(line: &str) -> Option<(String, Outcome)> {
    let (name, result) = line.strip_prefix("test ")?.split_once(" ... ")?;
    let outcome = match result {
        "ok" => Outcome::Passed,
        "FAILED" => Outcome::Failed,
        ignored if ignored == "ignored" || ignored.starts_with("ignored, ") => Outcome::Ignored,
        _ => return None,
    };
    let name = name.strip_suffix(" - should panic").unwrap_or(name);
    Some((name.to_owned(), outcome))
}

/// The case count of a test binary's header line, `running N test(s)`.
fn header(line: &str) -> Option<u64> {
    let count = line.strip_prefix("running ")?;
    let count = count
        .strip_suffix(" tests")
        .or_else(|| count.strip_suffix(" test"))?;
    count.parse().ok()
}

/// A section's summary counts: passed, failed, ignored, measured.
struct Summary {
    passed: u64,
    failed: u64,
    ignored: u64,
    measured: u64,
}

/// The counts of a `test result:` line; `None` for any other line.
fn summary(line: &str) -> Result<Option<Summary>, String> {
    let Some(rest) = line.strip_prefix("test result: ") else {
        return Ok(None);
    };
    let malformed = || format!("unrecognised libtest summary: {line:?}");
    let (_verdict, counts) = rest.split_once(". ").ok_or_else(malformed)?;
    let mut found = [None; 4];
    for part in counts.split(';') {
        let mut words = part.split_whitespace();
        let (Some(number), Some(label)) = (words.next(), words.next()) else {
            continue;
        };
        let slot = match label {
            "passed" => 0,
            "failed" => 1,
            "ignored" => 2,
            "measured" => 3,
            _ => continue,
        };
        found[slot] = Some(number.parse::<u64>().map_err(|_| malformed())?);
    }
    match found {
        [Some(passed), Some(failed), Some(ignored), Some(measured)] => Ok(Some(Summary {
            passed,
            failed,
            ignored,
            measured,
        })),
        _ => Err(malformed()),
    }
}

/// The number of `outcome` cases in `cases`.
fn count(cases: &[(String, Outcome)], outcome: Outcome) -> u64 {
    u64::try_from(cases.iter().filter(|(_, o)| *o == outcome).count()).unwrap_or(u64::MAX)
}

/// One test binary's section of the output, from its header on.
struct Section {
    /// N in `running N test(s)`.
    announced: u64,
    /// Its readable result lines.
    cases: Vec<(String, Outcome)>,
    /// Whether its failure report (captured output, never parsed) has begun.
    reporting: bool,
}

impl Section {
    /// Checks the section against its own header and summary.
    fn adds_up(&self, sum: &Summary) -> Result<(), String> {
        let (announced, cases) = (self.announced, &self.cases);
        let lines = u64::try_from(cases.len()).unwrap_or(u64::MAX);
        let summed = sum
            .passed
            .saturating_add(sum.failed)
            .saturating_add(sum.ignored)
            .saturating_add(sum.measured);
        let by_outcome = (
            count(cases, Outcome::Passed),
            count(cases, Outcome::Failed),
            count(cases, Outcome::Ignored),
        );
        if lines == announced
            && summed == announced
            && by_outcome == (sum.passed, sum.failed, sum.ignored)
        {
            Ok(())
        } else {
            Err(format!(
                "a test binary announced {announced} case(s), printed {lines} readable result \
                 line(s) ({} ok, {} FAILED, {} ignored) and summed up {} passed, {} failed, {} \
                 ignored, {} measured; the counts disagree",
                by_outcome.0,
                by_outcome.1,
                by_outcome.2,
                sum.passed,
                sum.failed,
                sum.ignored,
                sum.measured
            ))
        }
    }

    /// Why a section that never reached its summary cannot be trusted.
    fn cut_short(&self) -> String {
        format!(
            "a test binary announced {} case(s), printed {} result line(s) and stopped \
             before its `test result:` summary: it crashed, aborted or was killed",
            self.announced,
            self.cases.len()
        )
    }
}

/// Every case's outcome in one package's `cargo test --all-targets` output, trusted only
/// when the run is complete (br-judge-cargo-partial-run-afvw; owner decision, 2026-10-04):
///
/// 1. every test binary cargo started (a `     Running …` line on stderr) printed one
///    libtest section;
/// 2. every section's `running N test(s)` header is followed by its `test result:`
///    summary;
/// 3. in every section the readable result lines, N and the summary's passed + failed +
///    ignored + measured agree, outcome by outcome;
/// 4. a non-zero exit (`succeeded` false) is explained by at least one failed case.
///
/// Otherwise a binary stopped early (it crashed, aborted or was killed), or printed what
/// this parser cannot read, and the error says which. A doctest run (`--doc`) starts no
/// `Running` binaries, so it is not reconciled here.
///
/// After a section's `failures:` line, libtest prints the failing cases' captured output,
/// which may itself look like libtest output (a test of this module prints some). Until a
/// `test result:` line that adds up for the section, those lines are not read.
pub fn reconcile(
    stdout: &str,
    stderr: &str,
    succeeded: bool,
) -> Result<Vec<(String, Outcome)>, String> {
    let mut all = Vec::new();
    let mut open: Option<Section> = None;
    let mut sections = 0_usize;
    for line in stdout.lines() {
        if let Some(section) = open.as_ref().filter(|s| s.reporting) {
            // A failing case's captured output: only a summary that adds up ends it.
            let closes = summary(line)
                .ok()
                .flatten()
                .is_some_and(|sum| section.adds_up(&sum).is_ok());
            if closes && let Some(section) = open.take() {
                all.extend(section.cases);
            }
            continue;
        }
        if let Some(announced) = header(line) {
            if let Some(section) = &open {
                return Err(section.cut_short());
            }
            open = Some(Section {
                announced,
                cases: Vec::new(),
                reporting: false,
            });
            sections += 1;
        } else if let Some(case) = result_line(line) {
            let Some(section) = open.as_mut() else {
                return Err(format!(
                    "a result line outside any test binary's section: {line:?}"
                ));
            };
            section.cases.push(case);
        } else if line == "failures:"
            && let Some(section) = open.as_mut()
            && count(&section.cases, Outcome::Failed) > 0
        {
            section.reporting = true;
        } else if let Some(sum) = summary(line)? {
            let Some(section) = open.take() else {
                return Err(format!(
                    "a `test result:` summary with no `running N tests` header: {line:?}"
                ));
            };
            section.adds_up(&sum)?;
            all.extend(section.cases);
        }
    }
    if let Some(section) = &open {
        return Err(section.cut_short());
    }
    let started = stderr
        .lines()
        .filter(|line| line.starts_with("     Running ") && line.ends_with(')'))
        .count();
    if started != sections {
        return Err(format!(
            "cargo started {started} test binar(ies) but {sections} printed a libtest \
             section: a binary died before its `running N tests` header"
        ));
    }
    if !succeeded && count(&all, Outcome::Failed) == 0 {
        return Err(
            "cargo test exited unsuccessfully although no reported case failed: the run did \
             not finish cleanly"
                .to_owned(),
        );
    }
    Ok(all)
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
    fn outcomes_name_every_case_in_order() {
        let text = format!("{RUN}test c::boom - should panic ... ok\ntest c::later ... ignored\n");
        let observed = outcomes(&text);
        let expected: Vec<(String, Outcome)> = [
            ("a::passes", Outcome::Passed),
            ("a::fails", Outcome::Failed),
            ("a::skipped", Outcome::Ignored),
            ("b::one", Outcome::Passed),
            ("b::two", Outcome::Passed),
            ("c::boom", Outcome::Passed),
            ("c::later", Outcome::Ignored),
        ]
        .iter()
        .map(|&(name, outcome)| (name.to_owned(), outcome))
        .collect();
        assert_eq!(observed, expected, "input:\n{text}");
    }

    /// cargo's stderr for `n` started test binaries.
    fn started(n: usize) -> String {
        "     Running unittests src/lib.rs (target/debug/deps/b-0)\n".repeat(n)
    }

    #[test]
    fn outcomes_reconcile_a_complete_run_and_keep_failures_for_the_judge() {
        let observed = reconcile(RUN, &started(2), false);
        assert_eq!(
            observed.as_ref().map(Vec::len),
            Ok(5),
            "rule: a complete run's outcomes are trusted, its failed case included, and a \
             non-zero exit is explained by that failure; observed={observed:?}"
        );
        let clean = "running 1 test\ntest b::one ... ok\n\ntest result: ok. 1 passed; 0 \
                     failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        assert_eq!(
            reconcile(clean, &started(1), true),
            Ok(vec![("b::one".to_owned(), Outcome::Passed)])
        );
        assert_eq!(
            reconcile("", "", true),
            Ok(Vec::new()),
            "a package without test targets ran nothing, completely"
        );
    }

    #[test]
    fn outcomes_reconcile_past_a_failing_case_whose_output_looks_like_libtest() {
        // The failing case's captured output repeats this module's RUN fixture: headers,
        // result lines, a `failures:` line and a summary that are not the binary's own.
        let stdout = format!(
            "running 2 tests\ntest x::fine ... ok\ntest x::bad ... FAILED\n\nfailures:\n\n\
             ---- x::bad stdout ----\nassertion failed; input:\n{RUN}\n\nfailures:\n    \
             x::bad\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 \
             filtered out; finished in 0.00s\n"
        );
        assert_eq!(
            reconcile(&stdout, &started(1), false),
            Ok(vec![
                ("x::fine".to_owned(), Outcome::Passed),
                ("x::bad".to_owned(), Outcome::Failed),
            ]),
            "rule: a failure report's captured output is never read as results; input:\n{stdout}"
        );
        let cut = stdout
            .split("\nfailures:\n    x::bad")
            .next()
            .unwrap_or_default();
        assert!(
            reconcile(cut, &started(1), false)
                .is_err_and(|e| e.contains("stopped before its `test result:` summary")),
            "rule: a binary that dies inside its failure report is still cut short"
        );
    }

    #[test]
    fn outcomes_of_a_run_cut_short_or_unreadable_are_refused() {
        let aborted = "running 2 tests\ntest a::first ... ok\n";
        let summary_only = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 \
                            filtered out; finished in 0.00s\n";
        let short = format!(
            "running 3 tests\ntest a::one ... ok\ntest a::two ... ok\n\n{}",
            summary_only.replace("1 passed", "3 passed")
        );
        let cases: [(&str, String, String, bool, &str); 7] = [
            (
                "a binary that aborted after its first case",
                aborted.to_owned(),
                started(1),
                false,
                "stopped before its `test result:` summary",
            ),
            (
                "a binary aborted while another ran on",
                format!("{aborted}{RUN}"),
                started(3),
                false,
                "stopped before its `test result:` summary",
            ),
            (
                "a binary that died before its header",
                RUN.to_owned(),
                started(3),
                false,
                "died before its `running N tests` header",
            ),
            (
                "a result line the parser lost",
                short,
                started(1),
                true,
                "the counts disagree",
            ),
            (
                "a summary with no header",
                summary_only.to_owned(),
                started(1),
                true,
                "no `running N tests` header",
            ),
            (
                "a non-zero exit no failure explains",
                RUN.replace("test a::fails ... FAILED", "test a::fails ... ok")
                    .replace("1 passed; 1 failed", "2 passed; 0 failed"),
                started(2),
                false,
                "exited unsuccessfully although no reported case failed",
            ),
            (
                "a malformed summary",
                "running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n".to_owned(),
                started(1),
                true,
                "unrecognised libtest summary",
            ),
        ];
        for (what, stdout, stderr, succeeded, why) in cases {
            let observed = reconcile(&stdout, &stderr, succeeded);
            assert!(
                observed.as_ref().is_err_and(|e| e.contains(why)),
                "rule: {what} is an incomplete run, never trusted outcomes (afvw); \
                 expected an error containing {why:?}; observed={observed:?}"
            );
        }
    }

    #[test]
    fn failed_names_lists_only_failures() {
        assert_eq!(failed_names(RUN), ["a::fails"], "input:\n{RUN}");
    }
}
