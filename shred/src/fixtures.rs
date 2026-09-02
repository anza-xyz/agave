//! Test vectors: one real shred of each of the four valid layouts.
//!
//! Gated behind `dev-context-only-utils`, so this data is never compiled into a validator.
//!
//! # Provenance
//!
//! All four come from one deterministic run of `solana-ledger`'s shredder, and [`DATA_SHRED`]'s
//! bytes are exactly the vector `solana-ledger`'s `test_serde_compat_shred_data` pins. To
//! regenerate, shred 4096 bytes for slot [`FIXTURE_SLOT`] with parent `FIXTURE_SLOT - 1`, version
//! 42, reference tick 0, a default chained Merkle root and shred index [`FIXTURE_INDEX`], signed by
//! [`leader_keypair`], and take the first data and first code shred of the batch, once with
//! `is_last_in_slot` false, and once with it true.

use {bytes::Bytes, solana_keypair::Keypair, solana_pubkey::Pubkey, solana_signer::Signer};

/// Seed of the keypair that signed every fixture, drawn from `ChaChaRng::from_seed([1u8; 32])`.
/// The corresponding pubkey is `6Ciokjck2UiKvBgMkgvu2jq6FA4kN4Wr2PHaF4kYHBBD`.
pub const LEADER_SEED: [u8; 32] = [
    2, 63, 55, 32, 58, 36, 118, 196, 37, 102, 166, 28, 197, 92, 60, 168, 117, 219, 180, 204, 65,
    192, 222, 183, 137, 248, 231, 191, 136, 24, 54, 56,
];

/// Slot every fixture belongs to.
pub const FIXTURE_SLOT: u64 = 142_076_266;

/// Index every fixture carries, which is also its FEC set index: each is the first shred of its
/// batch, so its erasure shard index is 0 for a data shred and 32 for a code shred.
pub const FIXTURE_INDEX: u32 = 64;

/// A data shred: variant `0x96`.
pub const DATA_SHRED: Bytes = Bytes::from_static(include_bytes!("fixtures/data.bin"));

/// A data shred from the last batch of a slot, so its proof is followed by room for a retransmitter
/// signature: variant `0xb6`.
pub const DATA_SHRED_RESIGNED: Bytes =
    Bytes::from_static(include_bytes!("fixtures/data_resigned.bin"));

/// A code shred: variant `0x66`.
pub const CODE_SHRED: Bytes = Bytes::from_static(include_bytes!("fixtures/code.bin"));

/// A code shred from the last batch of a slot: variant `0x76`.
pub const CODE_SHRED_RESIGNED: Bytes =
    Bytes::from_static(include_bytes!("fixtures/code_resigned.bin"));

/// The keypair that signed every fixture, for tests that need to produce more of them.
pub fn leader_keypair() -> Keypair {
    Keypair::new_from_array(LEADER_SEED)
}

/// The leader that signed every fixture.
pub fn leader() -> Pubkey {
    leader_keypair().pubkey()
}
