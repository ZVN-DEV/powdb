#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_sync::segment::RetainedSegment;

// Retained-unit segments are the bytes one PowDB process accepts from
// another: a replica downloads them from the primary and applies them to its
// own data dir, and the primary re-reads its own retention directory on
// start. Either way the parser runs over bytes it did not just write — a
// truncated upload, a corrupted disk, or a hostile primary all land in
// `RetainedSegment::from_bytes` first.
//
// Invariant: `from_bytes` returns `Ok` or a clean `Err`; it must never
// panic, index out of bounds, or pre-allocate from an attacker-controlled
// length field. On `Ok`, the segment must survive its own `validate` and
// `to_bytes` (re-encoding what we just accepted must be equally total).
//
// Random bytes die at the magic/CRC checks instantly, so the checked-in
// seed is a REAL segment produced by `to_bytes`. The fuzzer mutates outward
// from valid structure into the near-valid space, where the interesting
// failures live.
fuzz_target!(|data: &[u8]| {
    if let Ok(segment) = RetainedSegment::from_bytes(data) {
        let _ = segment.validate();
        let _ = segment.to_bytes();
    }
});
