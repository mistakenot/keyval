//! W08.1's tests: every §7.10 rejection rule has its own test, plus the strict reader
//! (D-79), the range table, the consensus library's constraints (D-78), the accepted
//! fixture corpus and `config/local.toml`.
#![cfg(test)]

// check: CONFIG-RULES

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::rules::{self, Values};
use super::*;
use crate::{ClusterIdError, Key, OutOfRange, Quantity, Value};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../../config/local.toml");

/// The accepted fixture corpus (`config/fixtures/`, D-78), by file name.
const FIXTURES: [(&str, &str); 5] = [
    (
        "one-voter.toml",
        include_str!("../../../../config/fixtures/one-voter.toml"),
    ),
    (
        "three-voters.toml",
        include_str!("../../../../config/fixtures/three-voters.toml"),
    ),
    (
        "five-voters-explicit-limits.toml",
        include_str!("../../../../config/fixtures/five-voters-explicit-limits.toml"),
    ),
    (
        "small-limits.toml",
        include_str!("../../../../config/fixtures/small-limits.toml"),
    ),
    (
        "fastest-timing.toml",
        include_str!("../../../../config/fixtures/fastest-timing.toml"),
    ),
];

/// A valid three-voter config. Each rejection test changes one thing in it. Line numbers:
/// 1 `cluster_id`, 2–15 the required parameters, 17/23/29 the `[[nodes]]` headers, then
/// `id`, `client_address`, `peer_address`, `data_dir` on the next four lines of each.
const BASE: &str = "\
cluster_id = \"keyval-test\"
protocol_version = 1
tick_ms = 50
heartbeat_ticks = 2
election_min_ticks = 20
election_max_ticks_exclusive = 40
write_deadline_ticks = 100
cli_deadline_ms = 15000
max_key_bytes = 256
max_value_bytes = 65536
max_live_keys = 10000
max_live_bytes = 67108864
max_log_entries = 1000000
max_history_bytes = 3221225472
max_journal_bytes = 4294967296

[[nodes]]
id = 1
client_address = \"127.0.0.1:7001\"
peer_address = \"127.0.0.1:8001\"
data_dir = \"nodes/1\"

[[nodes]]
id = 2
client_address = \"127.0.0.1:7002\"
peer_address = \"127.0.0.1:8002\"
data_dir = \"nodes/2\"

[[nodes]]
id = 3
client_address = \"127.0.0.1:7003\"
peer_address = \"127.0.0.1:8003\"
data_dir = \"nodes/3\"
";

/// The directory the test configs notionally live in.
const CONFIG_DIR: &str = "/etc/keyval";

/// The line a key added by [`with_top`] lands on.
const ADDED_LINE: usize = 16;

/// Parses `text` as if it were in [`CONFIG_DIR`].
fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::parse(text, Path::new(CONFIG_DIR))
}

/// [`BASE`] with `from`, which must occur exactly once, replaced by `to`.
fn with(from: &str, to: &str) -> String {
    assert_eq!(
        BASE.matches(from).count(),
        1,
        "test setup: {from:?} must occur exactly once in BASE"
    );
    BASE.replacen(from, to, 1)
}

/// [`BASE`] with `line` added at the end of the top-level table, on [`ADDED_LINE`].
fn with_top(line: &str) -> String {
    with(
        "max_journal_bytes = 4294967296\n",
        &format!("max_journal_bytes = 4294967296\n{line}\n"),
    )
}

/// [`BASE`] with top-level key `key` set to `value`: replacing its line if `BASE` has
/// one, otherwise added. Returns the text and the key's line.
fn set_top(key: &str, value: &str) -> (String, usize) {
    let prefix = format!("{key} = ");
    BASE.lines()
        .enumerate()
        .find(|(_, line)| line.starts_with(&prefix))
        .map_or_else(
            || (with_top(&format!("{prefix}{value}")), ADDED_LINE),
            |(index, old)| {
                let replaced = with(&format!("{old}\n"), &format!("{prefix}{value}\n"));
                (replaced, index + 1)
            },
        )
}

/// [`BASE`]'s top-level table followed by `count` well-formed `[[nodes]]` tables.
fn with_voters(count: u64) -> String {
    let top = BASE.split("\n[[nodes]]").next().unwrap_or_default();
    let mut text = format!("{top}\n");
    for id in 1..=count {
        write!(
            text,
            "\n[[nodes]]\nid = {id}\nclient_address = \"127.0.0.1:{}\"\npeer_address = \
             \"127.0.0.1:{}\"\ndata_dir = \"nodes/{id}\"\n",
            7000 + id,
            8000 + id
        )
        .unwrap();
    }
    text
}

/// Asserts that `text` is refused with exactly `expected`, naming the rule.
fn refused(text: &str, expected: &ConfigError, rule: &str) {
    let parsed = parse(text);
    assert_eq!(
        parsed.as_ref().err(),
        Some(expected),
        "rule: {rule}; expected the config to be refused with {expected:?}, observed {:?}; \
         config:\n{text}",
        parsed.as_ref().map(|_| "accepted")
    );
}

