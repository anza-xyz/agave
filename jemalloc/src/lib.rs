#![cfg(feature = "agave-unstable-api")]
//! Jemalloc utilities shared by Agave crates.

use crate::{group::ArenaGroup, jemalloc::Arena};

pub mod group;
pub mod jemalloc;

/// Selects a worker arena in round-robin order, reserving the first arena when the group has
/// multiple arenas.
#[allow(clippy::arithmetic_side_effects)]
pub fn arena_for_worker(arenas: &ArenaGroup, worker_index: usize) -> Arena {
    let first_worker_arena = usize::from(arenas.len() > 1);
    let worker_arenas = arenas.len() - first_worker_arena;
    let arena_index = first_worker_arena + worker_index % worker_arenas;
    arenas[arena_index]
}
