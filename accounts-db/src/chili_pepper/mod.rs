mod chili_pepper_mutator_thread;

/// pub because it is used in bench.
pub mod chili_pepper_store;

pub const BLOCK_CHILI_PEPPER_LIMIT: u64 = 1024;
pub const GLOBAL_CHILI_PEPPER_CACHE_LIMIT: u64 = 1024 * 1024;

pub use chili_pepper_mutator_thread::ChiliPepperMutatorThreadCommand;
pub use chili_pepper_store::ChiliPepperStore;
