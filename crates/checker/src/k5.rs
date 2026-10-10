//! K-5, read validity (PLAN.md §10.5; C-3, C-8, F-2, F-4, F-9): every GET, SCAN page and
//! status lookup a node answered with `served_by` equals the reference model's state at the
//! committed prefix `[1..applied_index]` it reports.
//!
//! That prefix must have been applied by the serving node and committed before the node
//! answered (god view): a read cannot borrow a prefix that exists only after its response
//! (B-05, B-06). A SCAN page is judged against the page rules of F-4 (at most 128 items, 32 by
//! default, at most 1 MiB of decoded key and value bytes, the first item always returned,
//! `next_after` the last key returned when `has_more`), written out here independently of the
//! model's `scan` (B-07).
//!
//! Where a run records no `node_state` for the serving node, the node counts as having applied
//! nothing, so a read of a non-empty prefix is a violation rather than a silent pass.
//!
//! The unit examined is one read answered with `served_by`.

use model::{Key, Model, RequestId};

use crate::reads::{Answer, Query, Reads, Reply};
use crate::report::{CheckError, Kind, Report, Violation};

/// The default SCAN page size (F-4).
const DEFAULT_PAGE_ITEMS: u64 = 32;
/// The largest SCAN page size (F-4).
const MAX_PAGE_ITEMS: u64 = 128;
/// The most decoded key and value bytes one SCAN page holds (F-4).
const MAX_PAGE_BYTES: u64 = 1 << 20;

/// Judges `reads` against K-5.
///
/// # Errors
///
/// [`CheckError::Replay`] if the committed entries cannot be replayed, or
/// [`CheckError::Malformed`] if a read names a key the model rejects.
pub fn k5(reads: &Reads) -> Result<Report, CheckError> {
    let mut found: Vec<(u64, Violation)> = Vec::new();
    for answer in &reads.answers {
        prefix_violations(reads, answer, &mut found);
    }

    let mut order: Vec<&Answer> = reads.answers.iter().collect();
    order.sort_by_key(|answer| (answer.applied_index, answer.position));
    let mut model = Model::new(reads.limits);
    for answer in order {
        if answer.applied_index > reads.committed.len() as u64 {
            continue;
        }
        while model.applied_index() < answer.applied_index {
            let next =
                &reads.committed[usize::try_from(model.applied_index()).unwrap_or(usize::MAX)];
            model
                .apply(next.index, &next.entry)
                .map_err(|error| CheckError::Replay {
                    index: next.index.get(),
                    reason: error.to_string(),
                })?;
        }
        if let Some((kind, detail)) = content_violation(&model, answer)? {
            found.push((answer.position, violation(answer, kind, detail)));
        }
    }

    found.sort_by_key(|(position, _)| *position);
    Ok(Report {
        checker: "K-5",
        examined: reads.answers.len() as u64,
        violations: found.into_iter().map(|(_, v)| v).collect(),
    })
}

fn violation(answer: &Answer, kind: Kind, detail: String) -> Violation {
    Violation {
        kind,
        request_id: match &answer.query {
            Query::Status { request_id } => request_id.to_string(),
            Query::Get { .. } | Query::Scan { .. } => String::new(),
        },
        attempt: Some(answer.attempt),
        detail,
    }
}

