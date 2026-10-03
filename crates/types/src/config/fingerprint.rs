//! The semantic-config fingerprint (PLAN.md §7.10, DUR-8): a digest of every parameter
//! that can change a result, plus the canonical voter set, so two configurations that
//! would compute different results, or have different memberships, never interoperate.
//!
//! The genesis frame binds each data directory to it (§7.6, §7.7), and every peer
//! envelope carries it (§8.2).
//!
//! It is SHA-256 over [`Config::semantic_encoding`], a canonical text that starts with a
//! format line and then has one `key=value` line per covered value, in a fixed order:
//! - the command-format version ([`COMMAND_FORMAT_VERSION`]);
//! - every [`ConfigParamClass::Semantic`] parameter, in [`ConfigParam::ALL`] order: the
//!   key and value limits (DM-1, DM-2), the live-state limits (DM-6) and the history
//!   capacity (DM-7);
//! - the fixed SCAN caps ([`PageSize::MAX`] items, [`MAX_SCAN_PAGE_BYTES`] decoded bytes),
//!   which §7.11 makes part of the fingerprint;
//! - `voters=`, the voters' IDs ascending, comma-separated.
//!
//! Everything else is left out because it cannot change a result: the display cluster ID,
//! timing, addresses, data directories, `max_journal_bytes` and the queue and library
//! limits. Changing the encoding changes every fingerprint, so it is a format change: bump
//! [`SEMANTIC_ENCODING`]'s version and record it in §16.

use core::fmt;

use super::params::{COMMAND_FORMAT_VERSION, MAX_SCAN_PAGE_BYTES};
use super::{Config, ConfigParam, ConfigParamClass};
use crate::{PageSize, Sha256};

/// The canonical encoding's first line: its name and version.
pub const SEMANTIC_ENCODING: &str = "keyval semantic config v1";

/// The semantic-config fingerprint: SHA-256 over [`Config::semantic_encoding`] (DUR-8).
///
/// Invariant: equal for two configs exactly when their covered values and voter sets are
/// equal (up to SHA-256 collisions, a stated assumption, §10.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticFingerprint([u8; 32]);

