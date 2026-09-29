use std::sync::atomic::{AtomicU64, Ordering};

/// Identifies a bank. Unlike a `Slot`, it is unique within the process: two banks at the same
/// slot (e.g. a dumped bank and its replacement) have different ids. It is also local to the
/// process, so it is not agreed across the cluster and not stable across restarts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BankId(pub u64);

/// Hands out `BankId`s, starting at 0. A bank made from a parent shares its parent's generator,
/// so no two of them get the same id.
#[derive(Debug, Default)]
pub struct BankIdGenerator(AtomicU64);

impl BankIdGenerator {
    pub fn next(&self) -> BankId {
        BankId(self.0.fetch_add(1, Ordering::Relaxed))
    }
}
