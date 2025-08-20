use solana_lattice_hash::lt_hash::LtHash;

/// Lattice hash of an account
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AccountLtHash(pub LtHash);

/// The AccountLtHash for a zero-lamport account
pub const ZERO_LAMPORT_ACCOUNT_LT_HASH: AccountLtHash = AccountLtHash(LtHash::identity());

/// Lattice hash of all accounts
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AccountsLtHash(pub LtHash);