impl SemanticFingerprint {
    /// A fingerprint read back from storage or a peer envelope.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest's 32 bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Display for SemanticFingerprint {
    /// 64 lowercase hex digits, as in the observation schema's `config_fingerprint`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl Config {
    /// The canonical text the semantic-config fingerprint digests (module documentation).
    #[must_use]
    pub fn semantic_encoding(&self) -> String {
        let voters: Vec<String> = self.voters().map(|id| id.get().to_string()).collect();
        let mut lines = vec![
            SEMANTIC_ENCODING.to_owned(),
            format!("command_format_version={COMMAND_FORMAT_VERSION}"),
        ];
        lines.extend(
            ConfigParam::ALL
                .into_iter()
                .filter(|param| param.class() == ConfigParamClass::Semantic)
                .map(|param| format!("{}={}", param.key(), self.get(param))),
        );
        lines.push(format!("scan_page_max_items={}", PageSize::MAX));
        lines.push(format!("scan_page_max_bytes={MAX_SCAN_PAGE_BYTES}"));
        lines.push(format!("voters={}", voters.join(",")));
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// The semantic-config fingerprint (DUR-8): SHA-256 over
    /// [`semantic_encoding`](Self::semantic_encoding).
    #[must_use]
    pub fn semantic_fingerprint(&self) -> SemanticFingerprint {
        SemanticFingerprint(Sha256::digest(self.semantic_encoding().as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    // check: CONFIG-FINGERPRINT

    use std::fmt::Write as _;
    use std::path::Path;

    use super::super::{Config, ConfigParam, ConfigParamClass};
    use super::{SEMANTIC_ENCODING, SemanticFingerprint};

    /// The repository's local cluster config.
    const LOCAL: &str = include_str!("../../../../config/local.toml");

    /// Parses `text`, which must be valid.
    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("config"))
            .unwrap_or_else(|error| panic!("test setup: config must parse: {error}\n{text}"))
    }

    /// [`LOCAL`] with `from`, which must occur exactly once, replaced by `to`.
    fn local_with(from: &str, to: &str) -> String {
        assert_eq!(
            LOCAL.matches(from).count(),
            1,
            "test setup: {from:?} once in LOCAL"
        );
        LOCAL.replacen(from, to, 1)
    }

    #[test]
    fn local_toml_has_the_hand_written_canonical_encoding_and_pinned_fingerprint() {
        let config = parse(LOCAL);
        // Written by hand from PLAN.md §7.10/§7.11 and config/local.toml.
        let expected = "keyval semantic config v1\n\
                        command_format_version=1\n\
                        max_key_bytes=256\n\
                        max_value_bytes=65536\n\
                        max_live_keys=10000\n\
                        max_live_bytes=67108864\n\
                        max_log_entries=1000000\n\
                        max_history_bytes=3221225472\n\
                        scan_page_max_items=128\n\
                        scan_page_max_bytes=1048576\n\
                        voters=1,2,3,4,5\n";
        assert_eq!(
            config.semantic_encoding(),
            expected,
            "rule: the fingerprint covers exactly the semantic values and voter set (§7.10)"
        );
        assert_eq!(
            config.semantic_fingerprint().to_string(),
            "3b2e3ed34b0f5f49312b1d00bcebdc9f5100d408ae2209daf7a4c1695ec3c26c",
            "rule: the encoding is a format: changing it changes every genesis binding \
             (DUR-8); the digest is pinned from an independent SHA-256 of the text above"
        );
        assert!(expected.starts_with(SEMANTIC_ENCODING));
    }

    #[test]
    fn the_fingerprint_is_stable_across_re_parse_and_non_semantic_changes() {
        let base = parse(LOCAL).semantic_fingerprint();
        assert_eq!(
            parse(LOCAL).semantic_fingerprint(),
            base,
            "rule: stable across re-parse"
        );
        let nodes_reversed = {
            let (top, nodes) = LOCAL.split_once("\n[[nodes]]").unwrap();
            let mut tables: Vec<&str> = nodes.split("\n[[nodes]]").collect();
            tables.reverse();
            format!("{top}\n[[nodes]]{}", tables.join("\n[[nodes]]"))
        };
        let changes = [
            ("node tables in another order", nodes_reversed),
            (
                "comments and spacing",
                LOCAL.replace(" = ", "   =   ").replace('\n', "  # c\n"),
            ),
            (
                "the display cluster ID",
                local_with("\"keyval-local-v1\"", "\"another-name\""),
            ),
            ("tick length", local_with("tick_ms = 50", "tick_ms = 20")),
            (
                "timing",
                local_with(
                    "heartbeat_ticks = 2\nelection_min_ticks = 20\nelection_max_ticks_exclusive = 40",
                    "heartbeat_ticks = 3\nelection_min_ticks = 30\nelection_max_ticks_exclusive = 60",
                ),
            ),
            (
                "CLI deadline",
                local_with("cli_deadline_ms = 15000", "cli_deadline_ms = 20000"),
            ),
            (
                "the local journal hard stop",
                local_with(
                    "max_journal_bytes = 4294967296",
                    "max_journal_bytes = 5000000000",
                ),
            ),
            (
                "a local queue limit",
                local_with(
                    "max_journal_bytes = 4294967296",
                    "max_journal_bytes = 4294967296\npeer_send_queue_messages = 32",
                ),
            ),
            (
                "addresses",
                local_with("\"127.0.0.1:7001\"", "\"127.0.0.2:7001\""),
            ),
            (
                "data directories",
                local_with("\"../.local/nodes/1\"", "\"/srv/keyval/1\""),
            ),
        ];
        for (change, text) in changes {
            assert_eq!(
                parse(&text).semantic_fingerprint(),
                base,
                "rule: {change} cannot change a result, so it is not fingerprinted (§7.10)"
            );
        }
    }

    #[test]
    fn every_semantic_parameter_changes_the_fingerprint() {
        let base = parse(LOCAL).semantic_fingerprint();
        let semantic: Vec<ConfigParam> = ConfigParam::ALL
            .into_iter()
            .filter(|param| param.class() == ConfigParamClass::Semantic)
            .collect();
        assert_eq!(semantic.len(), 6, "rule: six semantic parameters (§7.10)");
        for param in semantic {
            let line = LOCAL
                .lines()
                .find(|line| line.starts_with(&format!("{} = ", param.key())))
                .unwrap();
            let value = parse(LOCAL).get(param);
            let lowered = value - 1;
            let text = local_with(line, &format!("{} = {lowered}", param.key()));
            let config = Config::parse(&text, Path::new("config"));
            match config {
                Ok(config) => assert_ne!(
                    config.semantic_fingerprint(),
                    base,
                    "rule: every semantic parameter is fingerprinted (DUR-8); {param}: \
                     {value} -> {lowered}"
                ),
                Err(error) => panic!("test setup: {param} = {lowered} must parse: {error}"),
            }
        }
    }

    #[test]
    fn different_memberships_with_one_cluster_id_never_share_a_fingerprint() {
        let three = |ids: [u64; 3]| {
            let top = LOCAL.split("\n[[nodes]]").next().unwrap();
            let mut text = format!("{top}\n");
            for (index, id) in ids.into_iter().enumerate() {
                write!(
                    text,
                    "\n[[nodes]]\nid = {id}\nclient_address = \"127.0.0.1:{}\"\n\
                     peer_address = \"127.0.0.1:{}\"\ndata_dir = \"n/{index}\"\n",
                    7001 + index,
                    8001 + index
                )
                .unwrap();
            }
            parse(&text)
        };
        let base = three([1, 2, 3]);
        assert_eq!(
            three([3, 1, 2]).semantic_fingerprint(),
            base.semantic_fingerprint(),
            "rule: the voter set is canonical (sorted IDs), so file order does not matter"
        );
        let five = parse(LOCAL);
        for (membership, config) in [
            ("{1,2,4}", three([1, 2, 4])),
            ("{1,2,12}", three([1, 2, 12])),
            ("{11,2,3}", three([11, 2, 3])),
            ("{1,2,3,4,5}", five),
        ] {
            assert_eq!(
                config.cluster_id(),
                base.cluster_id(),
                "test setup: one cluster ID"
            );
            assert_ne!(
                config.semantic_fingerprint(),
                base.semantic_fingerprint(),
                "rule: two configs with the same cluster ID but different memberships \
                 fingerprint differently (§7.10, DUR-8); {{1,2,3}} vs {membership}"
            );
        }
    }

    #[test]
    fn the_fingerprint_round_trips_through_its_bytes_and_prints_as_64_hex_digits() {
        let fingerprint = parse(LOCAL).semantic_fingerprint();
        assert_eq!(
            SemanticFingerprint::from_bytes(fingerprint.to_bytes()),
            fingerprint
        );
        let text = fingerprint.to_string();
        assert_eq!(text.len(), 64, "rule: 32 bytes as hex");
        assert!(
            text.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "rule: lowercase hex; text={text}"
        );
    }
}
