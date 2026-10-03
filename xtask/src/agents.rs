//! AGENTS-RULES: `AGENTS.md` carries every PLAN.md §11.5 rule, verbatim, plus the
//! explicit replacement of the ACFS git checklist (W01 acceptance, §11.5).
//!
//! Rules are compared after normalising layout only: whitespace is collapsed and list
//! markers are dropped, so the wording, numbering and order must match exactly. Any
//! drift fails: PLAN.md wins (§0.4), so AGENTS.md is corrected to match, never the
//! reverse. Each §11.5 rule is paired with its AGENTS.md copy by text and number, not
//! by position, so a missing, repeated or moved rule is reported as itself instead of
//! shifting the blame onto every rule after it.

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

/// One numbered rule: its number, and its normalised text after the number.
struct Rule {
    number: u32,
    text: String,
}

/// How a §11.5 rule found its AGENTS.md copy.
#[derive(Clone, Copy)]
enum Pairing {
    /// The same number and the same text.
    Exact,
    /// The same text under another number.
    Renumbered,
    /// The same number with other text.
    Reworded,
}

impl Pairing {
    /// The pairing passes, strongest first.
    const PASSES: [Self; 3] = [Self::Exact, Self::Renumbered, Self::Reworded];

    /// Whether `copy` pairs with `rule` in this way.
    fn matches(self, rule: &Rule, copy: &Rule) -> bool {
        match self {
            Self::Exact => rule.number == copy.number && rule.text == copy.text,
            Self::Renumbered => rule.text == copy.text,
            Self::Reworded => rule.number == copy.number,
        }
    }
}

/// A §11.5 rule's AGENTS.md copy: its position among the AGENTS.md rules, the copy,
/// and how the two paired.
#[derive(Clone, Copy)]
struct Pair<'a> {
    index: usize,
    copy: &'a Rule,
    how: Pairing,
}

/// The verdict for the given PLAN.md and AGENTS.md texts.
pub fn compare(plan: &str, agents: &str) -> StaticOutcome {
    let mut violations = Vec::new();
    let plan_rules = numbered_rules(section(plan, "### 11.5 ", "### "));
    let agents_rules = numbered_rules(section(agents, "## Agent Rules", "## "));
    if plan_rules.is_empty() {
        violations.push("PLAN.md §11.5 has no numbered rules; cannot check AGENTS.md".to_owned());
    }
    violations.extend(rule_violations(&plan_rules, &agents_rules));
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

/// Every way the AGENTS.md rules depart from the §11.5 rules, in §11.5 order, then any
/// rules out of order, then any AGENTS.md rules that pair with none.
fn rule_violations(plan: &[Rule], agents: &[Rule]) -> Vec<String> {
    let pairs = pair_rules(plan, agents);
    let mut violations = Vec::new();
    for (rule, pair) in plan.iter().zip(&pairs) {
        match pair {
            None => violations.push(format!(
                "AGENTS.md lacks §11.5 rule {}: {}",
                rule.number,
                excerpt(&rule.text, 0)
            )),
            Some(Pair {
                how: Pairing::Exact,
                ..
            }) => {}
            Some(Pair {
                copy,
                how: Pairing::Renumbered,
                ..
            }) => violations.push(format!(
                "AGENTS.md's copy of §11.5 rule {} is numbered {}",
                rule.number, copy.number
            )),
            Some(Pair {
                copy,
                how: Pairing::Reworded,
                ..
            }) => {
                let at = first_difference(&rule.text, &copy.text);
                violations.push(format!(
                    "AGENTS.md rule {} differs from PLAN.md §11.5 at character {at}:\n  \
                     plan:   {}\n  agents: {}",
                    rule.number,
                    excerpt(&rule.text, at),
                    excerpt(&copy.text, at)
                ));
            }
        }
    }
    violations.extend(order_violations(plan, &pairs));
    violations.extend(extra_violations(plan, agents, &pairs));
    violations
}

/// For each §11.5 rule, its AGENTS.md copy, if any. Pairing runs three passes over all
/// the rules, strongest first: the same number and text, then the same text, then the
/// same number. Each AGENTS.md rule pairs at most once, so a rule that was only
/// renumbered is never mistaken for a reworded one.
fn pair_rules<'a>(plan: &[Rule], agents: &'a [Rule]) -> Vec<Option<Pair<'a>>> {
    let mut taken = vec![false; agents.len()];
    let mut pairs: Vec<Option<Pair<'a>>> = vec![None; plan.len()];
    for how in Pairing::PASSES {
        for (slot, rule) in pairs.iter_mut().zip(plan) {
            if slot.is_some() {
                continue;
            }
            let found = agents
                .iter()
                .zip(&taken)
                .position(|(copy, used)| !used && how.matches(rule, copy));
            if let Some(index) = found
                && let (Some(copy), Some(used)) = (agents.get(index), taken.get_mut(index))
            {
                *used = true;
                *slot = Some(Pair { index, copy, how });
            }
        }
    }
    pairs
}