/// The served-by node, and whether the prefix was applied and committed before the answer.
fn prefix_violations(reads: &Reads, answer: &Answer, found: &mut Vec<(u64, Violation)>) {
    let reported = answer.applied_index;
    if answer.served_node != answer.node {
        found.push((
            answer.position,
            violation(
                answer,
                Kind::ReadServedByWrongNode,
                format!(
                    "rule: K-5 served_by names the node that answered; node {} answered \
                     (incarnation {}) but served_by names node {}",
                    answer.node, answer.incarnation, answer.served_node
                ),
            ),
        ));
    }
    let latest = reads
        .samples
        .get(&answer.node)
        .and_then(|samples| {
            let before = samples.partition_point(|sample| sample.position < answer.position);
            before.checked_sub(1).map(|last| samples[last])
        })
        .map_or((0, u64::MAX), |sample| (sample.applied, sample.commit));
    let (applied, commit) = latest;
    if reported > applied {
        found.push((
            answer.position,
            violation(
                answer,
                Kind::ReadPrefixNotApplied,
                format!(
                    "rule: K-5 a read reports a prefix its node had applied before answering; \
                     node {} reports applied_index {reported} at record {}, but its latest \
                     node_state before then has applied {applied}",
                    answer.node, answer.position
                ),
            ),
        ));
    }
    let committed_before = reads
        .committed_at
        .partition_point(|position| *position < answer.position) as u64;
    if reported > commit.min(committed_before) {
        found.push((
            answer.position,
            violation(
                answer,
                Kind::ReadPrefixNotCommitted,
                format!(
                    "rule: K-5 a read reports a prefix committed before answering; node {} \
                     reports applied_index {reported} at record {}, but the node's commit index \
                     was {commit} and {committed_before} entries were committed by then",
                    answer.node, answer.position
                ),
            ),
        ));
    }
}

/// Compares the answer with the model at its reported prefix.
fn content_violation(model: &Model, answer: &Answer) -> Result<Option<(Kind, String)>, CheckError> {
    let at = answer.applied_index;
    Ok(match (&answer.query, &answer.reply) {
        (Query::Get { key }, Reply::Get(reported)) => {
            let parsed = Key::new(key.as_str()).map_err(|error| {
                CheckError::malformed(format!("get attempt {}", answer.attempt), error.to_string())
            })?;
            let expected = model
                .get(&parsed)
                .map_err(|error| {
                    CheckError::malformed(
                        format!("get attempt {}", answer.attempt),
                        error.to_string(),
                    )
                })?
                .map(|(value, version)| (value.as_str().to_owned(), version.get()));
            (expected != *reported).then(|| {
                (
                    Kind::ReadValueWrong,
                    format!(
                        "rule: K-5 a GET equals the model at its reported prefix; key {key:?} at \
                         applied_index {at}: expected {expected:?}, reported {reported:?}"
                    ),
                )
            })
        }
        (
            Query::Scan {
                start,
                end,
                after,
                limit,
            },
            Reply::Scan {
                items,
                has_more,
                next_after,
            },
        ) => {
            let reported: Page = (
                items
                    .iter()
                    .map(|item| (item.key.clone(), item.value.clone(), item.version))
                    .collect(),
                *has_more,
                next_after.clone(),
            );
            let expected = expected_page(
                model,
                (start.as_deref(), end.as_deref(), after.as_deref()),
                *limit,
            );
            (expected.as_ref() != Some(&reported)).then(|| {
                (
                    Kind::ScanPageWrong,
                    format!(
                        "rule: K-5 a SCAN page is one page of the model at its reported prefix \
                         (F-4); start {start:?} end {end:?} after {after:?} limit {limit:?} at \
                         applied_index {at}: expected {expected:?}, reported {reported:?}"
                    ),
                )
            })
        }
        (Query::Status { request_id }, Reply::Status(reported)) => {
            let expected = status_of(model, request_id);
            (expected != *reported).then(|| {
                (
                    Kind::StatusAnswerWrong,
                    format!(
                        "rule: K-5 a status answer equals the model at its reported prefix \
                         (F-9); request {request_id} at applied_index {at}: expected \
                         {expected:?}, reported {reported:?}"
                    ),
                )
            })
        }
        (query, reply) => {
            return Err(CheckError::malformed(
                format!("attempt {}", answer.attempt),
                format!("a read of {query:?} was answered with {reply:?}"),
            ));
        }
    })
}

fn status_of(model: &Model, request_id: &RequestId) -> Option<model::TerminalResult> {
    model.status(request_id)
}

/// One SCAN page as items `(key, value, version)`, `has_more` and `next_after`.
type Page = (Vec<(String, String, u64)>, bool, Option<String>);

