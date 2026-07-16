use {
    crate::sockets::UNIQUE_ALLOC_BASE_PORT,
    std::{
        ops::Range,
        sync::atomic::{AtomicU16, Ordering},
    },
};

const SLICE_PER_PROCESS: u16 = (u16::MAX - UNIQUE_ALLOC_BASE_PORT) / 64;

/// When running under nextest, this will try to provide
/// a unique slice of port numbers (assuming no other nextest processes
/// are running on the same host) based on NEXTEST_TEST_GLOBAL_SLOT variable
/// The port ranges will be reused following nextest logic.
///
/// When running without nextest, this will only bump an atomic and eventually
/// panic when it runs out of port numbers to assign.
#[allow(clippy::arithmetic_side_effects)]
pub fn unique_port_range_for_tests_internal(size: u16) -> Range<u16> {
    static SLICE: AtomicU16 = AtomicU16::new(0);
    let offset = SLICE.fetch_add(size, Ordering::SeqCst);
    let start = offset
        + match std::env::var("NEXTEST_TEST_GLOBAL_SLOT") {
            Ok(slot) => {
                let slot: u16 = slot.parse().unwrap();
                assert!(
                    offset < SLICE_PER_PROCESS,
                    "Overrunning into the port range of another test! Consider using fewer ports \
                     per test."
                );
                UNIQUE_ALLOC_BASE_PORT + slot * SLICE_PER_PROCESS
            }
            Err(_) => UNIQUE_ALLOC_BASE_PORT,
        };
    assert!(start < u16::MAX - size, "Ran out of port numbers!");
    start..start + size
}
