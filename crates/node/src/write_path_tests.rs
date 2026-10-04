//! W14.2's tests of the write path (PLAN.md §7.8, F-7, A-5, DM-7): the decision order's
//! precedence, the attempt state machine, attach and the same-ID race, deadlines,
//! leadership loss and shutdown; first on a bench that drives the write path directly,
//! then end to end through the controller.
#![cfg(test)]

// check: NODE-WRITE-PATH

use statemachine::codec::{self, Command};
use statemachine::{StateLimits, StateMachine};
use types::{
    AttemptId, ClientOp, Decision, Effect, Entry, EntryIndex, EntryPayload, ExpectedVersion,
    HistoryLimit, Input, Lifecycle, LogIndex, Mutation, NodeId, NotReadyLifecycle, ProposeError,
    Refusal, RequestId, Response, Role, ServedBy, Status, Term, TerminalResult, Tick, Version,
};

use crate::deadlines::Deadlines;
use crate::requests::{Applying, Core, View};
use crate::test_support::{Member, config, key, put, request, value};
use crate::{Proposal, WritePath};

/// The write path of a node whose status, lifecycle and state a test sets by hand.
struct Bench {
    path: WritePath,
    core: Core,
    status: Status,
    /// Every effect, in order.
    effects: Vec<Effect>,
}

impl Bench {
    /// The leader of `config`'s first voter, in term 2, with nothing applied.
    fn leader(name: &str) -> Self {
        let config = config(name);
        let node = config.voters().next().unwrap();
        let pending_attempts = 256;
        Self {
            path: WritePath::new(&config, node),
            core: Core {
                node,
                now: Tick::new(10),
                lifecycle: Lifecycle::Ready,
                shutting_down: false,
                write_deadline: config.write_deadline(),
                deadlines: Deadlines::new(node, pending_attempts),
                state: StateMachine::new(node, StateLimits::of(&config)),
            },
            status: Status {
                role: Role::Leader,
                term: Term::new(2).unwrap(),
                leader: Some(node),
                commit: LogIndex::ZERO,
                last: LogIndex::ZERO,
                progress: Vec::new(),
            },
            effects: Vec::new(),
        }
    }

    fn node(&self) -> NodeId {
        self.core.node
    }

