//! K-12, unknown-outcome honesty (PLAN.md §10.5; A-5, P8, F-10): what a node tells a client
//! about a write it may already have submitted is never a guess.
//!
//! An attempt is **submitted** once its `attempt` records show `submitted`, or `attached` (it
//! waits on a same-ID same-digest attempt whose command is in flight). `dropped` (a synchronous
//! `ProposalDropped`, nothing appended) undoes the `submitted` before it, and `short_circuit` is
//! no submission. The judge reads
//! the client-side records in order and checks, for a write's responses:
//!
//! - after submission the only honest answers are a terminal decision (`ok`, `version_mismatch`
//!   or `capacity_exceeded`) and `unknown_outcome`; a failure that says nothing happened
//!   (`not_leader`, `busy`, `unavailable`, `recovering` or `history_full`) is a guess
//!   ([`Kind::GuessedFailureAfterSubmission`], the evidence of B-15);
//! - `attempt_state` is `not_submitted` only for an attempt never submitted, and `submitted`
//!   only for one that was ([`Kind::WrongAttemptState`]);
//! - `unknown_outcome` names an attempt that was submitted
//!   ([`Kind::UnknownOutcomeWithoutSubmission`]);
//! - an attempt gets at most one response ([`Kind::SecondResponse`]);
//! - a `status` answer is `known` with a result or `unknown` without one
//!   ([`Kind::StatusContradictsItself`]).
//!
//! And for each logical write's resolution:
//!
//! - every logical write has exactly one resolution ([`Kind::WriteNotResolved`],
//!   [`Kind::WriteResolvedTwice`]); `unresolved` is an honest resolution, and whether a correct
//!   run may leave one is K-9's question;
//! - a write resolved `not_executed` or `refused` has no stored result and no terminal decision
//!   anywhere in the run, and `refused` has no submitted attempt
//!   ([`Kind::AbsenceContradicted`]);
//! - a write resolved `not_executed` that had a submitted attempt rests on evidence, and every
//!   evidence attempt is a `status` lookup of that write that answered `unknown`
//!   ([`Kind::AbsenceWithoutEvidence`]): an unknown status alone never proves absence, only the
//!   lookups after a certified drain do.
//!
//! Limits: this judges what nodes said, not what is true: whether an `ok` was earned is K-1 and
//! K-2. It does not check that the evidence lookups follow the drain or cover every recoverable
//! node: that is the drain rule (K-9). A `dropped` record is trusted as the node's own word. The unit examined is one write response, status answer or resolution.

use std::collections::{BTreeMap, BTreeSet};

use crate::report::{CheckError, Kind, Report, Violation};
use crate::trace::{Answer, AttemptState, Item, Located, Resolved, Trace};

/// The most violations a report lists (CD-9): the judge is an offline oracle whose other state
/// is bounded by the size of the document it reads, so only the report needs a cap.
const MAX_VIOLATIONS: usize = 256;

/// Outcome codes that say a write did not happen and was never submitted.
const NOT_SUBMITTED_CODES: [&str; 5] = [
    "not_leader",
    "busy",
    "unavailable",
    "recovering",
    "history_full",
];

/// Outcome codes that decide a write's execution.
const DECISIONS: [&str; 3] = ["ok", "version_mismatch", "capacity_exceeded"];

/// Judges `trace` against K-12.
///
/// # Errors
///
/// Never today: every shape the judge relies on is checked while the [`Trace`] is parsed. The
/// result type stays so the judge can reject a stream the way K-8 does.
pub fn k12(trace: &Trace) -> Result<Report, CheckError> {
    let mut judge = Judge::default();
    for located in &trace.items {
        judge.observe(located);
    }
    judge.finish();
    Ok(Report {
        checker: "K-12",
        examined: judge.examined,
        violations: judge.violations,
    })
}

/// What one logical write's resolution rests on.
struct Settled {
    at: String,
    resolved: Resolved,
    evidence: Vec<u64>,
}