/// Asserts that `text` is accepted, naming the rule whose boundary it sits on.
fn accepted(text: &str, rule: &str) -> Config {
    parse(text).unwrap_or_else(|error| {
        panic!("rule: {rule}; expected acceptance, observed {error:?} ({error}); config:\n{text}")
    })
}

/// [`BASE`]'s validated parameter values, for the rule functions' direct tests.
fn base_values() -> Values {
    accepted(BASE, "BASE is a valid config").values
}

/// A named change to valid consensus settings, and the check's expected outcome.
type Mutation = (
    &'static str,
    fn(&mut ConsensusSettings),
    Result<(), ConsensusViolation>,
);

/// A node ID known to be valid.
fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

#[test]
fn local_toml_has_five_loopback_voters_and_the_plan_defaults() {
    let config = Config::parse(LOCAL, Path::new("config"))
        .unwrap_or_else(|error| panic!("rule: config/local.toml parses (W08); observed {error}"));
    let voters: Vec<u64> = config.voters().map(NodeId::get).collect();
    assert_eq!(
        voters,
        [1, 2, 3, 4, 5],
        "rule: the local profile pins 5 voters (D-45)"
    );
    for node in config.nodes() {
        assert!(
            node.client_address().ip().is_loopback() && node.peer_address().ip().is_loopback(),
            "rule: local addresses are loopback (O-9); node={node:?}"
        );
        let expected = PathBuf::from(format!(".local/nodes/{}", node.id()));
        assert_eq!(
            node.data_dir(),
            expected,
            "rule: data_dir resolves against the config file's directory (§7.7 step 0)"
        );
    }
    assert_eq!(config.cluster_id().as_str(), "keyval-local-v1");
    let expected = [
        (ConfigParam::ProtocolVersion, 1),
        (ConfigParam::TickMs, 50),
        (ConfigParam::HeartbeatTicks, 2),
        (ConfigParam::ElectionMinTicks, 20),
        (ConfigParam::ElectionMaxTicksExclusive, 40),
        (ConfigParam::WriteDeadlineTicks, 100),
        (ConfigParam::CliDeadlineMs, 15_000),
        (ConfigParam::MaxKeyBytes, 256),
        (ConfigParam::MaxValueBytes, 65_536),
        (ConfigParam::MaxLiveKeys, 10_000),
        (ConfigParam::MaxLiveBytes, 64 << 20),
        (ConfigParam::MaxLogEntries, 1_000_000),
        (ConfigParam::MaxHistoryBytes, 3 << 30),
        (ConfigParam::MaxJournalBytes, 4 << 30),
        (ConfigParam::ReplicationBatchBytes, 256 << 10),
        (ConfigParam::ReplicationInflightBatches, 16),
        (ConfigParam::PeerSendQueueMessages, 64),
        (ConfigParam::PeerSendQueueBytes, 8 << 20),
        (ConfigParam::MaxUncommittedBytes, 4 << 20),
        (ConfigParam::MaxCommittedBytesPerReady, 1 << 20),
    ];
    for (param, value) in expected {
        assert_eq!(
            config.get(param),
            value,
            "rule: config/local.toml carries the §7.10/§7.11 default for {param}"
        );
    }
}

#[test]
fn every_fixture_config_is_accepted_and_passes_the_consensus_checks() {
    for (name, text) in FIXTURES {
        let config = Config::parse(text, Path::new("config/fixtures")).unwrap_or_else(|error| {
            panic!("rule: every config/fixtures/ file is accepted (D-78); file={name}: {error}")
        });
        for id in config.voters() {
            let settings = config.consensus(id);
            assert_eq!(
                settings.map(|settings| settings.check()),
                Some(Ok(())),
                "rule: every accepted voter passes raft-rs 0.7 Config::validate (D-78); \
                 file={name} node={id} settings={settings:?}"
            );
        }
    }
}

#[test]
fn one_three_and_five_voters_are_accepted_and_other_counts_refused() {
    for count in 0..=6 {
        let text = with_voters(count);
        if matches!(count, 1 | 3 | 5) {
            let config = accepted(&text, "1, 3 or 5 voters (D-45)");
            assert_eq!(config.voter_count(), usize::try_from(count).unwrap());
        } else {
            refused(
                &text,
                &ConfigError::VoterCount {
                    count: usize::try_from(count).unwrap(),
                },
                "a voter count other than 1, 3 or 5 is refused (D-45)",
            );
        }
    }
}

/// Asserts that [`BASE`] with its line `from` replaced by each `to` is refused with that
/// case's syntax problem, on `from`'s line (D-79).
fn refused_lines(from: &str, cases: &[(&str, ConfigSyntax)]) {
    let line = BASE.lines().position(|line| line == from).unwrap() + 1;
    for &(to, problem) in cases {
        refused(
            &with(from, to),
            &ConfigError::Syntax { line, problem },
            &format!("the strict reader refuses {to:?} (D-79)"),
        );
    }
}

