//! Lint-policy checks (PLAN.md §5.2, §7.14, §11.5 rules 4 and 11; AGENTS.md "Lint
//! Policy").
//!
//! - **LINT-TIERS:** every workspace member inherits the workspace lint table, and every
//!   deterministic crate carries the canonical deterministic configuration byte for byte:
//!   `xtask/lint/deterministic.clippy.toml` as its `clippy.toml`, and
//!   `xtask/lint/deterministic-lints.rs` at the top of its `lib.rs`.
//! - **LINT-ALLOWS:** no first-party source `allow`s (or `expect`s) a lint that enforces
//!   a CD principle, and every other `allow` names the bead that decided it.
//! - **LINT-PROBES** (the tests in `probes` below): every denied lint and every
//!   determinism entry is proven to fire on the pinned toolchain, and the controls pass.
//! - **Test code** ([`test_code_ranges`]): the code CD-8 lets use `panic!`, in clippy's
//!   sense, so that the gate's UBS step can judge production code alone.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::check::StaticOutcome;
use crate::metadata::{self, Package};
use crate::tiers;

/// The probe crate that proves the deterministic configuration carries it too.
const DETERMINISTIC_PROBE_DIR: &str = "xtask/probes/deterministic";

/// The canonical clippy configuration of deterministic crates, relative to the root.
pub const CANONICAL_CLIPPY: &str = "xtask/lint/deterministic.clippy.toml";

/// The canonical lint block that opens every deterministic crate's `lib.rs`.
pub const CANONICAL_BLOCK: &str = "xtask/lint/deterministic-lints.rs";

/// Lints, and lint groups containing them, that enforce a CD principle or the
/// determinism rules. First-party code never `allow`s or `expect`s these (rule 11).
pub const CD_LINTS: &[&str] = &[
    "unsafe_code",
    "missing_docs",
    "clippy::unwrap_used",
    "clippy::expect_used",
    "clippy::panic",
    "clippy::todo",
    "clippy::unimplemented",
    "clippy::unreachable",
    "clippy::dbg_macro",
    "clippy::wildcard_enum_match_arm",
    "clippy::disallowed_methods",
    "clippy::disallowed_types",
    "clippy::disallowed_macros",
    // Groups that contain the lints above.
    "warnings",
    "clippy::all",
    "clippy::style",
    "clippy::restriction",
];

/// Paths (relative to the root) whose `.rs` files are not first-party policy subjects:
/// build output, third-party code, and fixtures that violate the policy on purpose.
const SCAN_EXCLUDED: &[&str] = &[
    "target",
    "vendor",
    "artifacts",
    "xtask/probes",
    "xtask/tests/fixtures",
];

/// How many lines above an `allow` may hold its decision-log comment.
const DECISION_LOG_WINDOW: usize = 3;

// ---------------------------------------------------------------------------------------
// LINT-TIERS

// check: LINT-TIERS
/// Checks every workspace member's lint configuration; one case per member.
pub fn check_tiers(root: &Path) -> Result<StaticOutcome, String> {
    let metadata = metadata::load(root)?;
    let canonical_clippy = fs::read(root.join(CANONICAL_CLIPPY))
        .map_err(|err| format!("cannot read {CANONICAL_CLIPPY}: {err}"))?;
    let canonical_block = fs::read_to_string(root.join(CANONICAL_BLOCK))
        .map_err(|err| format!("cannot read {CANONICAL_BLOCK}: {err}"))?;
    let root = &metadata.workspace_root;
    let members = metadata.members();
    let violations = members
        .iter()
        .flat_map(|package| {
            let dir = package
                .dir()
                .strip_prefix(root)
                .unwrap_or_else(|_| package.dir());
            let deterministic = dir == Path::new(DETERMINISTIC_PROBE_DIR)
                || tiers::classify(dir).is_some_and(tiers::Tier::is_deterministic);
            tier_violations(
                package,
                deterministic,
                root,
                &canonical_clippy,
                &canonical_block,
            )
        })
        .collect();
    Ok(StaticOutcome {
        cases: members.len(),
        violations,
    })
}

/// `deterministic`: whether the crate must carry the canonical deterministic config.
fn tier_violations(
    package: &Package,
    deterministic: bool,
    root: &Path,
    canonical_clippy: &[u8],
    canonical_block: &str,
) -> Vec<String> {
    let name = &package.name;
    let mut violations = Vec::new();
    if let Err(why) = inherits_workspace_lints(&package.manifest_path) {
        violations.push(format!("{name}: {why}"));
    }
    let effective = nearest_clippy_config(package.dir(), root);
    if deterministic {
        let own = package.dir().join("clippy.toml");
        if effective.as_deref() != Some(own.as_path()) {
            violations.push(format!(
                "{name}: a deterministic crate needs its own clippy.toml (a copy of \
                 {CANONICAL_CLIPPY}); clippy would use {effective:?}"
            ));
        } else if fs::read(&own).ok().as_deref() != Some(canonical_clippy) {
            violations.push(format!(
                "{name}: {} differs from {CANONICAL_CLIPPY}; copy it byte for byte",
                own.display()
            ));
        }
        match package.lib_src().map(fs::read_to_string) {
            Some(Ok(text)) if text.starts_with(canonical_block) => {}
            Some(Ok(_)) => violations.push(format!(
                "{name}: lib.rs must start with the exact block in {CANONICAL_BLOCK}"
            )),
            Some(Err(err)) => violations.push(format!("{name}: cannot read lib.rs: {err}")),
            None => violations.push(format!("{name}: a deterministic crate needs a lib target")),
        }
    } else if effective != Some(root.join("clippy.toml")) {
        violations.push(format!(
            "{name}: must use the workspace clippy.toml, but clippy would use {effective:?}"
        ));
    }
    violations
}