/// The copies must keep §11.5's order: reports each rule that AGENTS.md lists before the
/// paired rule preceding it in §11.5.
fn order_violations(plan: &[Rule], pairs: &[Option<Pair<'_>>]) -> Vec<String> {
    let mut violations = Vec::new();
    let mut previous: Option<(u32, usize)> = None;
    for (rule, pair) in plan.iter().zip(pairs) {
        let Some(pair) = pair else {
            continue;
        };
        if let Some((number, index)) = previous
            && pair.index < index
        {
            violations.push(format!(
                "AGENTS.md lists §11.5 rule {} before rule {number}; §11.5 orders {number} first",
                rule.number
            ));
        }
        previous = Some((rule.number, pair.index));
    }
    violations
}

/// The AGENTS.md rules that pair with no §11.5 rule: repeats of a §11.5 rule, second
/// rules under a §11.5 number, and rules §11.5 does not have.
fn extra_violations(plan: &[Rule], agents: &[Rule], pairs: &[Option<Pair<'_>>]) -> Vec<String> {
    agents
        .iter()
        .enumerate()
        .filter(|(index, _)| !pairs.iter().flatten().any(|pair| pair.index == *index))
        .map(|(_, copy)| {
            plan.iter().find(|rule| rule.text == copy.text).map_or_else(
                || unmatched(plan, copy),
                |rule| {
                    format!(
                        "AGENTS.md repeats §11.5 rule {} as its rule {}",
                        rule.number, copy.number
                    )
                },
            )
        })
        .collect()
}

/// The report for an AGENTS.md rule whose text is in no §11.5 rule.
fn unmatched(plan: &[Rule], copy: &Rule) -> String {
    if plan.iter().any(|rule| rule.number == copy.number) {
        format!(
            "AGENTS.md has a second rule {}, which matches no §11.5 rule: {}",
            copy.number,
            excerpt(&copy.text, 0)
        )
    } else {
        format!(
            "AGENTS.md has rule {}, which PLAN.md §11.5 lacks: {}",
            copy.number,
            excerpt(&copy.text, 0)
        )
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

/// Numbered rules (`N. **Title.** text`, with continuation lines), normalised, each
/// with its text after the number.
fn numbered_rules(lines: Vec<&str>) -> Vec<Rule> {
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
        .map(|(number, body)| {
            let full = normalise(&body.join("\n"));
            let prefix = format!("{number}. ");
            let text = full.strip_prefix(&prefix).unwrap_or(&full).to_owned();
            Rule { number, text }
        })
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

    /// The real PLAN.md, and the real AGENTS.md with its rule `number` cut out.
    fn real_texts_without_rule(number: u32) -> (String, String) {
        let root = crate::test_support::workspace_root();
        let read = |name: &str| fs::read_to_string(root.join(name)).unwrap();
        let agents = read("AGENTS.md");
        let start = format!("{number}. **");
        let next = format!("{}. **", number + 1);
        let mut cutting = false;
        let kept: Vec<&str> = agents
            .lines()
            .filter(|line| {
                if line.starts_with(&start) {
                    cutting = true;
                } else if line.starts_with(&next) || line.starts_with("## ") {
                    cutting = false;
                }
                !cutting
            })
            .collect();
        assert_ne!(
            kept.len(),
            agents.lines().count(),
            "rule {number} was not found"
        );
        (read("PLAN.md"), kept.join("\n"))
    }

    #[test]
    fn a_deleted_rule_is_reported_as_itself_not_as_the_last_rule() {
        let (plan, agents) = real_texts_without_rule(7);
        let outcome = compare(&plan, &agents);
        assert_eq!(
            outcome.violations.len(),
            1,
            "rule: one deleted rule is one defect, not a shift of every later rule; \
             outcome={outcome:#?}"
        );
        assert!(
            outcome.violations[0].contains("lacks §11.5 rule 7:")
                && !outcome.violations[0].contains("rule 18"),
            "rule: the deleted rule is named, not the last one; outcome={outcome:#?}"
        );
    }

    const PLAN_ABC: &str = "\
### 11.5 AGENTS.md rules (normative content)

1. **Alpha.** First rule.
2. **Beta.** Second rule.
3. **Gamma.** Third rule.

### 11.6 Next
";

    #[test]
    fn renumbering_after_a_deletion_reports_the_gap_and_the_shift() {
        let rules = "1. **Alpha.** First rule.\n2. **Gamma.** Third rule.\n";
        let outcome = compare(PLAN_ABC, &agents(rules));
        assert_eq!(outcome.violations.len(), 2, "outcome={outcome:#?}");
        assert!(
            outcome.violations[0].contains("lacks §11.5 rule 2:")
                && outcome.violations[1].contains("rule 3 is numbered 2"),
            "rule: a renumbered rule is recognised by its text, not reported as reworded; \
             outcome={outcome:#?}"
        );
    }

    #[test]
    fn a_moved_rule_is_reported_as_out_of_order() {
        let rules = "1. **Alpha.** First rule.\n3. **Gamma.** Third rule.\n\
                     2. **Beta.** Second rule.\n";
        let outcome = compare(PLAN_ABC, &agents(rules));
        assert_eq!(outcome.violations.len(), 1, "outcome={outcome:#?}");
        assert!(
            outcome.violations[0].contains("lists §11.5 rule 3 before rule 2"),
            "rule: §11.5's order is part of the contract; outcome={outcome:#?}"
        );
    }

    #[test]
    fn a_repeated_rule_is_reported_as_a_repeat() {
        let rules = "1. **Alpha.** First rule.\n2. **Beta.** Second rule.\n\
                     2. **Beta.** Second rule.\n3. **Gamma.** Third rule.\n";
        let outcome = compare(PLAN_ABC, &agents(rules));
        assert_eq!(outcome.violations.len(), 1, "outcome={outcome:#?}");
        assert!(
            outcome.violations[0].contains("repeats §11.5 rule 2"),
            "rule: a duplicate is one defect, not a renumbering of every later rule; \
             outcome={outcome:#?}"
        );
    }
}