#[derive(Default)]
struct Judge {
    /// The logical write each attempt belongs to, or a status lookup asks about.
    requests: BTreeMap<u64, Option<String>>,
    /// The attempts that invoked a write.
    writes: BTreeSet<u64>,
    submitted: BTreeSet<u64>,
    answered: BTreeSet<u64>,
    /// The status lookups: whether each answered `known`.
    lookups: BTreeMap<u64, bool>,
    /// The requests with a terminal decision, stored or fresh.
    decided: BTreeSet<String>,
    resolutions: BTreeMap<String, Vec<Settled>>,
    violations: Vec<Violation>,
    examined: u64,
}

impl Judge {
    fn flag(&mut self, kind: Kind, at: &str, attempt: Option<u64>, detail: impl Into<String>) {
        if self.violations.len() >= MAX_VIOLATIONS {
            return;
        }
        let request_id = attempt
            .and_then(|attempt| self.requests.get(&attempt))
            .and_then(Clone::clone)
            .unwrap_or_default();
        self.violations.push(Violation {
            kind,
            request_id,
            attempt,
            detail: format!("{} [{at}]", detail.into()),
        });
    }

    fn observe(&mut self, located: &Located) {
        let at = located.at.as_str();
        match &located.item {
            Item::Invoke {
                attempt,
                request_id,
                write,
            } => {
                self.requests.insert(*attempt, request_id.clone());
                if *write {
                    self.writes.insert(*attempt);
                }
            }
            Item::Attempt { attempt, state } => match state {
                AttemptState::Submitted | AttemptState::Attached => {
                    self.submitted.insert(*attempt);
                }
                // The proposal was handed over and refused at once, with nothing appended: the
                // `submitted` before it is retracted.
                AttemptState::Dropped => {
                    self.submitted.remove(attempt);
                }
                AttemptState::Admitted | AttemptState::ShortCircuit | AttemptState::Terminal => {}
            },
            Item::Response { attempt, answer } => self.response(*attempt, answer, at),
            Item::Resolution {
                request_id,
                resolved,
                evidence,
            } => self.resolution(request_id, *resolved, evidence, at),
        }
    }

    fn response(&mut self, attempt: u64, answer: &Answer, at: &str) {
        if matches!(answer, Answer::Other) {
            return;
        }
        self.examined += 1;
        if !self.answered.insert(attempt) {
            self.flag(
                Kind::SecondResponse,
                at,
                Some(attempt),
                format!(
                    "rule: an attempt gets at most one response; attempt {attempt} answered again"
                ),
            );
        }
        let submitted = self.submitted.contains(&attempt);
        match answer {
            Answer::Write {
                code,
                attempt_state,
            } => self.write_response(attempt, (code, attempt_state.as_deref()), submitted, at),
            Answer::Status { known, has_result } => {
                self.lookups.insert(attempt, *known);
                if known != has_result {
                    self.flag(
                        Kind::StatusContradictsItself,
                        at,
                        Some(attempt),
                        format!(
                            "rule: a status answer is known with a result or unknown without; \
                             known={known} result_present={has_result}"
                        ),
                    );
                }
                if *known && let Some(Some(id)) = self.requests.get(&attempt) {
                    let id = id.clone();
                    self.decided.insert(id);
                }
            }
            Answer::Other => {}
        }
    }

    fn write_response(
        &mut self,
        attempt: u64,
        (code, attempt_state): (&str, Option<&str>),
        submitted: bool,
        at: &str,
    ) {
        let claimed_not_submitted = attempt_state == Some("not_submitted");
        let claimed_submitted = attempt_state == Some("submitted");
        if claimed_not_submitted && submitted {
            self.flag(
                Kind::WrongAttemptState,
                at,
                Some(attempt),
                format!(
                    "rule: not_submitted names only an attempt never submitted (A-5); \
                     attempt {attempt} was submitted, code={code}"
                ),
            );
        }
        if claimed_submitted && !submitted {
            self.flag(
                Kind::WrongAttemptState,
                at,
                Some(attempt),
                format!(
                    "rule: submitted names only an attempt that was submitted; \
                     attempt {attempt} never was, code={code}"
                ),
            );
        }
        if code == "unknown_outcome" && !submitted {
            self.flag(
                Kind::UnknownOutcomeWithoutSubmission,
                at,
                Some(attempt),
                format!("rule: unknown_outcome follows a submission; attempt {attempt} never was submitted"),
            );
        }
        if submitted && NOT_SUBMITTED_CODES.contains(&code) {
            self.flag(
                Kind::GuessedFailureAfterSubmission,
                at,
                Some(attempt),
                format!(
                    "rule: after submission the answer is a decision or unknown_outcome (A-5); \
                     attempt {attempt} was answered {code}"
                ),
            );
        }
        if DECISIONS.contains(&code)
            && let Some(Some(id)) = self.requests.get(&attempt)
        {
            let id = id.clone();
            self.decided.insert(id);
        }
    }