#[test]
fn the_reader_refuses_values_other_than_decimal_integers_and_basic_strings() {
    use ConfigSyntax::{
        ControlCharacter, EscapeSequence, IntegerTooLarge, LeadingZero, MissingValue,
        UnsupportedValue, UnterminatedString,
    };
    refused_lines(
        "tick_ms = 50",
        &[
            ("tick_ms = 5_0", UnsupportedValue),
            ("tick_ms = 50.0", UnsupportedValue),
            ("tick_ms = 5e1", UnsupportedValue),
            ("tick_ms = +50", UnsupportedValue),
            ("tick_ms = -50", UnsupportedValue),
            ("tick_ms = 0x32", UnsupportedValue),
            ("tick_ms = true", UnsupportedValue),
            ("tick_ms = [50]", UnsupportedValue),
            ("tick_ms = { ms = 50 }", UnsupportedValue),
            ("tick_ms = 1979-05-27", UnsupportedValue),
            ("tick_ms = 050", LeadingZero),
            ("tick_ms = 9223372036854775808", IntegerTooLarge),
            ("tick_ms =", MissingValue),
            ("tick_ms = # none", MissingValue),
        ],
    );
    refused_lines(
        "cluster_id = \"keyval-test\"",
        &[
            ("cluster_id = 'keyval-test'", UnsupportedValue),
            ("cluster_id = \"\"\"keyval-test\"\"\"", UnsupportedValue),
            ("cluster_id = \"keyval\\ttest\"", EscapeSequence),
            ("cluster_id = \"keyval-test", UnterminatedString),
            ("cluster_id = \"keyval\u{0}test\"", ControlCharacter),
        ],
    );
}

#[test]
fn the_reader_refuses_other_keys_tables_lines_and_comments() {
    use ConfigSyntax::{
        ControlCharacter, ExpectedKeyValue, TrailingCharacters, UnsupportedKey, UnsupportedTable,
    };
    refused_lines(
        "tick_ms = 50",
        &[
            ("tick_ms 50", ExpectedKeyValue),
            ("= 50", ExpectedKeyValue),
            ("\"tick_ms\" = 50", UnsupportedKey),
            ("tick.ms = 50", UnsupportedKey),
            ("tick_ms = 50 60", TrailingCharacters),
            ("[limits]", UnsupportedTable),
            ("[[node]]", UnsupportedTable),
            ("[nodes]", UnsupportedTable),
            ("# bell \u{7}", ControlCharacter),
        ],
    );
}

#[test]
fn the_reader_accepts_comments_indentation_tabs_spaced_headers_and_crlf() {
    let expected = accepted(BASE, "BASE is valid");
    let crlf = BASE.replace('\n', "\r\n");
    let decorated = BASE
        .replace(
            "tick_ms = 50",
            "\t tick_ms\t=\t50   # a comment, with \"quotes\" and = signs",
        )
        .replace("[[nodes]]", "  [[ nodes ]]  # a node")
        .replace(
            "cluster_id = \"keyval-test\"",
            "# leading comment\ncluster_id = \"keyval-test\"",
        );
    for (variant, text) in [
        ("CRLF line ends", crlf),
        ("comments and spacing", decorated),
    ] {
        let config = accepted(&text, "the reader accepts TOML's spacing and comments");
        assert_eq!(
            config, expected,
            "rule: {variant} do not change the meaning (D-79)"
        );
    }
    let tab_in_string = with(
        "cluster_id = \"keyval-test\"",
        "cluster_id = \"keyval\ttest\"",
    );
    let config = accepted(&tab_in_string, "a tab is allowed in a basic string");
    assert_eq!(config.cluster_id().as_str(), "keyval\ttest");
}

#[test]
fn a_key_defined_twice_in_one_table_is_refused() {
    refused(
        &with_top("tick_ms = 50"),
        &ConfigError::DuplicateKey {
            line: ADDED_LINE,
            key: "tick_ms".to_owned(),
            first_line: 3,
        },
        "TOML forbids defining a key twice",
    );
    refused(
        &with("id = 2\n", "id = 2\nid = 2\n"),
        &ConfigError::DuplicateKey {
            line: 25,
            key: "id".to_owned(),
            first_line: 24,
        },
        "TOML forbids defining a key twice, in a node table too",
    );
}

#[test]
fn unknown_keys_are_refused_not_ignored() {
    refused(
        &with("tick_ms = 50", "tick_m = 50"),
        &ConfigError::UnknownKey {
            line: 3,
            key: "tick_m".to_owned(),
            table: ConfigTable::Top,
        },
        "a misspelt key is refused, never silently defaulted (O-5, D-79)",
    );
    refused(
        &with("id = 2\n", "id = 2\nport = 7002\n"),
        &ConfigError::UnknownKey {
            line: 25,
            key: "port".to_owned(),
            table: ConfigTable::Node { line: 23 },
        },
        "an unknown key in a [[nodes]] table is refused (O-5)",
    );
    let mut misplaced = BASE.to_owned();
    misplaced.push_str("tick_ms = 50\n");
    refused(
        &misplaced,
        &ConfigError::UnknownKey {
            line: 34,
            key: "tick_ms".to_owned(),
            table: ConfigTable::Node { line: 29 },
        },
        "as in TOML, a key after [[nodes]] belongs to that node",
    );
}

