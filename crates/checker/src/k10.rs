//! K-10, the black-box linearizability witness (PLAN.md §10.5; C-1, C-5): the judge that
//! trusts nothing a node says about order and nothing the harness saw inside the cluster.
//!
//! It reads only what clients observed: `invoke`, `response` as delivered, `delivery` and
//! `resolution` records. It never reads `committed`, `append` or any other god-view record, and
//! never calls [`History::first_executions`]. The steps, over **logical writes** (the non-misuse
//! attempts of one request ID collapsed into one; misuse attempts and their
//! `request_id_conflict` answers are not executions and are ignored):
//!
//! 1. The run must be complete and every logical write resolved. An `unresolved` or missing
//!    resolution, an overflow, an exhausted event budget or an invalid scenario is
//!    [`Kind::HistoryIncomplete`]: the run fails, because inconclusive is never a pass (P10).
//! 2. Each write's candidate position in the order is its reported `index`: the first delivered
//!    terminal answer's (its own or a known `status` answer), or, for a write no client saw
//!    terminal, its `executed` resolution's. Indices are untrusted: later answers that disagree
//!    ([`Kind::ObservationsDisagree`]), a resolution that contradicts the clients
//!    ([`Kind::ResolutionContradicted`]), two writes at one index ([`Kind::IndexCollision`]) and
//!    an index beyond the configured log capacity ([`Kind::IndexOutOfRange`]) are violations.
//! 3. The reference model runs the commands the clients invoked in that order. Indices no write
//!    claims are no-ops or no-effect entries and apply as empty entries, so the model's version
//!    (the execution index, DM-3) and every decision are its own; each write's reported decision
//!    is compared with it (the kinds of K-4).
//! 4. Each write is placed within its interval, from its first invocation to the first
//!    client-observed terminal decision of the request ID, by a greedy earliest-placement pass in
//!    index order: a point is the later of the previous write's point and this write's
//!    invocation, and fails ([`Kind::NoLinearization`]) if it falls after the interval closes.
//!    An unknown outcome does not close an interval, and a later replay is an observation of the
//!    same write, not a new one. The pass is linear in the writes (the gap fill is bounded by
//!    the configured log capacity), so it runs in polynomial time.
//!
//! Positions are record positions, the document's event order. The unit examined is one
//! logical write.

use std::collections::BTreeMap;

use model::{Command, Decision, Entry, Index, Model, RequestId};
use serde_json::{Value, json};

use crate::history::{
    Claim, History, array_field, bool_field, command_of, field, opt_u64, str_field, u64_field,
};
use crate::k4::differs;
use crate::report::{CheckError, Kind, Report, Violation};
use crate::trace::Resolved;

/// Judges `doc`, a parsed `keyval.observation/v1` document, against K-10.
///
/// # Errors
///
/// [`CheckError::Malformed`] if a field the witness reads is absent or wrong, or
/// [`CheckError::Replay`] if an index cannot be formed.
pub fn k10(doc: &Value) -> Result<Report, CheckError> {
    let history = History::from_value(doc)?;
    let mut violations = Vec::new();
    run_incomplete(doc, &mut violations)?;
    let manifest = field(doc, "", "manifest")?;
    let capacity = u64_field(
        field(manifest, "manifest", "limits")?,
        "manifest.limits",
        "max_log_entries",
    )?;
    let Records {
        writes,
        resolutions,
    } = read_records(doc)?;
    let observed = observations(&history);

    let mut order: BTreeMap<u64, Placed> = BTreeMap::new();
    for (id, write) in &writes {
        let seen = observed.get(id).map_or(&[][..], Vec::as_slice);
        let Some(candidate) = candidate(*id, seen, resolutions.get(id), &mut violations) else {
            continue;
        };
        let placed = Placed {
            id: *id,
            command: write.command.clone(),
            invoked_at: write.invoked_at,
            closed_at: seen.first().map(|(position, _)| *position),
            attempt: seen.first().map(|(_, claim)| claim.attempt),
            decision: seen.first().map(|(_, claim)| claim.result.decision),
        };
        if candidate > capacity {
            violations.push(violation(
                Kind::IndexOutOfRange,
                *id,
                placed.attempt,
                format!(
                    "rule: K-10 an index lies within the log capacity; write {id} reports \
                     index {candidate}, beyond max_log_entries {capacity}"
                ),
            ));
        } else if let Some(other) = order.get(&candidate) {
            violations.push(violation(
                Kind::IndexCollision,
                *id,
                placed.attempt,
                format!(
                    "rule: K-10 each index holds one logical write; writes {} and {id} both \
                     report index {candidate}",
                    other.id
                ),
            ));
        } else {
            order.insert(candidate, placed);
        }
    }

    replay(&history, &order, &mut violations)?;
    place(&order, &mut violations);
    Ok(Report {
        checker: "K-10",
        examined: writes.len() as u64,
        violations,
    })
}