/// The page F-4 gives for a request over the model's live keys; `None` when the page size is
/// outside 1 to 128, which no node may serve.
fn expected_page(
    model: &Model,
    (start, end, after): (Option<&str>, Option<&str>, Option<&str>),
    limit: Option<u64>,
) -> Option<Page> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_ITEMS);
    if !(1..=MAX_PAGE_ITEMS).contains(&limit) {
        return None;
    }
    let mut items: Vec<(String, String, u64)> = Vec::new();
    let mut bytes = 0_u64;
    let mut has_more = false;
    let in_range = model
        .entries()
        .filter(|(key, _, _)| start.is_none_or(|start| key.as_str() >= start))
        .filter(|(key, _, _)| after.is_none_or(|after| key.as_str() > after))
        .filter(|(key, _, _)| end.is_none_or(|end| key.as_str() < end));
    for (key, value, version) in in_range {
        let size = (key.as_str().len() + value.as_str().len()) as u64;
        let fits =
            items.is_empty() || (items.len() as u64) < limit && bytes + size <= MAX_PAGE_BYTES;
        if !fits {
            has_more = true;
            break;
        }
        bytes += size;
        items.push((
            key.as_str().to_owned(),
            value.as_str().to_owned(),
            version.get(),
        ));
    }
    let next_after = if has_more {
        items.last().map(|(key, _, _)| key.clone())
    } else {
        None
    };
    Some((items, has_more, next_after))
}

#[cfg(test)]
mod tests {
    // check: K-5
    use model::{Model, ScanRequest};

    use super::{expected_page, k5};
    use crate::fixtures::{
        A, B, Doc, get_absent, get_found, get_op, ok_put, put, scan_op, scan_page, status_found,
        status_op, status_unknown,
    };
    use crate::report::Kind;

    const C: &str = "000000000000000000000000000000a3";

