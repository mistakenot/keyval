//! `./dev check RAFT-AUDIT`: a static scan of the vendored Raft library's sources
//! (`vendor/raft/*/src`) that keeps the W02.2 audit's "none on any path" verdicts true
//! (spike/raft-audit.md, "Executable follow-up"; P4, T-3, PLAN.md §7.3).
//!
//! It fails on:
//! - entropy (`thread_rng`, a `rand::` path, `extern crate rand`), when the number of
//!   source lines naming it differs from [`EXPECTED_ENTROPY_SITES`] (one before W02.3's
//!   patch, none after);
//! - clocks (`Instant`, `SystemTime`) and threads (`thread::spawn`, `thread::scope`,
//!   `thread::Builder`);
//! - randomly seeded std hash collections (audit H-1, H-2): `RandomState`, and std's
//!   `HashMap`/`HashSet` with the default hasher. Two uses are allowed. One is the `FxHash`
//!   aliases in `raft/src/lib.rs`, which pass the fixed `DefaultHashBuilder` (itself pinned
//!   to `BuildHasherDefault<fxhash::FxHasher>`), so iteration order is deterministic per
//!   build. The other is `quorum.rs`'s `AckIndexer`, the one std map the audit accepts;
//! - pointer formatting (`{:p}`) in a string literal (audit P-1).
//!
//! Comments (doc comments included), items under `#[cfg(test)]` and `*_test.rs` files are
//! not scanned. Each violation names its file and line.

use std::fs;
use std::path::{Path, PathBuf};

use crate::check::StaticOutcome;

/// Where the vendored crates live, relative to the workspace root.
const VENDOR: &str = "vendor/raft";

/// Source lines that may name entropy. W02.3 (56c587f) replaced the only one,
/// `rand::thread_rng()` in `reset_randomized_election_timeout`, with the mandatory
/// `Entropy` source.
pub const EXPECTED_ENTROPY_SITES: usize = 0;

/// Where the entropy site was before W02.3, relative to [`VENDOR`]; a missing expected
/// site is reported against it.
const ENTROPY_FILE: &str = "raft/src/raft.rs";

/// The file, relative to [`VENDOR`], whose `AckIndexer` is the one std map allowed.
const ACK_INDEXER_FILE: &str = "raft/src/quorum.rs";

/// The only hasher `DefaultHashBuilder` may name: `FxHash`, fixed and unseeded.
const FX_HASH_BUILDER: &str = "std::hash::BuildHasherDefault<fxhash::FxHasher>";

/// Rules each run asserts, counted as cases besides the files: entropy, clocks, threads,
/// std hash collections and pointer formatting.
const RULES: usize = 5;

// check: RAFT-AUDIT
/// Runs RAFT-AUDIT on the workspace at `root`.
pub fn check_raft_audit(root: &Path) -> Result<StaticOutcome, String> {
    let vendor = root.join(VENDOR);
    let sources = vendored_sources(&vendor)?;
    if sources.is_empty() {
        return Err(format!(
            "no Rust sources under {VENDOR}/*/src; an empty scan is not a pass (P10)"
        ));
    }
    Ok(StaticOutcome {
        cases: sources.len() + RULES,
        violations: audit(&sources, EXPECTED_ENTROPY_SITES),
    })
}

/// One vendored source file: its path relative to [`VENDOR`], and its text.
struct Source {
    path: String,
    text: String,
}

/// Every `.rs` file under `vendor/<crate>/src`, except `*_test.rs`, in path order.
fn vendored_sources(vendor: &Path) -> Result<Vec<Source>, String> {
    let mut files = Vec::new();
    for entry in read_dir(vendor)? {
        let src = entry.join("src");
        if src.is_dir() {
            collect_rust_files(&src, &mut files)?;
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            let relative = path.strip_prefix(vendor).unwrap_or(&path);
            Ok(Source {
                path: relative.display().to_string(),
                text,
            })
        })
        .collect()
}

fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    fs::read_dir(dir)
        .and_then(|entries| entries.map(|entry| entry.map(|e| e.path())).collect())
        .map_err(|err| format!("cannot list {}: {err}", dir.display()))
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for path in read_dir(dir)? {
        let rust = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rs"));
        let test_file = path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().ends_with("_test"));
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if rust && !test_file {
            files.push(path);
        }
    }
    Ok(())
}

/// One finding: where it is and what is wrong.
struct Finding {
    path: String,
    line: usize,
    message: String,
}

/// Judges `sources`, expecting `expected_entropy` entropy sites, and returns one message
/// per violation, each naming its file and line.
fn audit(sources: &[Source], expected_entropy: usize) -> Vec<String> {
    let mut findings = Vec::new();
    let mut entropy = Vec::new();
    let mut fx_aliases = 0;
    let mut builders = Vec::new();
    for source in sources {
        let scrubbed = scrub(&source.text);
        let tokens = without_test_items(&tokenize(&scrubbed.code));
        let file = File {
            path: &source.path,
            tokens: &tokens,
        };
        entropy.extend(file.entropy_sites());
        findings.extend(file.clocks_and_threads());
        let hashes = file.std_hash_collections();
        fx_aliases += hashes.fx_aliases;
        findings.extend(hashes.findings);
        builders.extend(file.hash_builder_definitions());
        findings.extend(pointer_formats(&source.path, &scrubbed.strings));
    }
    findings.extend(entropy_verdict(entropy, expected_entropy));
    findings.extend(builder_verdict(&builders, fx_aliases));
    findings
        .into_iter()
        .map(|f| format!("{VENDOR}/{}:{}: {}", f.path, f.line, f.message))
        .collect()
}

fn entropy_verdict(mut sites: Vec<(String, usize)>, expected: usize) -> Vec<Finding> {
    sites.sort();
    sites.dedup();
    if sites.len() == expected {
        return Vec::new();
    }
    let found = sites.len();
    let message = format!(
        "entropy: {found} source line(s) name thread_rng or rand:: where {expected} expected \
         (W02.2 E-1; W02.3 routes randomness through the Entropy trait)"
    );
    if sites.is_empty() {
        return vec![Finding {
            path: ENTROPY_FILE.to_owned(),
            line: 0,
            message,
        }];
    }
    sites
        .into_iter()
        .map(|(path, line)| Finding {
            path,
            line,
            message: message.clone(),
        })
        .collect()
}

/// The `FxHash` aliases rely on `DefaultHashBuilder` being `FxHash`: every definition must
/// name exactly [`FX_HASH_BUILDER`], and one must exist if an alias uses it.
fn builder_verdict(builders: &[(String, usize, String)], fx_aliases: usize) -> Vec<Finding> {
    let mut findings: Vec<Finding> = builders
        .iter()
        .filter(|(_, _, definition)| definition != FX_HASH_BUILDER)
        .map(|(path, line, definition)| Finding {
            path: path.clone(),
            line: *line,
            message: format!(
                "hash: DefaultHashBuilder is {definition}; the std collections that use it are \
                 deterministic only with {FX_HASH_BUILDER} (W02.2 H-1)"
            ),
        })
        .collect();
    if fx_aliases > 0 && builders.is_empty() {
        findings.push(Finding {
            path: "raft/src/lib.rs".to_owned(),
            line: 0,
            message: format!(
                "hash: std collections name DefaultHashBuilder, but no `type DefaultHashBuilder \
                 = {FX_HASH_BUILDER};` is defined (W02.2 H-1)"
            ),
        });
    }
    findings
}

