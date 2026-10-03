//! AGENTS-RULES: `AGENTS.md` carries every PLAN.md §11.5 rule, verbatim, plus the
//! explicit replacement of the ACFS git checklist (W01 acceptance, §11.5).
//!
//! Rules are compared after normalising layout only: whitespace is collapsed and list
//! markers are dropped, so the wording, numbering and order must match exactly. Any
//! drift fails: PLAN.md wins (§0.4), so AGENTS.md is corrected to match, never the
//! reverse.

use std::fs;
use std::path::Path;

use crate::check::StaticOutcome;

/// Statements §11.5 requires besides the numbered rules: the explicit replacement of
/// the ACFS landing-the-plane checklist and its terms.
pub const REQUIRED_STATEMENTS: &[&str] = &[
    "REPLACES the ACFS \"Landing the Plane\" checklist",
    "Commits happen **only** through `./dev commit`",
    "Never raw `git add` / `git commit`",
    "Never `git pull --rebase` in the shared checkout",
    "under the commit lock",
];

// check: AGENTS-RULES
/// Compares `<root>/AGENTS.md` with `<root>/PLAN.md` §11.5; one case per plan rule plus
/// one per required statement.
pub fn check_agents_rules(root: &Path) -> Result<StaticOutcome, String> {
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).map_err(|err| format!("cannot read {name}: {err}"))
    };
    Ok(compare(&read("PLAN.md")?, &read("AGENTS.md")?))
}

/// The verdict for the given PLAN.md and AGENTS.md texts.
pub fn compare(plan: &str, agents: &str) -> StaticOutcome {
    let mut violations = Vec::new();
    let plan_rules = numbered_rules(section(plan, "### 11.5 ", "### "));
    let agents_rules = numbered_rules(section(agents, "## Agent Rules", "## "));
    if plan_rules.is_empty() {
        violations.push("PLAN.md §11.5 has no numbered rules; cannot check AGENTS.md".to_owned());
    }
    for (index, (number, rule)) in plan_rules.iter().enumerate() {
        match agents_rules.get(index) {
            None => violations.push(format!(
                "AGENTS.md lacks §11.5 rule {number}: {}",
                excerpt(rule, 0)
            )),
            Some((agents_number, _)) if agents_number != number => {
                violations.push(format!(
                    "AGENTS.md rule #{} is numbered {agents_number}; PLAN.md §11.5 numbers it {number}",
                    index + 1
                ));
            }
            Some((_, agents_rule)) if agents_rule != rule => {
                let at = first_difference(rule, agents_rule);
                violations.push(format!(
                    "AGENTS.md rule {number} differs from PLAN.md §11.5 at character {at}:\n  \
                     plan:   {}\n  agents: {}",
                    excerpt(rule, at),
                    excerpt(agents_rule, at)
                ));
            }
            Some(_) => {}
        }
    }
    for (number, _) in agents_rules.iter().skip(plan_rules.len()) {
        violations.push(format!(
            "AGENTS.md has rule {number}, which PLAN.md §11.5 lacks"
        ));
    }
    let flat = normalise(agents);
    for statement in REQUIRED_STATEMENTS {
        if !flat.contains(&normalise(statement)) {
            violations.push(format!(
                "AGENTS.md lacks the statement {statement:?} required by §11.5"
            ));
        }
    }
    StaticOutcome {
        cases: plan_rules.len() + REQUIRED_STATEMENTS.len(),
        violations,
    }
}

/// The lines from the first line starting with `heading` up to (excluding) the next
/// line starting with `next` or a `---` rule.
fn section<'a>(text: &'a str, heading: &str, next: &str) -> Vec<&'a str> {
    let mut lines = text.lines().skip_while(|line| !line.starts_with(heading));
    let Some(_heading) = lines.next() else {
        return Vec::new();
    };
    lines
        .take_while(|line| !line.starts_with(next) && line.trim() != "---")
        .collect()
}

/// Numbered rules (`N. **Title.** text`, with continuation lines), normalised.
fn numbered_rules(lines: Vec<&str>) -> Vec<(u32, String)> {
    let mut rules: Vec<(u32, Vec<&str>)> = Vec::new();
    for line in lines {
        let number = line
            .split_once(". **")
            .and_then(|(n, _)| n.parse::<u32>().ok());
        match number {
            Some(n) => rules.push((n, vec![line])),
            None => {
                if let Some((_, body)) = rules.last_mut() {
                    body.push(line);
                }
            }
        }
    }
    rules
        .into_iter()
        .map(|(n, body)| (n, normalise(&body.join("\n"))))
        .collect()
}

