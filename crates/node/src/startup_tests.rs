//! br-0fha's tests: a node that starts over a torn tail feeds the library nothing until the
//! truncation's sync completes, exactly as while a loop sync is outstanding (PLAN.md §7.4,
//! §7.6, §7.7, §7.11, DUR-6, DUR-9).
#![cfg(test)]

use statemachine::Applied;
use types::{
    AttemptId, ByteCount, Config, Effect, Input, Lifecycle, NodeId, RequestId, Role, SyncSeq, Term,
};

use crate::Counters;
use crate::test_support::{
    FIRST_SYNC, Handed, INCARNATION, Member, config, genesis_journal, key, put, request, value,
};

/// The storage effects among `effects`, in order.
fn storage(effects: &[Effect]) -> Vec<Effect> {
    effects
        .iter()
        .filter(|effect| effect.storage_op().is_some())
        .cloned()
        .collect()
}

/// `id` started over its genesis journal plus a 4-byte torn tail; the truncation's sync is
/// not completed. Returns the member and the genesis length.
fn torn(config: &Config, id: NodeId) -> (Member, u64) {
    let journal = genesis_journal(config, id);
    let genesis_len = journal.byte_len();
    journal.push(&[0xde, 0xad, 0xbe, 0xef]);
    let member = Member::start(config, id, journal, id.get());
    assert_eq!(
        storage(&member.effects),
        truncated(genesis_len),
        "fixture: node {id} starts by truncating its torn tail and syncing, and writes \
         nothing else"
    );
    (member, genesis_len)
}

/// The start-up storage effects over a torn tail: truncate to `len`, then sync.
fn truncated(len: u64) -> Vec<Effect> {
    vec![
        Effect::Truncate {
            len: ByteCount::new(len),
        },
        Effect::Sync {
            seq: FIRST_SYNC,
            incarnation: INCARNATION,
        },
    ]
}

/// The proposals the library has been handed.
fn proposals(member: &Member) -> usize {
    member
        .ctl
        .requests()
        .handed
        .iter()
        .filter(|handed| match handed {
            Handed::Proposed { .. } => true,
            Handed::Attempt { .. }
            | Handed::Applied { .. }
            | Handed::Expired { .. }
            | Handed::StatusChanged { .. }
            | Handed::Shutdown => false,
        })
        .count()
}

/// The request IDs the state machine executed, in order.
fn executed(member: &Member) -> Vec<RequestId> {
    member
        .ctl
        .requests()
        .applied()
        .into_iter()
        .filter_map(|(_, applied)| match applied {
            Applied::Executed { request_id, .. } => Some(request_id),
            Applied::Empty | Applied::Replayed { .. } | Applied::Conflict { .. } => None,
        })
        .collect()
}