    /// Runs `f` with the write path and a view of the bench, returning the effects it
    /// pushed.
    fn with<R>(
        &mut self,
        f: impl FnOnce(&mut WritePath, &mut View<'_>, &mut Vec<Effect>) -> R,
    ) -> (R, Vec<Effect>) {
        let mut effects = Vec::new();
        let mut view = View::new(&mut self.core, &self.status);
        let result = f(&mut self.path, &mut view, &mut effects);
        self.effects.extend(effects.clone());
        (result, effects)
    }

    /// Hands attempt `n`, writing `op`, to the write path.
    fn attempt(&mut self, n: u64, op: ClientOp) -> Vec<Effect> {
        let ClientOp::Write {
            request_id,
            mutation,
        } = op
        else {
            panic!("fixture: the bench takes writes only");
        };
        self.with(|path, view, effects| {
            path.attempt(view, AttemptId::new(n), request_id, mutation, effects);
        })
        .1
    }

    fn next_proposal(&mut self) -> Option<Proposal> {
        self.with(|path, view, _| path.next_proposal(view)).0
    }

    fn proposed(&mut self, attempt: u64, result: Result<(), ProposeError>) -> Vec<Effect> {
        self.with(|path, view, effects| {
            path.proposed(view, AttemptId::new(attempt), result, effects);
        })
        .1
    }

    /// Hands the next proposal to the library, which takes it.
    fn submit(&mut self) -> Proposal {
        let proposal = self.next_proposal().expect("fixture: a proposal is queued");
        assert!(
            self.proposed(proposal.attempt.get(), Ok(())).is_empty(),
            "rule: a taken proposal answers nothing yet; only its apply does (DUR-1)"
        );
        proposal
    }

    /// Applies the next entry, holding `command`, to the state machine, and tells the write
    /// path.
    fn apply(&mut self, command: Vec<u8>) -> Vec<Effect> {
        let index = self.core.state.applied_index().get() + 1;
        let entry = Entry {
            index: EntryIndex::new(index).unwrap(),
            term: self.status.term,
            payload: EntryPayload::Command(command),
        };
        let commit = entry.index.index();
        self.status.commit = commit;
        self.status.last = commit;
        let applied = self.core.state.apply(&entry, commit);
        let mut effects = Vec::new();
        let mut applying = Applying {
            node: self.core.node,
            entry: &entry,
            applied: &applied,
            commit,
            state: &self.core.state,
            deadlines: &mut self.core.deadlines,
        };
        self.path.applied(&mut applying, &mut effects);
        self.effects.extend(effects.clone());
        effects
    }

    fn expire(&mut self, attempt: u64) -> Vec<Effect> {
        self.with(|path, view, effects| path.expired(view, AttemptId::new(attempt), effects))
            .1
    }

    fn status_changed(&mut self) -> Vec<Effect> {
        self.with(WritePath::status_changed).1
    }

    fn shutdown(&mut self) -> Vec<Effect> {
        self.core.shutting_down = true;
        self.with(WritePath::shutdown).1
    }

    /// Makes this node a follower that believes `leader` leads, or knows no leader.
    fn follow(&mut self, leader: Option<NodeId>) {
        self.status.role = Role::Follower;
        self.status.leader = leader;
    }

    fn served_by(&self) -> ServedBy {
        ServedBy {
            node: self.node(),
            applied: self.core.state.applied_index(),
        }
    }
}

/// The canonical command of `op`, a write.
fn command(op: &ClientOp) -> Vec<u8> {
    let ClientOp::Write {
        request_id,
        mutation,
    } = op.clone()
    else {
        panic!("fixture: a write");
    };
    codec::encode(&Command {
        request_id,
        mutation,
    })
}

/// A PUT `k = v` whose request ID encodes `n`, so each `n` is a distinct request.
fn put_id(n: u64, k: &str, v: &str) -> ClientOp {
    let mut bytes = [9_u8; 16];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    let ClientOp::Write { mutation, .. } = put(1, k, v) else {
        panic!("fixture: a write");
    };
    ClientOp::Write {
        request_id: RequestId::from_bytes(bytes),
        mutation,
    }
}

fn respond(attempt: u64, response: Response) -> Effect {
    Effect::Respond {
        attempt: AttemptId::new(attempt),
        response,
    }
}

fn refused(attempt: u64, refusal: Refusal) -> Effect {
    respond(attempt, Response::Refused(refusal))
}

/// The written result of the entry at `index`: a PUT's.
fn written(attempt: u64, index: u64, replayed: bool, served_by: ServedBy) -> Effect {
    respond(
        attempt,
        Response::Written {
            result: types::TerminalResult::executed_at(
                EntryIndex::new(index).unwrap(),
                types::Decision::Set,
            ),
            replayed,
            served_by,
        },
    )
}

fn conflict(attempt: u64, original_index: u64) -> Effect {
    respond(
        attempt,
        Response::Conflict {
            original_index: EntryIndex::new(original_index).unwrap(),
        },
    )
}

fn other_node() -> NodeId {
    NodeId::new(7).unwrap()
}

// ---------------------------------------------------------------------------------------
// The decision order (F-7)
// ---------------------------------------------------------------------------------------

#[test]
fn a_node_that_is_not_ready_answers_recovering_before_anything_else() {
    let mut bench = Bench::leader("one-voter");
    let known = put(1, "k", "v");
    bench.attempt(1, known.clone());
    bench.submit();
    bench.apply(command(&known));
    bench.follow(None);
    bench.core.lifecycle = Lifecycle::Failed;
    assert_eq!(
        bench.attempt(2, known),
        vec![refused(
            2,
            Refusal::Recovering {
                lifecycle: NotReadyLifecycle::Failed
            }
        )],
        "rule: lifecycle is step 2: a failed node answers recovering, not the replay its \
         history holds and not unavailable (F-7, A-2)"
    );
}

#[test]
fn a_stopping_node_answers_unavailable() {
    let mut bench = Bench::leader("one-voter");
    bench.core.shutting_down = true;
    assert_eq!(
        bench.attempt(1, put(1, "k", "v")),
        vec![refused(1, Refusal::Unavailable)],
        "rule: a stopping node answers unavailable, not submitted (§8.4)"
    );
}

#[test]
fn the_pending_attempt_count_bound_is_busy_past_256_even_for_a_known_id() {
    let mut bench = Bench::leader("one-voter");
    let known = put(0, "known", "v");
    bench.attempt(1_000, known.clone());
    bench.submit();
    bench.apply(command(&known));
    for n in 1..=255 {
        assert!(
            bench.attempt(n, put_id(n, "k", "v")).is_empty(),
            "rule: attempt {n} of 256 waits (pending_attempts, §7.11)"
        );
    }
    assert_eq!(bench.path.waiting(), 255, "fixture: N-1 attempts wait");
    assert!(
        bench.attempt(256, put_id(256, "last", "v")).is_empty(),
        "rule: the 256th attempt still waits"
    );
    assert_eq!(
        bench.attempt(257, known),
        vec![refused(257, Refusal::Busy)],
        "rule: at the bound the 257th is busy, even with a known request ID: local \
         admission bounds are step 3, before the short-circuit (F-7)"
    );
}

#[test]
fn the_pending_attempt_byte_bound_is_busy_past_16_mib() {
    let mut bench = Bench::leader("one-voter");
    let big = "x".repeat(65_536);
    let mut n = 0_u64;
    let mut answered = Vec::new();
    // Attached waiters add bytes without adding pending proposals, so the 16 MiB bound is
    // reached before the 4 MiB in-flight bound or the 256-attempt bound.
    'requests: for id in 1..=16_u8 {
        for _ in 0..16 {
            n += 1;
            let effects = bench.attempt(n, put(id, &format!("k{id}"), &big));
            if !effects.is_empty() {
                answered = effects;
                break 'requests;
            }
        }
    }
    assert_eq!(
        (answered, bench.path.waiting()),
        (
            vec![refused(n, Refusal::Busy)],
            usize::try_from(n - 1).unwrap()
        ),
        "rule: an attempt whose command would take the waiting commands past \
         pending_attempt_bytes (16 MiB) is busy (§7.11); {} waited",
        n - 1
    );
    assert!(
        n - 1 < 256,
        "fixture: the byte bound, not the count, refused it"
    );
}

#[test]
fn a_known_request_id_is_answered_from_the_history_on_any_ready_node() {
    let mut bench = Bench::leader("small-limits");
    let first = put(1, "k", "v");
    bench.attempt(1, first.clone());
    bench.submit();
    bench.apply(command(&first));
    bench.follow(None);
    bench.status.commit = LogIndex::new(100).unwrap();
    let served_by = bench.served_by();
    assert_eq!(
        (
            bench.attempt(2, first),
            bench.attempt(3, put(1, "k", "other"))
        ),
        (vec![written(2, 1, true, served_by)], vec![conflict(3, 1)]),
        "rule: the short-circuit is step 4: a follower that knows no leader, at the history \
         capacity, answers a known ID from its applied history: the original result \
         (replayed) for the same command, request_id_conflict for another (F-6, F-7)"
    );
}

#[test]
fn a_follower_answers_not_leader_or_unavailable() {
    let mut bench = Bench::leader("three-voters");
    bench.follow(Some(other_node()));
    let to_leader = bench.attempt(1, put(1, "k", "v"));
    bench.follow(None);
    let to_nobody = bench.attempt(2, put(2, "k", "v"));
    assert_eq!(
        (to_leader, to_nobody),
        (
            vec![refused(
                1,
                Refusal::NotLeader {
                    leader: other_node()
                }
            )],
            vec![refused(2, Refusal::Unavailable)]
        ),
        "rule: a new write at a non-leader is not_leader naming the believed leader, or \
         unavailable if it knows none; never submitted (F-7, §8.4)"
    );
}