#[test]
fn a_missing_cluster_id_protocol_version_or_other_required_key_is_refused() {
    let mut required: Vec<&str> = ConfigParam::ALL
        .into_iter()
        .filter(|param| param.default_value().is_none())
        .map(ConfigParam::key)
        .collect();
    required.insert(0, "cluster_id");
    for key in required {
        let line = BASE
            .lines()
            .find(|line| line.starts_with(&format!("{key} = ")))
            .unwrap();
        refused(
            &with(&format!("{line}\n"), ""),
            &ConfigError::MissingKey {
                key: ConfigParam::from_key(key).map_or("cluster_id", ConfigParam::key),
                table: ConfigTable::Top,
            },
            &format!("a missing required key is refused (§7.10, D-79); key={key}"),
        );
    }
    for (line, key) in [
        ("id = 2\n", "id"),
        ("client_address = \"127.0.0.1:7002\"\n", "client_address"),
        ("peer_address = \"127.0.0.1:8002\"\n", "peer_address"),
        ("data_dir = \"nodes/2\"\n", "data_dir"),
    ] {
        refused(
            &with(line, ""),
            &ConfigError::MissingKey {
                key,
                table: ConfigTable::Node { line: 23 },
            },
            &format!("every [[nodes]] key is required; key={key}"),
        );
    }
}

#[test]
fn a_value_of_the_wrong_kind_is_refused() {
    let cases = [
        (
            "tick_ms = 50",
            "tick_ms = \"50\"",
            3,
            "tick_ms",
            ConfigValueKind::Integer,
        ),
        (
            "cluster_id = \"keyval-test\"",
            "cluster_id = 1",
            1,
            "cluster_id",
            ConfigValueKind::String,
        ),
        ("id = 2", "id = \"2\"", 24, "id", ConfigValueKind::Integer),
        (
            "client_address = \"127.0.0.1:7002\"",
            "client_address = 7002",
            25,
            "client_address",
            ConfigValueKind::String,
        ),
        (
            "data_dir = \"nodes/2\"",
            "data_dir = 2",
            27,
            "data_dir",
            ConfigValueKind::String,
        ),
    ];
    for (from, to, line, key, expected) in cases {
        refused(
            &with(from, to),
            &ConfigError::WrongType {
                line,
                key,
                expected,
            },
            &format!("{key} takes {expected}"),
        );
    }
}

#[test]
fn every_parameter_outside_its_range_is_refused() {
    for param in ConfigParam::ALL {
        let mut outside = vec![param.maximum() + 1];
        if param.minimum() > 0 {
            outside.push(param.minimum() - 1);
        }
        for value in outside {
            let (text, line) = set_top(param.key(), &value.to_string());
            refused(
                &text,
                &ConfigError::OutOfRange { line, param, value },
                &format!(
                    "every limit lies in its declared range (§7.10, §7.11); {param} allows {}..={}",
                    param.minimum(),
                    param.maximum()
                ),
            );
        }
    }
}

#[test]
fn a_heartbeat_interval_below_one_tick_is_refused() {
    let (text, line) = set_top("heartbeat_ticks", "0");
    refused(
        &text,
        &ConfigError::OutOfRange {
            line,
            param: ConfigParam::HeartbeatTicks,
            value: 0,
        },
        "heartbeat interval < 1 tick is refused (§7.10)",
    );
}

#[test]
fn a_protocol_version_other_than_this_builds_is_refused() {
    let (text, line) = set_top("protocol_version", "2");
    refused(
        &text,
        &ConfigError::OutOfRange {
            line,
            param: ConfigParam::ProtocolVersion,
            value: 2,
        },
        "the config's protocol version is the one this build speaks (§8.2)",
    );
}

