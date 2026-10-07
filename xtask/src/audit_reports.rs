//! AUDIT-REPORTS: the drift-audit reports under `reports/audits/` are filed with their
//! provenance and every finding is linked to a bead (PLAN.md §11.9, §12.2 W20).
//!
//! The check cannot judge whether an audit was good. It judges what a reader needs to
//! trust it: who audited and that they were independent, which exact tree they audited,
//! and that no finding was left as prose: each one names a bead that the tracker export
//! knows. Nothing here reads a report's conclusions.
//!
//! A report is Markdown. Its header is `Key: value` lines before the first `## ` heading:
//! `Milestone`, `Auditor`, `Independent`, `Tree` (40 hex digits) and `Commit` (7 to 40
//! hex digits). Each finding is a `## F-<n>` heading whose section holds a `Bead: <id>`
//! line. A report with no findings says `Findings: none` instead.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::check::StaticOutcome;

/// Where reports live, relative to the workspace root.
pub const REPORTS: &str = "reports/audits";

/// The tracker export, relative to the workspace root.
pub const EXPORT: &str = ".beads/issues.jsonl";

/// Header keys every report carries.
const HEADER_KEYS: [&str; 5] = ["Milestone", "Auditor", "Independent", "Tree", "Commit"];

/// What one report's judgement counted and found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Assertions evaluated.
    pub cases: usize,
    /// One message per violation, each naming the report.
    pub violations: Vec<String>,
}

/// Records a violation of `report` in `verdict`.
fn fail(verdict: &mut Verdict, report: &str, message: impl Into<String>) {
    let message: String = message.into();
    verdict.violations.push(format!("{report}: {message}"));
}

/// The value of the first `key: value` header line, if present and non-empty.
fn header_value<'a>(header: &[&'a str], key: &str) -> Option<&'a str> {
    header.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        (name.trim() == key && !value.is_empty()).then_some(value)
    })
}

/// Whether `text` is `len_min` to `len_max` lowercase hex digits.
fn is_hex(text: &str, len_min: usize, len_max: usize) -> bool {
    (len_min..=len_max).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Judges the header lines: every key present, a 40-digit tree and a 7 to 40 digit commit.
fn judge_header(name: &str, header: &[&str], verdict: &mut Verdict) {
    for key in HEADER_KEYS {
        verdict.cases += 1;
        if header_value(header, key).is_none() {
            fail(
                verdict,
                name,
                format!("the header has no `{key}: <value>` line before the first `## ` heading"),
            );
        }
    }
    verdict.cases += 1;
    if let Some(tree) = header_value(header, "Tree")
        && !is_hex(tree, 40, 40)
    {
        fail(
            verdict,
            name,
            format!("`Tree: {tree}` is not a 40-digit lowercase hex tree ID"),
        );
    }
    verdict.cases += 1;
    if let Some(commit) = header_value(header, "Commit")
        && !is_hex(commit, 7, 40)
    {
        fail(
            verdict,
            name,
            format!("`Commit: {commit}` is not 7 to 40 lowercase hex digits"),
        );
    }
}

/// Judges one report's `text` (named `name` in messages) against the bead IDs the
/// tracker export `known` holds.
pub fn judge(name: &str, text: &str, known: &BTreeSet<String>) -> Verdict {
    let mut verdict = Verdict::default();
    let lines: Vec<&str> = text.lines().collect();
    let first_section = lines
        .iter()
        .position(|line| line.starts_with("## "))
        .unwrap_or(lines.len());
    let header = &lines[..first_section];

    judge_header(name, header, &mut verdict);

    let mut findings = 0_usize;
    let mut index = first_section;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        let Some(title) = line.strip_prefix("## F-") else {
            continue;
        };
        findings += 1;
        let id = title.split_whitespace().next().unwrap_or_default();
        verdict.cases += 1;
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            fail(
                &mut verdict,
                name,
                format!("the finding heading `{line}` is not `## F-<number> <title>`"),
            );
        }
        let end = lines[index..]
            .iter()
            .position(|next| next.starts_with("## "))
            .map_or(lines.len(), |offset| index + offset);
        let beads: Vec<&str> = lines[index..end]
            .iter()
            .filter_map(|body| body.strip_prefix("Bead:"))
            .map(str::trim)
            .collect();
        verdict.cases += 1;
        if beads.is_empty() {
            fail(
                &mut verdict,
                name,
                format!("finding F-{id} has no `Bead: <id>` line, so it is not a bead (§11.9)"),
            );
        }
        for bead in beads {
            verdict.cases += 1;
            let bead = bead.split_whitespace().next().unwrap_or_default();
            if !known.contains(bead) {
                fail(
                    &mut verdict,
                    name,
                    format!(
                        "finding F-{id} names bead `{bead}`, which the tracker export does not hold"
                    ),
                );
            }
        }
    }
    verdict.cases += 1;
    let none_stated = header.iter().any(|line| line.trim() == "Findings: none");
    if findings == 0 && !none_stated {
        fail(
            &mut verdict,
            name,
            "the report has no `## F-<n>` finding and does not say `Findings: none`".to_owned(),
        );
    }
    if findings > 0 && none_stated {
        fail(
            &mut verdict,
            name,
            "the report says `Findings: none` and also lists findings".to_owned(),
        );
    }
    verdict
}

