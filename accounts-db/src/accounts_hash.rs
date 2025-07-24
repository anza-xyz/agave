use {
    bytemuck_derive::{Pod, Zeroable},
    solana_hash::{Hash, HASH_BYTES},
    solana_lattice_hash::lt_hash::LtHash,
};

/// Hash of an account
#[repr(transparent)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Pod, Zeroable)]
pub struct AccountHash(pub Hash);

// Ensure the newtype wrapper never changes size from the underlying Hash
// This also ensures there are no padding bytes, which is required to safely implement Pod
const _: () = assert!(std::mem::size_of::<AccountHash>() == std::mem::size_of::<Hash>());

/// The AccountHash for a zero-lamport account
pub const ZERO_LAMPORT_ACCOUNT_HASH: AccountHash =
    AccountHash(Hash::new_from_array([0; HASH_BYTES]));

/// Lattice hash of an account
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AccountLtHash(pub LtHash);

/// The AccountLtHash for a zero-lamport account
pub const ZERO_LAMPORT_ACCOUNT_LT_HASH: AccountLtHash = AccountLtHash(LtHash::identity());

/// Lattice hash of all accounts
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AccountsLtHash(pub LtHash);

/// Snapshot serde-safe accounts hash
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerdeAccountsHash(pub Hash);

/// Snapshot serde-safe incremental accounts hash
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerdeIncrementalAccountsHash(pub Hash);
