#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_storage::pj1::{parse_json_text, pj1_to_text, pj1_validate};

// The PJ1 codec is a PERMANENT on-disk format decoded from bytes of unknown
// provenance (a corrupted page, a torn overflow chain). Decoding arbitrary
// bytes must NEVER panic or read out of bounds.
fuzz_target!(|data: &[u8]| {
    // 1. Structural validation of arbitrary bytes must be total (no panic/OOB).
    if pj1_validate(data).is_ok() {
        // 2. Bytes that pass validation must render to text without panicking,
        //    and that text must itself re-parse to identical canonical bytes.
        let text = pj1_to_text(data).expect("validated PJ1 must render");
        let reparsed = parse_json_text(&text).expect("canonical text must re-parse");
        assert_eq!(
            reparsed, data,
            "validated PJ1 must be a canonicalization fixpoint"
        );
    }

    // 3. Treating arbitrary bytes as JSON text (when valid UTF-8) must also be
    //    total: parse never panics, and any accepted document validates.
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(encoded) = parse_json_text(s) {
            pj1_validate(&encoded).expect("parser output must validate");
            let _ = pj1_to_text(&encoded).expect("parser output must render");
        }
    }
});
