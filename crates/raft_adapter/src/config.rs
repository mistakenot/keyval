//! The validated consensus settings as raft-rs 0.7's `Config` (PLAN.md §7.3, §7.10, §7.11).

use raft::ReadOnlyOption;
use types::{ConsensusSettings, LogIndex, ReadMode};

/// The library configuration for `settings`. On a restart, entries through `applied` are
/// already applied (§7.7 step 6).
///
/// Pre-vote and check-quorum are on, and every bound comes from the settings, including
/// `max_committed_size_per_ready` (1 MiB by default), which the library would otherwise leave
/// unlimited. The settings parser mirrors the library's own validation rule for rule, so the
/// library accepts every config the parser accepts (D-78).
#[must_use]
pub fn library_config(settings: &ConsensusSettings, applied: LogIndex) -> raft::Config {
    raft::Config {
        id: settings.node_id().get(),
        election_tick: count(settings.election_tick().get()),
        heartbeat_tick: count(settings.heartbeat_tick().get()),
        applied: applied.get(),
        max_size_per_msg: settings.max_size_per_msg().get(),
        max_inflight_msgs: count(settings.max_inflight_msgs().get()),
        check_quorum: settings.check_quorum(),
        pre_vote: settings.pre_vote(),
        min_election_tick: count(settings.min_election_tick().get()),
        max_election_tick: count(settings.max_election_tick().get()),
        read_only_option: match settings.read_mode() {
            ReadMode::Safe => ReadOnlyOption::Safe,
            ReadMode::LeaseBased => ReadOnlyOption::LeaseBased,
        },
        max_uncommitted_size: settings.max_uncommitted_size().get(),
        max_committed_size_per_ready: settings.max_committed_size_per_ready().get(),
        ..raft::Config::default()
    }
}

/// A tick or message count as the library's `usize`. The settings ranges keep every such
/// value small, so the conversion always fits.
fn count(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::config;

    #[test]
    fn every_setting_reaches_the_library_config() {
        let config = config("five-voters-explicit-limits");
        let node = config.voters().next().unwrap();
        let settings = config.consensus(node).unwrap();
        let library = library_config(&settings, LogIndex::new(7).unwrap());
        assert_eq!(
            (
                library.id,
                library.election_tick,
                library.heartbeat_tick,
                library.min_election_tick,
                library.max_election_tick,
                library.applied,
            ),
            (
                node.get(),
                count(settings.election_tick().get()),
                count(settings.heartbeat_tick().get()),
                count(settings.min_election_tick().get()),
                count(settings.max_election_tick().get()),
                7,
            ),
            "rule: identity, timing and the applied index come from the settings (§7.10)"
        );
        assert_eq!(
            (
                library.max_size_per_msg,
                library.max_inflight_msgs,
                library.max_uncommitted_size,
                library.max_committed_size_per_ready,
            ),
            (
                settings.max_size_per_msg().get(),
                count(settings.max_inflight_msgs().get()),
                settings.max_uncommitted_size().get(),
                settings.max_committed_size_per_ready().get(),
            ),
            "rule: every §7.11 library bound is set, including the per-Ready committed \
             bytes the library leaves unlimited by default"
        );
        assert!(
            library.pre_vote && library.check_quorum,
            "rule: pre-vote and check-quorum are on (§7.3)"
        );
        assert_ne!(
            library.max_committed_size_per_ready,
            raft::NO_LIMIT,
            "rule: committed entries per Ready are bounded (W02 audit U-3)"
        );
        assert_eq!(
            library.validate().map_err(|err| err.to_string()),
            Ok(()),
            "rule: the library accepts the mapped config (D-78)"
        );
    }
}
