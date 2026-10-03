//! The [`invariant!`](crate::invariant) macro: CD-7 assertions, checked in every build.

/// Asserts an invariant (PLAN.md CD-7). It is checked in **every** build: it expands
/// to `assert!`, never `debug_assert!`, because performance is not a v1 concern.
///
/// A violated invariant means the code is wrong, so it panics at once (CD-6). In
/// simulation that panic is a failing seed. This is the only sanctioned panic in
/// first-party production code (CD-8). `panic!`, `unwrap`, `expect`, `todo!`,
/// `unimplemented!` and `unreachable!` stay denied, and no `allow` is ever needed.
///
/// Arguments:
///
/// 1. the condition that must hold;
/// 2. the invariant ID, a string literal such as `"SM-APPLIED-LE-COMMIT"`;
/// 3. a format string and its arguments.
///
/// The format arguments must carry what a reader needs to diagnose the failure without
/// a debugger: the node, the log index and the values involved, wherever they apply.
///
/// # Panics
///
/// Panics with `invariant <ID> violated: <context>` when the condition is false.
///
/// # Examples
///
/// ```
/// let (node, applied, commit) = (3_u64, 7_u64, 9_u64);
/// types::invariant!(
///     applied <= commit,
///     "SM-APPLIED-LE-COMMIT",
///     "node {node}: applied index {applied} exceeds commit index {commit}"
/// );
/// ```
#[macro_export]
macro_rules! invariant {
    ($condition:expr, $id:literal, $($context:tt)+) => {
        ::core::assert!(
            $condition,
            "invariant {} violated: {}",
            $id,
            ::core::format_args!($($context)+)
        )
    };
}

#[cfg(test)]
mod tests {
    // check: INVARIANT

    #[test]
    fn invariant_that_holds_does_nothing() {
        let (node, applied, commit) = (1_u64, 4_u64, 4_u64);
        crate::invariant!(
            applied <= commit,
            "TEST-HOLDS",
            "node {node}: applied {applied} > commit {commit}"
        );
    }

    #[test]
    #[should_panic(
        expected = "invariant SM-APPLIED-LE-COMMIT violated: node 3: applied index 9 exceeds commit index 7"
    )]
    fn invariant_violation_panics_with_id_and_context() {
        let (node, applied, commit) = (3_u64, 9_u64, 7_u64);
        crate::invariant!(
            applied <= commit,
            "SM-APPLIED-LE-COMMIT",
            "node {node}: applied index {applied} exceeds commit index {commit}"
        );
    }

    #[test]
    fn invariant_condition_is_evaluated_exactly_once() {
        let mut evaluations = 0_u32;
        let mut check = || {
            evaluations += 1;
            true
        };
        crate::invariant!(check(), "TEST-ONCE", "condition evaluated {} times", 1);
        assert_eq!(
            evaluations, 1,
            "rule: the condition has no hidden re-evaluation; evaluations={evaluations}"
        );
    }
}