#[test]
fn the_range_table_is_consistent_with_the_plan() {
    let keys: Vec<&str> = ConfigParam::ALL.into_iter().map(ConfigParam::key).collect();
    for param in ConfigParam::ALL {
        assert!(
            param.minimum() <= param.maximum(),
            "rule: min <= max; param={param}"
        );
        assert_eq!(
            ConfigParam::from_key(param.key()),
            Some(param),
            "rule: keys round-trip"
        );
        assert_eq!(
            keys.iter().filter(|key| **key == param.key()).count(),
            1,
            "rule: keys are unique; param={param}"
        );
        if let Some(default) = param.default_value() {
            assert!(
                param.allows(default),
                "rule: a default lies in its range; param={param}"
            );
            assert_eq!(
                param.class(),
                ConfigParamClass::Local,
                "rule: only local limits have defaults; semantic and timing keys are required \
                 (O-5, D-79); param={param}"
            );
        }
    }
    let required: Vec<&str> = ConfigParam::ALL
        .into_iter()
        .filter(|param| param.default_value().is_none())
        .map(ConfigParam::key)
        .collect();
    let example = BASE
        .lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split(" = ").next())
        .filter(|key| *key != "cluster_id")
        .collect::<Vec<_>>();
    assert_eq!(
        required, example,
        "rule: the required keys are §7.10's example keys"
    );
    let semantic: Vec<&str> = ConfigParam::ALL
        .into_iter()
        .filter(|param| param.class() == ConfigParamClass::Semantic)
        .map(ConfigParam::key)
        .collect();
    assert_eq!(
        semantic,
        [
            "max_key_bytes",
            "max_value_bytes",
            "max_live_keys",
            "max_live_bytes",
            "max_log_entries",
            "max_history_bytes"
        ],
        "rule: the semantic limits are §7.10's fingerprinted ones; max_journal_bytes is local"
    );
    assert_eq!(
        ConfigParam::MaxKeyBytes.maximum(),
        u64::try_from(Key::MAX_BYTES).unwrap(),
        "rule: one bound, in one place (DM-1)"
    );
    assert_eq!(
        ConfigParam::MaxValueBytes.maximum(),
        u64::try_from(Value::MAX_BYTES).unwrap(),
        "rule: one bound, in one place (DM-2)"
    );
}

#[test]
fn optional_local_limits_default_to_section_7_11_and_can_be_lowered() {
    let config = accepted(BASE, "BASE is valid");
    assert_eq!(config.replication_batch_bytes().get(), 256 << 10);
    assert_eq!(config.replication_inflight_batches(), 16);
    assert_eq!(config.peer_send_queue_messages(), 64);
    assert_eq!(config.peer_send_queue_bytes().get(), 8 << 20);
    assert_eq!(config.max_uncommitted_bytes().get(), 4 << 20);
    assert_eq!(config.max_committed_bytes_per_ready().get(), 1 << 20);
    let lowered = accepted(
        &with_top("replication_batch_bytes = 1024"),
        "a local limit may be lowered",
    );
    assert_eq!(lowered.replication_batch_bytes().get(), 1024);
}

#[test]
fn the_cluster_id_is_1_to_255_bytes() {
    refused(
        &with("cluster_id = \"keyval-test\"", "cluster_id = \"\""),
        &ConfigError::ClusterId {
            line: 1,
            error: ClusterIdError::Empty,
        },
        "an empty cluster ID is refused",
    );
    let long = "c".repeat(256);
    refused(
        &with(
            "cluster_id = \"keyval-test\"",
            &format!("cluster_id = \"{long}\""),
        ),
        &ConfigError::ClusterId {
            line: 1,
            error: ClusterIdError::TooLong { bytes: 256 },
        },
        "a cluster ID over 255 bytes is refused",
    );
    let longest = "c".repeat(255);
    accepted(
        &with(
            "cluster_id = \"keyval-test\"",
            &format!("cluster_id = \"{longest}\""),
        ),
        "a 255-byte cluster ID is accepted",
    );
}

#[test]
fn node_ids_are_nonzero_and_below_2_pow_53() {
    for raw in [0, 1 << 53] {
        refused(
            &with("id = 2\n", &format!("id = {raw}\n")),
            &ConfigError::NodeId {
                line: 24,
                error: OutOfRange {
                    quantity: Quantity::NodeId,
                    value: raw,
                },
            },
            "node IDs are nonzero and below 2^53 (§7.10, D-77)",
        );
    }
    let config = accepted(
        &with("id = 2\n", "id = 9007199254740991\n"),
        "2^53 - 1 is the largest node ID (D-77)",
    );
    assert!(config.node(node((1 << 53) - 1)).is_some());
}

#[test]
fn duplicate_node_ids_are_refused() {
    refused(
        &with("id = 2\n", "id = 1\n"),
        &ConfigError::DuplicateNodeId {
            id: node(1),
            line: 24,
            first_line: 18,
        },
        "duplicate node IDs are refused (§7.10)",
    );
}

#[test]
fn duplicate_client_or_peer_addresses_are_refused() {
    let address = |text: &str| text.parse::<SocketAddr>().unwrap();
    let cases = [
        (
            "client_address = \"127.0.0.1:7002\"",
            "client_address = \"127.0.0.1:7001\"",
            address("127.0.0.1:7001"),
            25,
            19,
        ),
        (
            "peer_address = \"127.0.0.1:8002\"",
            "peer_address = \"127.0.0.1:8001\"",
            address("127.0.0.1:8001"),
            26,
            20,
        ),
        (
            "client_address = \"127.0.0.1:7002\"",
            "client_address = \"127.0.0.1:8001\"",
            address("127.0.0.1:8001"),
            25,
            20,
        ),
        (
            "peer_address = \"127.0.0.1:8002\"",
            "peer_address = \"127.0.0.1:7002\"",
            address("127.0.0.1:7002"),
            26,
            25,
        ),
    ];
    for (from, to, address, line, first_line) in cases {
        refused(
            &with(from, to),
            &ConfigError::DuplicateAddress {
                address,
                line,
                first_line,
            },
            "no address is used twice, as client or peer address of any nodes (§7.10)",
        );
    }
}

