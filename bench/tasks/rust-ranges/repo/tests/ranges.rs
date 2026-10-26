use range_set::covered_length;

#[test]
fn nested_ranges_do_not_move_the_frontier_backwards() {
    assert_eq!(covered_length(&[(0, 10), (2, 3), (4, 8), (9, 12)]), 12);
}

#[test]
fn unordered_adjacent_and_disjoint_ranges() {
    assert_eq!(covered_length(&[(20, 25), (5, 10), (0, 5), (22, 30)]), 20);
}

#[test]
fn ignores_empty_and_reversed_ranges() {
    assert_eq!(covered_length(&[(0, 10), (2, 2), (4, 1), (8, 12)]), 12);
    assert_eq!(covered_length(&[(8, 2), (5, 5)]), 0);
    assert_eq!(covered_length(&[]), 0);
}

#[test]
fn full_width_and_duplicate_ranges() {
    assert_eq!(covered_length(&[(0, u32::MAX), (0, u32::MAX)]), u64::from(u32::MAX));
}
