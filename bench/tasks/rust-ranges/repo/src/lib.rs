/// Length of the union of half-open ranges. Empty and reversed ranges are ignored.
pub fn covered_length(ranges: &[(u32, u32)]) -> u64 {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut total = 0;
    let mut end = 0;
    for (start, next_end) in sorted {
        total += u64::from(next_end.saturating_sub(start.max(end)));
        end = next_end;
    }
    total
}
