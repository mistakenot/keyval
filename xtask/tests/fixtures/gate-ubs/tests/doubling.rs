//! UBS gate fixture: an integration test whose helper panics (CD-8: tests may).
#![cfg(test)]

fn check(observed: u32, expected: u32) {
    if observed != expected {
        panic!("observed={observed} expected={expected}");
    }
}

#[test]
fn doubles() {
    check(gate_ubs::double(2), 4);
}
