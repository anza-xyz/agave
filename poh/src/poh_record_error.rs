use {crate::working_bank_entry::WorkingBankEntry, crossbeam_channel::SendError, thiserror::Error};

pub(crate) type Result<T> = std::result::Result<T, PohRecordError>;

#[derive(Error, Debug, Clone)]
pub enum PohRecordError {
    #[error("max height reached")]
    MaxHeightReached,

    #[error("min height not reached")]
    MinHeightNotReached,

    #[error("send WorkingBankEntry error")]
    SendError(#[from] SendError<WorkingBankEntry>),
}