/// One logical write as the clients invoked it.
struct Write {
    command: Command,
    /// The first non-misuse invocation's record position.
    invoked_at: u64,
}

/// A logical write with its candidate index.
struct Placed {
    id: RequestId,
    command: Command,
    invoked_at: u64,
    /// The record position of the first delivered terminal answer, if any.
    closed_at: Option<u64>,
    attempt: Option<u64>,
    /// The first delivered terminal answer's decision, if any.
    decision: Option<Decision>,
}

/// What the records say of the writes.
struct Records {
    writes: BTreeMap<RequestId, Write>,
    resolutions: BTreeMap<RequestId, Vec<Settled>>,
}

/// How a resolution record settled a write.
struct Settled {
    resolved: Resolved,
    index: Option<u64>,
}

fn violation(kind: Kind, id: RequestId, attempt: Option<u64>, detail: String) -> Violation {
    Violation {
        kind,
        request_id: id.to_string(),
        attempt,
        detail,
    }
}

/// An overflowed, budget-exhausted or invalid-scenario run is incomplete (§10.16).
fn run_incomplete(doc: &Value, violations: &mut Vec<Violation>) -> Result<(), CheckError> {
    let mut reasons = Vec::new();
    if !bool_field(doc, "", "complete")? {
        reasons.push("complete is false");
    }
    if !array_field(doc, "", "overflowed")?.is_empty() {
        reasons.push("a required observation overflowed its bound");
    }
    if bool_field(field(doc, "", "event_budget")?, "event_budget", "exhausted")? {
        reasons.push("the event budget ran out");
    }
    if str_field(field(doc, "", "scenario")?, "scenario", "status")? != "valid" {
        reasons.push("the scenario broke the healthy envelope");
    }
    if !reasons.is_empty() {
        violations.push(Violation {
            kind: Kind::HistoryIncomplete,
            request_id: String::new(),
            attempt: None,
            detail: format!(
                "rule: K-10 judges only complete runs (P10); this run is incomplete: {}",
                reasons.join(", ")
            ),
        });
    }
    Ok(())
}

/// The logical writes (non-misuse write invocations by request ID) and the resolutions.
fn read_records(doc: &Value) -> Result<Records, CheckError> {
    let records = field(doc, "", "records")?
        .as_array()
        .ok_or_else(|| CheckError::malformed("records", "not an array"))?;
    let mut writes = BTreeMap::new();
    let mut resolutions: BTreeMap<RequestId, Vec<Settled>> = BTreeMap::new();
    for (position, record) in records.iter().enumerate() {
        let at = format!("records[{position}]");
        match str_field(record, &at, "kind")? {
            "invoke" => {
                let operation = field(record, &at, "operation")?;
                let is_write = ["put", "delete", "cas"].contains(&str_field(
                    operation,
                    &format!("{at}.operation"),
                    "op",
                )?);
                if !is_write || bool_field(record, &at, "misuse")? {
                    continue;
                }
                let command = command_of(
                    &json!({"request_id": field(record, &at, "request_id")?,
                        "operation": operation}),
                    &at,
                )?;
                writes.entry(command.request_id).or_insert(Write {
                    command,
                    invoked_at: position as u64,
                });
            }
            "resolution" => {
                let id =
                    RequestId::parse(str_field(record, &at, "request_id")?).map_err(|error| {
                        CheckError::malformed(format!("{at}.request_id"), error.to_string())
                    })?;
                let resolved = match str_field(record, &at, "resolved")? {
                    "executed" => Resolved::Executed,
                    "not_executed" => Resolved::NotExecuted,
                    "refused" => Resolved::Refused,
                    "unresolved" => Resolved::Unresolved,
                    other => {
                        return Err(CheckError::malformed(
                            format!("{at}.resolved"),
                            format!("unknown value {other:?}"),
                        ));
                    }
                };
                resolutions.entry(id).or_default().push(Settled {
                    resolved,
                    index: opt_u64(record, &at, "index")?,
                });
            }
            _ => {}
        }
    }
    Ok(Records {
        writes,
        resolutions,
    })
}