#[test]
fn history_admission_refuses_at_either_capacity_naming_entries_first() {
    let mut bench = Bench::leader("small-limits");
    // 4,096 bytes of first executions: 64 commands of 64+ bytes reach max_history_bytes.
    let mut applied = 0;
    while bench.core.state.committed_command_bytes() < types::ByteCount::new(4_096) {
        applied += 1;
        let n = u8::try_from(applied).unwrap();
        bench.apply(command(&put(
            n,
            &format!("k{}", applied % 8),
            &"v".repeat(60),
        )));
    }
    bench.status.commit = LogIndex::new(99).unwrap();
    let bytes = bench.attempt(1_000, put(250, "k", "v"));
    bench.status.commit = LogIndex::new(100).unwrap();
    let both = bench.attempt(1_001, put(251, "k", "v"));
    assert_eq!(
        (bytes, both),
        (
            vec![refused(
                1_000,
                Refusal::HistoryFull {
                    limit: HistoryLimit::HistoryBytes
                }
            )],
            vec![refused(
                1_001,
                Refusal::HistoryFull {
                    limit: HistoryLimit::LogEntries
                }
            )]
        ),
        "rule: DM-7 refuses a new write before proposal once the committed command bytes \
         reach max_history_bytes or the commit index reaches max_log_entries; with both \
         reached it names max_log_entries (br-camk may rank them)"
    );
}

#[test]
fn the_uncommitted_bytes_bound_is_busy_before_submitting() {
    let mut bench = Bench::leader("small-limits");
    let mut n = 0;
    let answered = loop {
        n += 1;
        let effects = bench.attempt(n, put(u8::try_from(n).unwrap(), "k", &"v".repeat(60)));
        if !effects.is_empty() {
            break effects;
        }
    };
    assert_eq!(
        answered,
        vec![refused(n, Refusal::Busy)],
        "rule: a write whose command would take the proposals not yet applied past \
         max_uncommitted_bytes (1,024) is busy, not submitted (§7.11)"
    );
    let refused_len =
        u64::try_from(command(&put(u8::try_from(n).unwrap(), "k", &"v".repeat(60))).len()).unwrap();
    assert!(
        bench.path.in_flight_bytes() <= 1_024 && bench.path.in_flight_bytes() + refused_len > 1_024,
        "fixture: the in-flight bound refused it: {} in flight, {refused_len} more",
        bench.path.in_flight_bytes()
    );
}

// ---------------------------------------------------------------------------------------
// The attempt state machine (§7.8, A-5)
// ---------------------------------------------------------------------------------------

#[test]
fn a_write_is_submitted_when_handed_over_and_answered_only_by_its_apply() {
    let mut bench = Bench::leader("one-voter");
    let op = put(1, "k", "v");
    assert!(
        bench.attempt(1, op.clone()).is_empty(),
        "rule: an admitted write waits"
    );
    assert_eq!(
        bench.core.deadlines.get(AttemptId::new(1)),
        Some(Tick::new(10 + 100)),
        "rule: its deadline is write_deadline_ticks from admission (§7.8)"
    );
    let proposal = bench.next_proposal();
    assert_eq!(
        proposal,
        Some(Proposal {
            attempt: AttemptId::new(1),
            command: command(&op),
        }),
        "rule: the proposal carries the attempt's canonical command"
    );
    bench.proposed(1, Ok(()));
    let served = ServedBy {
        node: bench.node(),
        applied: LogIndex::new(1).unwrap(),
    };
    assert_eq!(
        (
            bench.apply(command(&op)),
            bench.path.waiting(),
            bench.core.deadlines.len()
        ),
        (vec![written(1, 1, false, served)], 0, 0),
        "rule: the application of its entry answers it with the terminal result, and its \
         deadline is cancelled (DUR-1)"
    );
}

#[test]
fn a_synchronous_drop_is_busy_and_attached_waiters_answer_unknown_outcome() {
    let mut bench = Bench::leader("one-voter");
    let op = put(1, "k", "v");
    bench.attempt(1, op.clone());
    bench.attempt(2, op);
    bench.next_proposal().unwrap();
    assert_eq!(
        bench.proposed(1, Err(ProposeError::Dropped)),
        vec![
            refused(1, Refusal::Busy),
            refused(2, Refusal::UnknownOutcome)
        ],
        "rule: a ProposalDropped with nothing appended was never submitted: busy; an \
         attached waiter counts as submitted, so it answers unknown_outcome (A-5)"
    );
    assert_eq!(bench.path.pending(), 0, "rule: the proposal is gone");
}

#[test]
fn a_library_not_leader_answers_not_leader() {
    let mut bench = Bench::leader("three-voters");
    bench.attempt(1, put(1, "k", "v"));
    bench.next_proposal().unwrap();
    bench.follow(Some(other_node()));
    assert_eq!(
        bench.proposed(1, Err(ProposeError::NotLeader)),
        vec![refused(
            1,
            Refusal::NotLeader {
                leader: other_node()
            }
        )],
        "rule: the library refusing with nothing appended because it no longer leads is \
         not_leader, not submitted"
    );
}

#[test]
fn a_same_command_attempt_attaches_and_shares_the_proposals_result() {
    let mut bench = Bench::leader("one-voter");
    let op = put(1, "k", "v");
    bench.attempt(1, op.clone());
    bench.submit();
    assert!(
        bench.attempt(2, op.clone()).is_empty() && bench.next_proposal().is_none(),
        "rule: a same-ID, same-command attempt attaches: no second proposal (§7.8)"
    );
    let served = ServedBy {
        node: bench.node(),
        applied: LogIndex::new(1).unwrap(),
    };
    assert_eq!(
        bench.apply(command(&op)),
        vec![written(1, 1, false, served), written(2, 1, false, served)],
        "rule: the attached waiter gets the proposal's terminal result"
    );
}

#[test]
fn waiters_per_request_is_bounded_at_16() {
    let mut bench = Bench::leader("one-voter");
    let op = put(1, "k", "v");
    bench.attempt(1, op.clone());
    for n in 2..=17 {
        assert!(
            bench.attempt(n, op.clone()).is_empty(),
            "rule: waiter {} of 16 attaches",
            n - 1
        );
    }
    assert_eq!(
        bench.attempt(18, put(1, "k", "other")),
        vec![refused(18, Refusal::Busy)],
        "rule: the 17th waiter on one request ID, of either command, is busy (§7.11)"
    );
}

