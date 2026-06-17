#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_query::sql::parse_sql;

fuzz_target!(|data: &[u8]| {
    // The SQL frontend is reachable over the wire (MSG_QUERY_SQL), so its
    // from-scratch parser must never panic or overflow the stack on any input —
    // only return Ok/Err. This guards the unbounded-recursion DoS class.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = parse_sql(s);
    }
});