    /// Four entries: a no-op, `k=v1` (version 2), `k=v2` (version 3) and `j=w` (version 4).
    /// Node 1 has applied all four, node 2 only the first two; both know all four committed.
    fn base() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, "02a1", put(A, "k", "v1"));
        doc.commit_everywhere(3, "03a2", put(B, "k", "v2"));
        doc.commit_everywhere(4, "04a3", put(C, "j", "w"));
        doc.node_state(1, 4, 4).node_state(2, 2, 4);
        doc
    }

    #[test]
    fn a_get_at_the_reported_prefix_passes() {
        let mut doc = base();
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v2", 3, 1, 4));
        doc.read(2, get_op("zz"), (1, 0, 2), get_absent(1, 4));
        let report = k5(&doc.reads()).unwrap();
        assert!(report.passed(), "rule: K-5 pass; {report:#?}");
        assert_eq!(report.examined, 2, "two reads examined: {report:?}");
    }

    #[test]
    fn a_get_at_an_older_prefix_is_judged_at_that_prefix() {
        let mut doc = base();
        doc.read(1, get_op("k"), (2, 0, 1), get_found("v1", 2, 2, 2));
        doc.read(2, get_op("j"), (2, 0, 2), get_absent(2, 2));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-5 older prefix is legal; {report:#?}"
        );
    }

    #[test]
    fn a_stale_value_for_the_reported_prefix_fails() {
        let mut doc = base();
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v1", 2, 1, 4));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadValueWrong),
            "rule: C-3 a GET at prefix 4 sees k=v2 version 3, reported v1 version 2: {report:#?}"
        );
    }

    #[test]
    fn a_wrong_version_fails() {
        let mut doc = base();
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v2", 9, 1, 4));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadValueWrong),
            "rule: K-5 version 3 expected, 9 reported: {report:#?}"
        );
    }

    #[test]
    fn not_found_for_a_live_key_fails_and_so_does_the_reverse() {
        let mut doc = base();
        doc.read(1, get_op("k"), (1, 0, 1), get_absent(1, 4));
        doc.read(2, get_op("zz"), (1, 0, 2), get_found("v", 4, 1, 4));
        let report = k5(&doc.reads()).unwrap();
        let wrong = report
            .violations
            .iter()
            .filter(|v| v.kind == Kind::ReadValueWrong)
            .count();
        assert_eq!(wrong, 2, "rule: K-5 both answers are wrong: {report:#?}");
    }

    #[test]
    fn a_future_prefix_borrow_fails() {
        // B-06: node 2 has applied 2 entries, yet reports (and serves) prefix 4.
        let mut doc = base();
        doc.read(1, get_op("k"), (2, 0, 1), get_found("v2", 3, 2, 4));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadPrefixNotApplied),
            "rule: K-5 node 2 applied 2 but reported 4: {report:#?}"
        );
    }

    #[test]
    fn a_prefix_applied_only_after_the_answer_is_a_future_borrow() {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, "02a1", put(A, "k", "v1"));
        doc.node_state(1, 1, 2);
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v1", 2, 1, 2));
        doc.node_state(1, 2, 2);
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadPrefixNotApplied),
            "rule: K-5 applied 2 was reached only after the answer: {report:#?}"
        );
    }

    #[test]
    fn a_prefix_that_was_never_committed_fails() {
        // B-05: the node applied an entry the group never committed and published it.
        let mut doc = base();
        doc.node_state(1, 5, 5);
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v3", 5, 1, 5));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadPrefixNotCommitted),
            "rule: K-5 prefix 5 was not committed (4 entries): {report:#?}"
        );
    }

    #[test]
    fn a_prefix_not_yet_committed_at_the_answer_fails() {
        let mut doc = Doc::with_noop();
        doc.node_state(1, 2, 2);
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v1", 2, 1, 2));
        doc.commit_everywhere(2, "02a1", put(A, "k", "v1"));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadPrefixNotCommitted),
            "rule: K-5 entry 2 was committed only after the answer: {report:#?}"
        );
    }

    #[test]
    fn a_read_with_no_node_state_counts_as_applying_nothing() {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, "02a1", put(A, "k", "v1"));
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v1", 2, 1, 2));
        doc.read(2, get_op("k"), (1, 0, 2), get_absent(1, 0));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadPrefixNotApplied),
            "rule: K-5 no node_state, so applied 0 and prefix 2 is unproven: {report:#?}"
        );
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-5 an empty prefix 0 needs no node_state: {report:#?}"
        );
    }

    #[test]
    fn served_by_naming_another_node_fails() {
        let mut doc = base();
        doc.read(1, get_op("k"), (2, 0, 1), get_found("v1", 2, 1, 2));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadServedByWrongNode),
            "rule: K-5 node 2 answered, served_by names node 1: {report:#?}"
        );
    }

    #[test]
    fn a_scan_page_at_the_reported_prefix_passes() {
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&[("j", "w", 4), ("k", "v2", 3)], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(report.passed(), "rule: K-5 pass; {report:#?}");
    }

    #[test]
    fn a_page_built_from_two_prefixes_fails() {
        // B-07: `j` is from prefix 4, `k=v1` from prefix 2, under one served_by.
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&[("j", "w", 4), ("k", "v1", 2)], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: K-5 a page is one prefix; k is v2 at prefix 4: {report:#?}"
        );
    }

    #[test]
    fn a_page_omitting_a_key_of_its_prefix_fails() {
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&[("k", "v2", 3)], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: K-5 j is missing: {report:#?}"
        );
    }

    #[test]
    fn items_in_the_wrong_order_fail() {
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&[("k", "v2", 3), ("j", "w", 4)], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: F-4 keys ascend bytewise: {report:#?}"
        );
    }

    #[test]
    fn scan_bounds_are_honoured() {
        let mut doc = base();
        let both = [("j", "w", 4), ("k", "v2", 3)];
        doc.read(
            1,
            scan_op(Some("k"), None, None, None),
            (1, 0, 1),
            scan_page(&both[1..], false, None, 1, 4),
        );
        doc.read(
            2,
            scan_op(None, Some("k"), None, None),
            (1, 0, 2),
            scan_page(&both[..1], false, None, 1, 4),
        );
        doc.read(
            3,
            scan_op(None, None, Some("j"), None),
            (1, 0, 3),
            scan_page(&both[1..], false, None, 1, 4),
        );
        doc.read(
            4,
            scan_op(Some("a"), None, Some("k"), None),
            (1, 0, 4),
            scan_page(&[], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: F-4 start inclusive, end and after exclusive; {report:#?}"
        );
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, Some("k"), None, None),
            (1, 0, 1),
            scan_page(&both, false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: F-4 end is exclusive: {report:#?}"
        );
    }

    #[test]
    fn the_item_cap_and_continuation_are_checked() {
        let mut doc = base();
        let first = [("j", "w", 4)];
        doc.read(
            1,
            scan_op(None, None, None, Some(1)),
            (1, 0, 1),
            scan_page(&first, true, Some("j"), 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: F-4 limit 1 stops after j; {report:#?}"
        );
        for (has_more, next_after) in [(false, None), (true, None), (true, Some("k"))] {
            let mut doc = base();
            doc.read(
                1,
                scan_op(None, None, None, Some(1)),
                (1, 0, 1),
                scan_page(&first, has_more, next_after, 1, 4),
            );
            let report = k5(&doc.reads()).unwrap();
            assert!(
                report.has(Kind::ScanPageWrong),
                "rule: F-4 has_more={has_more} next_after={next_after:?} is not the limit-1 page: {report:#?}"
            );
        }
    }

    #[test]
    fn the_byte_cap_ends_a_page_before_the_item_that_would_exceed_it() {
        // 16 entries of 1 key byte + 65_536 value bytes: 15 fit in 1 MiB, the 16th does not.
        let value = "x".repeat(65_536);
        let keys: Vec<String> = ('a'..='p').map(String::from).collect();
        let mut doc = Doc::with_noop();
        for (offset, key) in keys.iter().enumerate() {
            let index = offset as u64 + 2;
            let id = format!("{index:032x}");
            doc.commit_everywhere(index, "00", put(&id, key, &value));
        }
        doc.node_state(1, 17, 17);
        let item = |position: usize| (keys[position].as_str(), value.as_str(), position as u64 + 2);
        let fifteen: Vec<_> = (0..15).map(item).collect();
        let mut good = doc.clone();
        good.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&fifteen, true, Some("o"), 1, 17),
        );
        let report = k5(&good.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: F-4 15 items fit 1 MiB, the 16th would exceed; {report:#?}"
        );
        let sixteen: Vec<_> = (0..16).map(item).collect();
        doc.read(
            1,
            scan_op(None, None, None, None),
            (1, 0, 1),
            scan_page(&sixteen, false, None, 1, 17),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: F-4 16 items exceed 1 MiB: {report:#?}"
        );
    }

    #[test]
    fn a_page_size_outside_one_to_128_cannot_have_been_served() {
        let mut doc = base();
        doc.read(
            1,
            scan_op(None, None, None, Some(0)),
            (1, 0, 1),
            scan_page(&[], false, None, 1, 4),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ScanPageWrong),
            "rule: F-4 limit 0 is invalid: {report:#?}"
        );
    }

    #[test]
    fn the_page_formula_agrees_with_the_models_scan() {
        let mut doc = Doc::with_noop();
        for offset in 0..40_u64 {
            let key = format!("key{:02}", (offset * 7) % 40);
            let value = "v".repeat(usize::try_from(offset).unwrap() + 1);
            doc.commit_everywhere(
                offset + 2,
                "00",
                put(&format!("{:032x}", offset + 2), &key, &value),
            );
        }
        let reads = doc.reads();
        let mut model = Model::new(reads.limits);
        for committed in &reads.committed {
            model.apply(committed.index, &committed.entry).unwrap();
        }
        for limit in [None, Some(1), Some(2), Some(7), Some(32), Some(128)] {
            for after in [None, Some("key05"), Some("key39"), Some("key055")] {
                for (start, end) in [
                    (None, None),
                    (Some("key10"), Some("key30")),
                    (Some("key39"), None),
                ] {
                    let request = ScanRequest::new(
                        start.map(|s| model::Key::new(s).unwrap()),
                        end.map(|s| model::Key::new(s).unwrap()),
                        after.map(|s| model::Key::new(s).unwrap()),
                        limit,
                    )
                    .unwrap();
                    let page = model.scan(&request).unwrap();
                    let from_model = (
                        page.items
                            .iter()
                            .map(|i| {
                                (
                                    i.key.as_str().to_owned(),
                                    i.value.as_str().to_owned(),
                                    i.version.get(),
                                )
                            })
                            .collect::<Vec<_>>(),
                        page.has_more,
                        page.next_after.map(|k| k.as_str().to_owned()),
                    );
                    let ours = expected_page(&model, (start, end, after), limit);
                    assert_eq!(
                        ours,
                        Some(from_model),
                        "rule: K-5 independent page formula equals the model's; start={start:?} end={end:?} after={after:?} limit={limit:?}"
                    );
                }
            }
        }
    }

    /// A at index 2 puts `k`; node 1 has applied both entries, node 2 only the no-op.
    fn status_doc() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, "02a1", put(A, "k", "v1"));
        doc.node_state(1, 2, 2).node_state(2, 1, 2);
        doc
    }

    #[test]
    fn status_answers_at_the_reported_prefix_pass() {
        let mut doc = status_doc();
        doc.read(
            1,
            status_op(A),
            (1, 0, 1),
            status_found(ok_put(A, 2, 2, true), 1, 2),
        );
        doc.read(2, status_op(A), (2, 0, 1), status_unknown(2, 1));
        let report = k5(&doc.reads()).unwrap();
        assert!(report.passed(), "rule: F-9 pass; {report:#?}");
    }

    #[test]
    fn unknown_where_the_prefix_executed_the_write_fails() {
        let mut doc = status_doc();
        doc.read(1, status_op(A), (1, 0, 1), status_unknown(1, 2));
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::StatusAnswerWrong),
            "rule: F-9 prefix 2 executed A, so status is known: {report:#?}"
        );
    }

    #[test]
    fn a_known_status_with_the_wrong_result_fails() {
        let mut doc = status_doc();
        doc.read(
            1,
            status_op(A),
            (1, 0, 1),
            status_found(ok_put(A, 2, 7, true), 1, 2),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::StatusAnswerWrong),
            "rule: F-9 version 2 expected, 7 reported: {report:#?}"
        );
    }

    #[test]
    fn a_known_status_below_the_executing_prefix_fails() {
        let mut doc = status_doc();
        doc.read(
            1,
            status_op(A),
            (2, 0, 1),
            status_found(ok_put(A, 2, 2, true), 2, 1),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::StatusAnswerWrong),
            "rule: F-9 prefix 1 has not executed A: {report:#?}"
        );
    }

    #[test]
    fn refused_reads_are_not_examined() {
        let mut doc = base();
        doc.read(
            1,
            get_op("k"),
            (1, 0, 1),
            serde_json::json!({"code": "busy", "served_by": null}),
        );
        let report = k5(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-5 a refusal states no prefix; {report:#?}"
        );
        assert_eq!(
            report.examined, 0,
            "rule: K-5 nothing served, nothing examined: {report:?}"
        );
    }

    #[test]
    fn a_history_without_reads_passes_with_nothing_examined() {
        let report = k5(&base().reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-5 vacuous pass is reported as 0 examined; {report:#?}"
        );
        assert_eq!(report.examined, 0, "no reads: {report:?}");
    }

    #[test]
    fn violations_come_in_record_order() {
        let mut doc = base();
        doc.read(1, get_op("k"), (1, 0, 1), get_found("v1", 2, 1, 4));
        doc.read(2, get_op("k"), (1, 0, 2), get_absent(1, 3));
        let report = k5(&doc.reads()).unwrap();
        let attempts: Vec<_> = report.violations.iter().map(|v| v.attempt).collect();
        assert_eq!(
            attempts,
            [Some(1), Some(2)],
            "rule: K-5 violations are in record order; {report:#?}"
        );
    }
}
