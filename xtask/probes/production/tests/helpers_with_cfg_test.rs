//! Control: an integration-test file that starts with `#![cfg(test)]` may unwrap and
//! panic in helper functions too. Clippy's `allow-*-in-tests` covers only code it sees as
//! test code: `#[test]` functions and `#[cfg(test)]` items.
#![cfg(test)]

fn helper(value: Option<u8>) -> u8 {
    value.unwrap_or_else(|| panic!("helper: no value"))
}

#[test]
fn helper_may_unwrap_and_panic() {
    assert_eq!(helper(Some(1)), 1);
    assert_eq!(std::hint::black_box(Some(2_u8)).unwrap(), 2);
}
