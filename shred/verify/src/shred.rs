//! The module path the shared `merkle_tree` file expects to be compiled under.

/// The Merkle tree of an erasure batch.
///
/// The file is a symlink to `ledger/src/shred/merkle_tree.rs`, so this is the very tree the cluster
/// already runs and not a reimplementation of it. It compiles unchanged in both crates because its
/// module path is the same in both, and because of the [`Error`] alias below.
///
/// Tree shape, hash prefixes and proof direction are consensus-critical, and a second
/// implementation of them, however carefully transcribed, is a second thing that can diverge. A
/// symlink cannot: it is visibly the same file, the same tests run over it in both crates, and a
/// change to it is a change to both. What the crate pays for that is this shim: an error alias, a
/// module path that matches the other crate's, a scoped lint allowance, which is a smaller price
/// than a copy.
// The shared file is written under `solana-ledger`'s lint configuration, which allows plain
// arithmetic; this crate's denies it. Scoped to the one module rather than relaxed crate-wide.
// `dead_code` is allowed for the same reason: the file carries constructors this crate reaches
// through [`merkle::tree`](crate::merkle::tree) instead, because they are not public in it.
#[allow(clippy::arithmetic_side_effects, dead_code)]
#[path = "merkle_tree.rs"]
pub mod merkle_tree;

/// The error the shared [`merkle_tree`] file raises, under the name it knows it by.
pub use crate::error::MerkleError as Error;
