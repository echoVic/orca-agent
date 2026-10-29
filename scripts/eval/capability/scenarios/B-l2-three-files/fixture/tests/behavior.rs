use cap_eval_l2::{compute_total, render, total};

#[test]
fn total_saturates_at_i64_bounds() {
    assert_eq!(total(&[i32::MAX, i32::MAX]), 2 * i64::from(i32::MAX));
}

#[test]
fn compute_total_is_exported_and_consistent() {
    assert_eq!(compute_total(&[1, 2, 3, 4]), 10);
}

#[test]
fn render_keeps_its_format() {
    assert_eq!(render(&[2, 3]), "total=5");
}
