//! The rebaseline script consumes the comparator's stdout as the list of
//! workloads to write into `baseline/main.json`. That makes the CLI surface a
//! load-bearing interface, not a convenience, so it gets driven as a process
//! here rather than called as a function.
//!
//! The failure this guards is silent. If `--list-workloads` stopped being
//! handled, the binary would fall through to an ordinary comparison run and
//! its report would land on stdout. The script would read those report lines
//! as workload names, write `main.json` entries under invented names, and
//! every real workload would then have no baseline entry. The comparator
//! treats an unbaselined workload as a first-run capture: it prints the number
//! it measured and passes. The gate would guard nothing at all.

use std::collections::BTreeSet;
use std::process::Command;

fn baselined_workloads() -> BTreeSet<String> {
    let raw = include_str!("../baseline/main.json");
    let parsed: serde_json::Value = serde_json::from_str(raw).expect("main.json parses");
    parsed
        .get("workloads")
        .and_then(|w| w.as_object())
        .expect("main.json has a workloads object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn list_workloads_prints_exactly_the_baselined_set_one_per_line() {
    let out = Command::new(env!("CARGO_BIN_EXE_compare"))
        .arg("--list-workloads")
        .output()
        .expect("run the comparator");

    assert!(
        out.status.success(),
        "--list-workloads must exit 0, got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "nothing may go to stderr; the caller cannot tell a diagnostic from a name: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let printed: Vec<&str> = stdout.lines().collect();

    // Every line has to look like a workload id. This is the check the script
    // makes too, and it is what stops an ordinary comparator report (which is
    // non-empty, so an "is it empty" guard waves it through) from being read
    // as a list of names.
    for line in &printed {
        assert!(
            !line.is_empty()
                && line
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && line.starts_with(|c: char| c.is_ascii_lowercase()),
            "not a workload id: {line:?}"
        );
    }

    let printed_set: BTreeSet<String> = printed.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        printed_set.len(),
        printed.len(),
        "the list must not repeat a workload"
    );
    assert_eq!(
        printed_set,
        baselined_workloads(),
        "what the binary prints is what the rebaseline script writes baselines for, \
         so it must be exactly the set that has baselines today"
    );
}

#[test]
fn an_unrecognised_flag_is_refused_rather_than_ignored() {
    let out = Command::new(env!("CARGO_BIN_EXE_compare"))
        .arg("--list-workloadz")
        .output()
        .expect("run the comparator");
    assert!(
        !out.status.success(),
        "a typo in the flag must not silently run a full comparison instead"
    );
}
