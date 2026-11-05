//! Loaded program cache.

pub mod ebpp;
pub mod entry;
pub mod env;
pub mod fork_graph;
pub mod global;
pub mod loading_task;
pub mod metrics;
pub mod owner;
pub mod tx_batch;

use solana_clock::Slot;

/// Slots between a program's deployment and when it takes effect. A program
/// deployed in slot `N` is not visible to transactions until slot `N + 1`.
pub const DELAY_VISIBILITY_SLOT_OFFSET: Slot = 1;

/// Soft limit on the number of `Loaded` entries in the cache. Eviction shrinks
/// the cache to a percentage of this count after a transaction batch, so it can
/// be exceeded in the meantime.
pub const MAX_LOADED_ENTRY_COUNT: usize = 1024;

/// Slots since its last access after which a program's sole tombstone or
/// `Unloaded` entry is pruned. 7.5 minutes at 200ms slot time.
pub const MAX_TOMBSTONE_AGE_IN_SLOTS: u64 = 2250;