/// The bead IDs in the tracker export `text` (one JSON object per line; a line that is
/// not an object with a string `id` is skipped).
pub fn bead_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value.get("id")?.as_str().map(str::to_owned))
        .collect()
}

// check: AUDIT-REPORTS
/// Judges every report under `<root>/reports/audits/`. No report at all is a failure: an
/// audit that was never filed has not satisfied §11.9 (P10).
pub fn check_audit_reports(root: &Path) -> Result<StaticOutcome, String> {
    let dir = root.join(REPORTS);
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|err| format!("cannot read {REPORTS}: {err}"))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                && name != "README.md"
        })
        .collect();
    names.sort();
    let export = fs::read_to_string(root.join(EXPORT))
        .map_err(|err| format!("cannot read {EXPORT}: {err}"))?;
    let known = bead_ids(&export);
    let mut outcome = StaticOutcome {
        cases: 1,
        violations: Vec::new(),
    };
    if names.is_empty() {
        outcome
            .violations
            .push(format!("{REPORTS}/ holds no audit report (*.md)"));
    }
    for name in names {
        let text = fs::read_to_string(dir.join(&name))
            .map_err(|err| format!("cannot read {REPORTS}/{name}: {err}"))?;
        let verdict = judge(&format!("{REPORTS}/{name}"), &text, &known);
        outcome.cases += verdict.cases;
        outcome.violations.extend(verdict.violations);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "b133ff95f89e7af59545c33dc90982c13d402ad4";

    fn known() -> BTreeSet<String> {
        ["br-a", "br-b"].map(str::to_owned).into_iter().collect()
    }

    fn report(header_extra: &str, body: &str) -> String {
        format!(
            "# M1 drift audit\n\nMilestone: M1\nAuditor: a fresh subagent\nIndependent: yes\n\
             Tree: {TREE}\nCommit: df75a44\n{header_extra}\n{body}"
        )
    }

    fn violations(text: &str) -> Vec<String> {
        judge("r.md", text, &known()).violations
    }

    fn assert_one_violation(text: &str, needle: &str, rule: &str) {
        let found = violations(text);
        assert!(
            found.len() == 1 && found[0].contains(needle),
            "rule: {rule}; wanted one violation containing {needle:?}; found={found:?}"
        );
    }

    #[test]
    fn a_report_whose_findings_each_name_a_known_bead_is_clean() {
        let text = report(
            "",
            "## F-1 first\nBead: br-a\n\nprose\n\n## F-2 second\nBead: br-b (conflict)\n",
        );
        let verdict = judge("r.md", &text, &known());
        assert_eq!(
            verdict.violations,
            Vec::<String>::new(),
            "rule: findings that name known beads pass"
        );
        assert!(
            verdict.cases > 10,
            "rule: the cases are counted, cases={}",
            verdict.cases
        );
    }

    #[test]
    fn a_finding_without_a_bead_is_a_violation() {
        assert_one_violation(
            &report("", "## F-1 first\nno bead here\n"),
            "F-1 has no `Bead:",
            "a finding left as prose is not a bead (§11.9)",
        );
    }

    #[test]
    fn a_finding_that_names_an_unknown_bead_is_a_violation() {
        assert_one_violation(
            &report("", "## F-1 first\nBead: br-nope\n"),
            "br-nope",
            "a bead the tracker export does not hold is not a bead",
        );
    }

    #[test]
    fn a_bead_line_belongs_to_its_own_finding_only() {
        let found = violations(&report("", "## F-1 first\nBead: br-a\n## F-2 second\n"));
        assert!(
            found.len() == 1 && found[0].contains("F-2"),
            "rule: F-1's bead does not cover F-2; found={found:?}"
        );
    }

    #[test]
    fn each_header_key_is_required_and_non_empty() {
        for key in HEADER_KEYS {
            let text = report("", "Findings: none\n")
                .lines()
                .map(|line| {
                    if line.starts_with(&format!("{key}:")) {
                        format!("{key}:")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_one_violation(
                &text,
                &format!("`{key}:"),
                "a header key with no value is missing",
            );
        }
    }

    #[test]
    fn a_header_key_below_the_first_section_does_not_count() {
        let text = "# t\n\nMilestone: M1\nAuditor: x\nIndependent: yes\n\n## F-1 a\nBead: br-a\nTree: aaaaaaaa\nCommit: abcdef0\n";
        let found = violations(text);
        assert_eq!(
            found.len(),
            2,
            "rule: Tree and Commit must be in the header; found={found:?}"
        );
    }

    #[test]
    fn the_tree_and_commit_must_be_hex_of_the_right_length() {
        for (tree, commit, needle) in [
            ("b133ff9", "df75a44", "Tree: b133ff9"),
            (
                "B133FF95F89E7AF59545C33DC90982C13D402AD4",
                "df75a44",
                "Tree: B133",
            ),
            (TREE, "df75a4", "Commit: df75a4"),
            (TREE, "main", "Commit: main"),
        ] {
            let text = format!(
                "# t\n\nMilestone: M1\nAuditor: x\nIndependent: yes\nTree: {tree}\nCommit: {commit}\nFindings: none\n"
            );
            assert_one_violation(
                &text,
                needle,
                "tree and commit identify the audited commit exactly",
            );
        }
    }

    #[test]
    fn a_report_must_list_findings_or_say_there_are_none() {
        assert_one_violation(
            &report("", "## Notes\nnothing\n"),
            "no `## F-<n>` finding",
            "silence is not a verdict",
        );
        assert!(
            violations(&report("Findings: none", "## Notes\nnothing\n")).is_empty(),
            "rule: `Findings: none` is an explicit empty verdict"
        );
        assert_one_violation(
            &report("Findings: none", "## F-1 a\nBead: br-a\n"),
            "also lists findings",
            "none and some contradict",
        );
    }

    #[test]
    fn a_malformed_finding_heading_is_a_violation() {
        let found = violations(&report("", "## F-x bad\nBead: br-a\n"));
        assert!(
            found
                .iter()
                .any(|message| message.contains("not `## F-<number>")),
            "rule: finding IDs are numbers; found={found:?}"
        );
    }

    #[test]
    fn bead_ids_come_from_the_ids_of_the_export_lines_only() {
        let ids = bead_ids(
            "{\"id\":\"br-a\",\"x\":1}\nnot json\n{\"id\":7}\n{\"y\":\"br-z\"}\n{\"id\":\"br-b\"}\n",
        );
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            ["br-a", "br-b"],
            "rule: only string ids of JSON lines count"
        );
    }

    #[test]
    fn a_workspace_without_reports_fails_and_one_with_a_good_report_passes() {
        let root = crate::test_support::scratch_dir("audit-reports");
        let reports = root.join(REPORTS);
        fs::create_dir_all(&reports).unwrap();
        fs::create_dir_all(root.join(".beads")).unwrap();
        fs::write(root.join(EXPORT), "{\"id\":\"br-a\"}\n").unwrap();
        let empty = check_audit_reports(&root).unwrap();
        assert_eq!(
            empty.violations.len(),
            1,
            "rule: no report is a failure; {empty:?}"
        );
        fs::write(reports.join("m1.md"), report("", "## F-1 a\nBead: br-a\n")).unwrap();
        fs::write(reports.join("README.md"), "not a report").unwrap();
        let good = check_audit_reports(&root).unwrap();
        assert_eq!(
            good.violations,
            Vec::<String>::new(),
            "rule: a good report passes"
        );
        assert!(
            good.cases > 10,
            "rule: cases are counted; cases={}",
            good.cases
        );
    }
}
