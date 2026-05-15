//! The [`TransactionReturnData`] value returned at the end of a Solana
//! transaction.

use solana_address::Address;

/// Return data at the end of a transaction.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "wincode", derive(wincode::SchemaRead, wincode::SchemaWrite))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionReturnData {
    pub program_id: Address,
    pub data: Vec<u8>,
}