#[test]
fn a_different_command_waits_and_gets_request_id_conflict_on_apply() {
    let mut bench = Bench::leader("one-voter");
    let first = put(1, "k", "v");
    bench.attempt(1, first.clone());
    bench.submit();
    assert!(
        bench.attempt(2, put(1, "k", "other")).is_empty(),
        "rule: a same-ID attempt with another command never attaches; it waits for the \
         pending proposal to resolve (§7.8)"
    );
    let served = ServedBy {
        node: bench.node(),
        applied: LogIndex::new(1).unwrap(),
    };
    assert_eq!(
        bench.apply(command(&first)),
        vec![written(1, 1, false, served), conflict(2, 1)],
        "rule: once the proposal applied, the waiter is evaluated against the committed \
         history: request_id_conflict, never the other command's result (§7.5 step 3)"
    );
}

#[test]
fn a_different_command_behind_a_dropped_proposal_proposes_its_own() {
    let mut bench = Bench::leader("one-voter");
    let other = put(1, "k", "other");
    bench.attempt(1, put(1, "k", "v"));
    bench.attempt(2, other.clone());
    bench.next_proposal().unwrap();
    assert_eq!(
        bench.proposed(1, Err(ProposeError::Dropped)),
        vec![refused(1, Refusal::Busy)],
        "fixture: the first proposal is dropped synchronously"
    );
    assert_eq!(
        bench.next_proposal(),
        Some(Proposal {
            attempt: AttemptId::new(2),
            command: command(&other),
        }),
        "rule: a waiter whose proposal failed without applying enters the decision order \
         again and may propose its own command; whichever commits first is the original \
         (§7.8)"
    );
}

#[test]
fn another_command_for_the_id_resolves_a_submitted_proposal_with_request_id_conflict() {
    let mut bench = Bench::leader("one-voter");
    let mine = put(1, "k", "mine");
    let theirs = put(1, "k", "theirs");
    bench.attempt(1, mine.clone());
    bench.submit();
    bench.attempt(2, mine.clone());
    assert_eq!(
        bench.apply(command(&theirs)),
        vec![conflict(1, 1), conflict(2, 1)],
        "rule: another command carrying the ID, committed first (from another leadership), \
         decides the pending proposal: its owner and the attached attempt answer \
         request_id_conflict naming the original index at once, never the other command's \
         result and never unknown_outcome later (§7.8 Submitted row, §7.5 step 3)"
    );
    assert_eq!(
        (
            bench.core.deadlines.len(),
            bench.expire(1),
            bench.expire(2),
            bench.path.waiting()
        ),
        (0, Vec::new(), Vec::new(), 0),
        "rule: an answered attempt is out of the table with its deadline cancelled, so its \
         deadline answers nothing more"
    );
    assert!(
        bench.apply(command(&mine)).is_empty(),
        "rule: the proposal's own copy, applying later as a conflict, has nobody left to \
         answer"
    );
}

#[test]
fn attempts_behind_are_answered_from_the_history_when_another_command_decides() {
    let mut bench = Bench::leader("one-voter");
    let mine = put(1, "k", "mine");
    let theirs = put(1, "k", "theirs");
    bench.attempt(1, mine);
    bench.submit();
    bench.attempt(2, theirs.clone());
    bench.attempt(3, put(1, "k", "third"));
    let served = ServedBy {
        node: bench.node(),
        applied: LogIndex::new(1).unwrap(),
    };
    assert_eq!(
        bench.apply(command(&theirs)),
        vec![conflict(1, 1), written(2, 1, true, served), conflict(3, 1)],
        "rule: when another command decides the pending proposal, the attempts behind it are \
         answered from the history at once: the attempt with the applied command gets the \
         original result as replayed, any other request_id_conflict (F-6, §7.5 step 3)"
    );
}

#[test]
fn a_queued_proposal_is_withdrawn_when_another_command_for_its_id_applies() {
    let mut bench = Bench::leader("one-voter");
    bench.attempt(1, put(1, "k", "mine"));
    assert_eq!(
        bench.apply(command(&put(1, "k", "theirs"))),
        vec![conflict(1, 1)],
        "rule: a proposal not yet handed to the library is decided like a submitted one: \
         request_id_conflict naming the original index (§7.8)"
    );
    assert_eq!(
        (
            bench.next_proposal(),
            bench.path.pending(),
            bench.path.in_flight_bytes()
        ),
        (None, 0, 0),
        "rule: the queued proposal is withdrawn, never proposed, and holds no bytes in flight"
    );
}

#[test]
fn a_decided_submitted_proposal_keeps_its_bytes_until_its_copy_applies_or_leadership_is_lost() {
    let mut bench = Bench::leader("one-voter");
    let mine = put(1, "k", "mine");
    let theirs = put(1, "k", "theirs");
    let bytes = u64::try_from(command(&mine).len()).unwrap();
    bench.attempt(1, mine.clone());
    bench.submit();
    bench.apply(command(&theirs));
    assert_eq!(
        (
            bench.path.in_flight_bytes(),
            bench.path.pending(),
            bench.path.waiting()
        ),
        (bytes, 1, 0),
        "rule: the decided proposal's own copy is still uncommitted in the library's log, so \
         its bytes stay in flight (§7.11: proposed this leadership and not yet applied), with \
         nobody waiting on it"
    );
    assert_eq!(
        bench.attempt(2, mine.clone()),
        vec![conflict(2, 1)],
        "rule: a new attempt with the ID is answered from the history (F-7 step 4) before it \
         could attach to the decided proposal"
    );
    assert_eq!(
        (
            bench.apply(command(&mine)),
            bench.path.in_flight_bytes(),
            bench.path.pending()
        ),
        (Vec::new(), 0, 0),
        "rule: its bytes leave the in-flight count when its own copy applies"
    );

    let mut lost = Bench::leader("three-voters");
    lost.attempt(1, mine);
    lost.submit();
    lost.apply(command(&theirs));
    lost.follow(Some(other_node()));
    assert_eq!(
        (
            lost.status_changed(),
            lost.path.in_flight_bytes(),
            lost.path.pending()
        ),
        (Vec::new(), 0, 0),
        "rule: or when the leadership it was proposed under is lost, answering nobody"
    );
}

