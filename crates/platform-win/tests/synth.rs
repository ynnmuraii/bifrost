use platform_win::synth::{consume_motion, expect_motions, pending};

// This integration binary has exactly one test: no other test in this process
// can alter the global synthetic-motion budget.
#[test]
fn motion_budget_consumes_once_and_retraction_requires_new_credit() {
    assert_eq!(pending(), 0);
    for _ in 0..3 {
        assert!(!consume_motion());
    }
    assert_eq!(pending(), 0);
    expect_motions(2);
    expect_motions(1);
    expect_motions(0);
    assert_eq!(pending(), 3);
    for remaining in (0..3).rev() {
        assert!(consume_motion());
        assert_eq!(pending(), remaining);
    }
    assert!(!consume_motion());
    expect_motions(3);
    expect_motions(-1);
    assert_eq!(pending(), 2);
    assert!(consume_motion());
    expect_motions(-3);
    assert_eq!(pending(), -2);
    for _ in 0..3 {
        assert!(!consume_motion());
    }
    assert_eq!(pending(), -2);
    expect_motions(1);
    assert!(!consume_motion());
    assert_eq!(pending(), -1);
    expect_motions(1);
    assert!(!consume_motion());
    assert_eq!(pending(), 0);
    expect_motions(1);
    assert!(consume_motion());
    assert!(!consume_motion());
    assert_eq!(pending(), 0);
}
