//! The tier round's span selector (ADR-0155 D1, D6) against an independent
//! prefix-sum oracle: arbitrary record lengths — empty and huge ones
//! included — a slice and a hard bound; the selector must agree with the
//! oracle, end at or before the bound, take at most `max(S, first record)`
//! bytes, and visit each record's header at most once.
#![no_main]

use inf_store::select_whole_records;
use libfuzzer_sys::fuzz_target;

/// The largest prefix of whole records whose sum fits `slice`, else the
/// first record alone; `None` when the first record is empty or crosses
/// `hard`. Complete prefix sums, bisected: no shared code with the
/// selector's walk.
fn oracle(lengths: &[u64], hard: u64, slice: u64) -> Option<u64> {
    let mut sums = vec![0u64];
    for &len in lengths {
        let Some(next) = sums.last().and_then(|sum| sum.checked_add(len)) else { break };
        if len == 0 || next > hard {
            break;
        }
        sums.push(next);
    }
    if sums.len() == 1 {
        return None;
    }
    let fit = sums.partition_point(|&sum| sum <= slice);
    Some(sums[fit.saturating_sub(1).max(1)])
}

fuzz_target!(|data: &[u8]| {
    let Some((head, rest)) = data.split_first_chunk::<8>() else { return };
    let slice = u64::from(u32::from_le_bytes([head[0], head[1], head[2], head[3]])) + 1;
    let hard_pick = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let lengths: Vec<u64> = rest
        .chunks_exact(3)
        .take(4096)
        .map(|c| u64::from(c[0]) | u64::from(c[1]) << 8 | u64::from(c[2]) << 16)
        .collect();
    if lengths.is_empty() {
        return;
    }
    let mut starts = Vec::with_capacity(lengths.len());
    let mut total = 0u64;
    for &len in &lengths {
        starts.push(total);
        total += len;
    }
    let hard = u64::from(hard_pick) % (total + 2);
    let mut visited = vec![false; lengths.len()];
    let got = select_whole_records(0, hard, slice, |addr| {
        // The first record at `addr` with a nonzero length, else an empty
        // one there, else past the records: the bytes are not records.
        let Some(index) = starts.iter().position(|&start| start == addr) else { return 0 };
        assert!(!visited[index], "a header visited twice at {addr}");
        visited[index] = true;
        lengths[index]
    });
    assert_eq!(got, oracle(&lengths, hard, slice), "{lengths:?} hard {hard} slice {slice}");
    if let Some(end) = got {
        assert!(end <= hard, "the span passes its bound");
        assert!(end <= slice.max(lengths[0]), "A > max(S, the first record)");
    }
});