#[test]
fn addresses_are_an_ip_address_and_a_fixed_port() {
    let cases = [
        ("\"localhost:7002\"", AddressProblem::NotIpAndPort),
        ("\"127.0.0.1\"", AddressProblem::NotIpAndPort),
        ("\"::1:7002\"", AddressProblem::NotIpAndPort),
        ("\"127.0.0.1:0\"", AddressProblem::PortZero),
    ];
    for (value, problem) in cases {
        refused(
            &with(
                "client_address = \"127.0.0.1:7002\"",
                &format!("client_address = {value}"),
            ),
            &ConfigError::Address {
                line: 25,
                key: "client_address",
                problem,
            },
            &format!("an address is an IP address and a nonzero port; value={value}"),
        );
    }
    accepted(
        &with(
            "client_address = \"127.0.0.1:7002\"",
            "client_address = \"[::1]:7002\"",
        ),
        "an IPv6 address is accepted",
    );
}

#[test]
fn duplicate_or_empty_data_directories_are_refused_after_resolving() {
    for same in [
        "\"nodes/./1\"",
        "\"nodes/x/../1\"",
        "\"/etc/keyval/nodes/1\"",
        "\"../keyval/nodes/1\"",
    ] {
        refused(
            &with("data_dir = \"nodes/2\"", &format!("data_dir = {same}")),
            &ConfigError::DuplicateDataDir {
                path: PathBuf::from("/etc/keyval/nodes/1"),
                line: 27,
                first_line: 21,
            },
            &format!("data directories are compared after resolving (§7.10); data_dir={same}"),
        );
    }
    refused(
        &with("data_dir = \"nodes/2\"", "data_dir = \"\""),
        &ConfigError::EmptyDataDir { line: 27 },
        "an empty data directory is refused",
    );
}

#[test]
fn relative_data_directories_resolve_against_the_config_directory() {
    let cases = [
        ("/etc/keyval", "nodes/1", "/etc/keyval/nodes/1"),
        ("/etc/keyval", "/srv/keyval/1", "/srv/keyval/1"),
        ("config", "../.local/nodes/1", ".local/nodes/1"),
        ("config", "../../up/1", "../up/1"),
        ("", "nodes/./1/", "nodes/1"),
        ("/", "../x", "/x"),
        ("config", "..", "."),
    ];
    for (config_dir, data_dir, expected) in cases {
        let text = with(
            "data_dir = \"nodes/1\"",
            &format!("data_dir = \"{data_dir}\""),
        );
        let config = Config::parse(&text, Path::new(config_dir)).unwrap_or_else(|error| {
            panic!("rule: data_dir {data_dir:?} in {config_dir:?} is accepted; observed {error}")
        });
        assert_eq!(
            config.node(node(1)).map(NodeConfig::data_dir),
            Some(Path::new(expected)),
            "rule: relative data_dir resolves against the config file's directory (§7.7 \
             step 0); config_dir={config_dir:?} data_dir={data_dir:?}"
        );
    }
}

#[test]
fn an_election_minimum_below_five_heartbeats_is_refused() {
    refused(
        &with("election_min_ticks = 20", "election_min_ticks = 9"),
        &ConfigError::ElectionMinTooShort {
            election_min_ticks: 9,
            heartbeat_ticks: 2,
        },
        "election timeout minimum < 5 × heartbeat interval is refused (§7.10)",
    );
    accepted(
        &with(
            "election_min_ticks = 20\nelection_max_ticks_exclusive = 40",
            "election_min_ticks = 10\nelection_max_ticks_exclusive = 20",
        ),
        "an election minimum of exactly 5 heartbeats is accepted",
    );
}

#[test]
fn an_election_maximum_not_above_the_minimum_is_refused() {
    for max in [20, 19] {
        refused(
            &with(
                "election_max_ticks_exclusive = 40",
                &format!("election_max_ticks_exclusive = {max}"),
            ),
            &ConfigError::ElectionMaxNotAboveMin {
                election_min_ticks: 20,
                election_max_ticks_exclusive: max,
            },
            "election timeout maximum <= minimum is refused (§7.10)",
        );
    }
}

#[test]
fn an_election_range_other_than_t_to_2t_is_refused() {
    for max in [21, 39, 41] {
        refused(
            &with(
                "election_max_ticks_exclusive = 40",
                &format!("election_max_ticks_exclusive = {max}"),
            ),
            &ConfigError::ElectionRangeShape {
                election_min_ticks: 20,
                election_max_ticks_exclusive: max,
            },
            "election_max_ticks_exclusive != 2 × election_min_ticks is refused (§7.10, D-75)",
        );
    }
}