/// Whether the manifest's `[lints]` table is exactly `workspace = true`.
fn inherits_workspace_lints(manifest: &Path) -> Result<(), String> {
    let text = fs::read_to_string(manifest).map_err(|err| format!("cannot read: {err}"))?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|err| format!("invalid manifest: {}", err.message()))?;
    let lints = table.get("lints").and_then(toml::Value::as_table);
    let inherits = lints.is_some_and(|lints| {
        lints.len() == 1 && lints.get("workspace").and_then(toml::Value::as_bool) == Some(true)
    });
    if inherits {
        Ok(())
    } else {
        Err("Cargo.toml must have `[lints] workspace = true` and nothing else under [lints]".into())
    }
}

/// The configuration file clippy uses for a crate in `dir`: the nearest `clippy.toml` or
/// `.clippy.toml` in `dir` or an ancestor, up to `root`.
fn nearest_clippy_config(dir: &Path, root: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        for name in ["clippy.toml", ".clippy.toml"] {
            let candidate = ancestor.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if ancestor == root {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------------------
// LINT-ALLOWS

// check: LINT-ALLOWS
/// Scans every first-party `.rs` file under `root`; one case per file.
pub fn check_allows(root: &Path) -> Result<StaticOutcome, String> {
    let files = first_party_sources(root)?;
    let mut violations = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file)
            .map_err(|err| format!("cannot read {}: {err}", file.display()))?;
        let shown = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        violations.extend(scan_allows(&shown, &text));
    }
    Ok(StaticOutcome {
        cases: files.len(),
        violations,
    })
}

/// Every first-party `.rs` file under `root`, sorted: everything except hidden
/// directories and the [`SCAN_EXCLUDED`] paths.
pub fn first_party_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_sources(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_sources(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden || SCAN_EXCLUDED.iter().any(|ex| relative == Path::new(ex)) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot stat {}: {err}", path.display()))?;
        if kind.is_dir() {
            collect_sources(root, &path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// The policy violations among the lint-relaxing attributes (`allow`/`expect`, also
/// inside `cfg_attr`) of one source file. `path` is only used in the messages.
pub fn scan_allows(path: &str, text: &str) -> Vec<String> {
    let masked = mask_comments_and_strings(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut violations = Vec::new();
    for attribute in find_attributes(&masked) {
        for relax in relaxations(&masked, &attribute) {
            let line = line_of(&masked, attribute.start);
            for lint in &relax.lints {
                if CD_LINTS.contains(&lint.as_str()) {
                    violations.push(format!(
                        "{path}:{line}: `{}({lint})` relaxes a lint that enforces a CD \
                         principle; fix the code instead (§11.5 rule 11)",
                        relax.kind
                    ));
                }
            }
            let window_start = line.saturating_sub(1 + DECISION_LOG_WINDOW);
            let window_end = line_of(&masked, attribute.end).min(lines.len());
            let documented = lines
                .get(window_start..window_end)
                .is_some_and(|window| window.iter().any(|l| names_a_bead(l)));
            if !documented {
                violations.push(format!(
                    "{path}:{line}: `{}({})` has no decision-log comment naming the bead \
                     that decided it (AGENTS.md escape-hatch policy)",
                    relax.kind,
                    relax.lints.join(", ")
                ));
            }
        }
    }
    violations
}

/// A `#[...]` or `#![...]` attribute: byte offsets of `#` and of the closing `]`.
struct Attribute {
    start: usize,
    end: usize,
    body_start: usize,
}

/// An `allow(...)` or `expect(...)` and the lints it names.
struct Relaxation {
    kind: &'static str,
    lints: Vec<String>,
}

fn find_attributes(masked: &str) -> Vec<Attribute> {
    let bytes = masked.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(offset) = masked.get(i..).and_then(|rest| rest.find('#')) {
        let start = i + offset;
        let mut j = start + 1;
        if bytes.get(j) == Some(&b'!') {
            j += 1;
        }
        if bytes.get(j) == Some(&b'[')
            && let Some(close) = matching(bytes, j, b'[', b']')
        {
            found.push(Attribute {
                start,
                end: close,
                body_start: j + 1,
            });
            i = close + 1;
            continue;
        }
        i = start + 1;
    }
    found
}

fn relaxations(masked: &str, attribute: &Attribute) -> Vec<Relaxation> {
    let bytes = masked.as_bytes();
    let body = masked
        .get(attribute.body_start..attribute.end)
        .unwrap_or("");
    let mut found = Vec::new();
    for kind in ["allow", "expect"] {
        let mut from = 0;
        while let Some(offset) = body.get(from..).and_then(|rest| rest.find(kind)) {
            let at = from + offset;
            from = at + kind.len();
            let before = body.get(..at).and_then(|b| b.chars().next_back());
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let after = body.get(from..).unwrap_or("");
            let open = from + (after.len() - after.trim_start().len());
            if body.as_bytes().get(open) != Some(&b'(') {
                continue;
            }
            let absolute_open = attribute.body_start + open;
            let Some(close) = matching(bytes, absolute_open, b'(', b')') else {
                continue;
            };
            let list = masked.get(absolute_open + 1..close).unwrap_or("");
            found.push(Relaxation {
                kind,
                lints: split_lints(list),
            });
        }
    }
    found
}

fn split_lints(list: &str) -> Vec<String> {
    let mut lints = Vec::new();
    let mut depth = 0_u32;
    let mut current = String::new();
    for c in list.chars().chain(std::iter::once(',')) {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let item: String = current.chars().filter(|c| !c.is_whitespace()).collect();
                if !item.is_empty() && !item.contains('=') {
                    lints.push(item);
                }
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    lints
}

fn matching(bytes: &[u8], open_at: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, &byte) in bytes.iter().enumerate().skip(open_at) {
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn line_of(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(0, |prefix| prefix.matches('\n').count())
        + 1
}

/// Whether a line names a bead, e.g. `br-w01-3-lint-policy-probes-c71`.
fn names_a_bead(line: &str) -> bool {
    line.match_indices("br-").any(|(at, _)| {
        let before = line.get(..at).and_then(|b| b.chars().next_back());
        let after = line.get(at + 3..).and_then(|a| a.chars().next());
        !before.is_some_and(char::is_alphanumeric)
            && after.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// `text` with the contents of comments, string literals and character literals
/// replaced by spaces, so that attribute-like text inside them is never matched.
/// Newlines are kept, so byte offsets and line numbers are unchanged.
pub fn mask_comments_and_strings(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in out.iter_mut().take(to).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    while i < bytes.len() {
        let rest = bytes.get(i..).unwrap_or_default();
        if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |p| i + p);
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(b"/*") {
            let mut depth = 0_usize;
            let mut j = i;
            while j < bytes.len() {
                let here = bytes.get(j..).unwrap_or_default();
                if here.starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if here.starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if let Some(end) = raw_string_end(bytes, i) {
            blank(&mut out, i, end);
            i = end;
        } else if rest.first() == Some(&b'"') {
            let mut j = i + 1;
            while j < bytes.len() && bytes.get(j) != Some(&b'"') {
                j += if bytes.get(j) == Some(&b'\\') { 2 } else { 1 };
            }
            let end = (j + 1).min(bytes.len());
            blank(&mut out, i, end);
            i = end;
        } else if let Some(end) = char_literal_end(rest) {
            blank(&mut out, i, i + end);
            i += end;
        } else {
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// If a raw string literal (`r"…"`, `r#"…"#`, `br#"…"#`) starts at `i`, its end offset.
fn raw_string_end(bytes: &[u8], i: usize) -> Option<usize> {
    let previous = i.checked_sub(1).and_then(|p| bytes.get(p));
    if previous.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        return None;
    }
    let mut j = i;
    if bytes.get(j) == Some(&b'b') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = bytes.get(j..)?.iter().take_while(|&&b| b == b'#').count();
    j += hashes;
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let mut terminator = vec![b'"'];
    terminator.extend(std::iter::repeat_n(b'#', hashes));
    let body = bytes.get(j + 1..)?;
    let close = body
        .windows(terminator.len())
        .position(|window| window == terminator.as_slice())?;
    Some(j + 1 + close + terminator.len())
}

/// If a character literal such as `'"'` or `'\''` starts `rest`, its length. Lifetimes
/// (`'a`) are not literals.
fn char_literal_end(rest: &[u8]) -> Option<usize> {
    if rest.first() != Some(&b'\'') {
        return None;
    }
    let lead = *rest.get(1)?;
    if lead == b'\\' {
        // Skip the backslash and the escaped byte, which may itself be a quote.
        let close = rest.iter().skip(3).position(|&b| b == b'\'')?;
        return Some(close + 4);
    }
    // Width of the UTF-8 sequence that starts with `lead`.
    let width = match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    };
    (rest.get(1 + width) == Some(&b'\'')).then_some(width + 2)
}

// ---------------------------------------------------------------------------------------
// Test code (CD-8: tests may use unwrap/expect/panic!)

/// Keywords that start an item. An outer `#[cfg(test)]` or `#[test]` makes test code only
/// of an item that starts with one of these (after any further attributes and a
/// visibility), so attributes on statements, fields, variants and match arms never
/// widen a test range over the production code that follows them.
const ITEM_KEYWORDS: &[&str] = &[
    "async",
    "const",
    "enum",
    "extern",
    "fn",
    "impl",
    "macro_rules",
    "mod",
    "static",
    "struct",
    "trait",
    "type",
    "union",
    "unsafe",
    "use",
];

/// The byte ranges of a Rust source that are test code in clippy's sense, the sense of
/// `allow-panic-in-tests` (AGENTS.md "Lint Policy"): every item under `#[cfg(test)]` or
/// `#[test]`, and the whole module or file of a `#![cfg(test)]` (which every
/// `tests/*.rs` file starts with). The classification is conservative. Anything it
/// cannot delimit with certainty stays production code, including the file of an
/// out-of-line `#[cfg(test)] mod name;` unless that file starts with `#![cfg(test)]`.
pub fn test_code_ranges(text: &str) -> Vec<Range<usize>> {
    let masked = mask_comments_and_strings(text);
    let bytes = masked.as_bytes();
    let mut ranges = Vec::new();
    for attribute in find_attributes(&masked) {
        let body: String = masked
            .get(attribute.body_start..attribute.end)
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let inner = bytes.get(attribute.start + 1) == Some(&b'!');
        let range = match (inner, body.as_str()) {
            (true, "cfg(test)") => enclosing_scope(bytes, attribute.start),
            (false, "cfg(test)" | "test") => {
                item_end(&masked, attribute.end + 1).map(|end| attribute.start..end)
            }
            _ => None,
        };
        ranges.extend(range);
    }
    ranges
}

/// `text` with its test code ([`test_code_ranges`]) blanked to spaces. Newlines are
/// kept, so every production line keeps its line number.
pub fn production_view(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for range in test_code_ranges(text) {
        for byte in bytes.get_mut(range).into_iter().flatten() {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // Every range starts and ends at an ASCII byte, so the result is still UTF-8.
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The brace-delimited scope that contains `at`, braces included, or the whole text
/// when `at` is at the top level.
fn enclosing_scope(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    let mut depth = 0_usize;
    for index in (0..at).rev() {
        match bytes.get(index) {
            Some(b'}') => depth += 1,
            Some(b'{') if depth == 0 => {
                let close = matching(bytes, index, b'{', b'}')?;
                return Some(index..close + 1);
            }
            Some(b'{') => depth -= 1,
            _ => {}
        }
    }
    Some(0..bytes.len())
}

/// Where the item after an outer attribute ends: just past its `;`, or past the `}`
/// that closes its first top-level block. `None` unless the item starts with one of
/// the [`ITEM_KEYWORDS`] after any further attributes and a visibility.
fn item_end(masked: &str, from: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    let skip_whitespace = |at: usize| {
        at + bytes.get(at..).map_or(0, |rest| {
            rest.iter().take_while(|b| b.is_ascii_whitespace()).count()
        })
    };
    let word_at = |at: usize| {
        let rest = masked.get(at..).unwrap_or("");
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        rest.get(..len).unwrap_or("")
    };
    let mut at = skip_whitespace(from);
    while bytes.get(at) == Some(&b'#') && bytes.get(at + 1) == Some(&b'[') {
        at = skip_whitespace(matching(bytes, at + 1, b'[', b']')? + 1);
    }
    if word_at(at) == "pub" {
        at = skip_whitespace(at + 3);
        if bytes.get(at) == Some(&b'(') {
            at = skip_whitespace(matching(bytes, at, b'(', b')')? + 1);
        }
    }
    if !ITEM_KEYWORDS.contains(&word_at(at)) {
        return None;
    }
    let mut depth = 0_usize;
    for (index, &byte) in bytes.iter().enumerate().skip(at) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth = depth.checked_sub(1)?,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            b';' if depth == 0 => return Some(index + 1),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violations(text: &str) -> Vec<String> {
        scan_allows("fixture.rs", text)
    }

    #[test]
    fn allowing_a_cd_lint_is_rejected_even_when_documented() {
        let text = "// decided in br-w01-3-x\n#[allow(clippy::unwrap_used)]\nfn f() {}\n";
        let observed = violations(text);
        assert_eq!(observed.len(), 1, "input={text:?} observed={observed:#?}");
        assert!(
            observed[0].contains("fixture.rs:2") && observed[0].contains("clippy::unwrap_used"),
            "observed={observed:#?}"
        );
    }

    #[test]
    fn every_relaxing_form_is_seen() {
        let cases = [
            "#![allow(missing_docs)]\n",
            "#[expect(clippy::panic)]\nfn f() {}\n",
            "#[cfg_attr(test, allow(clippy::todo))]\nfn f() {}\n",
            "#[allow(\n    clippy::too_many_lines,\n    clippy::wildcard_enum_match_arm\n)]\nfn f() {}\n",
            "#[allow(warnings)]\nfn f() {}\n",
            "#[allow(clippy::all)]\nfn f() {}\n",
            "#[allow(clippy :: disallowed_methods, reason = \"br-x\")]\nfn f() {}\n",
        ];
        for text in cases {
            let observed = violations(text);
            assert!(
                observed.iter().any(|v| v.contains("CD principle")),
                "rule: every allow/expect form of a CD lint is rejected; input={text:?} \
                 observed={observed:#?}"
            );
        }
    }

    #[test]
    fn other_allows_need_a_bead_in_a_nearby_comment_or_reason() {
        let undocumented = "#[allow(clippy::too_many_lines)]\nfn f() {}\n";
        let observed = violations(undocumented);
        assert!(
            observed.len() == 1 && observed[0].contains("decision-log"),
            "input={undocumented:?} observed={observed:#?}"
        );
        let documented = [
            "// too_many_lines fired on the dispatch table; it reads best as one match.\n\
             // Decided in br-w01-3-lint-policy-probes-c71.\n\
             #[allow(clippy::too_many_lines)]\nfn f() {}\n",
            "#[allow(clippy::too_many_lines, reason = \"one table; br-w01-3-x\")]\nfn f() {}\n",
        ];
        for text in documented {
            assert_eq!(violations(text), Vec::<String>::new(), "input={text:?}");
        }
    }

    #[test]
    fn a_comment_too_far_above_does_not_document_an_allow() {
        let text = "// br-w01-3-x\n\n\n\n#[allow(clippy::too_many_lines)]\nfn f() {}\n";
        assert_eq!(violations(text).len(), 1, "input={text:?}");
    }

    #[test]
    fn attribute_text_in_comments_strings_and_chars_is_ignored() {
        let text = concat!(
            "// #[allow(clippy::unwrap_used)]\n",
            "/* #[allow(clippy::panic)] /* nested */ */\n",
            "const A: &str = \"#[allow(clippy::todo)]\";\n",
            "const B: &str = r#\"#[allow(clippy::dbg_macro)] \"quoted\"\"#;\n",
            "const C: char = '\"';\n",
            "const E: char = '\\'';\n",
            "const F: &str = \"#[allow(clippy::panic)]\";\n",
            "const D: &str = \"#[allow(missing_docs)]\";\n",
            "fn lifetime<'a>(x: &'a str) -> &'a str { x }\n",
        );
        assert_eq!(violations(text), Vec::<String>::new(), "input:\n{text}");
    }

    #[test]
    fn non_relaxing_attributes_are_ignored() {
        let text = "#[derive(Debug)]\n#[must_use]\n#[cfg(test)]\n#[deny(clippy::panic)]\n\
                    #[forbid(unsafe_code)]\nfn f() {}\n#[allowed_by_name]\nfn g() {}\n";
        assert_eq!(violations(text), Vec::<String>::new(), "input:\n{text}");
    }

    #[test]
    fn the_workspace_has_no_lint_allow_violations() {
        let root = crate::test_support::workspace_root();
        let outcome = check_allows(&root).unwrap();
        assert!(
            outcome.cases >= 10,
            "rule: the scan covers the first-party sources; files={}",
            outcome.cases
        );
        assert_eq!(outcome.violations, Vec::<String>::new());
    }

    #[test]
    fn the_scan_skips_fixtures_and_probes_but_not_first_party_code() {
        let root = crate::test_support::workspace_root();
        let mut files = Vec::new();
        collect_sources(&root, &root, &mut files).unwrap();
        let shown: Vec<String> = files
            .iter()
            .map(|f| f.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        assert!(
            shown.iter().any(|f| f == "xtask/src/lint.rs"),
            "files={shown:#?}"
        );
        assert!(
            shown.iter().any(|f| f == "crates/types/src/lib.rs"),
            "files={shown:#?}"
        );
        assert!(
            !shown
                .iter()
                .any(|f| f.starts_with("xtask/probes/") || f.contains("tests/fixtures/")),
            "rule: deliberate violations are not policy subjects; files={shown:#?}"
        );
    }

    #[test]
    fn the_workspace_lint_tiers_are_consistent() {
        let root = crate::test_support::workspace_root();
        let outcome = check_tiers(&root).unwrap();
        assert!(outcome.cases >= 4, "members checked={}", outcome.cases);
        assert_eq!(outcome.violations, Vec::<String>::new());
    }

    #[test]
    fn tier_violations_are_detected() {
        let root = crate::test_support::workspace_root();
        let canonical_clippy = fs::read(root.join(CANONICAL_CLIPPY)).unwrap();
        let canonical_block = fs::read_to_string(root.join(CANONICAL_BLOCK)).unwrap();
        let fixtures = crate::test_support::manifest_dir().join("tests/fixtures/lint-tiers");
        let package = |dir: &str| Package {
            name: dir.to_owned(),
            id: dir.to_owned(),
            manifest_path: fixtures.join(dir).join("Cargo.toml"),
            targets: vec![crate::metadata::Target {
                kind: vec!["lib".to_owned()],
                src_path: fixtures.join(dir).join("src/lib.rs"),
            }],
            dependencies: Vec::new(),
        };
        let cases = [
            ("no-workspace-lints", false, "[lints] workspace = true"),
            (
                "own-clippy-config",
                false,
                "must use the workspace clippy.toml",
            ),
            ("det-missing-config", true, "needs its own clippy.toml"),
            ("det-drifted-config", true, "differs from"),
            ("det-missing-block", true, "must start with the exact block"),
        ];
        for (dir, deterministic, expected) in cases {
            let found = tier_violations(
                &package(dir),
                deterministic,
                &fixtures,
                &canonical_clippy,
                &canonical_block,
            );
            assert!(
                found.iter().any(|v| v.contains(expected)),
                "fixture={dir} expected a violation containing {expected:?}; found={found:#?}"
            );
        }
    }

    #[test]
    fn test_code_is_what_clippy_lets_panic() {
        // (input, whether its `panic!("T")` is test code). `panic!("P")` is production
        // code in every case and must survive into the production view.
        let cases = [
            (
                "fn p() { panic!(\"P\") }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    \
                 #[test]\n    fn t() { panic!(\"T\") }\n}\nfn q() { panic!(\"P\") }\n",
                true,
            ),
            (
                "#[test]\nfn t() { panic!(\"T\") }\nfn p() { panic!(\"P\") }\n",
                true,
            ),
            (
                "//! An integration test.\n#![cfg(test)]\nfn h() { panic!(\"T\") }\n",
                true,
            ),
            (
                "mod t {\n    #![cfg(test)]\n    fn h() { panic!(\"T\") }\n}\nfn p() { panic!(\"P\") }\n",
                true,
            ),
            (
                "#[cfg( test )]\n#[allow(dead_code)] // br-x\npub(crate) fn h() -> [u8; 2] { \
                 panic!(\"T\") }\nfn p() { panic!(\"P\") }\n",
                true,
            ),
            (
                "#[cfg(test)]\nimpl<A, B> S<A, B> where A: Fn() -> B {\n    fn h() { panic!(\"T\") }\n}\n\
                 fn p() { panic!(\"P\") }\n",
                true,
            ),
            // Attributes on arms, fields and statements never classify what follows.
            (
                "fn p(x: E) {\n    match x {\n        #[cfg(test)]\n        E::A => panic!(\"T\"),\n        \
                 E::B => panic!(\"P\"),\n    }\n}\n",
                false,
            ),
            (
                "struct S {\n    #[cfg(test)]\n    a: u8,\n}\nfn p() { panic!(\"P\") }\n",
                false,
            ),
            (
                "fn p() {\n    #[cfg(test)]\n    let _x = 1;\n    panic!(\"P\");\n}\n",
                false,
            ),
            // Only test-only configurations count.
            (
                "#[cfg(any(test, feature = \"x\"))]\nfn h() { panic!(\"T\") }\n",
                false,
            ),
            // Attribute text in comments and strings is not an attribute.
            (
                "// #[cfg(test)]\nfn p() { panic!(\"P\") }\nconst S: &str = \"#[test] fn\";\n\
                 fn q() { panic!(\"P\") }\n",
                false,
            ),
            // An out-of-line module's file is not classified from here.
            (
                "#[cfg(test)]\nmod tests;\nfn p() { panic!(\"P\") }\n",
                false,
            ),
        ];
        for (text, test_only) in cases {
            let view = production_view(text);
            assert_eq!(
                view.contains("panic!(\"T\")"),
                text.contains("panic!(\"T\")") && !test_only,
                "rule: test code is #[test]/#[cfg(test)] items and #![cfg(test)] scopes; \
                 input:\n{text}\nview:\n{view}"
            );
            assert_eq!(
                (view.len(), view.lines().count()),
                (text.len(), text.lines().count()),
                "rule: the view keeps every byte offset and line; input:\n{text}"
            );
            for (line, (original, viewed)) in text.lines().zip(view.lines()).enumerate() {
                if original.contains("panic!(\"P\")") {
                    assert_eq!(
                        viewed,
                        original,
                        "rule: production code is never blanked; line {}; input:\n{text}",
                        line + 1
                    );
                }
            }
        }
    }

    #[test]
    fn the_production_view_keeps_utf8_and_unterminated_items_intact() {
        let text =
            "#[cfg(test)]\nmod t { const S: &str = \"é→\"; }\nfn p() -> &'static str { \"ü\" }\n";
        let view = production_view(text);
        assert!(
            view.ends_with("fn p() -> &'static str { \"ü\" }\n") && !view.contains('é'),
            "input:\n{text}\nview:\n{view}"
        );
        let unterminated = "#[cfg(test)]\nmod t {\n    fn h() { panic!(\"T\") }\n";
        assert_eq!(
            production_view(unterminated),
            unterminated,
            "rule: an item whose end cannot be found stays production code"
        );
    }

    #[test]
    fn lint_list_parsing_drops_reasons_and_whitespace() {
        assert_eq!(
            split_lints(" clippy :: a ,\n clippy::b, reason = \"  \" "),
            ["clippy::a", "clippy::b"]
        );
    }
}

#[cfg(test)]
mod probes {
    //! Lint probes: compile each deliberate violation in `xtask/probes/*` with
    //! `--cfg keyval_probe="<name>"` and assert that clippy rejects it with the expected
    //! lint, in the expected file. Clippy's JSON diagnostics are read, not its text (§11.5
    //! rule 14). Re-run one probe with `./dev check LINT-PROBES --case <name>`.

    // check: LINT-PROBES
    use std::collections::BTreeSet;
    use std::time::Duration;

    use serde_json::Value;

    use std::path::PathBuf;

    use crate::process::{self, Captured};
    use crate::steplog::{Outcome, Step, StepFailed, StepLog, StepRecord};
    use crate::test_support::{nested_cargo, scratch_dir, workspace_root};

    #[derive(Clone, Copy, Debug)]
    enum Tier {
        Production,
        Deterministic,
    }

    impl Tier {
        const fn package(self) -> &'static str {
            match self {
                Self::Production => "lint-probe-production",
                Self::Deterministic => "lint-probe-deterministic",
            }
        }
    }

    #[derive(Debug)]
    struct Diagnostic {
        level: String,
        code: Option<String>,
        file: Option<String>,
        message: String,
    }

    struct ClippyRun {
        succeeded: bool,
        diagnostics: Vec<Diagnostic>,
        captured: Captured,
    }

    fn clippy(tier: Tier, probe: Option<&str>) -> ClippyRun {
        let cfg = probe.map(|name| format!("keyval_probe=\"{name}\""));
        let mut args = vec![
            "clippy",
            "--locked",
            "-p",
            tier.package(),
            "--all-targets",
            "--message-format=json",
            "--",
            "-D",
            "warnings",
        ];
        if let Some(cfg) = &cfg {
            args.extend(["--cfg", cfg.as_str()]);
        }
        let cargo = nested_cargo("keyval-probes");
        let captured = process::run(
            &mut cargo.command(&workspace_root(), &args),
            Duration::from_mins(10),
        )
        .unwrap_or_else(|err| panic!("cannot run {}: {err}", cargo.describe(&args)));
        let diagnostics = captured
            .stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["reason"] == "compiler-message")
            .map(|value| {
                let message = &value["message"];
                let primary = message["spans"]
                    .as_array()
                    .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true));
                Diagnostic {
                    level: message["level"].as_str().unwrap_or_default().to_owned(),
                    code: message["code"]["code"].as_str().map(str::to_owned),
                    file: primary
                        .and_then(|s| s["file_name"].as_str())
                        .map(str::to_owned),
                    message: message["message"].as_str().unwrap_or_default().to_owned(),
                }
            })
            .collect();
        ClippyRun {
            succeeded: captured.status.success(),
            diagnostics,
            captured,
        }
    }

    fn errors(run: &ClippyRun) -> Vec<&Diagnostic> {
        run.diagnostics
            .iter()
            .filter(|d| d.level == "error")
            .collect()
    }

    /// Runs one probe as a logged step (W01.6 standard): the record names the clippy
    /// command, the expected rejection, what clippy actually reported, and the exact
    /// command that re-runs the probe. It is written to `<target>/keyval-scratch/…`.
    fn probe_step(
        tier: Tier,
        probe: &str,
        file: &str,
        expected: &[&str],
    ) -> (Result<StepRecord, StepFailed>, PathBuf) {
        let log_path = scratch_dir("lint-probes").join(format!("{probe}.jsonl"));
        let mut log = StepLog::new("lint probes", Some(&log_path), false).unwrap();
        let action = format!(
            "cargo clippy -p {} --all-targets -- -D warnings --cfg keyval_probe=\"{probe}\"",
            tier.package()
        );
        let expectation = format!("rejected by {expected:?} in {file}");
        let rerun = format!("./dev check LINT-PROBES --case {probe}");
        let step = Step {
            name: probe,
            action: &action,
            expected: &expectation,
            rerun: &rerun,
        };
        let started = std::time::Instant::now();
        let observed = (|| {
            let run = clippy(tier, Some(probe));
            let errors = errors(&run);
            let reported: Vec<String> = errors
                .iter()
                .map(|d| {
                    format!(
                        "{} in {}",
                        d.code.as_deref().unwrap_or("?"),
                        d.file.as_deref().unwrap_or("?")
                    )
                })
                .collect();
            if run.succeeded {
                return Err(format!(
                    "clippy passed with no errors; stderr tail: {}",
                    run.captured
                        .stderr
                        .lines()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            let all_found = expected.iter().all(|code| {
                errors.iter().any(|d| {
                    d.code.as_deref() == Some(*code)
                        && d.file.as_deref().is_some_and(|f| f.ends_with(file))
                })
            });
            let observed = format!("rejected with {reported:?}");
            if all_found {
                Ok(observed)
            } else {
                Err(observed)
            }
        })();
        (log.record(step, observed, started.elapsed()), log_path)
    }

    fn assert_rejected(tier: Tier, probe: &str, file: &str, expected: &[&str]) {
        let (outcome, log_path) = probe_step(tier, probe, file, expected);
        if let Err(failure) = outcome {
            panic!(
                "rule: every lint probe is rejected by its lint (§5.2: the probe is the \
                 proof); {failure}; step log: {}",
                log_path.display()
            );
        }
    }

    /// The W01.6 demonstration: a probe whose expectation is wrong fails with a record
    /// that names the command, the expectation, what clippy really reported, and the
    /// command that re-runs it.
    #[test]
    fn a_failing_probe_yields_a_rerunnable_record() {
        let (outcome, log_path) = probe_step(
            Tier::Production,
            "unwrap_used",
            "src/unwrap_used.rs",
            &["clippy::panic"],
        );
        let failure = outcome.expect_err("the wrong expectation must fail the step");
        let record = &failure.record;
        assert_eq!(record.outcome, Outcome::Failed, "record={record:#?}");
        assert!(
            record.action.contains("--cfg keyval_probe=\"unwrap_used\""),
            "rule: the record names the exact action; record={record:#?}"
        );
        assert!(
            record.expected.contains("clippy::panic")
                && record.observed.contains("clippy::unwrap_used"),
            "rule: expected and observed are both recorded; record={record:#?}"
        );
        assert_eq!(
            record.rerun.as_deref(),
            Some("./dev check LINT-PROBES --case unwrap_used"),
            "rule: a failure is re-runnable from its log"
        );
        let logged = std::fs::read_to_string(&log_path).unwrap();
        let line: Value = serde_json::from_str(logged.lines().next().unwrap()).unwrap();
        assert_eq!(line["schema"], "keyval.dev.step/v1", "log={logged}");
        assert_eq!(line["outcome"], "failed", "log={logged}");
    }

    #[test]
    fn unsafe_code() {
        assert_rejected(
            Tier::Production,
            "unsafe_code",
            "src/unsafe_code.rs",
            &["unsafe_code"],
        );
    }

    #[test]
    fn unwrap_used() {
        assert_rejected(
            Tier::Production,
            "unwrap_used",
            "src/unwrap_used.rs",
            &["clippy::unwrap_used"],
        );
    }

    #[test]
    fn expect_used() {
        assert_rejected(
            Tier::Production,
            "expect_used",
            "src/expect_used.rs",
            &["clippy::expect_used"],
        );
    }

    #[test]
    fn panic() {
        assert_rejected(
            Tier::Production,
            "panic",
            "src/panic.rs",
            &["clippy::panic"],
        );
    }

    #[test]
    fn todo() {
        assert_rejected(Tier::Production, "todo", "src/todo.rs", &["clippy::todo"]);
    }

    #[test]
    fn unimplemented() {
        assert_rejected(
            Tier::Production,
            "unimplemented",
            "src/unimplemented.rs",
            &["clippy::unimplemented"],
        );
    }

    #[test]
    fn unreachable() {
        assert_rejected(
            Tier::Production,
            "unreachable",
            "src/unreachable.rs",
            &["clippy::unreachable"],
        );
    }

    #[test]
    fn dbg_macro() {
        assert_rejected(
            Tier::Production,
            "dbg_macro",
            "src/dbg_macro.rs",
            &["clippy::dbg_macro"],
        );
    }

    #[test]
    fn group_all_is_denied() {
        assert_rejected(
            Tier::Production,
            "group_all",
            "src/group_all.rs",
            &["clippy::needless_return"],
        );
    }

    #[test]
    fn group_pedantic_is_denied() {
        assert_rejected(
            Tier::Production,
            "group_pedantic",
            "src/group_pedantic.rs",
            &["clippy::cast_possible_truncation"],
        );
    }

    #[test]
    fn group_nursery_is_denied() {
        assert_rejected(
            Tier::Production,
            "group_nursery",
            "src/group_nursery.rs",
            &["clippy::missing_const_for_fn"],
        );
    }

    #[test]
    fn test_helper_without_cfg_test() {
        assert_rejected(
            Tier::Production,
            "test_helper_without_cfg_test",
            "tests/helper_without_cfg_test.rs",
            &["clippy::panic"],
        );
    }

    #[test]
    fn wildcard_arm() {
        assert_rejected(
            Tier::Deterministic,
            "wildcard_arm",
            "src/wildcard_arm.rs",
            &["clippy::wildcard_enum_match_arm"],
        );
    }

    #[test]
    fn binding_catch_all_arm() {
        assert_rejected(
            Tier::Deterministic,
            "binding_catch_all_arm",
            "src/binding_catch_all_arm.rs",
            &["clippy::wildcard_enum_match_arm"],
        );
    }

    #[test]
    fn missing_docs() {
        assert_rejected(
            Tier::Deterministic,
            "missing_docs",
            "src/missing_docs.rs",
            &["missing_docs"],
        );
    }

    #[test]
    fn allow_under_forbid() {
        assert_rejected(
            Tier::Deterministic,
            "allow_under_forbid",
            "src/allow_under_forbid.rs",
            &["E0453"],
        );
    }

    /// Every entry of the canonical determinism configuration, read from the file
    /// itself, must be reported by the `disallowed` probe.
    #[test]
    fn disallowed_every_determinism_entry_fires() {
        let config_text =
            std::fs::read_to_string(workspace_root().join(super::CANONICAL_CLIPPY)).unwrap();
        let config: toml::Table = toml::from_str(&config_text).unwrap();
        let mut configured = BTreeSet::new();
        for (key, lint) in [
            ("disallowed-methods", "clippy::disallowed_methods"),
            ("disallowed-types", "clippy::disallowed_types"),
            ("disallowed-macros", "clippy::disallowed_macros"),
        ] {
            let entries = config[key].as_array().unwrap();
            assert!(!entries.is_empty(), "{key} is empty");
            for entry in entries {
                let path = entry
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .or_else(|| entry.as_str())
                    .unwrap();
                assert!(
                    entry.get("reason").is_some(),
                    "rule: each entry's reason names the port to use (AGENTS.md); entry={entry:?}"
                );
                configured.insert((lint.to_owned(), path.to_owned()));
            }
        }
        let run = clippy(Tier::Deterministic, Some("disallowed"));
        let reported: BTreeSet<(String, String)> = errors(&run)
            .into_iter()
            .filter(|d| {
                d.file
                    .as_deref()
                    .is_some_and(|f| f.ends_with("src/disallowed.rs"))
            })
            .filter_map(|d| {
                let path = d.message.split('`').nth(1)?;
                Some((d.code.clone()?, path.to_owned()))
            })
            .collect();
        let missing: Vec<_> = configured.difference(&reported).collect();
        assert!(
            missing.is_empty(),
            "rule: every determinism entry must fire (§7.14); missing={missing:#?}; \
             configured={}; reported={}; rerun=`./dev check LINT-PROBES --case disallowed`",
            configured.len(),
            reported.len()
        );
    }

    #[test]
    fn controls_pass_the_full_policy() {
        for tier in [Tier::Production, Tier::Deterministic] {
            let run = clippy(tier, None);
            let errors = errors(&run);
            assert!(
                run.succeeded && errors.is_empty(),
                "rule: the controls (test-only unwrap/expect/panic, invariant!, exhaustive \
                 matches, ordered maps) pass the full policy; crate={} errors={errors:#?}\n\
                 stderr tail:\n{}",
                tier.package(),
                run.captured
                    .stderr
                    .lines()
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    mod compile_fail {
        //! Compile-fail fixtures for the `types` crate (W04.4, CD-2, CD-3, CD-4): each one
        //! forges a value that its invariants forbid, from outside the crate, and must be
        //! refused by rustc with the expected error. The control build (`controls_pass_the_full_policy`)
        //! proves the probe crate compiles without them, so a pass is never vacuous.

        // check: COMPILE-FAIL
        use super::{Tier, assert_rejected};

        /// Asserts that fixture `name` (in `src/<name>.rs`) fails with `code`.
        fn refused(name: &str, code: &str) {
            assert_rejected(
                Tier::Deterministic,
                name,
                &format!("src/{name}.rs"),
                &[code],
            );
        }

        #[test]
        fn forge_key() {
            refused("forge_key", "E0423");
        }

        #[test]
        fn forge_value() {
            refused("forge_value", "E0423");
        }

        #[test]
        fn forge_node_id() {
            refused("forge_node_id", "E0423");
        }

        #[test]
        fn forge_version() {
            refused("forge_version", "E0423");
        }

        #[test]
        fn forge_page_size() {
            refused("forge_page_size", "E0423");
        }

        #[test]
        fn forge_terminal_result() {
            refused("forge_terminal_result", "E0451");
        }

        #[test]
        fn forge_scan_range() {
            refused("forge_scan_range", "E0451");
        }

        #[test]
        fn version_vs_index() {
            refused("version_vs_index", "E0308");
        }

        #[test]
        fn submitted_with_result() {
            refused("submitted_with_result", "E0618");
        }

        #[test]
        fn ready_and_failed() {
            refused("ready_and_failed", "E0559");
        }
    }
}