/// Pointer formatting (`{:p}`, `{x:#p}`) in string literals; `{{` is a literal brace.
fn pointer_formats(path: &str, strings: &[(usize, String)]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line, content) in strings {
        let mut rest = content.as_str();
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            if let Some(escaped) = after.strip_prefix('{') {
                rest = escaped;
                continue;
            }
            let Some(close) = after.find('}') else {
                break;
            };
            let spec = &after[..close];
            if spec
                .rsplit_once(':')
                .is_some_and(|(_, format)| format.ends_with('p'))
            {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: *line,
                    message: format!(
                        "pointer formatting: \"{{{spec}}}\" prints an address, which differs \
                         between runs (W02.2 P-1)"
                    ),
                });
            }
            rest = &after[close + 1..];
        }
    }
    findings
}

// --- Lexing: comments out, literal contents blanked, line breaks kept. ---

/// A file's code with comments removed and literal contents blanked, keeping every line
/// break so that lines still count; and each string literal's contents with its line.
struct Scrubbed {
    code: String,
    strings: Vec<(usize, String)>,
}

fn scrub(text: &str) -> Scrubbed {
    let mut scrubber = Scrubber {
        chars: text.chars().collect(),
        at: 0,
        line: 1,
        code: String::with_capacity(text.len()),
        strings: Vec::new(),
    };
    scrubber.run();
    Scrubbed {
        code: scrubber.code,
        strings: scrubber.strings,
    }
}

struct Scrubber {
    chars: Vec<char>,
    at: usize,
    line: usize,
    code: String,
    strings: Vec<(usize, String)>,
}

impl Scrubber {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    /// Keeps a character in the code.
    fn keep(&mut self, c: char) {
        if c == '\n' {
            self.line += 1;
        }
        self.code.push(c);
    }

    /// Drops a character from the code, keeping its line break.
    fn blank(&mut self, c: char) {
        self.keep(if c == '\n' { '\n' } else { ' ' });
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match (c, self.peek(1)) {
                ('/', Some('/')) => self.line_comment(),
                ('/', Some('*')) => self.block_comment(),
                ('"', _) => self.string_body(false, 0),
                ('\'', _) => self.char_or_lifetime(),
                _ if c.is_alphabetic() || c == '_' => self.word(),
                _ => {
                    self.keep(c);
                    self.at += 1;
                }
            }
        }
    }

    fn line_comment(&mut self) {
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.at += 1;
        }
    }

    /// `/* … */`, nested.
    fn block_comment(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek(0) {
            match (c, self.peek(1)) {
                ('/', Some('*')) => {
                    depth += 1;
                    self.at += 2;
                    continue;
                }
                ('*', Some('/')) => {
                    depth = depth.saturating_sub(1);
                    self.at += 2;
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
                _ => self.blank(c),
            }
            self.at += 1;
        }
    }

    /// A string literal whose opening quote is at the cursor, closed by `"` and then
    /// `hashes` `#`s. Backslash escapes count only when it is not `raw`.
    fn string_body(&mut self, raw: bool, hashes: usize) {
        let line = self.line;
        let mut content = String::new();
        self.at += 1;
        while let Some(c) = self.peek(0) {
            if c == '\\' && !raw {
                content.push(c);
                self.blank(c);
                if let Some(escaped) = self.peek(1) {
                    content.push(escaped);
                    self.blank(escaped);
                }
                self.at += 2;
                continue;
            }
            if c == '"' && (0..hashes).all(|k| self.peek(1 + k) == Some('#')) {
                self.at += 1 + hashes;
                break;
            }
            content.push(c);
            self.blank(c);
            self.at += 1;
        }
        self.strings.push((line, content));
    }

    /// A char literal (`'a'`, `'\n'`, `'\u{1F600}'`), or a lifetime or label (`'a`).
    fn char_or_lifetime(&mut self) {
        let literal = matches!(
            (self.peek(1), self.peek(2)),
            (Some('\\'), _) | (Some(_), Some('\''))
        );
        self.at += 1;
        if !literal {
            self.keep('\'');
            return;
        }
        while let Some(c) = self.peek(0) {
            self.at += 1;
            if c == '\\' {
                self.at += 1;
            } else if c == '\'' {
                return;
            }
        }
    }

    /// An identifier, or a raw or byte string or byte char that starts like one.
    fn word(&mut self) {
        let start = self.at;
        while self
            .peek(0)
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            self.at += 1;
        }
        let word: String = self.chars[start..self.at].iter().collect();
        match (word.as_str(), self.peek(0)) {
            ("r" | "br", Some('"' | '#')) => {
                let mut hashes = 0;
                while self.peek(hashes) == Some('#') {
                    hashes += 1;
                }
                if self.peek(hashes) == Some('"') {
                    self.at += hashes;
                    self.string_body(true, hashes);
                } else {
                    // A raw identifier, `r#type`: keep it as code.
                    for c in word.chars() {
                        self.keep(c);
                    }
                }
            }
            ("b", Some('"')) => self.string_body(false, 0),
            ("b", Some('\'')) => self.char_or_lifetime(),
            _ => {
                for c in word.chars() {
                    self.keep(c);
                }
            }
        }
    }
}