#[test]
fn a_write_deadline_below_the_election_maximum_is_refused() {
    refused(
        &with("write_deadline_ticks = 100", "write_deadline_ticks = 39"),
        &ConfigError::WriteDeadlineTooShort {
            write_deadline_ticks: 39,
            election_max_ticks_exclusive: 40,
        },
        "write deadline < election timeout maximum is refused (§7.10)",
    );
    accepted(
        &with("write_deadline_ticks = 100", "write_deadline_ticks = 40"),
        "a write deadline equal to the election maximum is accepted",
    );
}

#[test]
fn a_cli_deadline_below_the_write_deadline_is_refused() {
    // §7.10 lists the conditions the parser REJECTS: "CLI deadline < write deadline" is a
    // refused config, compared in milliseconds (write deadline = 100 ticks × 50 ms).
    refused(
        &with("cli_deadline_ms = 15000", "cli_deadline_ms = 4999"),
        &ConfigError::CliDeadlineTooShort {
            cli_deadline_ms: 4999,
            write_deadline_ms: 5000,
        },
        "CLI deadline < write deadline is refused (§7.10)",
    );
    accepted(
        &with("cli_deadline_ms = 15000", "cli_deadline_ms = 5000"),
        "a CLI deadline equal to the write deadline is accepted",
    );
}

#[test]
fn the_largest_command_fits_the_canonical_command_limit() {
    let mut values = base_values();
    assert_eq!(
        rules::command_fits(&values),
        Ok(()),
        "rule: the largest command fits"
    );
    values.max_value_bytes += 1_000;
    assert_eq!(
        rules::command_fits(&values),
        Err(ConfigError::CommandTooLarge {
            command_bytes: 256 + 66_536 + COMMAND_ENVELOPE_BOUND,
            limit: MAX_COMMAND_BYTES,
        }),
        "rule: max_key_bytes + max_value_bytes + envelope > canonical-command limit is \
         refused (§7.10); values={values:?}"
    );
    let largest =
        u64::try_from(Key::MAX_BYTES + Value::MAX_BYTES).unwrap() + COMMAND_ENVELOPE_BOUND;
    assert!(
        largest <= MAX_COMMAND_BYTES
            && MAX_COMMAND_BYTES <= MAX_FRAME_PAYLOAD_BYTES
            && MAX_COMMAND_BYTES <= MAX_REQUEST_BODY_BYTES,
        "rule: the largest valid command fits the canonical-command limit, which fits the \
         journal-frame and HTTP-body limits (§7.10, §7.11); largest={largest}"
    );
}

#[test]
fn the_journal_margin_formula_is_pinned_and_exact() {
    let frame: u64 = FRAME_FIELDS.iter().map(|(_, bytes)| bytes).sum();
    assert_eq!(
        (frame, FRAME_OVERHEAD_BYTES, PER_ENTRY_OVERHEAD_BOUND),
        (32, 32, 64),
        "rule: the per-entry overhead bound is a frame's fixed fields (§7.6) plus the entry \
         envelope bound, pinned at 64 B (§7.10, D-68)"
    );
    assert_eq!(
        JOURNAL_PROTOCOL_ALLOWANCE,
        64 << 20,
        "rule: the allowance is 64 MiB (§7.11)"
    );
    let required = 4_026_531_840 + 1_000_000 * 64 + (64 << 20);
    assert_eq!(
        rules::required_journal_bytes(3 << 30, 1_000_000),
        required,
        "rule: 1.25 × 3 GiB + 1,000,000 × 64 + 64 MiB"
    );
    let (text, line) = set_top("max_journal_bytes", &(required - 1).to_string());
    assert_eq!(line, 15);
    refused(
        &text,
        &ConfigError::JournalMarginTooSmall {
            max_journal_bytes: u64::try_from(required - 1).unwrap(),
            required: u64::try_from(required).unwrap(),
        },
        "max_journal_bytes below the margin formula is refused (§7.10, D-68)",
    );
    accepted(
        &set_top("max_journal_bytes", &required.to_string()).0,
        "max_journal_bytes exactly at the formula is accepted",
    );
    assert_eq!(
        rules::required_journal_bytes(1, 0),
        2 + (64 << 20),
        "rule: 1.25 × history rounds up, so the comparison is exact"
    );
}

#[test]
fn in_flight_replication_bytes_must_fit_the_send_queue() {
    let inflight = 16 * (256 << 10);
    refused(
        &with_top(&format!("peer_send_queue_bytes = {}", inflight - 1)),
        &ConfigError::ReplicationExceedsSendQueue {
            inflight_bytes: inflight,
            peer_send_queue_bytes: inflight - 1,
        },
        "in-flight batches × batch size > per-peer send-queue bytes is refused (§7.10)",
    );
    accepted(
        &with_top(&format!("peer_send_queue_bytes = {inflight}")),
        "in-flight bytes equal to the send queue's bytes are accepted",
    );
}

