use {solana_hash::Hash, solana_lattice_hash::lt_hash::LtHash};

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