// ---------------------------------------------------------------------------------------
// Deadlines, leadership loss, failure and shutdown
// ---------------------------------------------------------------------------------------

#[test]
fn deadlines_answer_busy_if_never_proposed_and_unknown_outcome_if_submitted() {
    let mut bench = Bench::leader("one-voter");
    let submitted = put(1, "a", "v");
    bench.attempt(1, submitted.clone());
    bench.submit();
    bench.attempt(2, submitted.clone());
    bench.attempt(3, put(1, "a", "other"));
    bench.attempt(4, put(2, "b", "v"));
    let expired: Vec<Vec<Effect>> = [1, 2, 3, 4].map(|n| bench.expire(n)).into();
    assert_eq!(
        expired,
        vec![
            vec![refused(1, Refusal::UnknownOutcome)],
            vec![refused(2, Refusal::UnknownOutcome)],
            vec![refused(3, Refusal::Busy)],
            vec![refused(4, Refusal::Busy)],
        ],
        "rule: at its deadline a submitted attempt and an attached one answer \
         unknown_outcome; one behind another command, and one whose proposal never reached \
         the library, answer busy: never proposed (§7.8, A-5)"
    );
    assert!(
        bench.attempt(5, submitted.clone()).is_empty(),
        "rule: the submitted proposal stays in flight after its owner's deadline, and a retry \
         attaches to it instead of proposing again"
    );
    let served = ServedBy {
        node: bench.node(),
        applied: LogIndex::new(1).unwrap(),
    };
    assert_eq!(
        (bench.apply(command(&submitted)), bench.next_proposal()),
        (vec![written(5, 1, false, served)], None),
        "rule: when it commits, the attached retry gets its result; the expired proposal of \
         attempt 4 was withdrawn and is never proposed"
    );
}

#[test]
fn losing_the_leadership_fails_pending_proposals() {
    let mut bench = Bench::leader("three-voters");
    let submitted = put(1, "a", "v");
    bench.attempt(1, submitted.clone());
    bench.submit();
    bench.attempt(2, submitted);
    bench.attempt(3, put(1, "a", "other"));
    bench.attempt(4, put(2, "b", "v"));
    bench.follow(Some(other_node()));
    let not_leader = Refusal::NotLeader {
        leader: other_node(),
    };
    assert_eq!(
        bench.status_changed(),
        vec![
            refused(1, Refusal::UnknownOutcome),
            refused(2, Refusal::UnknownOutcome),
            refused(3, not_leader),
            refused(4, not_leader),
        ],
        "rule: on leadership loss a submitted or attached attempt answers unknown_outcome \
         (A-5); attempts never proposed enter the decision order again: not_leader (§7.8)"
    );
    assert_eq!(
        (
            bench.path.waiting(),
            bench.path.pending(),
            bench.core.deadlines.len()
        ),
        (0, 0, 0),
        "rule: nothing is left waiting"
    );
}

#[test]
fn a_new_term_fails_submitted_proposals_and_requeues_the_rest() {
    let mut bench = Bench::leader("one-voter");
    let submitted = put(1, "a", "v");
    let queued = put(2, "b", "v");
    bench.attempt(1, submitted);
    bench.submit();
    bench.attempt(2, queued.clone());
    bench.status.term = Term::new(3).unwrap();
    assert_eq!(
        bench.status_changed(),
        vec![refused(1, Refusal::UnknownOutcome)],
        "rule: leading again in a new term, a proposal from the old term may or may not \
         commit: unknown_outcome (A-5)"
    );
    assert_eq!(
        bench.next_proposal(),
        Some(Proposal {
            attempt: AttemptId::new(2),
            command: command(&queued),
        }),
        "rule: an attempt never proposed enters the decision order again and is proposed in \
         the new term"
    );
}

#[test]
fn a_failed_node_fails_pending_proposals() {
    let mut bench = Bench::leader("one-voter");
    bench.attempt(1, put(1, "a", "v"));
    bench.submit();
    bench.attempt(2, put(2, "b", "v"));
    bench.core.lifecycle = Lifecycle::Failed;
    assert_eq!(
        bench.status_changed(),
        vec![
            refused(1, Refusal::UnknownOutcome),
            refused(
                2,
                Refusal::Recovering {
                    lifecycle: NotReadyLifecycle::Failed
                }
            ),
        ],
        "rule: a node that fails answers its submitted attempts unknown_outcome and the rest \
         recovering (DM-7, A-5)"
    );
}

#[test]
fn shutdown_answers_unknown_outcome_if_submitted_and_unavailable_otherwise() {
    let mut bench = Bench::leader("one-voter");
    let submitted = put(1, "a", "v");
    bench.attempt(1, submitted.clone());
    bench.submit();
    bench.attempt(2, submitted);
    bench.attempt(3, put(1, "a", "other"));
    bench.attempt(4, put(2, "b", "v"));
    assert_eq!(
        bench.shutdown(),
        vec![
            refused(1, Refusal::UnknownOutcome),
            refused(2, Refusal::UnknownOutcome),
            refused(3, Refusal::Unavailable),
            refused(4, Refusal::Unavailable),
        ],
        "rule: at shutdown a submitted or attached attempt answers unknown_outcome, the \
         others unavailable (§7.8, §8.4)"
    );
    assert_eq!(
        bench.attempt(5, put(3, "c", "v")),
        vec![refused(5, Refusal::Unavailable)],
        "rule: a new attempt is unavailable once the node is stopping"
    );
}

#[test]
fn a_retry_after_a_withdrawn_proposal_is_handed_over_once() {
    // SunnyDog's review of W14.2: a queued owner's expiry left its request ID in the queue
    // of proposals, so the client's retry was handed to the library twice; had the library
    // dropped the second copy, the submitted attempt would have answered busy (A-5).
    let mut bench = Bench::leader("one-voter");
    let op = put(1, "k", "v");
    bench.attempt(1, op.clone());
    assert_eq!(
        bench.expire(1),
        vec![refused(1, Refusal::Busy)],
        "fixture: the queued owner expires, never proposed"
    );
    bench.attempt(2, op);
    let first = bench.next_proposal().map(|proposal| proposal.attempt);
    bench.proposed(2, Ok(()));
    assert_eq!(
        (first, bench.next_proposal()),
        (Some(AttemptId::new(2)), None),
        "rule: the retry's proposal is handed to the library once; a withdrawn proposal \
         leaves no trace in the queue of proposals"
    );
}