/// Each logical write's delivered terminal answers, earliest delivery first. Misuse attempts
/// and undelivered copies are not observations of an execution.
fn observations(history: &History) -> BTreeMap<RequestId, Vec<(u64, &Claim)>> {
    let mut seen: BTreeMap<RequestId, Vec<(u64, &Claim)>> = BTreeMap::new();
    for claim in &history.claims {
        if history
            .invoked
            .get(&claim.attempt)
            .is_some_and(|invoke| invoke.misuse)
        {
            continue;
        }
        if let Some(position) = history.first_delivery.get(&claim.attempt) {
            seen.entry(claim.result.request_id)
                .or_default()
                .push((*position, claim));
        }
    }
    for answers in seen.values_mut() {
        answers.sort_by_key(|(position, claim)| (*position, claim.attempt));
    }
    seen
}

/// A write's candidate index, judging its resolution and its answers against each other.
/// `None` when the write has no place in the order.
fn candidate(
    id: RequestId,
    seen: &[(u64, &Claim)],
    resolutions: Option<&Vec<Settled>>,
    violations: &mut Vec<Violation>,
) -> Option<u64> {
    let first = seen.first().map(|(_, claim)| *claim);
    for (_, claim) in seen.iter().skip(1) {
        if let Some(first) = first.filter(|first| {
            first.result.index != claim.result.index
                || first.result.decision != claim.result.decision
        }) {
            violations.push(violation(
                Kind::ObservationsDisagree,
                id,
                Some(claim.attempt),
                format!(
                    "rule: K-10 every answer to one logical write reports the same result; \
                     attempt {} said index {} {:?}, attempt {} said index {} {:?}",
                    first.attempt,
                    first.result.index.get(),
                    first.result.decision,
                    claim.attempt,
                    claim.result.index.get(),
                    claim.result.decision
                ),
            ));
        }
    }
    let attempt = first.map(|claim| claim.attempt);
    let Some(settled) = resolutions.and_then(|all| all.first()) else {
        violations.push(violation(
            Kind::HistoryIncomplete,
            id,
            attempt,
            format!("rule: K-10 judges resolved histories (P10); write {id} has no resolution"),
        ));
        return first.map(|claim| claim.result.index.get());
    };
    match settled.resolved {
        Resolved::Unresolved => violations.push(violation(
            Kind::HistoryIncomplete,
            id,
            attempt,
            format!(
                "rule: K-10 judges resolved histories (P10); write {id} is unresolved, so the \
                 run is incomplete and is not a pass"
            ),
        )),
        Resolved::NotExecuted | Resolved::Refused => {
            if let Some(first) = first {
                violations.push(violation(
                    Kind::ResolutionContradicted,
                    id,
                    Some(first.attempt),
                    format!(
                        "rule: K-10 a write clients saw execute was executed; write {id} was \
                         resolved as never executed, but attempt {} was delivered a terminal \
                         answer at index {}",
                        first.attempt,
                        first.result.index.get()
                    ),
                ));
            }
        }
        Resolved::Executed => {
            if let (Some(first), Some(resolved)) = (first, settled.index)
                && first.result.index.get() != resolved
            {
                violations.push(violation(
                    Kind::ResolutionContradicted,
                    id,
                    Some(first.attempt),
                    format!(
                        "rule: K-10 a resolution names the index clients saw; write {id} was \
                         resolved as executed at index {resolved}, but attempt {} was \
                         delivered index {}",
                        first.attempt,
                        first.result.index.get()
                    ),
                ));
            }
            if first.is_none() {
                return settled.index;
            }
        }
    }
    first.map(|claim| claim.result.index.get())
}