    fn resolution(&mut self, request_id: &str, resolved: Resolved, evidence: &[u64], at: &str) {
        self.examined += 1;
        self.resolutions
            .entry(request_id.to_owned())
            .or_default()
            .push(Settled {
                at: at.to_owned(),
                resolved,
                evidence: evidence.to_vec(),
            });
    }

    /// Judges the resolutions, once every attempt's records and answers are in.
    fn finish(&mut self) {
        let writes: BTreeSet<String> = self
            .writes
            .iter()
            .filter_map(|attempt| self.requests.get(attempt).and_then(Clone::clone))
            .collect();
        for request_id in &writes {
            let count = self.resolutions.get(request_id).map_or(0, Vec::len);
            if count == 0 {
                self.flag(
                    Kind::WriteNotResolved,
                    "end of run",
                    None,
                    format!("rule: every logical write has one resolution; {request_id} has none"),
                );
            }
        }
        let resolutions = std::mem::take(&mut self.resolutions);
        for (request_id, settled) in &resolutions {
            if settled.len() > 1 {
                let at = settled.last().map_or("", |s| s.at.as_str());
                self.flag(
                    Kind::WriteResolvedTwice,
                    at,
                    None,
                    format!(
                        "rule: every logical write has one resolution; {request_id} has {}",
                        settled.len()
                    ),
                );
            }
            for one in settled {
                self.judge_absence(request_id, one);
            }
        }
    }