#[test]
fn a_torn_tail_holds_the_library_until_its_truncation_is_synced() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let (mut member, genesis_len) = torn(&config, id);
    // Far past the election timeout, after which a fresh lone voter campaigns.
    let ticks = 100;
    for _ in 0..ticks {
        member.step(Input::Tick);
    }
    let attempt = AttemptId::new(1);
    member.step(Input::Client {
        attempt,
        op: put(1, "a", "x"),
    });
    assert_eq!(
        (
            storage(&member.effects),
            member.syncs.iter().copied().collect::<Vec<SyncSeq>>(),
            member.journal.byte_len(),
            member.ctl.status().role,
            member.ctl.status().term,
            member.ctl.outstanding_sync(),
        ),
        (
            truncated(genesis_len),
            vec![FIRST_SYNC],
            genesis_len,
            Role::Follower,
            Term::ZERO,
            None,
        ),
        "rule: while the start-up truncation's sync is outstanding the library takes no \
         input, so {ticks} ticks and a client write bring no Append, no second Sync and no \
         campaign (§7.6: truncated, then synced, before the next append; §7.11: at most one \
         sync outstanding); node={id}"
    );
    assert_eq!(
        (
            member.ctl.requests().attempts(),
            member.ctl.requests().waiting.len(),
            proposals(&member),
            member.ctl.lifecycle(),
        ),
        (vec![attempt], 1, 0, Lifecycle::Ready),
        "rule: client attempts are still handed over while the start-up sync is outstanding, \
         and the proposal waits for the library (§7.4 step 3); node={id}"
    );

    assert!(
        member.complete_sync(),
        "fixture: the truncation's sync completes"
    );
    let second = FIRST_SYNC.checked_next().unwrap();
    assert_eq!(
        (
            member.syncs.iter().copied().collect::<Vec<SyncSeq>>(),
            member.ctl.outstanding_sync(),
            member.ctl.status().term > Term::ZERO,
            proposals(&member),
        ),
        (vec![second], Some(second), true, 1),
        "rule: once the truncation is synced the proposal that waited is handed to the \
         library (§7.4 step 3), and the owed ticks reach it: it campaigns and asks for exactly \
         one sync, numbered after the start-up sync's (§7.10); node={id}"
    );

    member.elect(100);
    member.step(Input::Client {
        attempt: AttemptId::new(2),
        op: put(2, "b", "y"),
    });
    while member.complete_sync() {}
    assert_eq!(
        (
            member.ctl.status().role,
            executed(&member),
            member
                .ctl
                .state()
                .get(&key("b"))
                .map(|stored| stored.value().clone()),
            member.ctl.counters(),
            member.ctl.lifecycle(),
        ),
        (
            Role::Leader,
            vec![request(2)],
            Some(value("y")),
            Counters::default(),
            Lifecycle::Ready,
        ),
        "rule: after the start-up sync the node proceeds normally: it leads, and a write is \
         proposed, committed and executed, with nothing dropped; node={id}"
    );
}

#[test]
fn a_peer_message_waits_for_the_start_up_sync_and_is_then_stepped() {
    let config = config("three-voters");
    let ids: Vec<NodeId> = config.voters().collect();
    let (a, b) = (ids[0], ids[1]);
    let mut candidate = Member::fresh(&config, a);
    let mut ticks = 0;
    let envelope = loop {
        assert!(
            ticks < 200,
            "fixture: node {a} sent node {b} nothing in 200 ticks"
        );
        candidate.step(Input::Tick);
        while candidate.complete_sync() {}
        ticks += 1;
        if let Some(index) = candidate.outbox.iter().position(|(to, _)| *to == b) {
            break candidate.outbox.remove(index).unwrap().1;
        }
    };
    let (mut member, genesis_len) = torn(&config, b);
    let effects = member.step(Input::Peer {
        from: a,
        bytes: envelope,
    });
    assert_eq!(
        (
            effects,
            member.outbox.len(),
            member.syncs.iter().copied().collect::<Vec<SyncSeq>>(),
            member.ctl.status().term,
            member.ctl.counters(),
            member.ctl.envelope_drops().total(),
        ),
        (
            Vec::new(),
            0,
            vec![FIRST_SYNC],
            Term::ZERO,
            Counters::default(),
            0,
        ),
        "rule: a peer envelope that arrives while the start-up sync is outstanding waits in \
         the queue: it is neither stepped (no reply, no write, no term change) nor dropped \
         (§7.4, §7.6); from={a} node={b}"
    );
    assert_eq!(
        storage(&member.effects),
        truncated(genesis_len),
        "rule: nothing is written over the truncation before its sync completes; node={b}"
    );

    while member.complete_sync() {}
    let replies = member.outbox.iter().filter(|(to, _)| *to == a).count();
    assert_eq!(
        (
            replies,
            member.ctl.counters(),
            member.ctl.envelope_drops().total(),
        ),
        (1, Counters::default(), 0),
        "rule: once the truncation is synced the waiting envelope is opened and stepped, and \
         the node answers its sender, with nothing dropped; from={a} node={b}"
    );
}
