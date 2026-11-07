//! Disposable, one-engine-per-build worker for comparative read experiments.
//!
//! JSON input/output and fixture setup are outside query timing. The controller
//! owns result validation, process-tree memory sampling, run order and scoring.

mod competitive;

fn main() {
    if let Err(error) = competitive::run() {
        eprintln!("competitive worker: {error}");
        std::process::exit(1);
    }
}
