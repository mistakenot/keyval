//! Wrong-response fixtures (PLAN.md §10.8, W31.3): the suite must fail a bad server.
//!
//! A [`Fault`] rewrites what a correct server said into what a plausible wrong one would say.
//! The suite sees only responses, so a fault applied between the transport and the test is a
//! wrong server as far as the suite can tell. Each test below runs one corpus test under one
//! fault and requires it to fail on the rule the fault breaks. That validates the suite (and,
//! before the real cluster exists, the topology tests' teeth) without trusting the model.

#![cfg(test)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

use crate::http::{Request, Response};

/// One way a server can be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Responses lack `Cache-Control: no-store` (C-6).
    NoStoreDropped,
    /// A `version_mismatch` answers 412 instead of 409.
    MismatchWrongStatus,
    /// An `invalid_request` answers 404 instead of 400.
    InvalidRequestWrongStatus,
    /// A replayed result carries an index one too high.
    ReplayIndexShifted,
    /// A replayed result says `replayed: false`.
    ReplayNotMarked,
    /// A write result's version is one more than its index (DM-3).
    VersionOffByOne,
    /// A known request status is reported as unknown (F-7).
    KnownReportedUnknown,
    /// A scan page's `has_more` is inverted.
    ScanHasMoreInverted,
    /// A delete reports the opposite `existed`.
    DeleteExistedInverted,
    /// Reads report an applied index that never advances (C-7).
    AppliedIndexStuck,
    /// A read of a present key answers `not_found`: the write was lost (K-10).
    WriteLost,
}

impl Fault {
    /// Every fault, in declaration order.
    pub const ALL: [Self; 11] = [
        Self::NoStoreDropped,
        Self::MismatchWrongStatus,
        Self::InvalidRequestWrongStatus,
        Self::ReplayIndexShifted,
        Self::ReplayNotMarked,
        Self::VersionOffByOne,
        Self::KnownReportedUnknown,
        Self::ScanHasMoreInverted,
        Self::DeleteExistedInverted,
        Self::AppliedIndexStuck,
        Self::WriteLost,
    ];

    /// Rewrites the response the way a server with this fault would have sent it.
    pub fn apply(self, request: &Request, response: &mut Response) {
        match self {
            Self::NoStoreDropped => response
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("Cache-Control")),
            Self::MismatchWrongStatus => {
                if error_code(response) == Some("version_mismatch") {
                    response.status = 412;
                }
            }
            Self::InvalidRequestWrongStatus => {
                if error_code(response) == Some("invalid_request") {
                    response.status = 404;
                }
            }
            Self::ReplayIndexShifted => edit(response, |body| {
                let replayed = body["replayed"] == true
                    || body.pointer("/error/details/replayed") == Some(&json!(true))
                    || body.pointer("/result/replayed") == Some(&json!(true));
                if replayed {
                    for pointer in ["/index", "/error/details/index", "/result/index"] {
                        bump(body, pointer);
                    }
                }
            }),
            Self::ReplayNotMarked => edit(response, |body| {
                for pointer in ["/replayed", "/error/details/replayed", "/result/replayed"] {
                    if let Some(slot) = body.pointer_mut(pointer) {
                        *slot = json!(false);
                    }
                }
            }),
            Self::VersionOffByOne => edit(response, |body| {
                if body.get("index").is_some() && body.get("replayed") == Some(&json!(false)) {
                    bump(body, "/version");
                }
            }),
            Self::KnownReportedUnknown => edit(response, |body| {
                if body["state"] == "known" {
                    body["state"] = json!("unknown");
                }
            }),
            Self::ScanHasMoreInverted => edit(response, |body| {
                if let Some(more) = body.get("has_more").and_then(Value::as_bool) {
                    body["has_more"] = json!(!more);
                }
            }),
            Self::DeleteExistedInverted => edit(response, |body| {
                if request.method == "DELETE"
                    && let Some(existed) = body.get("existed").and_then(Value::as_bool)
                {
                    body["existed"] = json!(!existed);
                }
            }),
            Self::AppliedIndexStuck => edit(response, |body| {
                for pointer in [
                    "/served_by/applied_index",
                    "/error/details/served_by/applied_index",
                ] {
                    if let Some(slot) = body.pointer_mut(pointer) {
                        *slot = json!(0);
                    }
                }
            }),
            Self::WriteLost => {
                if request.method == "GET"
                    && request.target.starts_with("/v1/kv")
                    && response.status == 200
                    && let Ok(body) = response.json()
                {
                    let lost = json!({"error": {
                        "code": "not_found",
                        "message": "no such key",
                        "details": {"served_by": body["served_by"].clone()},
                    }});
                    response.status = 404;
                    response.body = lost.to_string().into_bytes();
                }
            }
        }
    }
}

fn error_code(response: &Response) -> Option<&'static str> {
    let body = response.json().ok()?;
    let code = body.pointer("/error/code")?.as_str()?.to_owned();
    ["version_mismatch", "invalid_request"]
        .into_iter()
        .find(|known| *known == code)
}

/// Parses a JSON body, lets `change` rewrite it, and puts it back; other bodies stay as sent.
fn edit(response: &mut Response, change: impl FnOnce(&mut Value)) {
    if let Ok(mut body) = response.json() {
        change(&mut body);
        response.body = body.to_string().into_bytes();
    }
}

fn bump(body: &mut Value, pointer: &str) {
    if let Some(slot) = body.pointer_mut(pointer)
        && let Some(number) = slot.as_u64()
    {
        *slot = json!(number + 1);
    }
}

thread_local! {
    static ACTIVE: Cell<Option<Fault>> = const { Cell::new(None) };
}