fn index_at(position: u64) -> Result<Index, CheckError> {
    Index::new(position).map_err(|error| CheckError::Replay {
        index: position,
        reason: error.to_string(),
    })
}

/// Runs the model over the candidate order and compares every reported decision.
fn replay(
    history: &History,
    order: &BTreeMap<u64, Placed>,
    violations: &mut Vec<Violation>,
) -> Result<(), CheckError> {
    let mut model = Model::new(history.limits);
    let mut next = 1;
    for (&index, placed) in order {
        while next < index {
            apply_empty(&mut model, next)?;
            next += 1;
        }
        let at = index_at(index)?;
        match model.apply(at, &Entry::Command(placed.command.clone())) {
            Ok(model::Applied::Executed(result)) => {
                let kind = placed
                    .decision
                    .and_then(|reported| differs(result.decision, reported));
                if let (Some(kind), Some(reported)) = (kind, placed.decision) {
                    violations.push(violation(
                        kind,
                        placed.id,
                        placed.attempt,
                        format!(
                            "rule: K-10 the model run in index order decides every write; write \
                             {} at index {index} should be {:?} but was reported {reported:?}",
                            placed.id, result.decision
                        ),
                    ));
                }
            }
            Ok(
                model::Applied::Empty
                | model::Applied::Replayed(_)
                | model::Applied::Conflict { .. },
            ) => violations.push(violation(
                Kind::ReportedDecisionWrong,
                placed.id,
                placed.attempt,
                format!(
                    "rule: K-10 each logical write executes once; the model did not execute \
                     write {} at index {index}",
                    placed.id
                ),
            )),
            Err(error) => {
                violations.push(violation(
                    Kind::ReportedDecisionWrong,
                    placed.id,
                    placed.attempt,
                    format!(
                        "rule: K-10 only admissible commands execute; write {} at index \
                         {index}: {error}",
                        placed.id
                    ),
                ));
                apply_empty(&mut model, index)?;
            }
        }
        next = index + 1;
    }
    Ok(())
}

fn apply_empty(model: &mut Model, position: u64) -> Result<(), CheckError> {
    model
        .apply(index_at(position)?, &Entry::Empty)
        .map(|_| ())
        .map_err(|error| CheckError::Replay {
            index: position,
            reason: error.to_string(),
        })
}

/// The greedy earliest-placement pass: in index order, a write's point is the later of the
/// previous point and its invocation, and must not fall after its interval closed.
fn place(order: &BTreeMap<u64, Placed>, violations: &mut Vec<Violation>) {
    let mut point: Option<(u64, RequestId)> = None;
    for (&index, placed) in order {
        let (at, from) = match point {
            Some((previous, by)) if previous > placed.invoked_at => (previous, by),
            Some(_) | None => (placed.invoked_at, placed.id),
        };
        if let Some(closed) = placed.closed_at.filter(|closed| at > *closed) {
            violations.push(violation(
                Kind::NoLinearization,
                placed.id,
                placed.attempt,
                format!(
                    "rule: K-10 each write takes effect between its first invocation and its \
                     first observed terminal decision, in index order; write {} (index \
                     {index}) was observed terminal at record {closed}, but the writes before \
                     it in index order cannot be placed before record {at} (write {from} \
                     invoked then)",
                    placed.id
                ),
            ));
            continue;
        }
        point = Some((at, from));
    }
}

#[cfg(test)]
mod tests {
    // check: K-10
    use super::k10;
    use crate::fixtures::{
        A, B, Doc, conflict, delete, ok_delete, ok_put, put, version_mismatch_absent,
    };
    use crate::report::Kind;
    use serde_json::json;

    const C: &str = "000000000000000000000000000000a3";

    /// A write that executed at `index`: invoked, answered and delivered, and resolved.
    fn put_at(doc: &mut Doc, attempt: u64, id: &str, index: u64) {
        doc.invoke(attempt, id, false)
            .put_ok(attempt, id, index, false)
            .resolve(id, "executed", Some(index), &[attempt]);
    }

