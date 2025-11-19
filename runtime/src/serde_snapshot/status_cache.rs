//! Serialize and deserialize the status cache for snapshots

#[cfg(feature = "shuttle-test")]
use shuttle::sync::Mutex;
#[cfg(not(feature = "shuttle-test"))]
use std::sync::Mutex;
use {
    crate::{bank::BankSlotDelta, snapshot_utils, status_cache::KeySlice},
    bincode::{self, Options as _},
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_transaction_error::TransactionError,
    std::{collections::HashMap, path::Path, sync::Arc},
};

#[cfg_attr(
    feature = "frozen-abi",
    frozen_abi(digest = "AardUUq1At4qq6oNNp9V2JZFsMR5k54RZmBmZkxUfk7m")
)]
type SerdeBankSlotDelta = SerdeSlotDelta<Result<(), SerdeTransactionError>>;
type SerdeSlotDelta<T> = (Slot, bool, SerdeStatus<T>);
type SerdeStatus<T> = ahash::HashMap<Hash, (usize, Vec<(KeySlice, T)>)>;

/// Serializes the status cache's `slot_deltas` to file at `status_cache_path`
///
/// This fn serializes the status cache into the binary format required by snapshots.
pub fn serialize_status_cache(
    slot_deltas: &[BankSlotDelta],
    status_cache_path: &Path,
) -> agave_snapshots::Result<u64> {
    snapshot_utils::serialize_snapshot_data_file(status_cache_path, |stream| {
        let snapshot_slot_deltas = slot_deltas
            .iter()
            .map(|slot_delta| {
                let status_map = slot_delta.2.lock().unwrap();
                let snapshot_status_map = status_map
                    .iter()
                    .map(|(key, value)| {
                        (
                            *key,
                            (
                                value.0,
                                value
                                    .1
                                    .iter()
                                    .map(|(key_slice, result)| {
                                        (
                                            *key_slice,
                                            result.clone().map_err(SerdeTransactionError::from),
                                        )
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                (slot_delta.0, slot_delta.1, snapshot_status_map)
            })
            .collect::<Vec<_>>();
        bincode::serialize_into(stream, &snapshot_slot_deltas)?;
        Ok(())
    })
}

/// Deserializes the status cache from file at `status_cache_path`
///
/// This fn deserializes the status cache from a snapshot.
pub fn deserialize_status_cache(
    status_cache_path: &Path,
) -> agave_snapshots::Result<Vec<BankSlotDelta>> {
    snapshot_utils::deserialize_snapshot_data_file(status_cache_path, |stream| {
        let snapshot_slot_deltas: Vec<SerdeBankSlotDelta> = bincode::options()
            .with_limit(snapshot_utils::MAX_SNAPSHOT_DATA_FILE_SIZE)
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .deserialize_from(stream)?;

        let slot_deltas = snapshot_slot_deltas
            .iter()
            .map(|slot_delta| {
                let status_map = slot_delta
                    .2
                    .iter()
                    .map(|(key, value)| {
                        (
                            *key,
                            (
                                value.0,
                                value
                                    .1
                                    .iter()
                                    .map(|(key_slice, result)| {
                                        (*key_slice, result.clone().map_err(TransactionError::from))
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                        )
                    })
                    .collect::<ahash::HashMap<_, _>>();
                (slot_delta.0, slot_delta.1, Arc::new(Mutex::new(status_map)))
            })
            .collect::<Vec<_>>();
        Ok(slot_deltas)
    })
}

/// Copy of `TransactionError` that uses a different `InstructionError` type to
/// contain a string in the BorshIoError variant.
#[cfg_attr(
    feature = "frozen-abi",
    frozen_abi(digest = "5pMgydVNgsYbg64Trhjxbftsug5La7fRDmooyrsHd4wy"),
    derive(AbiExample, AbiEnumVisitor)
)]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
enum SerdeTransactionError {
    AccountInUse,
    AccountLoadedTwice,
    AccountNotFound,
    ProgramAccountNotFound,
    InsufficientFundsForFee,
    InvalidAccountForFee,
    AlreadyProcessed,
    BlockhashNotFound,
    InstructionError(u8, SerdeInstructionError),
    CallChainTooDeep,
    MissingSignatureForFee,
    InvalidAccountIndex,
    SignatureFailure,
    InvalidProgramForExecution,
    SanitizeFailure,
    ClusterMaintenance,
    AccountBorrowOutstanding,
    WouldExceedMaxBlockCostLimit,
    UnsupportedVersion,
    InvalidWritableAccount,
    WouldExceedMaxAccountCostLimit,
    WouldExceedAccountDataBlockLimit,
    TooManyAccountLocks,
    AddressLookupTableNotFound,
    InvalidAddressLookupTableOwner,
    InvalidAddressLookupTableData,
    InvalidAddressLookupTableIndex,
    InvalidRentPayingAccount,
    WouldExceedMaxVoteCostLimit,
    WouldExceedAccountDataTotalLimit,
    DuplicateInstruction(u8),
    InsufficientFundsForRent { account_index: u8 },
    MaxLoadedAccountsDataSizeExceeded,
    InvalidLoadedAccountsDataSizeLimit,
    ResanitizationNeeded,
    ProgramExecutionTemporarilyRestricted { account_index: u8 },
    UnbalancedTransaction,
    ProgramCacheHitMaxLimit,
    CommitCancelled,
}

impl From<TransactionError> for SerdeTransactionError {
    fn from(err: TransactionError) -> Self {
        match err {
            TransactionError::AccountInUse => Self::AccountInUse,
            TransactionError::AccountLoadedTwice => Self::AccountLoadedTwice,
            TransactionError::AccountNotFound => Self::AccountNotFound,
            TransactionError::ProgramAccountNotFound => Self::ProgramAccountNotFound,
            TransactionError::InsufficientFundsForFee => Self::InsufficientFundsForFee,
            TransactionError::InvalidAccountForFee => Self::InvalidAccountForFee,
            TransactionError::AlreadyProcessed => Self::AlreadyProcessed,
            TransactionError::BlockhashNotFound => Self::BlockhashNotFound,
            TransactionError::InstructionError(i, inner) => Self::InstructionError(i, inner.into()),
            TransactionError::CallChainTooDeep => Self::CallChainTooDeep,
            TransactionError::MissingSignatureForFee => Self::MissingSignatureForFee,
            TransactionError::InvalidAccountIndex => Self::InvalidAccountIndex,
            TransactionError::SignatureFailure => Self::SignatureFailure,
            TransactionError::InvalidProgramForExecution => Self::InvalidProgramForExecution,
            TransactionError::SanitizeFailure => Self::SanitizeFailure,
            TransactionError::ClusterMaintenance => Self::ClusterMaintenance,
            TransactionError::AccountBorrowOutstanding => Self::AccountBorrowOutstanding,
            TransactionError::WouldExceedMaxBlockCostLimit => Self::WouldExceedMaxBlockCostLimit,
            TransactionError::UnsupportedVersion => Self::UnsupportedVersion,
            TransactionError::InvalidWritableAccount => Self::InvalidWritableAccount,
            TransactionError::WouldExceedMaxAccountCostLimit => {
                Self::WouldExceedMaxAccountCostLimit
            }
            TransactionError::WouldExceedAccountDataBlockLimit => {
                Self::WouldExceedAccountDataBlockLimit
            }
            TransactionError::TooManyAccountLocks => Self::TooManyAccountLocks,
            TransactionError::AddressLookupTableNotFound => Self::AddressLookupTableNotFound,
            TransactionError::InvalidAddressLookupTableOwner => {
                Self::InvalidAddressLookupTableOwner
            }
            TransactionError::InvalidAddressLookupTableData => Self::InvalidAddressLookupTableData,
            TransactionError::InvalidAddressLookupTableIndex => {
                Self::InvalidAddressLookupTableIndex
            }
            TransactionError::InvalidRentPayingAccount => Self::InvalidRentPayingAccount,
            TransactionError::WouldExceedMaxVoteCostLimit => Self::WouldExceedMaxVoteCostLimit,
            TransactionError::WouldExceedAccountDataTotalLimit => {
                Self::WouldExceedAccountDataTotalLimit
            }
            TransactionError::DuplicateInstruction(i) => Self::DuplicateInstruction(i),
            TransactionError::InsufficientFundsForRent { account_index } => {
                Self::InsufficientFundsForRent { account_index }
            }
            TransactionError::MaxLoadedAccountsDataSizeExceeded => {
                Self::MaxLoadedAccountsDataSizeExceeded
            }
            TransactionError::InvalidLoadedAccountsDataSizeLimit => {
                Self::InvalidLoadedAccountsDataSizeLimit
            }
            TransactionError::ResanitizationNeeded => Self::ResanitizationNeeded,
            TransactionError::ProgramExecutionTemporarilyRestricted { account_index } => {
                Self::ProgramExecutionTemporarilyRestricted { account_index }
            }
            TransactionError::UnbalancedTransaction => Self::UnbalancedTransaction,
            TransactionError::ProgramCacheHitMaxLimit => Self::ProgramCacheHitMaxLimit,
            TransactionError::CommitCancelled => Self::CommitCancelled,
        }
    }
}

impl From<SerdeTransactionError> for TransactionError {
    fn from(err: SerdeTransactionError) -> Self {
        match err {
            SerdeTransactionError::AccountInUse => Self::AccountInUse,
            SerdeTransactionError::AccountLoadedTwice => Self::AccountLoadedTwice,
            SerdeTransactionError::AccountNotFound => Self::AccountNotFound,
            SerdeTransactionError::ProgramAccountNotFound => Self::ProgramAccountNotFound,
            SerdeTransactionError::InsufficientFundsForFee => Self::InsufficientFundsForFee,
            SerdeTransactionError::InvalidAccountForFee => Self::InvalidAccountForFee,
            SerdeTransactionError::AlreadyProcessed => Self::AlreadyProcessed,
            SerdeTransactionError::BlockhashNotFound => Self::BlockhashNotFound,
            SerdeTransactionError::InstructionError(i, inner) => {
                Self::InstructionError(i, inner.into())
            }
            SerdeTransactionError::CallChainTooDeep => Self::CallChainTooDeep,
            SerdeTransactionError::MissingSignatureForFee => Self::MissingSignatureForFee,
            SerdeTransactionError::InvalidAccountIndex => Self::InvalidAccountIndex,
            SerdeTransactionError::SignatureFailure => Self::SignatureFailure,
            SerdeTransactionError::InvalidProgramForExecution => Self::InvalidProgramForExecution,
            SerdeTransactionError::SanitizeFailure => Self::SanitizeFailure,
            SerdeTransactionError::ClusterMaintenance => Self::ClusterMaintenance,
            SerdeTransactionError::AccountBorrowOutstanding => Self::AccountBorrowOutstanding,
            SerdeTransactionError::WouldExceedMaxBlockCostLimit => {
                Self::WouldExceedMaxBlockCostLimit
            }
            SerdeTransactionError::UnsupportedVersion => Self::UnsupportedVersion,
            SerdeTransactionError::InvalidWritableAccount => Self::InvalidWritableAccount,
            SerdeTransactionError::WouldExceedMaxAccountCostLimit => {
                Self::WouldExceedMaxAccountCostLimit
            }
            SerdeTransactionError::WouldExceedAccountDataBlockLimit => {
                Self::WouldExceedAccountDataBlockLimit
            }
            SerdeTransactionError::TooManyAccountLocks => Self::TooManyAccountLocks,
            SerdeTransactionError::AddressLookupTableNotFound => Self::AddressLookupTableNotFound,
            SerdeTransactionError::InvalidAddressLookupTableOwner => {
                Self::InvalidAddressLookupTableOwner
            }
            SerdeTransactionError::InvalidAddressLookupTableData => {
                Self::InvalidAddressLookupTableData
            }
            SerdeTransactionError::InvalidAddressLookupTableIndex => {
                Self::InvalidAddressLookupTableIndex
            }
            SerdeTransactionError::InvalidRentPayingAccount => Self::InvalidRentPayingAccount,
            SerdeTransactionError::WouldExceedMaxVoteCostLimit => Self::WouldExceedMaxVoteCostLimit,
            SerdeTransactionError::WouldExceedAccountDataTotalLimit => {
                Self::WouldExceedAccountDataTotalLimit
            }
            SerdeTransactionError::DuplicateInstruction(i) => Self::DuplicateInstruction(i),
            SerdeTransactionError::InsufficientFundsForRent { account_index } => {
                Self::InsufficientFundsForRent { account_index }
            }
            SerdeTransactionError::MaxLoadedAccountsDataSizeExceeded => {
                Self::MaxLoadedAccountsDataSizeExceeded
            }
            SerdeTransactionError::InvalidLoadedAccountsDataSizeLimit => {
                Self::InvalidLoadedAccountsDataSizeLimit
            }
            SerdeTransactionError::ResanitizationNeeded => Self::ResanitizationNeeded,
            SerdeTransactionError::ProgramExecutionTemporarilyRestricted { account_index } => {
                Self::ProgramExecutionTemporarilyRestricted { account_index }
            }
            SerdeTransactionError::UnbalancedTransaction => Self::UnbalancedTransaction,
            SerdeTransactionError::ProgramCacheHitMaxLimit => Self::ProgramCacheHitMaxLimit,
            SerdeTransactionError::CommitCancelled => Self::CommitCancelled,
        }
    }
}

/// Copy of `InstructionError` type in which the `BorshIoError` variant
/// contains a string.
#[cfg_attr(test, derive(strum_macros::FromRepr, strum_macros::EnumIter))]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
enum SerdeInstructionError {
    GenericError,
    InvalidArgument,
    InvalidInstructionData,
    InvalidAccountData,
    AccountDataTooSmall,
    InsufficientFunds,
    IncorrectProgramId,
    MissingRequiredSignature,
    AccountAlreadyInitialized,
    UninitializedAccount,
    UnbalancedInstruction,
    ModifiedProgramId,
    ExternalAccountLamportSpend,
    ExternalAccountDataModified,
    ReadonlyLamportChange,
    ReadonlyDataModified,
    DuplicateAccountIndex,
    ExecutableModified,
    RentEpochModified,
    NotEnoughAccountKeys,
    AccountDataSizeChanged,
    AccountNotExecutable,
    AccountBorrowFailed,
    AccountBorrowOutstanding,
    DuplicateAccountOutOfSync,
    Custom(u32),
    InvalidError,
    ExecutableDataModified,
    ExecutableLamportChange,
    ExecutableAccountNotRentExempt,
    UnsupportedProgramId,
    CallDepth,
    MissingAccount,
    ReentrancyNotAllowed,
    MaxSeedLengthExceeded,
    InvalidSeeds,
    InvalidRealloc,
    ComputationalBudgetExceeded,
    PrivilegeEscalation,
    ProgramEnvironmentSetupFailure,
    ProgramFailedToComplete,
    ProgramFailedToCompile,
    Immutable,
    IncorrectAuthority,
    BorshIoError(String),
    AccountNotRentExempt,
    InvalidAccountOwner,
    ArithmeticOverflow,
    UnsupportedSysvar,
    IllegalOwner,
    MaxAccountsDataAllocationsExceeded,
    MaxAccountsExceeded,
    MaxInstructionTraceLengthExceeded,
    BuiltinProgramsMustConsumeComputeUnits,
}

impl From<SerdeInstructionError> for InstructionError {
    fn from(err: SerdeInstructionError) -> Self {
        match err {
            SerdeInstructionError::GenericError => Self::GenericError,
            SerdeInstructionError::InvalidArgument => Self::InvalidArgument,
            SerdeInstructionError::InvalidInstructionData => Self::InvalidInstructionData,
            SerdeInstructionError::InvalidAccountData => Self::InvalidAccountData,
            SerdeInstructionError::AccountDataTooSmall => Self::AccountDataTooSmall,
            SerdeInstructionError::InsufficientFunds => Self::InsufficientFunds,
            SerdeInstructionError::IncorrectProgramId => Self::IncorrectProgramId,
            SerdeInstructionError::MissingRequiredSignature => Self::MissingRequiredSignature,
            SerdeInstructionError::AccountAlreadyInitialized => Self::AccountAlreadyInitialized,
            SerdeInstructionError::UninitializedAccount => Self::UninitializedAccount,
            SerdeInstructionError::UnbalancedInstruction => Self::UnbalancedInstruction,
            SerdeInstructionError::ModifiedProgramId => Self::ModifiedProgramId,
            SerdeInstructionError::ExternalAccountLamportSpend => Self::ExternalAccountLamportSpend,
            SerdeInstructionError::ExternalAccountDataModified => Self::ExternalAccountDataModified,
            SerdeInstructionError::ReadonlyLamportChange => Self::ReadonlyLamportChange,
            SerdeInstructionError::ReadonlyDataModified => Self::ReadonlyDataModified,
            SerdeInstructionError::DuplicateAccountIndex => Self::DuplicateAccountIndex,
            SerdeInstructionError::ExecutableModified => Self::ExecutableModified,
            SerdeInstructionError::RentEpochModified => Self::RentEpochModified,
            #[allow(deprecated)]
            SerdeInstructionError::NotEnoughAccountKeys => Self::NotEnoughAccountKeys,
            SerdeInstructionError::AccountDataSizeChanged => Self::AccountDataSizeChanged,
            SerdeInstructionError::AccountNotExecutable => Self::AccountNotExecutable,
            SerdeInstructionError::AccountBorrowFailed => Self::AccountBorrowFailed,
            SerdeInstructionError::AccountBorrowOutstanding => Self::AccountBorrowOutstanding,
            SerdeInstructionError::DuplicateAccountOutOfSync => Self::DuplicateAccountOutOfSync,
            SerdeInstructionError::Custom(n) => Self::Custom(n),
            SerdeInstructionError::InvalidError => Self::InvalidError,
            SerdeInstructionError::ExecutableDataModified => Self::ExecutableDataModified,
            SerdeInstructionError::ExecutableLamportChange => Self::ExecutableLamportChange,
            SerdeInstructionError::ExecutableAccountNotRentExempt => {
                Self::ExecutableAccountNotRentExempt
            }
            SerdeInstructionError::UnsupportedProgramId => Self::UnsupportedProgramId,
            SerdeInstructionError::CallDepth => Self::CallDepth,
            SerdeInstructionError::MissingAccount => Self::MissingAccount,
            SerdeInstructionError::ReentrancyNotAllowed => Self::ReentrancyNotAllowed,
            SerdeInstructionError::MaxSeedLengthExceeded => Self::MaxSeedLengthExceeded,
            SerdeInstructionError::InvalidSeeds => Self::InvalidSeeds,
            SerdeInstructionError::InvalidRealloc => Self::InvalidRealloc,
            SerdeInstructionError::ComputationalBudgetExceeded => Self::ComputationalBudgetExceeded,
            SerdeInstructionError::PrivilegeEscalation => Self::PrivilegeEscalation,
            SerdeInstructionError::ProgramEnvironmentSetupFailure => {
                Self::ProgramEnvironmentSetupFailure
            }
            SerdeInstructionError::ProgramFailedToComplete => Self::ProgramFailedToComplete,
            SerdeInstructionError::ProgramFailedToCompile => Self::ProgramFailedToCompile,
            SerdeInstructionError::Immutable => Self::Immutable,
            SerdeInstructionError::IncorrectAuthority => Self::IncorrectAuthority,
            SerdeInstructionError::BorshIoError(_) => Self::BorshIoError,
            SerdeInstructionError::AccountNotRentExempt => Self::AccountNotRentExempt,
            SerdeInstructionError::InvalidAccountOwner => Self::InvalidAccountOwner,
            SerdeInstructionError::ArithmeticOverflow => Self::ArithmeticOverflow,
            SerdeInstructionError::UnsupportedSysvar => Self::UnsupportedSysvar,
            SerdeInstructionError::IllegalOwner => Self::IllegalOwner,
            SerdeInstructionError::MaxAccountsDataAllocationsExceeded => {
                Self::MaxAccountsDataAllocationsExceeded
            }
            SerdeInstructionError::MaxAccountsExceeded => Self::MaxAccountsExceeded,
            SerdeInstructionError::MaxInstructionTraceLengthExceeded => {
                Self::MaxInstructionTraceLengthExceeded
            }
            SerdeInstructionError::BuiltinProgramsMustConsumeComputeUnits => {
                Self::BuiltinProgramsMustConsumeComputeUnits
            }
        }
    }
}

impl From<InstructionError> for SerdeInstructionError {
    fn from(err: InstructionError) -> Self {
        match err {
            InstructionError::GenericError => Self::GenericError,
            InstructionError::InvalidArgument => Self::InvalidArgument,
            InstructionError::InvalidInstructionData => Self::InvalidInstructionData,
            InstructionError::InvalidAccountData => Self::InvalidAccountData,
            InstructionError::AccountDataTooSmall => Self::AccountDataTooSmall,
            InstructionError::InsufficientFunds => Self::InsufficientFunds,
            InstructionError::IncorrectProgramId => Self::IncorrectProgramId,
            InstructionError::MissingRequiredSignature => Self::MissingRequiredSignature,
            InstructionError::AccountAlreadyInitialized => Self::AccountAlreadyInitialized,
            InstructionError::UninitializedAccount => Self::UninitializedAccount,
            InstructionError::UnbalancedInstruction => Self::UnbalancedInstruction,
            InstructionError::ModifiedProgramId => Self::ModifiedProgramId,
            InstructionError::ExternalAccountLamportSpend => Self::ExternalAccountLamportSpend,
            InstructionError::ExternalAccountDataModified => Self::ExternalAccountDataModified,
            InstructionError::ReadonlyLamportChange => Self::ReadonlyLamportChange,
            InstructionError::ReadonlyDataModified => Self::ReadonlyDataModified,
            InstructionError::DuplicateAccountIndex => Self::DuplicateAccountIndex,
            InstructionError::ExecutableModified => Self::ExecutableModified,
            InstructionError::RentEpochModified => Self::RentEpochModified,
            #[allow(deprecated)]
            InstructionError::NotEnoughAccountKeys => Self::NotEnoughAccountKeys,
            InstructionError::AccountDataSizeChanged => Self::AccountDataSizeChanged,
            InstructionError::AccountNotExecutable => Self::AccountNotExecutable,
            InstructionError::AccountBorrowFailed => Self::AccountBorrowFailed,
            InstructionError::AccountBorrowOutstanding => Self::AccountBorrowOutstanding,
            InstructionError::DuplicateAccountOutOfSync => Self::DuplicateAccountOutOfSync,
            InstructionError::Custom(n) => Self::Custom(n),
            InstructionError::InvalidError => Self::InvalidError,
            InstructionError::ExecutableDataModified => Self::ExecutableDataModified,
            InstructionError::ExecutableLamportChange => Self::ExecutableLamportChange,
            InstructionError::ExecutableAccountNotRentExempt => {
                Self::ExecutableAccountNotRentExempt
            }
            InstructionError::UnsupportedProgramId => Self::UnsupportedProgramId,
            InstructionError::CallDepth => Self::CallDepth,
            InstructionError::MissingAccount => Self::MissingAccount,
            InstructionError::ReentrancyNotAllowed => Self::ReentrancyNotAllowed,
            InstructionError::MaxSeedLengthExceeded => Self::MaxSeedLengthExceeded,
            InstructionError::InvalidSeeds => Self::InvalidSeeds,
            InstructionError::InvalidRealloc => Self::InvalidRealloc,
            InstructionError::ComputationalBudgetExceeded => Self::ComputationalBudgetExceeded,
            InstructionError::PrivilegeEscalation => Self::PrivilegeEscalation,
            InstructionError::ProgramEnvironmentSetupFailure => {
                Self::ProgramEnvironmentSetupFailure
            }
            InstructionError::ProgramFailedToComplete => Self::ProgramFailedToComplete,
            InstructionError::ProgramFailedToCompile => Self::ProgramFailedToCompile,
            InstructionError::Immutable => Self::Immutable,
            InstructionError::IncorrectAuthority => Self::IncorrectAuthority,
            InstructionError::BorshIoError => Self::BorshIoError(String::new()),
            InstructionError::AccountNotRentExempt => Self::AccountNotRentExempt,
            InstructionError::InvalidAccountOwner => Self::InvalidAccountOwner,
            InstructionError::ArithmeticOverflow => Self::ArithmeticOverflow,
            InstructionError::UnsupportedSysvar => Self::UnsupportedSysvar,
            InstructionError::IllegalOwner => Self::IllegalOwner,
            InstructionError::MaxAccountsDataAllocationsExceeded => {
                Self::MaxAccountsDataAllocationsExceeded
            }
            InstructionError::MaxAccountsExceeded => Self::MaxAccountsExceeded,
            InstructionError::MaxInstructionTraceLengthExceeded => {
                Self::MaxInstructionTraceLengthExceeded
            }
            InstructionError::BuiltinProgramsMustConsumeComputeUnits => {
                Self::BuiltinProgramsMustConsumeComputeUnits
            }
        }
    }
}