    fn judge_absence(&mut self, request_id: &str, settled: &Settled) {
        let absent = match settled.resolved {
            Resolved::NotExecuted | Resolved::Refused => true,
            Resolved::Executed | Resolved::Unresolved => false,
        };
        if !absent {
            return;
        }
        let had_submitted = self.writes.iter().any(|attempt| {
            self.submitted.contains(attempt)
                && self.requests.get(attempt).and_then(Option::as_deref) == Some(request_id)
        });
        let at = settled.at.as_str();
        if self.decided.contains(request_id) {
            self.flag(
                Kind::AbsenceContradicted,
                at,
                None,
                format!(
                    "rule: a write resolved absent has no decision or stored result; \
                     {request_id} resolved {:?} but a response decided it",
                    settled.resolved
                ),
            );
        }
        if settled.resolved == Resolved::Refused && had_submitted {
            self.flag(
                Kind::AbsenceContradicted,
                at,
                None,
                format!("rule: a refused write was never submitted; {request_id} has a submitted attempt"),
            );
        }
        if settled.resolved == Resolved::NotExecuted && had_submitted {
            let all_unknown = !settled.evidence.is_empty()
                && settled.evidence.iter().all(|attempt| {
                    // A lookup of this very write, not of another one.
                    self.lookups.get(attempt) == Some(&false)
                        && self.requests.get(attempt).and_then(Option::as_deref) == Some(request_id)
                });
            if !all_unknown {
                self.flag(
                    Kind::AbsenceWithoutEvidence,
                    at,
                    None,
                    format!(
                        "rule: not_executed after a submission rests on unknown status lookups only; \
                         {request_id} evidence={:?}",
                        settled.evidence
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // check: K-12
    use super::k12;
    use crate::report::{CheckError, Kind, Report};
    use crate::trace::{Answer, AttemptState, Item, Located, Resolved, Trace};

    const W1: &str = "000000000000000000000000000000a1";
    const W2: &str = "000000000000000000000000000000a2";

    /// A trace under construction.
    struct Script(Vec<Located>);

    impl Script {
        fn new() -> Self {
            Self(Vec::new())
        }

        fn push(&mut self, item: Item) -> &mut Self {
            let at = format!("records[{}]", self.0.len());
            self.0.push(Located { at, item });
            self
        }

        fn write(&mut self, attempt: u64, request_id: &str) -> &mut Self {
            self.push(Item::Invoke {
                attempt,
                request_id: Some(request_id.to_owned()),
                write: true,
            })
        }

        fn lookup(&mut self, attempt: u64, request_id: &str) -> &mut Self {
            self.push(Item::Invoke {
                attempt,
                request_id: Some(request_id.to_owned()),
                write: false,
            })
        }

        fn state(&mut self, attempt: u64, state: AttemptState) -> &mut Self {
            self.push(Item::Attempt { attempt, state })
        }

        fn submitted(&mut self, attempt: u64) -> &mut Self {
            self.state(attempt, AttemptState::Admitted)
                .state(attempt, AttemptState::Submitted)
        }

        fn answer(&mut self, attempt: u64, code: &str, attempt_state: Option<&str>) -> &mut Self {
            self.push(Item::Response {
                attempt,
                answer: Answer::Write {
                    code: code.to_owned(),
                    attempt_state: attempt_state.map(str::to_owned),
                },
            })
        }

        fn status(&mut self, attempt: u64, known: bool, has_result: bool) -> &mut Self {
            self.push(Item::Response {
                attempt,
                answer: Answer::Status { known, has_result },
            })
        }

        fn resolve(&mut self, request_id: &str, resolved: Resolved, evidence: &[u64]) -> &mut Self {
            self.push(Item::Resolution {
                request_id: request_id.to_owned(),
                resolved,
                evidence: evidence.to_vec(),
            })
        }

        fn trace(&self) -> Trace {
            Trace {
                items: self.0.clone(),
            }
        }
    }

    fn judged(script: &Script) -> Report {
        k12(&script.trace()).unwrap()
    }

    fn expect(script: &Script, kind: Kind) {
        let report = judged(script);
        assert!(
            report.violations.iter().any(|v| v.kind == kind),
            "rule: K-12 flags {kind}; report={report:#?}"
        );
    }

    fn expect_pass(script: &Script, what: &str) {
        let report = judged(script);
        assert!(
            report.passed(),
            "rule: K-12 passes {what}; report={report:#?}"
        );
    }

    #[test]
    fn a_submitted_write_answered_unknown_outcome_passes() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        script.resolve(W1, Resolved::Unresolved, &[1]);
        let report = judged(&script);
        assert!(
            report.passed(),
            "rule: K-12 passes an honest unknown_outcome; report={report:#?}"
        );
        assert_eq!(report.examined, 2, "one response and one resolution judged");
    }

    #[test]
    fn a_submitted_write_answered_with_a_decision_passes() {
        for code in ["ok", "version_mismatch", "capacity_exceeded"] {
            let mut script = Script::new();
            script.write(1, W1).submitted(1);
            script.answer(1, code, None);
            script.resolve(W1, Resolved::Executed, &[1]);
            expect_pass(&script, &format!("a submitted write decided {code}"));
        }
    }

    #[test]
    fn a_never_submitted_write_may_say_not_submitted() {
        for (code, state) in [
            (AttemptState::Dropped, "not_leader"),
            (AttemptState::Admitted, "busy"),
            (AttemptState::Admitted, "history_full"),
        ] {
            let mut script = Script::new();
            script
                .write(1, W1)
                .state(1, AttemptState::Admitted)
                .state(1, code);
            script.answer(1, state, Some("not_submitted"));
            script.resolve(W1, Resolved::Refused, &[1]);
            expect_pass(&script, &format!("{state} for an attempt never submitted"));
        }
    }

    #[test]
    fn flags_not_submitted_on_a_submitted_attempt_b15() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("not_submitted"));
        script.resolve(W1, Resolved::Unresolved, &[1]);
        expect(&script, Kind::WrongAttemptState);
    }

    #[test]
    fn flags_a_guessed_failure_after_submission_b15() {
        for code in [
            "not_leader",
            "busy",
            "unavailable",
            "recovering",
            "history_full",
        ] {
            let mut script = Script::new();
            script.write(1, W1).submitted(1);
            script.answer(1, code, None);
            script.resolve(W1, Resolved::Unresolved, &[1]);
            expect(&script, Kind::GuessedFailureAfterSubmission);
        }
    }

    #[test]
    fn an_attached_attempt_counts_as_submitted() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.write(2, W1).state(2, AttemptState::Attached);
        script.answer(2, "unavailable", Some("not_submitted"));
        script.resolve(W1, Resolved::Unresolved, &[1, 2]);
        expect(&script, Kind::GuessedFailureAfterSubmission);
        expect(&script, Kind::WrongAttemptState);
    }

    #[test]
    fn a_dropped_attempt_counts_as_never_submitted() {
        let mut script = Script::new();
        script
            .write(1, W1)
            .state(1, AttemptState::Admitted)
            .state(1, AttemptState::Dropped);
        script.answer(1, "unavailable", Some("not_submitted"));
        script.resolve(W1, Resolved::Refused, &[1]);
        expect_pass(&script, "a dropped proposal answered not_submitted");
    }

    #[test]
    fn a_proposal_dropped_at_once_retracts_its_submission() {
        let mut script = Script::new();
        script
            .write(1, W1)
            .submitted(1)
            .state(1, AttemptState::Dropped);
        script.answer(1, "busy", Some("not_submitted"));
        script.resolve(W1, Resolved::Refused, &[1]);
        expect_pass(&script, "submitted then dropped, answered not_submitted");
    }

    #[test]
    fn flags_submitted_on_a_never_submitted_attempt() {
        let mut script = Script::new();
        script.write(1, W1).state(1, AttemptState::Admitted);
        script.answer(1, "ok", Some("submitted"));
        script.resolve(W1, Resolved::Executed, &[1]);
        expect(&script, Kind::WrongAttemptState);
    }

    #[test]
    fn flags_unknown_outcome_for_an_attempt_never_submitted() {
        let mut script = Script::new();
        script.write(1, W1).state(1, AttemptState::Admitted);
        script.answer(1, "unknown_outcome", None);
        script.resolve(W1, Resolved::Unresolved, &[1]);
        expect(&script, Kind::UnknownOutcomeWithoutSubmission);
    }

    #[test]
    fn flags_a_second_response_to_one_attempt() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        script.answer(1, "ok", None);
        script.resolve(W1, Resolved::Executed, &[1]);
        expect(&script, Kind::SecondResponse);
    }

