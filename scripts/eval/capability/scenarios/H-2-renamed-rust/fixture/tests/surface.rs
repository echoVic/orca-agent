use cap_eval_h2::{aggregate_sum, describe, total};

#[test]
fn total_keeps_its_shape() {
    assert_eq!(total(&[i32::MAX, i32::MAX]), 2 * i64::from(i32::MAX));
}

#[test]
fn aggregate_sum_is_exported() {
    assert_eq!(aggregate_sum(&[1, 2, 3, 4]), 10);
}

#[test]
fn describe_keeps_its_format() {
    assert_eq!(describe(&[2, 3]), "total=5");
}