/// Collapses whitespace and drops list markers (`- `) at line starts.
fn normalise(text: &str) -> String {
    text.lines()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix("- ").unwrap_or(line)
        })
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_difference(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Up to 70 characters of `text` around character `at`.
fn excerpt(text: &str, at: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(20);
    let end = (start + 70).min(chars.len());
    chars.get(start..end).map_or_else(String::new, |slice| {
        format!("…{}…", slice.iter().collect::<String>())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "\
### 11.5 AGENTS.md rules (normative content)

Preamble that is not a rule.

1. **Owner override.** Explicit owner instructions win (§0.4).
3. **Oracles are sacred (P9).**
   - Never weaken a checker.
   - A genuine bug MUST be filed.

### 11.6 Next
";

    fn agents(rules: &str) -> String {
        format!(
            "# AGENTS\n\n## Git Discipline — REPLACES the ACFS \"Landing the Plane\" checklist\n\
             - Commits happen **only** through `./dev commit`.\n\
             - Never raw `git add` / `git commit`; never `git add -A`.\n\
             - Never `git pull --rebase` in the shared checkout.\n\
             - Pushes run under the commit lock.\n\n\
             ## Agent Rules (PLAN.md §11.5)\n\n{rules}\n---\n\n## Toolchain\n"
        )
    }

    #[test]
    fn layout_differences_are_ignored() {
        let rules = "1. **Owner override.** Explicit owner\n   instructions win (§0.4).\n\
                     3. **Oracles are sacred (P9).** Never weaken a checker. A genuine bug MUST\n\
                     \x20  be filed.\n";
        let outcome = compare(PLAN, &agents(rules));
        assert_eq!(
            outcome.violations,
            Vec::<String>::new(),
            "outcome={outcome:#?}"
        );
        assert_eq!(outcome.cases, 2 + REQUIRED_STATEMENTS.len());
    }

    #[test]
    fn a_wording_drift_is_reported_with_its_position() {
        let rules = "1. **Owner override.** Explicit owner instructions win (§0.4).\n\
                     3. **Oracles are sacred (P9).** Never weaken a checker. A genuine bug must be filed.\n";
        let outcome = compare(PLAN, &agents(rules));
        assert_eq!(outcome.violations.len(), 1, "outcome={outcome:#?}");
        assert!(
            outcome.violations[0].contains("rule 3 differs")
                && outcome.violations[0].contains("MUST"),
            "rule: MUST vs must is drift (the plan's normative capitalisation); outcome={outcome:#?}"
        );
    }

    #[test]
    fn missing_renumbered_and_extra_rules_are_reported() {
        let cases = [
            (
                "1. **Owner override.** Explicit owner instructions win (§0.4).\n",
                "lacks §11.5 rule 3",
            ),
            (
                "1. **Owner override.** Explicit owner instructions win (§0.4).\n\
                 2. **Oracles are sacred (P9).** Never weaken a checker. A genuine bug MUST be filed.\n",
                "is numbered 2",
            ),
            (
                "1. **Owner override.** Explicit owner instructions win (§0.4).\n\
                 3. **Oracles are sacred (P9).** Never weaken a checker. A genuine bug MUST be filed.\n\
                 4. **Extra.** Not in the plan.\n",
                "has rule 4, which PLAN.md §11.5 lacks",
            ),
        ];
        for (rules, expected) in cases {
            let outcome = compare(PLAN, &agents(rules));
            assert!(
                outcome.violations.iter().any(|v| v.contains(expected)),
                "rules={rules:?} expected {expected:?}; outcome={outcome:#?}"
            );
        }
    }

    #[test]
    fn the_git_checklist_replacement_is_required() {
        let rules = "1. **Owner override.** Explicit owner instructions win (§0.4).\n\
                     3. **Oracles are sacred (P9).** Never weaken a checker. A genuine bug MUST be filed.\n";
        let without = agents(rules).replace("under the commit lock", "by anyone");
        let outcome = compare(PLAN, &without);
        assert!(
            outcome
                .violations
                .iter()
                .any(|v| v.contains("under the commit lock")),
            "outcome={outcome:#?}"
        );
    }

    #[test]
    fn the_real_agents_md_matches_the_plan() {
        let root = crate::test_support::workspace_root();
        let outcome = check_agents_rules(&root).unwrap();
        assert_eq!(
            outcome.cases,
            18 + REQUIRED_STATEMENTS.len(),
            "outcome={outcome:#?}"
        );
        assert_eq!(outcome.violations, Vec::<String>::new());
    }
}
