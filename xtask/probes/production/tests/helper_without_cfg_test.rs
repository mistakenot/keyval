//! Probe: without `#![cfg(test)]`, a helper function in an integration-test file is not
//! test code to clippy, so `panic!` there is rejected (`clippy::panic`).
#![cfg(keyval_probe = "test_helper_without_cfg_test")]

fn helper(value: Option<u8>) -> u8 {
    value.unwrap_or_else(|| panic!("helper: no value"))
}

#[test]
fn helper_panics_on_none() {
    assert_eq!(helper(Some(1)), 1);
}