    #[test]
    fn flags_a_status_answer_that_contradicts_itself() {
        for (known, has_result) in [(true, false), (false, true)] {
            let mut script = Script::new();
            script.lookup(1, W1);
            script.status(1, known, has_result);
            expect(&script, Kind::StatusContradictsItself);
        }
    }

    #[test]
    fn flags_a_write_with_no_resolution_and_one_with_two() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        expect(&script, Kind::WriteNotResolved);
        script.resolve(W1, Resolved::Unresolved, &[1]);
        script.resolve(W1, Resolved::Unresolved, &[1]);
        expect(&script, Kind::WriteResolvedTwice);
    }

    #[test]
    fn unresolved_is_an_honest_resolution() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.resolve(W1, Resolved::Unresolved, &[]);
        expect_pass(&script, "a write the run reports unresolved");
    }

    #[test]
    fn flags_not_executed_for_a_write_that_has_a_stored_result() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        script.lookup(2, W1);
        script.status(2, true, true);
        script.lookup(3, W1);
        script.status(3, false, false);
        script.resolve(W1, Resolved::NotExecuted, &[3]);
        expect(&script, Kind::AbsenceContradicted);
    }

    #[test]
    fn flags_not_executed_for_a_write_that_was_decided() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "ok", None);
        script.lookup(2, W1);
        script.status(2, false, false);
        script.resolve(W1, Resolved::NotExecuted, &[2]);
        expect(&script, Kind::AbsenceContradicted);
    }

    #[test]
    fn flags_refused_for_a_write_with_a_submitted_attempt() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        script.resolve(W1, Resolved::Refused, &[1]);
        expect(&script, Kind::AbsenceContradicted);
    }

    #[test]
    fn not_executed_after_a_submission_needs_unknown_lookups_only() {
        let mut bare = Script::new();
        bare.write(1, W1).submitted(1);
        bare.answer(1, "unknown_outcome", Some("submitted"));
        bare.resolve(W1, Resolved::NotExecuted, &[]);
        expect(&bare, Kind::AbsenceWithoutEvidence);

        let mut by_write = Script::new();
        by_write.write(1, W1).submitted(1);
        by_write.answer(1, "unknown_outcome", Some("submitted"));
        by_write.resolve(W1, Resolved::NotExecuted, &[1]);
        expect(&by_write, Kind::AbsenceWithoutEvidence);

        let mut proven = Script::new();
        proven.write(1, W1).submitted(1);
        proven.answer(1, "unknown_outcome", Some("submitted"));
        proven.lookup(2, W1);
        proven.status(2, false, false);
        proven.lookup(3, W1);
        proven.status(3, false, false);
        proven.resolve(W1, Resolved::NotExecuted, &[2, 3]);
        expect_pass(
            &proven,
            "not_executed proven by unknown lookups on every node",
        );
    }

    #[test]
    fn not_executed_evidence_must_look_up_the_same_write() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "unknown_outcome", Some("submitted"));
        script.lookup(2, W2);
        script.status(2, false, false);
        script.resolve(W1, Resolved::NotExecuted, &[2]);
        expect(&script, Kind::AbsenceWithoutEvidence);
    }

    #[test]
    fn writes_are_judged_one_by_one() {
        let mut script = Script::new();
        script.write(1, W1).submitted(1);
        script.answer(1, "ok", None);
        script.resolve(W1, Resolved::Executed, &[1]);
        script.write(2, W2).submitted(2);
        script.answer(2, "unavailable", None);
        script.resolve(W2, Resolved::Unresolved, &[2]);
        let report = judged(&script);
        assert_eq!(
            report
                .violations
                .iter()
                .map(|v| v.attempt)
                .collect::<Vec<_>>(),
            vec![Some(2)],
            "rule: only attempt 2's guess is flagged; report={report:#?}"
        );
        assert_eq!(
            report.violations[0].request_id, W2,
            "the violation names its write"
        );
    }

    #[test]
    fn passes_the_run_fixture() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let report = k12(&Trace::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-12 passes run.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-12 examined something");
    }

    #[test]
    fn passes_the_drain_fixture() {
        let text = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
        let report = k12(&Trace::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-12 passes drain.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-12 examined something");
    }

    #[test]
    fn fails_a_fixture_whose_unknown_outcome_turned_into_a_guess() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        let outcome = doc["records"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["kind"] == "response" && r["outcome"]["code"] == "unknown_outcome")
            .map(|r| &mut r["outcome"])
            .unwrap();
        outcome["code"] = serde_json::json!("unavailable");
        outcome["attempt_state"] = serde_json::json!("not_submitted");
        let report = k12(&Trace::from_value(&doc).unwrap()).unwrap();
        assert!(
            report.has(Kind::GuessedFailureAfterSubmission) && report.has(Kind::WrongAttemptState),
            "rule: K-12 flags an unknown_outcome rewritten as not_submitted in a mutated \
             run.v1 (B-15); report={report:#?}"
        );
    }

    #[test]
    fn fails_a_fixture_whose_unknown_lookup_was_made_known() {
        let text = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        for record in doc["records"].as_array_mut().unwrap() {
            if record["kind"] == "response"
                && record["op"] == "status"
                && record["outcome"]["state"] == "unknown"
            {
                record["outcome"]["state"] = serde_json::json!("known");
            }
        }
        let report = k12(&Trace::from_value(&doc).unwrap()).unwrap();
        assert!(
            report.has(Kind::StatusContradictsItself) && report.has(Kind::AbsenceWithoutEvidence),
            "rule: K-12 flags a not_executed whose lookups were not all unknown in a mutated \
             drain.v1; report={report:#?}"
        );
    }

    #[test]
    fn a_document_with_an_unknown_attempt_state_is_malformed() {
        let doc = serde_json::json!({"records": [{
            "kind": "attempt", "seq": 1, "time": 0, "attempt": 1,
            "at": {"node": 1, "incarnation": 0}, "state": "sent", "attached_to": null
        }]});
        let error = Trace::from_value(&doc).unwrap_err();
        assert!(
            matches!(error, CheckError::Malformed { .. }),
            "rule: an unknown attempt state is malformed, never skipped; error={error:?}"
        );
    }
}