    fn verdict(doc: &Doc) -> crate::report::Report {
        k10(&doc.value()).expect("fixture reads")
    }

    fn assert_fails(doc: &Doc, kind: Kind, rule: &str) {
        let report = verdict(doc);
        assert!(
            report.has(kind),
            "rule: {rule}; expected {kind}, observed {:?}",
            report.violations
        );
    }

    fn assert_passes(doc: &Doc, rule: &str) {
        let report = verdict(doc);
        assert!(
            report.passed(),
            "rule: {rule}; observed {:?}",
            report.violations
        );
    }

    #[test]
    fn passes_sequential_writes() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        put_at(&mut doc, 2, B, 3);
        let report = verdict(&doc);
        assert!(report.passed(), "observed {:?}", report.violations);
        assert_eq!(report.examined, 2, "one case per logical write");
    }

    #[test]
    fn collapses_retries_into_one_write() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .invoke(2, A, false)
            .put_ok(2, A, 2, false)
            .put_ok(1, A, 2, true)
            .resolve(A, "executed", Some(2), &[1, 2]);
        let report = verdict(&doc);
        assert!(report.passed(), "observed {:?}", report.violations);
        assert_eq!(report.examined, 1, "two attempts are one logical write");
    }

    #[test]
    fn excludes_conflict_misuse_from_executions() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        doc.invoke(2, A, true)
            .respond(2, "put", conflict(A, 2))
            .deliver(2);
        assert_passes(
            &doc,
            "a misuse attempt and its conflict answer are not executions",
        );
    }

    #[test]
    fn ignores_a_misuse_attempt_answered_with_another_index() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        doc.invoke(2, A, true).put_ok(2, A, 9, false);
        assert_passes(&doc, "misuse answers never place a write (K-2 judges them)");
    }

    #[test]
    fn fills_gaps_left_by_no_ops() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 4);
        put_at(&mut doc, 2, B, 9);
        assert_passes(&doc, "indices no write claims are no-ops");
    }

    #[test]
    fn a_delayed_first_response_and_a_replay_are_one_write() {
        let mut doc = Doc::new();
        // The first answer is generated but never delivered; the retry's replay closes it.
        doc.invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 2, false))
            .invoke(2, A, false)
            .put_ok(2, A, 2, true)
            .resolve(A, "executed", Some(2), &[1, 2]);
        put_at(&mut doc, 3, B, 3);
        assert_passes(&doc, "an undelivered response closes no interval");
    }

    #[test]
    fn concurrent_writes_may_take_either_order() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .invoke(2, B, false)
            .put_ok(2, B, 2, false)
            .put_ok(1, A, 3, false)
            .resolve(A, "executed", Some(3), &[1])
            .resolve(B, "executed", Some(2), &[2]);
        assert_passes(&doc, "overlapping writes linearize in index order");
    }

    #[test]
    fn an_unknown_outcome_still_executes_at_its_resolved_index() {
        let mut doc = Doc::new();
        // A's response never arrives; the status lookup shows index 2. B is a CAS expecting
        // A's version, which succeeds only because A executed first.
        doc.invoke(1, A, false)
            .invoke(3, B, false)
            .invoke_op(
                3,
                B,
                false,
                json!({"op": "cas", "key": "k",
                    "expected": {"kind": "version", "version": 2}, "value": "w"}),
            )
            .respond(3, "cas", ok_put(B, 3, 3, false))
            .deliver(3)
            .resolve(A, "executed", Some(2), &[1])
            .resolve(B, "executed", Some(3), &[3]);
        assert_passes(
            &doc,
            "an executed resolution places a write nobody saw terminal",
        );
    }

    #[test]
    fn a_resolved_not_executed_write_takes_no_place() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        doc.invoke(2, B, false)
            .resolve(B, "not_executed", None, &[9]);
        assert_passes(&doc, "a write that never executed is not in the order");
    }

    #[test]
    fn judges_delete_and_cas_decisions() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        doc.invoke_op(2, B, false, json!({"op": "delete", "key": "k"}))
            .respond(2, "delete", ok_delete(B, 3, true, false))
            .deliver(2)
            .resolve(B, "executed", Some(3), &[2]);
        doc.invoke_op(
            3,
            C,
            false,
            json!({"op": "cas", "key": "k",
                "expected": {"kind": "version", "version": 2}, "value": "w"}),
        )
        .respond(3, "cas", version_mismatch_absent(C, 4))
        .deliver(3)
        .resolve(C, "executed", Some(4), &[3]);
        assert_passes(
            &doc,
            "a delete then a CAS on the deleted key mismatches as absent",
        );
    }

    #[test]
    fn rejects_an_index_collision() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        put_at(&mut doc, 2, B, 2);
        assert_fails(&doc, Kind::IndexCollision, "two writes at one index");
    }

    #[test]
    fn rejects_a_wrong_version() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 7, false))
            .deliver(1)
            .resolve(A, "executed", Some(2), &[1]);
        assert_fails(
            &doc,
            Kind::ReportedVersionWrong,
            "version is the execution index",
        );
    }

    #[test]
    fn rejects_a_wrong_existed() {
        let mut doc = Doc::new();
        doc.invoke_op(1, A, false, json!({"op": "delete", "key": "k"}))
            .respond(1, "delete", ok_delete(A, 2, true, false))
            .deliver(1)
            .resolve(A, "executed", Some(2), &[1]);
        assert_fails(
            &doc,
            Kind::ReportedExistedWrong,
            "a delete of an absent key",
        );
    }

    #[test]
    fn rejects_a_success_the_model_refuses() {
        let mut doc = Doc::new();
        doc.invoke_op(
            1,
            A,
            false,
            json!({"op": "cas", "key": "k",
                "expected": {"kind": "version", "version": 5}, "value": "w"}),
        )
        .respond(1, "cas", ok_put(A, 2, 2, false))
        .deliver(1)
        .resolve(A, "executed", Some(2), &[1]);
        assert_fails(
            &doc,
            Kind::ReportedCasOutcomeWrong,
            "a CAS on an absent key mismatches",
        );
    }

    #[test]
    fn rejects_impossible_real_time_placement() {
        let mut doc = Doc::new();
        // A is observed terminal at index 3 before B is invoked, yet B executes at index 2.
        put_at(&mut doc, 1, A, 3);
        put_at(&mut doc, 2, B, 2);
        assert_fails(
            &doc,
            Kind::NoLinearization,
            "a write invoked after another's terminal decision executes later",
        );
    }

    #[test]
    fn rejects_a_blocked_placement_through_a_chain() {
        let mut doc = Doc::new();
        // Index order B(2) < A(3) < C(4). C is observed terminal before B is invoked.
        doc.invoke(1, C, false).put_ok(1, C, 4, false);
        put_at(&mut doc, 2, B, 2);
        put_at(&mut doc, 3, A, 3);
        doc.resolve(C, "executed", Some(4), &[1]);
        assert_fails(
            &doc,
            Kind::NoLinearization,
            "the pass carries the latest point forward",
        );
    }

    #[test]
    fn a_late_terminal_answer_does_not_constrain_order() {
        let mut doc = Doc::new();
        // A executes at 3 but its answer is delivered after B (index 2) was invoked and
        // answered: intervals overlap.
        doc.invoke(1, A, false)
            .invoke(2, B, false)
            .put_ok(2, B, 2, false);
        doc.put_ok(1, A, 3, false)
            .resolve(A, "executed", Some(3), &[1])
            .resolve(B, "executed", Some(2), &[2]);
        assert_passes(
            &doc,
            "an interval closes at the first delivery, not at execution",
        );
    }

    #[test]
    fn rejects_an_unresolved_write_as_incomplete() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false).resolve(A, "unresolved", None, &[]);
        assert_fails(
            &doc,
            Kind::HistoryIncomplete,
            "unresolved is never a pass (P10)",
        );
    }

    #[test]
    fn rejects_a_missing_resolution_as_incomplete() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        assert_fails(
            &doc,
            Kind::HistoryIncomplete,
            "every write needs a resolution",
        );
    }

    #[test]
    fn rejects_an_incomplete_run() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        doc.incomplete();
        assert_fails(&doc, Kind::HistoryIncomplete, "complete=false fails (P10)");
    }

    #[test]
    fn rejects_answers_that_disagree() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .put_ok(1, A, 2, false)
            .invoke(2, A, false)
            .put_ok(2, A, 5, true)
            .resolve(A, "executed", Some(2), &[1]);
        assert_fails(
            &doc,
            Kind::ObservationsDisagree,
            "a retry returns the original index",
        );
    }

    #[test]
    fn rejects_a_resolution_that_contradicts_the_clients() {
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .put_ok(1, A, 2, false)
            .resolve(A, "not_executed", None, &[1]);
        assert_fails(
            &doc,
            Kind::ResolutionContradicted,
            "a seen execution did execute",
        );
        let mut doc = Doc::new();
        doc.invoke(1, A, false)
            .put_ok(1, A, 2, false)
            .resolve(A, "executed", Some(3), &[1]);
        assert_fails(
            &doc,
            Kind::ResolutionContradicted,
            "the resolution names the seen index",
        );
    }

    #[test]
    fn rejects_an_index_beyond_the_log_capacity() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2_000_000);
        assert_fails(
            &doc,
            Kind::IndexOutOfRange,
            "no log holds an index beyond capacity",
        );
    }

    #[test]
    fn ignores_god_view_records() {
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 2);
        // A committed record that lies about the log changes nothing: K-10 is black-box.
        doc.commit_everywhere(1, "", serde_json::Value::Null)
            .commit(2, "00", put(B, "k", "other"));
        assert_passes(&doc, "committed records are not read");
        let mut doc = Doc::new();
        put_at(&mut doc, 1, A, 3);
        doc.commit(2, "", delete(A, "k"));
        assert_passes(&doc, "committed records do not place a write");
    }

    #[test]
    fn handles_ten_thousand_operations_in_polynomial_time() {
        let mut doc = Doc::new();
        for n in 0..10_000_u64 {
            let id = format!("{:032x}", n + 1);
            put_at(&mut doc, n + 1, &id, n + 2);
        }
        let report = verdict(&doc);
        assert!(
            report.passed(),
            "rule: 10,000 operations judge without violations; observed {} violations, first \
             {:?}",
            report.violations.len(),
            report.violations.first()
        );
        assert_eq!(report.examined, 10_000, "one case per logical write");
    }

    /// The fixed negative-history corpus (§10.5, B-22): each case breaks one rule of the
    /// witness and must be caught as named. Removing a case lowers the check's case count.
    #[test]
    fn negative_corpus_is_caught() {
        let cases: Vec<(&str, Doc, Kind)> = vec![
            (
                "two writes at one index",
                {
                    let mut doc = Doc::new();
                    put_at(&mut doc, 1, A, 2);
                    put_at(&mut doc, 2, B, 2);
                    doc
                },
                Kind::IndexCollision,
            ),
            (
                "a later write ordered before an observed one",
                {
                    let mut doc = Doc::new();
                    put_at(&mut doc, 1, A, 3);
                    put_at(&mut doc, 2, B, 2);
                    doc
                },
                Kind::NoLinearization,
            ),
            (
                "a duplicate execution reported as a fresh one",
                {
                    let mut doc = Doc::new();
                    put_at(&mut doc, 1, A, 2);
                    doc.invoke(2, A, false).put_ok(2, A, 4, false);
                    doc
                },
                Kind::ObservationsDisagree,
            ),
            (
                "an unresolved write",
                {
                    let mut doc = Doc::new();
                    doc.invoke(1, A, false).resolve(A, "unresolved", None, &[]);
                    doc
                },
                Kind::HistoryIncomplete,
            ),
            (
                "a wrong version",
                {
                    let mut doc = Doc::new();
                    doc.invoke(1, A, false)
                        .respond(1, "put", ok_put(A, 2, 3, false))
                        .deliver(1)
                        .resolve(A, "executed", Some(2), &[1]);
                    doc
                },
                Kind::ReportedVersionWrong,
            ),
        ];
        for (name, doc, kind) in cases {
            let report = verdict(&doc);
            assert!(
                report.has(kind),
                "rule: corpus case {name:?} is caught as {kind}; observed {:?}",
                report.violations
            );
        }
    }
}
