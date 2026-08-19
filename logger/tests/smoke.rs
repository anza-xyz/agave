//! End-to-end check that records reach the log file.
//!
//! The logger is process-global and `initialize_logging` repoints stderr, so all phases share a
//! single test rather than racing each other under the default parallel harness.
//!
//! Linux only: the log files are written to `/dev/shm` to keep the test off the disk.
#![cfg(all(target_os = "linux", feature = "agave-unstable-api"))]

use {
    log::info,
    std::{fs, path::PathBuf, process, thread},
};

const THREADS: usize = 8;
const RECORDS_PER_THREAD: usize = 200;
/// Records the doomed thread writes before it panics.
const RECORDS_BEFORE_PANIC: usize = 100;
const RECORDS_AFTER_PANIC: usize = 50;
/// Thread that panics partway through logging.
const DOOMED: usize = 0;

/// Removes the scratch directory even when an assertion unwinds.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn count_matching(log: &str, needle: &str) -> usize {
    log.lines().filter(|line| line.contains(needle)).count()
}

#[test]
fn smoke_logging_survives_a_panicking_thread() {
    let dir = PathBuf::from(format!("/dev/shm/agave-logger-smoke-{}", process::id()));
    fs::create_dir_all(&dir).expect("scratch dir in /dev/shm must be creatable");
    let scratch = ScratchDir(dir.clone());
    let first = dir.join("first.log");
    let second = dir.join("second.log");

    agave_logger::initialize_logging(Some(first.clone()));
    // `DEFAULT_FILTER` only covers the `solana` and `agave` targets, and this test binary is
    // neither, so widen the filter to let its own records through.
    agave_logger::setup_with("info");

    let writers: Vec<_> = (0..THREADS)
        .map(|thread_id| {
            thread::spawn(move || {
                for record in 0..RECORDS_PER_THREAD {
                    if thread_id == DOOMED && record == RECORDS_BEFORE_PANIC {
                        panic!("deliberate panic from the smoke test");
                    }
                    info!("smoke t{thread_id} r{record}");
                }
            })
        })
        .collect();
    let panicked: Vec<_> = writers
        .into_iter()
        .enumerate()
        .filter_map(|(thread_id, writer)| writer.join().is_err().then_some(thread_id))
        .collect();
    assert_eq!(
        panicked,
        vec![DOOMED],
        "only the doomed thread should have panicked"
    );

    // A panicking thread must not take the writer thread with it, so records logged afterwards
    // still have to reach the file.
    for record in 0..RECORDS_AFTER_PANIC {
        info!("after panic r{record}");
    }
    log::logger().flush();

    let logged = fs::read_to_string(&first).expect("log file must be readable");
    for thread_id in 0..THREADS {
        let expected = if thread_id == DOOMED {
            RECORDS_BEFORE_PANIC
        } else {
            RECORDS_PER_THREAD
        };
        assert_eq!(
            count_matching(&logged, &format!("smoke t{thread_id} r")),
            expected,
            "thread {thread_id} lost records"
        );
    }
    assert_eq!(
        count_matching(&logged, "after panic r"),
        RECORDS_AFTER_PANIC,
        "logger stopped working after a thread panicked"
    );

    // Reopening is an ordering barrier: records queued before it belong to the old file, and
    // everything after it to the new one.
    let lines_before_rotation = logged.lines().count();
    agave_logger::reopen(&second);
    info!("after rotation");
    log::logger().flush();

    let rotated_out = fs::read_to_string(&first).expect("log file must be readable");
    let rotated_in = fs::read_to_string(&second).expect("rotated log file must be readable");
    assert_eq!(
        rotated_out.lines().count(),
        lines_before_rotation,
        "rotation must not add to the old file"
    );
    assert_eq!(
        count_matching(&rotated_in, "after rotation"),
        1,
        "post-rotation record missing from the new file"
    );
    assert_eq!(
        count_matching(&rotated_in, "smoke t"),
        0,
        "pre-rotation records leaked into the new file"
    );

    drop(scratch);
}
