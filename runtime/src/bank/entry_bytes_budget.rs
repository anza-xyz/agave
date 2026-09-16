use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryBytesReserveError {
    ExceedsSlotLimit,
}

#[derive(Debug)]
pub struct EntryBytesBudget {
    consumed: AtomicU64,
    slot_limit: u64,
}

impl EntryBytesBudget {
    pub const fn new(slot_limit: u64) -> Self {
        Self {
            consumed: AtomicU64::new(0),
            slot_limit,
        }
    }

    pub const fn slot_limit(&self) -> u64 {
        self.slot_limit
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Acquire)
    }

    pub fn reserve(&self, bytes: u64) -> std::result::Result<(), EntryBytesReserveError> {
        let mut current = self.consumed.load(Ordering::Acquire);
        loop {
            let next = current.saturating_add(bytes);
            if next > self.slot_limit {
                return Err(EntryBytesReserveError::ExceedsSlotLimit);
            }

            match self.consumed.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TEST_SLOT_LIMIT: u64 = 1_000;

    #[test]
    fn test_load_new() {
        let budget = EntryBytesBudget::new(TEST_SLOT_LIMIT);
        assert_eq!(budget.consumed(), 0);
        assert_eq!(budget.slot_limit(), TEST_SLOT_LIMIT);
    }

    #[test]
    fn test_reserve() {
        let budget = EntryBytesBudget::new(TEST_SLOT_LIMIT);

        assert!(budget.reserve(100).is_ok());
        assert_eq!(budget.consumed(), 100);
    }

    #[test]
    fn test_reserve_rejects_over_limit() {
        let budget = EntryBytesBudget::new(TEST_SLOT_LIMIT);

        assert!(budget.reserve(TEST_SLOT_LIMIT - 1).is_ok());
        assert_eq!(
            budget.reserve(2),
            Err(EntryBytesReserveError::ExceedsSlotLimit)
        );
        assert_eq!(budget.consumed(), TEST_SLOT_LIMIT - 1);
    }
}
