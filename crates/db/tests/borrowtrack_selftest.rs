#![cfg(feature = "borrowtrack")]

use keel_db::borrowtrack::{findings, TrackedRefCell};

#[test]
fn detector_reports_a_nested_shared_borrow() {
    let c = TrackedRefCell::labelled(vec![1u8, 2, 3], "selftest_cell");
    let before = findings().len();
    let outer = c.borrow();
    let inner = c.borrow();
    assert_eq!(outer.len(), inner.len());
    drop(inner);
    drop(outer);
    let after = findings();
    assert!(
        after.len() > before,
        "detector did not report a nested shared borrow"
    );
    assert!(
        after.iter().any(|f| f.contains("selftest_cell")),
        "finding did not name the cell: {after:?}"
    );
}

#[test]
fn detector_stays_silent_on_sequential_borrows() {
    let c = TrackedRefCell::labelled(0u32, "sequential_cell");
    {
        let _a = c.borrow();
    }
    {
        let _b = c.borrow();
    }
    assert!(
        !findings().iter().any(|f| f.contains("sequential_cell")),
        "detector fired on non-nested borrows"
    );
}