// --- Tokens. ---

/// An identifier, `::`, or one punctuation character, with its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Token<'a> {
    text: &'a str,
    line: usize,
}

fn tokenize(code: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut chars = code.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        if c == '\n' {
            line += 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            while let Some(&(at, next)) = chars.peek() {
                if !(next.is_alphanumeric() || next == '_') {
                    break;
                }
                end = at + next.len_utf8();
                chars.next();
            }
            // Number literals (`1u64`) are not identifiers.
            if !c.is_ascii_digit() {
                tokens.push(Token {
                    text: &code[start..end],
                    line,
                });
            }
            continue;
        }
        if c == ':' && chars.peek().is_some_and(|&(_, next)| next == ':') {
            chars.next();
            end += 1;
        } else if !"#![](){};<>,=*&".contains(c) {
            continue;
        }
        tokens.push(Token {
            text: &code[start..end],
            line,
        });
    }
    tokens
}

fn starts_with(tokens: &[Token<'_>], at: usize, words: &[&str]) -> bool {
    tokens
        .get(at..at + words.len())
        .is_some_and(|found| found.iter().zip(words).all(|(t, w)| t.text == *w))
}

const CFG_TEST: &[&str] = &["#", "[", "cfg", "(", "test", ")", "]"];

/// The tokens outside `#[cfg(test)]` items; none if the file is `#![cfg(test)]`.
fn without_test_items<'a>(tokens: &[Token<'a>]) -> Vec<Token<'a>> {
    if starts_with(tokens, 0, &["#", "!", "[", "cfg", "(", "test", ")", "]"]) {
        return Vec::new();
    }
    let mut kept = Vec::with_capacity(tokens.len());
    let mut at = 0;
    while let Some(&token) = tokens.get(at) {
        if starts_with(tokens, at, CFG_TEST) {
            at = end_of_item(tokens, at + CFG_TEST.len());
        } else {
            kept.push(token);
            at += 1;
        }
    }
    kept
}

