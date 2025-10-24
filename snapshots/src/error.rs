use {
    crate::{hardened_unpack::UnpackError, snapshot_hash::SnapshotHash},
    semver::Version,
    solana_accounts_db::{accounts_db::AccountsFileId, accounts_file::AccountsFileError},
    solana_clock::{Epoch, Slot},
    std::{io, ops::RangeInclusive, path::PathBuf, process::ExitStatus},
    thiserror::Error,
};

#[derive(Error, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("AccountsFile error: {0}")]
    AccountsFileError(#[from] AccountsFileError),

    #[error("serialization error: {0}")]
    Serialize(#[from] bincode::Error),

    #[error("crossbeam send error: {0}")]
    CrossbeamSend(#[from] crossbeam_channel::SendError<PathBuf>),

    #[error("archive generation failure {0}")]
    ArchiveGenerationFailure(ExitStatus),

    #[error("Unpack error: {0}")]
    UnpackError(#[from] UnpackError),

    #[error("source({1}) - I/O error: {0}")]
    IoWithSource(io::Error, &'static str),

    #[error("could not get file name from path '{0}'")]
    PathToFileNameError(PathBuf),

    #[error("could not get str from file name '{0}'")]
    FileNameToStrError(PathBuf),

    #[error("could not parse snapshot archive's file name '{0}'")]
    ParseSnapshotArchiveFileNameError(String),

    #[error(
        "snapshots are incompatible: full snapshot slot ({0}) and incremental snapshot base slot \
         ({1}) do not match"
    )]
    MismatchedBaseSlot(Slot, Slot),

    #[error("no snapshot archives to load from '{0}'")]
    NoSnapshotArchives(PathBuf),

    #[error("snapshot slot mismatch: deserialized bank: {0}, snapshot archive: {1}")]
    MismatchedSlot(Slot, Slot),

    #[error("snapshot hash mismatch: deserialized bank: {0:?}, snapshot archive: {1:?}")]
    MismatchedHash(SnapshotHash, SnapshotHash),

    #[error(
        "snapshot accounts file id mismatch: deserialized obsolete accounts file id: {0}, \
         snapshot archive: {1}"
    )]
    MismatchedAccountsFileId(AccountsFileId, AccountsFileId),

    #[error("snapshot slot deltas are invalid: {0}")]
    VerifySlotDeltas(#[from] VerifySlotDeltasError),

    #[error("snapshot epoch stakes are invalid: {0}")]
    VerifyEpochStakes(#[from] VerifyEpochStakesError),

    #[error("bank_snapshot_info new_from_dir failed: {0}")]
    NewFromDir(#[from] SnapshotNewFromDirError),

    #[error("invalid snapshot dir path '{0}'")]
    InvalidSnapshotDirPath(PathBuf),

    #[error("invalid AppendVec path '{0}'")]
    InvalidAppendVecPath(PathBuf),

    #[error("invalid account path '{0}'")]
    InvalidAccountPath(PathBuf),

    #[error("no valid snapshot dir found under '{0}'")]
    NoSnapshotSlotDir(PathBuf),

    #[error("snapshot dir account paths mismatching")]
    AccountPathsMismatch,

    #[error("failed to add bank snapshot for slot {1}: {0}")]
    AddBankSnapshot(#[source] AddBankSnapshotError, Slot),

    #[error("failed to archive snapshot package: {0}")]
    ArchiveSnapshotPackage(#[from] ArchiveSnapshotPackageError),

    #[error("failed to rebuild snapshot storages: {0}")]
    RebuildStorages(String),
}

#[derive(Error, Debug)]
pub enum SnapshotFastbootError {
    #[error("incompatible fastboot version '{0}'")]
    IncompatibleVersion(Version),
}

#[derive(Error, Debug)]
pub enum SnapshotNewFromDirError {
    #[error("invalid bank snapshot directory '{0}'")]
    InvalidBankSnapshotDir(PathBuf),

    #[error("missing status cache file '{0}'")]
    MissingStatusCacheFile(PathBuf),

    #[error("missing version file '{0}'")]
    MissingVersionFile(PathBuf),

    #[error("invalid snapshot version '{0}'")]
    InvalidVersion(String),

    #[error("invalid version string for fastboot '{0}'")]
    InvalidFastbootVersion(String),

    #[error("snapshot directory incomplete '{1}': {0}")]
    IncompleteDir(#[source] io::Error, PathBuf),

    #[error("missing snapshot file '{0}'")]
    MissingSnapshotFile(PathBuf),
}

/// Errors that can happen in `verify_slot_deltas()`
#[derive(Error, Debug, PartialEq, Eq)]
pub enum VerifySlotDeltasError {
    #[error("too many entries: {0} (max: {1})")]
    TooManyEntries(usize, usize),

    #[error("slot {0} is not a root")]
    SlotIsNotRoot(Slot),

    #[error("slot {0} is greater than bank slot {1}")]
    SlotGreaterThanMaxRoot(Slot, Slot),

    #[error("slot {0} has multiple entries")]
    SlotHasMultipleEntries(Slot),

    #[error("slot {0} was not found in slot history")]
    SlotNotFoundInHistory(Slot),

    #[error("slot {0} was in history but missing from slot deltas")]
    SlotNotFoundInDeltas(Slot),

    #[error("snapshot slot history is invalid: {0}")]
    VerifySlotHistory(#[from] VerifySlotHistoryError),
}

/// Errors that can happen in `verify_slot_history()`
#[derive(Error, Debug, PartialEq, Eq)]
pub enum VerifySlotHistoryError {
    #[error("newest slot does not match snapshot")]
    InvalidNewestSlot,

    #[error("invalid number of entries")]
    InvalidNumEntries,
}

/// Errors that can happen in `verify_epoch_stakes()`
#[derive(Error, Debug, PartialEq, Eq)]
pub enum VerifyEpochStakesError {
    #[error("epoch {0} is greater than the max {1}")]
    EpochGreaterThanMax(Epoch, Epoch),

    #[error("stakes not found for epoch {0} (required epochs: {1:?})")]
    StakesNotFound(Epoch, RangeInclusive<Epoch>),
}

/// Errors that can happen in `add_bank_snapshot()`
#[derive(Error, Debug)]
pub enum AddBankSnapshotError {
    #[error("bank snapshot dir already exists '{0}'")]
    SnapshotDirAlreadyExists(PathBuf),

    #[error("failed to create snapshot dir '{1}': {0}")]
    CreateSnapshotDir(#[source] io::Error, PathBuf),

    #[error("failed to flush storage '{1}': {0}")]
    FlushStorage(#[source] AccountsFileError, PathBuf),

    #[error("failed to hard link storages: {0}")]
    HardLinkStorages(#[source] HardLinkStoragesToSnapshotError),

    #[error("failed to serialize bank: {0}")]
    SerializeBank(#[source] Box<SnapshotError>),

    #[error("failed to serialize status cache: {0}")]
    SerializeStatusCache(#[source] Box<SnapshotError>),

    #[error("failed to serialize obsolete accounts: {0}")]
    SerializeObsoleteAccounts(#[source] Box<SnapshotError>),

    #[error("failed to write snapshot version file '{1}': {0}")]
    WriteSnapshotVersionFile(#[source] io::Error, PathBuf),

    #[error("failed to mark snapshot as 'loadable': {0}")]
    MarkSnapshotLoadable(#[source] io::Error),
}

/// Errors that can happen in `archive_snapshot_package()`
#[derive(Error, Debug)]
pub enum ArchiveSnapshotPackageError {
    #[error("failed to create archive path '{1}': {0}")]
    CreateArchiveDir(#[source] io::Error, PathBuf),

    #[error("failed to create staging dir inside '{1}': {0}")]
    CreateStagingDir(#[source] io::Error, PathBuf),

    #[error("failed to create snapshot staging dir '{1}': {0}")]
    CreateSnapshotStagingDir(#[source] io::Error, PathBuf),

    #[error("failed to canonicalize snapshot source dir '{1}': {0}")]
    CanonicalizeSnapshotSourceDir(#[source] io::Error, PathBuf),

    #[error("failed to symlink snapshot from '{1}' to '{2}': {0}")]
    SymlinkSnapshot(#[source] io::Error, PathBuf, PathBuf),

    #[error("failed to symlink status cache from '{1}' to '{2}': {0}")]
    SymlinkStatusCache(#[source] io::Error, PathBuf, PathBuf),

    #[error("failed to symlink version file from '{1}' to '{2}': {0}")]
    SymlinkVersionFile(#[source] io::Error, PathBuf, PathBuf),

    #[error("failed to create archive file '{1}': {0}")]
    CreateArchiveFile(#[source] io::Error, PathBuf),

    #[error("failed to archive version file: {0}")]
    ArchiveVersionFile(#[source] io::Error),

    #[error("failed to archive snapshots dir: {0}")]
    ArchiveSnapshotsDir(#[source] io::Error),

    #[error("failed to archive account storage file '{1}': {0}")]
    ArchiveAccountStorageFile(#[source] io::Error, PathBuf),

    #[error("failed to archive snapshot: {0}")]
    FinishArchive(#[source] io::Error),

    #[error("failed to create encoder: {0}")]
    CreateEncoder(#[source] io::Error),

    #[error("failed to encode archive: {0}")]
    FinishEncoder(#[source] io::Error),

    #[error("failed to query archive metadata '{1}': {0}")]
    QueryArchiveMetadata(#[source] io::Error, PathBuf),

    #[error("failed to move archive from '{1}' to '{2}': {0}")]
    MoveArchive(#[source] io::Error, PathBuf, PathBuf),

    #[error("failed to create account storage reader '{1}': {0}")]
    AccountStorageReaderError(#[source] io::Error, PathBuf),
}

/// Errors that can happen in `hard_link_storages_to_snapshot()`
#[derive(Error, Debug)]
pub enum HardLinkStoragesToSnapshotError {
    #[error("failed to create accounts hard links dir '{1}': {0}")]
    CreateAccountsHardLinksDir(#[source] io::Error, PathBuf),

    #[error("failed to get the snapshot's accounts hard link dir: {0}")]
    GetSnapshotHardLinksDir(#[from] GetSnapshotAccountsHardLinkDirError),

    #[error("failed to hard link storage from '{1}' to '{2}': {0}")]
    HardLinkStorage(#[source] io::Error, PathBuf, PathBuf),
}

/// Errors that can happen in `get_snapshot_accounts_hardlink_dir()`
#[derive(Error, Debug)]
pub enum GetSnapshotAccountsHardLinkDirError {
    #[error("invalid account storage path '{0}'")]
    GetAccountPath(PathBuf),

    #[error("failed to create the snapshot hard link dir '{1}': {0}")]
    CreateSnapshotHardLinkDir(#[source] io::Error, PathBuf),

    #[error("failed to symlink snapshot hard link dir '{link}' to '{original}': {source}")]
    SymlinkSnapshotHardLinkDir {
        source: io::Error,
        original: PathBuf,
        link: PathBuf,
    },
}
