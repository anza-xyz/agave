/// Identifies a bank. Unlike a `Slot`, it is unique within the process: two banks at the same
/// slot (e.g. a dumped bank and its replacement) have different ids. It is also local to the
/// process, so it is not agreed across the cluster and not stable across restarts.
pub type BankId = u64;