#[test]
fn consensus_settings_mirror_the_config() {
    let config = accepted(BASE, "BASE is valid");
    let settings = config.consensus(node(2)).unwrap();
    let observed = (
        settings.node_id(),
        settings.heartbeat_tick(),
        settings.election_tick(),
        settings.min_election_tick(),
        settings.max_election_tick(),
        settings.max_size_per_msg(),
        settings.max_inflight_msgs(),
        settings.max_uncommitted_size(),
        settings.max_committed_size_per_ready(),
        settings.check_quorum(),
        settings.pre_vote(),
        settings.read_mode(),
    );
    assert_eq!(
        observed,
        (
            2,
            2,
            20,
            20,
            40,
            256 << 10,
            16,
            4 << 20,
            1 << 20,
            true,
            true,
            ReadMode::Safe
        ),
        "rule: the library's settings come from the config: election_tick = the election \
         minimum, range [min, 2 × min), pre-vote and check-quorum on (§7.10)"
    );
    assert_eq!(settings.check(), Ok(()));
    assert_eq!(
        config.consensus(node(4)),
        None,
        "rule: only voters have settings"
    );
}

#[test]
fn each_raft_rs_validate_constraint_is_its_own_rule() {
    let valid = accepted(BASE, "BASE is valid").consensus(node(1)).unwrap();
    let cases: [Mutation; 11] = [
        (
            "node id 0",
            |s| s.node_id = 0,
            Err(ConsensusViolation::ZeroNodeId),
        ),
        (
            "heartbeat 0",
            |s| s.heartbeat_tick = 0,
            Err(ConsensusViolation::ZeroHeartbeat),
        ),
        (
            "election = heartbeat",
            |s| s.election_tick = s.heartbeat_tick,
            Err(ConsensusViolation::ElectionNotAboveHeartbeat),
        ),
        (
            "min election below election",
            |s| s.min_election_tick = s.election_tick - 1,
            Err(ConsensusViolation::MinElectionBelowElection),
        ),
        (
            "min election = max",
            |s| s.max_election_tick = s.min_election_tick,
            Err(ConsensusViolation::MinElectionNotBelowMax),
        ),
        (
            "no in-flight messages",
            |s| s.max_inflight_msgs = 0,
            Err(ConsensusViolation::ZeroInflight),
        ),
        (
            "lease reads without check-quorum",
            |s| {
                s.read_mode = ReadMode::LeaseBased;
                s.check_quorum = false;
            },
            Err(ConsensusViolation::LeaseReadsWithoutCheckQuorum),
        ),
        (
            "lease reads with check-quorum",
            |s| s.read_mode = ReadMode::LeaseBased,
            Ok(()),
        ),
        (
            "uncommitted below message size",
            |s| s.max_uncommitted_size = s.max_size_per_msg - 1,
            Err(ConsensusViolation::UncommittedBelowMessageSize),
        ),
        (
            "uncommitted equal to message size",
            |s| s.max_uncommitted_size = s.max_size_per_msg,
            Ok(()),
        ),
        ("unchanged", |_| {}, Ok(())),
    ];
    for (case, mutate, expected) in cases {
        let mut settings = valid;
        mutate(&mut settings);
        assert_eq!(
            settings.check(),
            expected,
            "rule: raft-rs 0.7 Config::validate, one check per rule (§7.10, D-78); \
             case={case} settings={settings:?}"
        );
    }
}

#[test]
fn the_parser_refuses_settings_the_consensus_library_would_refuse() {
    refused(
        &with_top("max_uncommitted_bytes = 1024"),
        &ConfigError::Consensus {
            node: node(1),
            violation: ConsensusViolation::UncommittedBelowMessageSize,
        },
        "max_uncommitted_size < max_size_per_msg is refused before RawNode construction \
         (§7.10, D-78)",
    );
}

#[test]
fn a_config_file_over_64_kib_is_refused() {
    let mut text = BASE.to_owned();
    while text.len() <= 64 * 1024 {
        text.push_str("# padding padding padding padding padding padding padding padding\n");
    }
    let bytes = u64::try_from(text.len()).unwrap();
    refused(
        &text,
        &ConfigError::FileTooLarge { bytes },
        "the config file is bounded (CD-9)",
    );
}

#[test]
fn errors_name_the_line_and_the_rule() {
    let cases = [
        (
            ConfigError::UnknownKey {
                line: 3,
                key: "tick_m".to_owned(),
                table: ConfigTable::Top,
            },
            "line 3: unknown key `tick_m` in the top-level table",
        ),
        (
            ConfigError::Syntax {
                line: 7,
                problem: ConfigSyntax::UnsupportedTable,
            },
            "line 7: unsupported table header: the only table is `[[nodes]]`",
        ),
        (
            ConfigError::MissingKey {
                key: "id",
                table: ConfigTable::Node { line: 23 },
            },
            "missing required key `id` in the [[nodes]] table at line 23",
        ),
        (
            ConfigError::CliDeadlineTooShort {
                cli_deadline_ms: 4999,
                write_deadline_ms: 5000,
            },
            "cli_deadline_ms = 4999 is below the write deadline (write_deadline_ticks × tick_ms \
             = 5000 ms)",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(
            error.to_string(),
            expected,
            "rule: errors are specific (§7.10)"
        );
    }
}