#[test]
fn a_waiter_behind_a_withdrawn_proposal_is_handed_over_once() {
    let mut bench = Bench::leader("one-voter");
    bench.attempt(1, put(1, "k", "a"));
    bench.attempt(2, put(1, "k", "b"));
    assert_eq!(
        bench.expire(1),
        vec![refused(1, Refusal::Busy)],
        "fixture: the queued owner expires; the waiter behind it enters the order again and \
         proposes its own command"
    );
    let first = bench.next_proposal().map(|proposal| proposal.attempt);
    bench.proposed(2, Ok(()));
    assert_eq!(
        (first, bench.next_proposal()),
        (Some(AttemptId::new(2)), None),
        "rule: the waiter's proposal is handed to the library once (A-5)"
    );
}

/// A PUT to key `k` with request ID `n` whose canonical command is exactly `len` bytes.
fn put_of_len(n: u8, len: usize) -> ClientOp {
    (1..=64)
        .map(|value_len| put(n, "k", &"v".repeat(value_len)))
        .find(|op| command(op).len() == len)
        .expect("fixture: a PUT of that command length")
}

#[test]
fn the_uncommitted_bound_is_exact_counts_submitted_proposals_and_reopens_on_apply() {
    // small-limits: max_uncommitted_bytes = 1,024; values of 1 to 64 bytes.
    let mut bench = Bench::leader("small-limits");
    let shortest = command(&put(1, "k", "v")).len();
    let longest = command(&put(1, "k", &"v".repeat(64))).len();
    let mut n = 0_u8;
    let mut in_flight = 0;
    let admit = |bench: &mut Bench, n: &mut u8, len: usize| {
        *n += 1;
        let op = put_of_len(*n, len);
        assert!(
            bench.attempt(u64::from(*n), op).is_empty(),
            "fixture: write {n} of {len} bytes is admitted"
        );
        len
    };
    while in_flight + longest + shortest <= 1_024 {
        in_flight += admit(&mut bench, &mut n, longest);
    }
    let room = 1_024 - in_flight;
    if room <= longest {
        in_flight += admit(&mut bench, &mut n, room);
    } else {
        in_flight += admit(&mut bench, &mut n, room - shortest);
        in_flight += admit(&mut bench, &mut n, shortest);
    }
    assert_eq!(
        (in_flight, bench.path.in_flight_bytes()),
        (1_024, 1_024),
        "fixture: the proposals in flight fill max_uncommitted_bytes exactly"
    );
    assert_eq!(
        bench.attempt(100, put_of_len(100, shortest)),
        vec![refused(100, Refusal::Busy)],
        "rule: at exactly max_uncommitted_bytes, one more byte is busy (§7.11)"
    );
    let mut submitted = Vec::new();
    while let Some(proposal) = bench.next_proposal() {
        bench.proposed(proposal.attempt.get(), Ok(()));
        submitted.push(proposal.command);
    }
    assert_eq!(
        bench.attempt(101, put_of_len(101, shortest)),
        vec![refused(101, Refusal::Busy)],
        "rule: submitted proposals count until they apply, not only queued ones"
    );
    bench.apply(submitted[0].clone());
    assert!(
        bench.attempt(102, put_of_len(102, shortest)).is_empty(),
        "rule: the bound reopens by the applied command's bytes"
    );
}

// ---------------------------------------------------------------------------------------
// DELETE and compare-and-swap (W27.3, C-5)
// ---------------------------------------------------------------------------------------

/// A write of `mutation` whose request ID encodes `n`, as [`put_id`]'s does.
fn write_id(n: u64, mutation: Mutation) -> ClientOp {
    let mut bytes = [9_u8; 16];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    ClientOp::Write {
        request_id: RequestId::from_bytes(bytes),
        mutation,
    }
}

fn delete_id(n: u64, k: &str) -> ClientOp {
    write_id(n, Mutation::Delete { key: key(k) })
}

fn cas_id(n: u64, k: &str, expected: ExpectedVersion, v: &str) -> ClientOp {
    write_id(
        n,
        Mutation::Cas {
            key: key(k),
            expected,
            value: value(v),
        },
    )
}

fn version(index: u64) -> Version {
    Version::at(LogIndex::new(index).unwrap()).unwrap()
}

/// The answer to `attempt`: the entry at `index` decided `decision`.
fn decided(
    attempt: u64,
    index: u64,
    decision: Decision,
    replayed: bool,
    served_by: ServedBy,
) -> Effect {
    respond(
        attempt,
        Response::Written {
            result: TerminalResult::executed_at(EntryIndex::new(index).unwrap(), decision),
            replayed,
            served_by,
        },
    )
}

/// The node `node` serving with `applied` entries applied.
fn serving(node: NodeId, applied: u64) -> ServedBy {
    ServedBy {
        node,
        applied: LogIndex::new(applied).unwrap(),
    }
}

#[test]
fn a_delete_and_a_compare_and_swap_are_proposed_as_sent_and_answered_by_their_apply() {
    let mut bench = Bench::leader("one-voter");
    let node = bench.node();
    let ops = [
        put_id(1, "k", "v"),
        delete_id(2, "k"),
        cas_id(3, "k", ExpectedVersion::Version(version(1)), "w"),
        cas_id(4, "k", ExpectedVersion::Absent, "x"),
        delete_id(5, "gone"),
    ];
    let mut proposed = Vec::new();
    let mut answers = Vec::new();
    for (n, op) in (1..).zip(&ops) {
        assert!(
            bench.attempt(n, op.clone()).is_empty(),
            "rule: an admitted write waits, whatever its kind; attempt {n}: {op:?}"
        );
        let proposal = bench.submit();
        proposed.push(proposal.command.clone());
        answers.extend(bench.apply(proposal.command));
    }
    assert_eq!(
        (proposed, answers),
        (
            ops.iter().map(command).collect::<Vec<_>>(),
            vec![
                decided(1, 1, Decision::Set, false, serving(node, 1)),
                decided(
                    2,
                    2,
                    Decision::Deleted { existed: true },
                    false,
                    serving(node, 2)
                ),
                decided(
                    3,
                    3,
                    Decision::VersionMismatch { current: None },
                    false,
                    serving(node, 3)
                ),
                decided(4, 4, Decision::Set, false, serving(node, 4)),
                decided(
                    5,
                    5,
                    Decision::Deleted { existed: false },
                    false,
                    serving(node, 5)
                ),
            ],
        ),
        "rule: a DELETE and a compare-and-swap take the PUT's path: each is proposed as its \
         canonical command, unchanged, and answered with the terminal result its entry's \
         application decided (§7.8, F-3, F-5)"
    );
}