/// The fault the current thread's targets are to exhibit, if any.
pub fn active() -> Option<Fault> {
    ACTIVE.with(Cell::get)
}

/// Clears the active fault when dropped, so a failing test cannot leak one.
struct Armed;

impl Drop for Armed {
    fn drop(&mut self) {
        ACTIVE.with(|slot| slot.set(None));
    }
}

/// Runs `corpus_test` with every target on this thread exhibiting `fault`, and returns the
/// failure it raised. The corpus test passing is itself a failure of the suite.
fn failure_under(fault: Fault, corpus_test: fn()) -> String {
    ACTIVE.with(|slot| slot.set(Some(fault)));
    let _armed = Armed;
    match catch_unwind(AssertUnwindSafe(corpus_test)) {
        Ok(()) => panic!(
            "rule: the suite fails a server with fault {fault:?}, but the corpus test passed"
        ),
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_default(),
    }
}

/// Asserts `corpus_test` fails under `fault`, on the rule named by `needle`.
fn rejects(fault: Fault, corpus_test: fn(), needle: &str) {
    let message = failure_under(fault, corpus_test);
    assert!(
        message.contains(needle),
        "rule: the suite fails fault {fault:?} on the rule it breaks; expected the failure to \
         mention {needle:?}; failure={message}"
    );
}

// check: CONF-FIXTURES
#[test]
fn a_missing_no_store_header_is_rejected() {
    rejects(
        Fault::NoStoreDropped,
        super::headers::every_response_is_no_store_json,
        "every response is Cache-Control: no-store",
    );
}

#[test]
fn a_wrong_status_for_a_version_mismatch_is_rejected() {
    rejects(
        Fault::MismatchWrongStatus,
        super::cas::cas_mismatch_is_a_terminal_result_that_consumes_the_id,
        "CAS at the wrong version",
    );
}

#[test]
fn a_wrong_status_for_an_invalid_request_is_rejected() {
    rejects(
        Fault::InvalidRequestWrongStatus,
        super::encoding::raw_plus_is_refused_and_changes_nothing,
        "a raw + in GET key is refused",
    );
}

#[test]
fn a_wrong_replay_index_is_rejected() {
    rejects(
        Fault::ReplayIndexShifted,
        super::writes::retried_put_replays_the_original_result,
        "a replay returns the original index (F-6)",
    );
}

#[test]
fn a_replay_not_marked_as_a_replay_is_rejected() {
    rejects(
        Fault::ReplayNotMarked,
        super::writes::retried_put_replays_the_original_result,
        "a retry is a replay",
    );
}

#[test]
fn a_version_that_is_not_the_execution_index_is_rejected() {
    rejects(
        Fault::VersionOffByOne,
        super::writes::put_result_shape_and_versions_follow_the_log,
        "version == execution index (DM-3)",
    );
}

#[test]
fn a_known_status_reported_as_unknown_is_rejected() {
    rejects(
        Fault::KnownReportedUnknown,
        super::status::status_of_an_executed_write_is_known_and_replayed,
        "an executed write is known",
    );
}

#[test]
fn an_inverted_scan_has_more_is_rejected() {
    rejects(
        Fault::ScanHasMoreInverted,
        super::scan::scan_limit_caps_are_exact,
        "more remain after",
    );
}

#[test]
fn an_inverted_delete_existed_is_rejected() {
    rejects(
        Fault::DeleteExistedInverted,
        super::writes::delete_reports_existence_and_replays,
        "deleting an absent key succeeds",
    );
}

#[test]
fn an_applied_index_that_never_advances_fails_the_poll_as_a_timeout() {
    rejects(
        Fault::AppliedIndexStuck,
        super::encoding::escaped_plus_and_space_name_distinct_keys,
        "a read catches up",
    );
}

#[test]
fn a_lost_write_is_rejected_by_the_witness_run() {
    rejects(
        Fault::WriteLost,
        super::witness::concurrent_clients_produce_a_linearizable_history,
        "GET of the final state",
    );
}

/// The test above that rejects `fault`. A new `Fault` variant fails to compile here until
/// it is paired with one (CD-5).
fn rejecting_test(fault: Fault) -> &'static str {
    match fault {
        Fault::NoStoreDropped => "a_missing_no_store_header_is_rejected",
        Fault::MismatchWrongStatus => "a_wrong_status_for_a_version_mismatch_is_rejected",
        Fault::InvalidRequestWrongStatus => "a_wrong_status_for_an_invalid_request_is_rejected",
        Fault::ReplayIndexShifted => "a_wrong_replay_index_is_rejected",
        Fault::ReplayNotMarked => "a_replay_not_marked_as_a_replay_is_rejected",
        Fault::VersionOffByOne => "a_version_that_is_not_the_execution_index_is_rejected",
        Fault::KnownReportedUnknown => "a_known_status_reported_as_unknown_is_rejected",
        Fault::ScanHasMoreInverted => "an_inverted_scan_has_more_is_rejected",
        Fault::DeleteExistedInverted => "an_inverted_delete_existed_is_rejected",
        Fault::AppliedIndexStuck => {
            "an_applied_index_that_never_advances_fails_the_poll_as_a_timeout"
        }
        Fault::WriteLost => "a_lost_write_is_rejected_by_the_witness_run",
    }
}

#[test]
fn every_fault_has_a_test_that_rejects_it() {
    let source = include_str!("fixtures.rs");
    for fault in Fault::ALL {
        let name = rejecting_test(fault);
        assert!(
            source.contains(&format!("#[test]\nfn {name}()")),
            "rule: every fault is paired with a test that exists; fault={fault:?} test={name}"
        );
    }
}
