//! Corrupt-state fixtures for the read path's invariant (CD-7): it fires.

use types::{AttemptId, NodeId};

use super::ReadPath;

#[test]
#[should_panic(
    expected = "invariant NODE-READ-NO-SCAN violated: node 1: attempt 4 is a SCAN, \
                which no node serves before W28"
)]
fn a_scan_reaching_the_read_path_fires_node_read_no_scan() {
    ReadPath::new(NodeId::new(1).unwrap()).scan(AttemptId::new(4));
}
