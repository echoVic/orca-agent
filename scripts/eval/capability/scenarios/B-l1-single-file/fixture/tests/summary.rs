use cap_eval_l1::{summarize, Summary};

#[test]
fn empty_slice_is_all_zeroes() {
    assert_eq!(summarize(&[]), Summary { sum: 0, min: 0, max: 0, count: 0 });
}

#[test]
fn sums_without_overflowing_i32() {
    let summary = summarize(&[i32::MAX, i32::MAX, 1]);
    assert_eq!(summary.sum, i64::from(i32::MAX) * 2 + 1);
}

#[test]
fn tracks_min_max_and_count() {
    let summary = summarize(&[5, -3, 9, 5]);
    assert_eq!(summary, Summary { sum: 16, min: -3, max: 9, count: 4 });
}

#[test]
fn single_value() {
    let summary = summarize(&[-7]);
    assert_eq!(summary, Summary { sum: -7, min: -7, max: -7, count: 1 });
}
