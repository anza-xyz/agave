/// A byte that represents the version of the transaction.
#[derive(Copy, Clone, Debug, Default)]
#[repr(u8)]
pub enum TransactionVersion {
    #[default]
    Legacy = u8::MAX,
    V0 = 0,
    V1 = 1,
}

impl From<TransactionVersion> for solana_transaction::versioned::TransactionVersion {
    fn from(version: TransactionVersion) -> Self {
        match version {
            TransactionVersion::Legacy => Self::LEGACY,
            TransactionVersion::V0 => Self::Number(0),
            TransactionVersion::V1 => Self::Number(1),
        }
    }
}
