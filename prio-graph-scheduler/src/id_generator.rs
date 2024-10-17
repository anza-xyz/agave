/// Simple reverse-sequential ID generator for `TransactionId`s.
/// These IDs uniquely identify transactions during the scheduling process.
pub struct IdGenerator {
    next_id: u64,
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self { next_id: u64::MAX }
    }
}

impl IdGenerator {
    pub fn next<T: From<u64>>(&mut self) -> T {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_sub(1);
        T::from(id)
    }
}