#[test]
fn a_compare_and_swap_is_evaluated_when_its_entry_applies_never_at_admission() {
    // C-5: all four writes are admitted and proposed before any applies. At admission both
    // keys are absent, so the first CAS would mismatch and the second would match; each is
    // decided instead by the state when its entry applies, after the PUT ahead of it.
    let mut bench = Bench::leader("one-voter");
    let node = bench.node();
    let ops = [
        put_id(1, "k", "v"),
        cas_id(2, "k", ExpectedVersion::Version(version(1)), "w"),
        put_id(3, "j", "v"),
        cas_id(4, "j", ExpectedVersion::Absent, "w"),
    ];
    for (n, op) in (1..).zip(&ops) {
        assert!(
            bench.attempt(n, op.clone()).is_empty(),
            "rule: a write is admitted without evaluating it (C-5); attempt {n}: {op:?}"
        );
    }
    let proposals: Vec<Proposal> = ops.iter().map(|_| bench.submit()).collect();
    let answers: Vec<Effect> = proposals
        .iter()
        .flat_map(|proposal| bench.apply(proposal.command.clone()))
        .collect();
    assert_eq!(
        (
            proposals
                .iter()
                .map(|proposal| (proposal.attempt.get(), proposal.command.clone()))
                .collect::<Vec<_>>(),
            answers,
            bench
                .core
                .state
                .get(&key("k"))
                .map(|stored| (stored.value().clone(), stored.version())),
        ),
        (
            (1..).zip(ops.iter().map(command)).collect::<Vec<_>>(),
            vec![
                decided(1, 1, Decision::Set, false, serving(node, 1)),
                decided(2, 2, Decision::Set, false, serving(node, 2)),
                decided(3, 3, Decision::Set, false, serving(node, 3)),
                decided(
                    4,
                    4,
                    Decision::VersionMismatch {
                        current: Some(version(3))
                    },
                    false,
                    serving(node, 4)
                ),
            ],
            Some((value("w"), version(2))),
        ),
        "rule: COMPARE_SWAP is evaluated against the key's state when its entry applies, \
         never at admission (C-5): the CAS expecting the PUT's version matches although the \
         key was absent when it was admitted, and the CAS expecting absence mismatches \
         although it would have matched then"
    );
}

#[test]
fn retried_deletes_and_mismatches_are_answered_from_the_history() {
    let mut bench = Bench::leader("one-voter");
    let node = bench.node();
    let ops = [
        put_id(1, "k", "v"),
        cas_id(2, "k", ExpectedVersion::Absent, "w"),
        delete_id(3, "k"),
    ];
    for (n, op) in (1..).zip(&ops) {
        bench.attempt(n, op.clone());
        let proposal = bench.submit();
        bench.apply(proposal.command);
    }
    // The key is absent now: evaluated afresh, the CAS would match and the DELETE would
    // report existed = false.
    assert_eq!(
        (
            bench.attempt(12, ops[1].clone()),
            bench.attempt(13, ops[2].clone()),
            bench.next_proposal(),
        ),
        (
            vec![decided(
                12,
                2,
                Decision::VersionMismatch {
                    current: Some(version(1))
                },
                true,
                serving(node, 3)
            )],
            vec![decided(
                13,
                3,
                Decision::Deleted { existed: true },
                true,
                serving(node, 3)
            )],
            None,
        ),
        "rule: a retried DELETE or compare-and-swap is answered from the history with its \
         original terminal result, replayed, and nothing is proposed again (F-6, DM-5)"
    );
}

// ---------------------------------------------------------------------------------------
// End to end, through the controller
// ---------------------------------------------------------------------------------------

#[test]
fn a_lone_leader_commits_a_put_and_answers_retries_from_its_history() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut member = Member::fresh_writes(&config, id);
    member.elect(100);
    let op = put(1, "k", "v");
    member.step(Input::Client {
        attempt: AttemptId::new(1),
        op: op.clone(),
    });
    while member.complete_sync() {}
    let index = member
        .ctl
        .state()
        .history()
        .get(&request(1))
        .unwrap()
        .first_index()
        .get();
    let served = ServedBy {
        node: id,
        applied: member.ctl.state().applied_index(),
    };
    let retry = member.step(Input::Client {
        attempt: AttemptId::new(2),
        op,
    });
    let misuse = member.step(Input::Client {
        attempt: AttemptId::new(3),
        op: put(1, "k", "other"),
    });
    assert_eq!(
        (
            member
                .effects
                .iter()
                .find(|effect| is_response_to(effect, 1))
                .cloned(),
            retry,
            misuse,
            member
                .ctl
                .state()
                .get(&key("k"))
                .map(|stored| stored.value().clone()),
        ),
        (
            Some(written(1, index, false, served)),
            vec![written(2, index, true, served)],
            vec![conflict(3, index)],
            Some(value("v")),
        ),
        "rule: the leader proposes the PUT and answers it when it applies; a retry with the \
         same command replays the result, another command is request_id_conflict (F-1, F-6)"
    );
}