/// Just past the item at `at`: its other attributes, then up to its `;` outside brackets,
/// or through its brace-delimited body (and a `;` straight after).
fn end_of_item(tokens: &[Token<'_>], mut at: usize) -> usize {
    while starts_with(tokens, at, &["#", "["]) {
        at = past_group(tokens, at + 1);
    }
    let mut depth = 0usize;
    while let Some(token) = tokens.get(at) {
        match token.text {
            "(" | "[" => depth += 1,
            ")" | "]" => depth = depth.saturating_sub(1),
            ";" if depth == 0 => return at + 1,
            "{" if depth == 0 => {
                let end = past_group(tokens, at);
                let semicolon = tokens.get(end).is_some_and(|t| t.text == ";");
                return end + usize::from(semicolon);
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// Just past the bracketed group that opens at `at`.
fn past_group(tokens: &[Token<'_>], at: usize) -> usize {
    let mut depth = 0usize;
    let mut end = at;
    while let Some(token) = tokens.get(end) {
        end += 1;
        match token.text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return end;
                }
            }
            _ => {}
        }
    }
    end
}

// --- The rules, over one file's tokens outside test items. ---

struct File<'a, 'b> {
    path: &'a str,
    tokens: &'a [Token<'b>],
}

/// Std hash collection findings in one file, and how many `FxHash` aliases it defines.
struct Hashes {
    findings: Vec<Finding>,
    fx_aliases: usize,
}

impl File<'_, '_> {
    fn text(&self, at: usize) -> &str {
        self.tokens.get(at).map_or("", |t| t.text)
    }

    fn finding(&self, at: usize, message: String) -> Finding {
        Finding {
            path: self.path.to_owned(),
            line: self.tokens.get(at).map_or(0, |t| t.line),
            message,
        }
    }

    /// Lines that name `thread_rng`, a `rand::` path or `extern crate rand`.
    fn entropy_sites(&self) -> Vec<(String, usize)> {
        (0..self.tokens.len())
            .filter(|&at| match self.text(at) {
                "thread_rng" => true,
                "rand" => self.text(at + 1) == "::" || (at > 0 && self.text(at - 1) == "crate"),
                _ => false,
            })
            .filter_map(|at| self.tokens.get(at))
            .map(|t| (self.path.to_owned(), t.line))
            .collect()
    }

    fn clocks_and_threads(&self) -> Vec<Finding> {
        let mut findings = Vec::new();
        for at in 0..self.tokens.len() {
            let word = self.text(at);
            if word == "Instant" || word == "SystemTime" {
                findings.push(self.finding(
                    at,
                    format!("clock: `{word}` reads wall-clock time (W02.2 C-1)"),
                ));
            }
            if word == "thread"
                && self.text(at + 1) == "::"
                && matches!(self.text(at + 2), "spawn" | "scope" | "Builder")
            {
                findings.push(self.finding(
                    at,
                    format!(
                        "thread: `thread::{}` starts a thread (W02.2 T-1)",
                        self.text(at + 2)
                    ),
                ));
            }
        }
        findings
    }

    /// The token ranges of `use` declarations and of `type <name> = …;` items.
    fn statement(&self, keyword: &str, name: Option<&str>) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        for at in 0..self.tokens.len() {
            let named = name.is_none_or(|name| self.text(at + 1) == name);
            if self.text(at) == keyword && named {
                let end = (at..self.tokens.len())
                    .find(|&k| self.text(k) == ";")
                    .unwrap_or(self.tokens.len());
                spans.push((at, end));
            }
        }
        spans
    }

    /// Whether the std collection named at `at` has generic arguments whose last one is
    /// exactly `DefaultHashBuilder`.
    fn names_fx_hasher(&self, at: usize) -> bool {
        if self.text(at + 1) != "<" {
            return false;
        }
        let mut depth = 0usize;
        let mut last_argument = Vec::new();
        for k in at + 1..self.tokens.len() {
            match self.text(k) {
                "<" => {
                    depth += 1;
                    if depth == 1 {
                        continue;
                    }
                }
                ">" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return last_argument == ["DefaultHashBuilder"];
                    }
                }
                "," if depth == 1 => {
                    last_argument.clear();
                    continue;
                }
                _ => {}
            }
            last_argument.push(self.text(k));
        }
        false
    }

    fn std_hash_collections(&self) -> Hashes {
        let in_span =
            |spans: &[(usize, usize)], at: usize| spans.iter().any(|&(s, e)| s <= at && at <= e);
        let uses = self.statement("use", None);
        let ack_indexer = if self.path == ACK_INDEXER_FILE {
            self.statement("type", Some("AckIndexer"))
        } else {
            Vec::new()
        };
        let mut hashes = Hashes {
            findings: Vec::new(),
            fx_aliases: 0,
        };
        let mut bound = Vec::new();
        for &(start, end) in &uses {
            let from_std = (start..end).any(|k| self.text(k) == "collections");
            for k in start..end {
                let word = self.text(k);
                if !from_std || !matches!(word, "HashMap" | "HashSet") {
                    continue;
                }
                let alias = if self.text(k + 1) == "as" {
                    self.text(k + 2)
                } else {
                    word
                };
                bound.push(alias.to_owned());
                if self.path != ACK_INDEXER_FILE {
                    hashes.findings.push(self.finding(
                        k,
                        format!(
                            "hash: imports std's `{word}`, whose default hasher is randomly \
                             seeded (W02.2 H-2)"
                        ),
                    ));
                }
            }
        }
        for at in 0..self.tokens.len() {
            let word = self.text(at);
            if word == "RandomState" {
                hashes.findings.push(self.finding(
                    at,
                    "hash: `RandomState` is a randomly seeded hasher (W02.2 H-2)".to_owned(),
                ));
                continue;
            }
            let qualified = matches!(word, "HashMap" | "HashSet")
                && at > 1
                && self.text(at - 1) == "::"
                && matches!(self.text(at - 2), "collections" | "hash_map" | "hash_set");
            if in_span(&uses, at) || in_span(&ack_indexer, at) {
                continue;
            }
            if qualified && self.names_fx_hasher(at) {
                hashes.fx_aliases += 1;
            } else if qualified || bound.iter().any(|name| name == word) {
                hashes.findings.push(self.finding(
                    at,
                    format!(
                        "hash: std's `{word}` with its default, randomly seeded hasher; only \
                         quorum.rs's AckIndexer may use it (W02.2 H-2)"
                    ),
                ));
            }
        }
        hashes
    }

    /// Each `type DefaultHashBuilder = …;` with its line and its definition's text.
    fn hash_builder_definitions(&self) -> Vec<(String, usize, String)> {
        self.statement("type", Some("DefaultHashBuilder"))
            .into_iter()
            .map(|(start, end)| {
                let definition: String = (start + 3..end).map(|k| self.text(k)).collect();
                let line = self.tokens.get(start).map_or(0, |t| t.line);
                (self.path.to_owned(), line, definition)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{scratch_dir, workspace_root};

    fn source(path: &str, text: &str) -> Source {
        Source {
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    /// The violations RAFT-AUDIT reports for one file `raft/src/fixture.rs`.
    fn violations(text: &str) -> Vec<String> {
        audit(&[source("raft/src/fixture.rs", text)], 0)
    }

    #[test]
    fn the_vendored_raft_library_passes() {
        let outcome = check_raft_audit(&workspace_root()).unwrap();
        assert!(
            outcome.cases > RULES,
            "rule: RAFT-AUDIT scans the vendored sources, nonzero files (P10); cases={}",
            outcome.cases
        );
        assert!(
            outcome.violations.is_empty(),
            "rule: the W02.2 audit's verdicts hold for vendor/raft at this tree; \
             violations={:#?}",
            outcome.violations
        );
    }

    /// One fixture per forbidden pattern: (what, source, line, expected text).
    const FORBIDDEN: &[(&str, &str, usize, &str)] = &[
        (
            "thread_rng",
            "fn timeout() -> usize {\n    thread_rng().gen_range(10..20)\n}\n",
            2,
            "entropy",
        ),
        ("a rand:: path", "use rand::Rng;\n", 1, "entropy"),
        ("extern crate rand", "extern crate rand;\n", 1, "entropy"),
        (
            "Instant",
            "\nuse std::time::Instant;\n",
            2,
            "clock: `Instant`",
        ),
        (
            "SystemTime",
            "fn now() {\n    let _ = std::time::SystemTime::now();\n}\n",
            2,
            "clock: `SystemTime`",
        ),
        (
            "thread::spawn",
            "fn go() {\n\n    std::thread::spawn(|| {});\n}\n",
            3,
            "thread: `thread::spawn`",
        ),
        (
            "a qualified std HashMap",
            "type Votes = std::collections::HashMap<u64, bool>;\n",
            1,
            "std's `HashMap` with its default",
        ),
        (
            "an imported std HashSet",
            "use std::collections::{HashSet, VecDeque};\n",
            1,
            "imports std's `HashSet`",
        ),
        (
            "a use of an imported std HashMap",
            "use std::collections::HashMap as Map;\nfn f() -> Map<u8, u8> {\n    Map::new()\n}\n",
            2,
            "std's `Map` with its default",
        ),
        (
            "RandomState",
            "fn h() -> std::collections::hash_map::RandomState {\n    todo()\n}\n",
            1,
            "`RandomState`",
        ),
        (
            "{:p}",
            "fn show(x: &u8) {\n    debug!(\"at {:p}\", x);\n}\n",
            2,
            "pointer formatting",
        ),
        (
            "a named {x:#p}",
            "fn show(x: &u8) {\n    format!(\"{x:#p}\");\n}\n",
            2,
            "pointer formatting",
        ),
    ];

    #[test]
    fn each_forbidden_pattern_fails_and_names_its_file_and_line() {
        for &(what, text, line, expected) in FORBIDDEN {
            let found = violations(text);
            let at = format!("vendor/raft/raft/src/fixture.rs:{line}: ");
            assert!(
                found
                    .iter()
                    .any(|v| v.starts_with(&at) && v.contains(expected)),
                "rule: RAFT-AUDIT rejects {what} at {at}…{expected}; source={text:?} \
                 violations={found:#?}"
            );
        }
    }

    #[test]
    fn comments_literals_and_test_items_are_not_scanned() {
        let text = r##"//! Doc: `Instant`, `thread_rng()` and `std::collections::HashMap` in prose.
/// let start = std::time::Instant::now();
/* block /* nested Instant */ SystemTime */
fn messages() -> (&'static str, &'static str, char, char) {
    let quoted = "thread_rng() Instant // not a comment";
    let raw = r#"a "quoted" std::thread::spawn"#;
    (quoted, raw, '"', '\'')
}
fn lifetimes<'a>(x: &'a str) -> &'a str { x }
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;
    fn noise() { let _ = rand::thread_rng(); std::thread::spawn(|| {}); }
}
#[cfg(test)]
#[inline]
fn helper() -> Instant { Instant::now() }
#[cfg(test)]
pub mod raft;
fn after() -> u64 { 1 }
"##;
        let found = violations(text);
        assert!(
            found.is_empty(),
            "rule: comments, doc comments, literals and #[cfg(test)] items are not scanned; \
             violations={found:#?}"
        );
        let whole = "#![cfg(test)]\nuse std::time::Instant;\n";
        assert!(
            violations(whole).is_empty(),
            "rule: a #![cfg(test)] file is test code"
        );
        let after = format!("{text}fn late() {{ let _ = std::time::Instant::now(); }}\n");
        let found = violations(&after);
        let line = after.lines().count();
        assert!(
            found.len() == 1
                && found[0].starts_with(&format!("vendor/raft/raft/src/fixture.rs:{line}: ")),
            "rule: scanning resumes after a test item, on the right line; violations={found:#?}"
        );
    }

    const LIB_ALIASES: &str = "type DefaultHashBuilder = std::hash::BuildHasherDefault<fxhash::FxHasher>;\n\
                               type HashMap<K, V> = std::collections::HashMap<K, V, DefaultHashBuilder>;\n\
                               type HashSet<K> = std::collections::HashSet<K, DefaultHashBuilder>;\n";
    const QUORUM: &str = "use std::collections::HashMap;\n\
                          pub type AckIndexer = HashMap<u64, Index>;\n\
                          impl AckedIndexer for AckIndexer {}\n";

    #[test]
    fn the_fxhash_aliases_and_the_ack_indexer_are_the_allowed_std_maps() {
        let allowed = [
            source("raft/src/lib.rs", LIB_ALIASES),
            source(ACK_INDEXER_FILE, QUORUM),
        ];
        let found = audit(&allowed, 0);
        assert!(
            found.is_empty(),
            "rule: FxHash aliases and quorum.rs's AckIndexer are allowed (W02.2 H-1, H-2); \
             violations={found:#?}"
        );

        let quorum = format!("{QUORUM}fn tally() -> HashMap<u64, bool> {{ HashMap::new() }}\n");
        let found = audit(&[source(ACK_INDEXER_FILE, &quorum)], 0);
        assert_eq!(
            found.len(),
            2,
            "rule: quorum.rs's std HashMap is allowed only as AckIndexer; violations={found:#?}"
        );
        assert!(
            found.iter().all(|v| v.contains("quorum.rs:4: ")),
            "violations={found:#?}"
        );

        let seeded = LIB_ALIASES.replace(
            "std::hash::BuildHasherDefault<fxhash::FxHasher>",
            "std::collections::hash_map::RandomState",
        );
        let found = audit(&[source("raft/src/lib.rs", &seeded)], 0);
        assert!(
            found
                .iter()
                .any(|v| v.contains("lib.rs:1: hash: DefaultHashBuilder is")),
            "rule: the aliases are allowed only while DefaultHashBuilder is FxHash; \
             violations={found:#?}"
        );

        let unpinned = LIB_ALIASES.lines().skip(1).collect::<Vec<_>>().join("\n");
        let found = audit(&[source("raft/src/lib.rs", &unpinned)], 0);
        assert!(
            found
                .iter()
                .any(|v| v.contains("no `type DefaultHashBuilder")),
            "rule: an alias naming an undefined DefaultHashBuilder fails; violations={found:#?}"
        );
    }

    #[test]
    fn entropy_sites_must_match_the_expected_count() {
        let site = "fn reset() {\n    let t = rand::thread_rng().gen_range(1..9);\n}\n";
        let one = [source("raft/src/raft.rs", site)];
        assert!(
            audit(&one, 1).is_empty(),
            "rule: one line naming both rand:: and thread_rng is one site, as expected \
             before W02.3"
        );
        let found = audit(&one, 0);
        assert!(
            found.len() == 1 && found[0].starts_with("vendor/raft/raft/src/raft.rs:2: entropy: 1"),
            "rule: after W02.3 no entropy site remains; violations={found:#?}"
        );
        let none = [source("raft/src/raft.rs", "fn reset() {}\n")];
        assert!(
            audit(&none, 1)
                .iter()
                .any(|v| v.contains("entropy: 0 source line(s)")),
            "rule: fewer sites than expected also fails, so the count is kept honest"
        );
        assert_eq!(
            EXPECTED_ENTROPY_SITES, 0,
            "W02.3 (56c587f) removed the only site"
        );
    }

    #[test]
    fn the_scan_covers_crate_sources_only_and_skips_test_files() {
        let root = scratch_dir("raft-audit-layout");
        let write = |path: &str, text: &str| {
            let full = root.join(VENDOR).join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        };
        write("raft/src/lib.rs", "pub fn clean() {}\n");
        write(
            "raft/src/quorum/datadriven_test.rs",
            "use std::time::Instant;\n",
        );
        write("raft/benches/bench.rs", "use std::time::Instant;\n");
        write(
            "raft-proto/src/protos/eraftpb.rs",
            "\n\nuse std::time::Instant;\n",
        );
        let outcome = check_raft_audit(&root).unwrap();
        assert_eq!(outcome.cases, 2 + RULES, "two crate sources and the rules");
        assert_eq!(
            outcome.violations,
            [
                "vendor/raft/raft-proto/src/protos/eraftpb.rs:3: clock: `Instant` reads \
              wall-clock time (W02.2 C-1)"
            ],
            "rule: every crate's src/ is scanned; *_test.rs and non-src files are not"
        );
    }

    #[test]
    fn an_empty_scan_is_not_a_pass() {
        let root = scratch_dir("raft-audit-empty");
        assert!(
            check_raft_audit(&root).is_err(),
            "rule: no vendored sources means RAFT-AUDIT examined nothing (P10)"
        );
    }
}