#[test]
fn a_lone_leader_commits_a_delete_and_a_compare_and_swap() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut member = Member::fresh_writes(&config, id);
    member.elect(100);
    let ops = [
        put_id(1, "k", "v"),
        cas_id(2, "k", ExpectedVersion::Absent, "w"),
        delete_id(3, "k"),
    ];
    for (n, op) in (1..).zip(&ops) {
        member.step(Input::Client {
            attempt: AttemptId::new(n),
            op: op.clone(),
        });
        while member.complete_sync() {}
    }
    let index = |op: &ClientOp| {
        let ClientOp::Write { request_id, .. } = op else {
            panic!("fixture: a write");
        };
        member
            .ctl
            .state()
            .history()
            .get(request_id)
            .unwrap()
            .first_index()
            .get()
    };
    let [put_at, cas_at, delete_at] = [index(&ops[0]), index(&ops[1]), index(&ops[2])];
    let answers: Vec<Option<Effect>> = (1..=3)
        .map(|n| {
            member
                .effects
                .iter()
                .find(|effect| is_response_to(effect, n))
                .cloned()
        })
        .collect();
    assert_eq!(
        (answers, member.ctl.state().get(&key("k")).is_none()),
        (
            vec![
                Some(decided(
                    1,
                    put_at,
                    Decision::Set,
                    false,
                    serving(id, put_at)
                )),
                Some(decided(
                    2,
                    cas_at,
                    Decision::VersionMismatch {
                        current: Some(version(put_at))
                    },
                    false,
                    serving(id, cas_at)
                )),
                Some(decided(
                    3,
                    delete_at,
                    Decision::Deleted { existed: true },
                    false,
                    serving(id, delete_at)
                )),
            ],
            true,
        ),
        "rule: through the controller, a lone leader proposes a compare-and-swap and a DELETE \
         like a PUT and answers each with the result its entry's application decided: a \
         mismatch naming the live version, then existed = true (F-3, F-5, C-5)"
    );
}

fn is_response_to(effect: &Effect, attempt: u64) -> bool {
    match effect {
        Effect::Respond { attempt: to, .. } => to.get() == attempt,
        Effect::Append { .. }
        | Effect::Truncate { .. }
        | Effect::Sync { .. }
        | Effect::CreateJournal
        | Effect::SyncDir { .. }
        | Effect::Send { .. }
        | Effect::Log(_)
        | Effect::Observe(_) => false,
    }
}

#[test]
fn replays_are_answered_while_a_sync_is_outstanding_and_new_writes_queue_behind_it() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut member = Member::fresh_writes(&config, id);
    member.elect(100);
    let done = put(1, "a", "v");
    member.step(Input::Client {
        attempt: AttemptId::new(1),
        op: done.clone(),
    });
    while member.complete_sync() {}
    member.step(Input::Client {
        attempt: AttemptId::new(2),
        op: put(2, "b", "v"),
    });
    assert!(
        member.ctl.outstanding_sync().is_some(),
        "fixture: the second write's entry waits for its sync"
    );
    let replay = member.step(Input::Client {
        attempt: AttemptId::new(3),
        op: done,
    });
    let queued = member.step(Input::Client {
        attempt: AttemptId::new(4),
        op: put(3, "c", "v"),
    });
    assert!(
        replay.iter().any(|effect| is_response_to(effect, 3)) && queued.is_empty(),
        "rule: a short-circuit is answered while the sync is outstanding; a new write queues \
         behind it (F-7, §7.4 step 3); replay={replay:?}"
    );
    while member.complete_sync() {}
    assert!(
        member
            .effects
            .iter()
            .any(|effect| is_response_to(effect, 4)),
        "rule: the queued write is proposed and answered once the sync completed"
    );
}

#[test]
fn a_write_whose_commit_is_not_observed_by_its_deadline_is_unknown_outcome() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut member = Member::fresh_writes(&config, id);
    member.elect(100);
    member.step(Input::Client {
        attempt: AttemptId::new(1),
        op: put(1, "k", "v"),
    });
    assert!(
        member.ctl.outstanding_sync().is_some(),
        "fixture: the write is submitted and its sync never completes"
    );
    let mut answered = Vec::new();
    for _ in 0..config.write_deadline().get() {
        answered.extend(member.step(Input::Tick));
    }
    assert_eq!(
        answered
            .into_iter()
            .filter(|effect| is_response_to(effect, 1))
            .collect::<Vec<_>>(),
        vec![refused(1, Refusal::UnknownOutcome)],
        "rule: a submitted write whose commit is not observed before write_deadline_ticks \
         answers unknown_outcome, even while its sync is outstanding (§7.8, §7.12, A-5)"
    );
}

#[test]
fn a_three_voter_follower_redirects_to_the_leader() {
    let config = config("three-voters");
    let ids: Vec<NodeId> = config.voters().collect();
    let mut members: Vec<Member<crate::test_support::Writes>> = ids
        .iter()
        .map(|id| Member::fresh_writes(&config, *id))
        .collect();
    let before = members[0].step(Input::Client {
        attempt: AttemptId::new(1),
        op: put(1, "k", "v"),
    });
    assert_eq!(
        before,
        vec![refused(1, Refusal::Unavailable)],
        "rule: with no leader known, a write is unavailable, not submitted (F-7)"
    );
    let mut leader = None;
    for _ in 0..300 {
        for member in &mut members {
            member.step(Input::Tick);
            while member.complete_sync() {}
        }
        for from in 0..members.len() {
            let outbox: Vec<(NodeId, Vec<u8>)> = members[from].outbox.drain(..).collect();
            for (to, bytes) in outbox {
                let sender = members[from].id;
                if let Some(target) = members.iter_mut().find(|member| member.id == to) {
                    target.step(Input::Peer {
                        from: sender,
                        bytes,
                    });
                    while target.complete_sync() {}
                }
            }
        }
        leader = members
            .iter()
            .find(|member| member.ctl.status().role == Role::Leader)
            .map(|member| member.id);
        if leader.is_some()
            && members
                .iter()
                .all(|member| member.ctl.status().leader == leader)
        {
            break;
        }
    }
    let leader = leader.expect("fixture: a leader within 300 rounds");
    let follower = members
        .iter_mut()
        .find(|member| member.id != leader)
        .unwrap();
    assert_eq!(
        follower.step(Input::Client {
            attempt: AttemptId::new(2),
            op: put(2, "k", "v"),
        }),
        vec![refused(2, Refusal::NotLeader { leader })],
        "rule: a follower answers a new write not_leader, naming the leader it believes in \
         (F-7, §8.4)"
    );
}
