#[cfg(feature = "frozen-abi")]
use solana_frozen_abi::stable_abi;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::{
    ffi::{CStr, CString},
    path::Path,
};
use {
    crate::{
        bank::{Bank, BankFieldsToDeserialize, BankFieldsToSerialize, BankHashStats, BankRc},
        epoch_stakes::{DeserializableVersionedEpochStakes, VersionedEpochStakes},
        runtime_config::RuntimeConfig,
        snapshot_utils::StorageAndNextAccountsFileId,
        stake_account::StakeAccount,
        stakes::{
            DeserializableDelegationStakes, Stakes, serialize_stake_accounts_to_delegation_format,
        },
    },
    agave_fs::FileInfo,
    agave_snapshots::error::SnapshotError,
    bincode::{self, Error, config::Options},
    log::*,
    serde::{Deserialize, Serialize},
    smallvec::{SmallVec, smallvec},
    solana_accounts_db::{
        ObsoleteAccounts,
        account_storage_entry::AccountStorageEntry,
        accounts::Accounts,
        accounts_db::{
            AccountsDb, AccountsDbConfig, AccountsFileId, AtomicAccountsFileId, IndexGenerationInfo,
        },
        accounts_file::AccountsFile,
        accounts_hash::AccountsLtHash,
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        blockhash_queue::BlockhashQueue,
    },
    solana_clock::{Epoch, Slot, UnixTimestamp},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::FeeRateGovernor,
    solana_genesis_config::GenesisConfig,
    solana_hard_forks::HardForks,
    solana_hash::Hash,
    solana_inflation::Inflation,
    solana_lattice_hash::lt_hash::LtHash,
    solana_leader_schedule::SlotLeader,
    solana_pubkey::Pubkey,
    solana_serde::default_on_eof,
    solana_stake_interface::state::Delegation,
    std::{
        borrow::Borrow,
        collections::{HashMap, HashSet},
        io::{self, BufReader, Read, Write},
        path::PathBuf,
        result::Result,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Instant,
    },
    types::{SerdeAccountsLtHash, UnusedRentCollector},
    wincode::{
        ReadResult, SchemaRead, SchemaReadOwned, SchemaWrite, WriteResult,
        containers::FromIntoIterator,
        io::{Reader, std_write::WriteAdapter},
        len::BincodeLen,
    },
};

mod obsolete_accounts;
mod status_cache;
mod storage;
mod storages_list;
mod tests;
mod types;

pub(crate) use {
    obsolete_accounts::{SerdeObsoleteAccounts, SerdeObsoleteAccountsMap},
    status_cache::{deserialize_status_cache, serialize_status_cache},
    storage::{SerializableAccountStorageEntry, SerializedAccountsFileId},
    storages_list::{StorageListItem, StoragesList},
};

const MAX_STREAM_SIZE: usize = 32 * 1024 * 1024 * 1024;
type MaxStreamSizeConfig = wincode::config::Configuration<true, MAX_STREAM_SIZE>;

/// wincode read helpers mirroring serde's `default_on_eof` for the snapshot read-path structs.
mod wincode_compat {
    use {
        std::{marker::PhantomData, mem::MaybeUninit},
        wincode::{
            ReadError, ReadResult, SchemaRead, SchemaWrite, WriteResult,
            config::Config,
            io::{ReadError as IoReadError, Reader, Writer},
        },
    };

    /// Deserializes using `T` normally, but returns `T::Dst::default()` if the reader is
    /// exhausted (EOF), for backward compatibility when new fields are appended to a struct.
    /// Equivalent to `#[serde(deserialize_with = "default_on_eof")]`.
    pub(super) struct DefaultOnEmptyRead<T>(PhantomData<T>);

    // Note: TYPE_META is left dynamic, since during reading both 0-size or non-0-size reads are
    // allowed, so trusted readers can't rely on encoding to be static sized.
    unsafe impl<'de, C: Config, T> SchemaRead<'de, C> for DefaultOnEmptyRead<T>
    where
        T: SchemaRead<'de, C>,
        T::Dst: Default,
    {
        type Dst = T::Dst;

        fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
            match <T as SchemaRead<'de, C>>::read(reader, dst) {
                Ok(()) => Ok(()),
                Err(ReadError::Io(IoReadError::ReadSizeLimit(_))) => {
                    dst.write(Self::Dst::default());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }

    unsafe impl<C: Config, T> SchemaWrite<C> for DefaultOnEmptyRead<T>
    where
        T: SchemaWrite<C>,
    {
        type Src = T::Src;

        const TYPE_META: wincode::TypeMeta = T::TYPE_META;

        fn size_of(src: &Self::Src) -> WriteResult<usize> {
            <T as SchemaWrite<C>>::size_of(src)
        }

        fn write(writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
            <T as SchemaWrite<C>>::write(writer, src)
        }
    }
}

/// A slot paired with its account storage entries, used as the `slot -> [entry]` map item on both
/// the read path ([`AccountsDbFields`]) and the write ABI type [`SerializableAccountsDbForAbi`].
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Debug, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub(crate) struct SlotAccountStorageEntries {
    slot: Slot,
    /// In a real snapshot this always holds exactly one entry; it is sampled as an arbitrary
    /// (`0..=5`-length) collection only to keep the abi digest compatible with the current one.
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "solana_frozen_abi::stable_abi::sample_collection_sized(rng, \
                                  solana_frozen_abi::stable_abi::context::SequenceLenRange::new(0.\
                                  .=5))")
    )]
    entries: SmallVec<[SerializableAccountStorageEntry; 1]>,
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, Serialize, SchemaWrite, StableAbi, StableAbiSample)
)]
#[derive(Debug, Deserialize, SchemaRead)]
pub(crate) struct AccountsDbFields(
    Vec<SlotAccountStorageEntries>,
    u64, // unused, formerly write_version
    Slot,
    BankHashInfo,
    /// all slots that were roots within the last epoch
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<Vec<Slot>>")]
    Vec<Slot>,
    /// slots that were roots within the last epoch for which we care about the hash value
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<Vec<(Slot, Hash)>>")]
    Vec<(Slot, Hash)>,
);

#[repr(C)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default, PartialEq))]
#[derive(Serialize, Deserialize, Clone, Debug, SchemaRead, SchemaWrite)]
pub struct UnusedIncrementalSnapshotPersistence {
    pub full_slot: u64,
    pub full_hash: [u8; 32],
    pub full_capitalization: u64,
    pub incremental_hash: [u8; 32],
    pub incremental_capitalization: u64,
}

#[repr(C)]
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "EcPdH21GSyYYTiSZbAN157YfrT3G8rKvDiNh7q1fw8Bc",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "eq_and_wire"
    )
)]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq, SchemaRead, SchemaWrite)]
struct BankHashInfo {
    unused_accounts_delta_hash: [u8; 32],
    unused_accounts_hash: [u8; 32],
    stats: BankHashStats,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Default, Clone, PartialEq, Eq, Debug, Deserialize, Serialize, SchemaRead, SchemaWrite)]
struct UnusedAccounts {
    unused1: HashSet<Pubkey>,
    unused2: HashSet<Pubkey>,
    unused3: HashMap<Pubkey, u64>,
}

// Deserializable version of Bank; keep fields synced with SerializableVersionedBank.
// frozen-abi Serialize/SchemaWrite exist only to pin the read-path abi digest (see
// DeserializableBankSnapshot).
#[cfg_attr(
    feature = "frozen-abi",
    derive(Serialize, SchemaWrite, StableAbi, StableAbiSample)
)]
#[derive(Clone, Deserialize, SchemaRead)]
struct DeserializableVersionedBank {
    blockhash_queue: BlockhashQueue,
    _unused_ancestors: HashMap<Slot, usize>,
    hash: Hash,
    parent_hash: Hash,
    parent_slot: Slot,
    hard_forks: HardForks,
    transaction_count: u64,
    tick_height: u64,
    signature_count: u64,
    capitalization: u64,
    max_tick_height: u64,
    hashes_per_tick: Option<u64>,
    ticks_per_slot: u64,
    ns_per_slot: u128,
    genesis_creation_time: UnixTimestamp,
    slots_per_year: f64,
    accounts_data_len: u64,
    slot: Slot,
    _unused_epoch: Epoch,
    block_height: u64,
    leader_id: Pubkey,
    _unused_collector_fees: u64,
    _unused_fee_calculator: u64,
    fee_rate_governor: FeeRateGovernor,
    _unused_collected_rent: u64,
    _unused_rent_collector: UnusedRentCollector,
    epoch_schedule: EpochSchedule,
    inflation: Inflation,
    stakes: DeserializableDelegationStakes,
    _unused_accounts: UnusedAccounts,
    unused_epoch_stakes: HashMap<Epoch, ()>,
    is_delta: bool,
}

impl From<DeserializableVersionedBank> for BankFieldsToDeserialize {
    fn from(dvb: DeserializableVersionedBank) -> Self {
        // This serves as a canary for the LtHash.
        // If it is not replaced during deserialization, it indicates a bug.
        const LT_HASH_CANARY: LtHash = LtHash([0xCAFE; LtHash::NUM_ELEMENTS]);
        BankFieldsToDeserialize {
            blockhash_queue: dvb.blockhash_queue,
            hash: dvb.hash,
            parent_hash: dvb.parent_hash,
            parent_slot: dvb.parent_slot,
            hard_forks: dvb.hard_forks,
            transaction_count: dvb.transaction_count,
            tick_height: dvb.tick_height,
            signature_count: dvb.signature_count,
            capitalization: dvb.capitalization,
            max_tick_height: dvb.max_tick_height,
            hashes_per_tick: dvb.hashes_per_tick,
            ticks_per_slot: dvb.ticks_per_slot,
            ns_per_slot: dvb.ns_per_slot,
            genesis_creation_time: dvb.genesis_creation_time,
            slots_per_year: dvb.slots_per_year,
            accounts_data_len: dvb.accounts_data_len,
            slot: dvb.slot,
            block_height: dvb.block_height,
            leader_id: dvb.leader_id,
            fee_rate_governor: dvb.fee_rate_governor,
            epoch_schedule: dvb.epoch_schedule,
            inflation: dvb.inflation,
            stakes: dvb.stakes,
            is_delta: dvb.is_delta,
            versioned_epoch_stakes: vec![], // populated from ExtraFieldsToDeserialize
            accounts_lt_hash: AccountsLtHash(LT_HASH_CANARY), // populated from ExtraFieldsToDeserialize
            bank_hash_stats: BankHashStats::default(),        // populated from AccountsDbFields
            block_id: None, // populated from ExtraFieldsToDeserialize
        }
    }
}

// Serializable version of Bank, not Deserializable to avoid cloning by using refs.
// Sync fields with DeserializableVersionedBank!
#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    // Write-only type (its deserialize counterpart is `DeserializableVersionedBank`), so the abi
    // digest only verifies the serialized wire format; there is no roundtrip.
    frozen_abi(
        abi_digest = "7bTCffg34CBt8zAyc1H81TUazqPTUC1Xtkd597FV7wjr",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "no"
    )
)]
#[derive(Serialize, SchemaWrite)]
struct SerializableVersionedBank {
    blockhash_queue: BlockhashQueue,
    unused_ancestors: HashMap<Slot, usize>,
    hash: Hash,
    parent_hash: Hash,
    parent_slot: Slot,
    hard_forks: HardForks,
    transaction_count: u64,
    tick_height: u64,
    signature_count: u64,
    capitalization: u64,
    max_tick_height: u64,
    hashes_per_tick: Option<u64>,
    ticks_per_slot: u64,
    ns_per_slot: u128,
    genesis_creation_time: UnixTimestamp,
    slots_per_year: f64,
    accounts_data_len: u64,
    slot: Slot,
    unused_epoch: Epoch,
    block_height: u64,
    leader_id: Pubkey,
    unused_collector_fees: u64,
    unused_fee_calculator: u64,
    fee_rate_governor: FeeRateGovernor,
    unused_collected_rent: u64,
    unused_rent_collector: UnusedRentCollector,
    epoch_schedule: EpochSchedule,
    inflation: Inflation,
    #[serde(serialize_with = "serialize_stake_accounts_to_delegation_format")]
    stakes: Stakes<StakeAccount<Delegation>>,
    unused_accounts: UnusedAccounts,
    unused_epoch_stakes: HashMap<Epoch, ()>,
    is_delta: bool,
}

impl From<BankFieldsToSerialize> for SerializableVersionedBank {
    fn from(rhs: BankFieldsToSerialize) -> Self {
        Self {
            blockhash_queue: rhs.blockhash_queue,
            unused_ancestors: HashMap::default(),
            hash: rhs.hash,
            parent_hash: rhs.parent_hash,
            parent_slot: rhs.parent_slot,
            hard_forks: rhs.hard_forks,
            transaction_count: rhs.transaction_count,
            tick_height: rhs.tick_height,
            signature_count: rhs.signature_count,
            capitalization: rhs.capitalization,
            max_tick_height: rhs.max_tick_height,
            hashes_per_tick: rhs.hashes_per_tick,
            ticks_per_slot: rhs.ticks_per_slot,
            ns_per_slot: rhs.ns_per_slot,
            genesis_creation_time: rhs.genesis_creation_time,
            slots_per_year: rhs.slots_per_year,
            accounts_data_len: rhs.accounts_data_len,
            slot: rhs.slot,
            unused_epoch: 0,
            block_height: rhs.block_height,
            leader_id: rhs.leader_id,
            unused_collector_fees: 0,
            unused_fee_calculator: 0,
            fee_rate_governor: rhs.fee_rate_governor,
            unused_collected_rent: u64::default(),
            unused_rent_collector: UnusedRentCollector::zeroed(),
            epoch_schedule: rhs.epoch_schedule,
            inflation: rhs.inflation,
            stakes: rhs.stakes,
            unused_accounts: UnusedAccounts::default(),
            unused_epoch_stakes: HashMap::default(),
            is_delta: rhs.is_delta,
        }
    }
}

/// Helper type to wrap BufReader streams when deserializing and reconstructing from either just a
/// full snapshot, or both a full and incremental snapshot
pub struct SnapshotStreams<'a, R> {
    pub full_snapshot_stream: &'a mut BufReader<R>,
    pub incremental_snapshot_stream: Option<&'a mut BufReader<R>>,
}

/// Helper type to wrap BankFields when reconstructing Bank from either just a full
/// snapshot, or both a full and incremental snapshot
#[derive(Debug)]
pub struct SnapshotBankFields {
    full: BankFieldsToDeserialize,
    incremental: Option<BankFieldsToDeserialize>,
}

impl SnapshotBankFields {
    pub fn new(
        full: BankFieldsToDeserialize,
        incremental: Option<BankFieldsToDeserialize>,
    ) -> Self {
        Self { full, incremental }
    }

    /// Collapse the SnapshotBankFields into a single (the latest) BankFieldsToDeserialize.
    pub fn collapse_into(self) -> BankFieldsToDeserialize {
        self.incremental.unwrap_or(self.full)
    }
}

/// Helper type to wrap AccountsDbFields when reconstructing AccountsDb from either just a full
/// snapshot, or both a full and incremental snapshot
#[derive(Debug)]
pub struct SnapshotAccountsDbFields {
    full_snapshot_accounts_db_fields: AccountsDbFields,
    incremental_snapshot_accounts_db_fields: Option<AccountsDbFields>,
}

impl SnapshotAccountsDbFields {
    pub(crate) fn new(
        full_snapshot_accounts_db_fields: AccountsDbFields,
        incremental_snapshot_accounts_db_fields: Option<AccountsDbFields>,
    ) -> Self {
        Self {
            full_snapshot_accounts_db_fields,
            incremental_snapshot_accounts_db_fields,
        }
    }

    /// Extract final bank hash info from full and incremental accounts db fields.
    ///
    /// If there is no incremental snapshot, this returns the field from the full snapshot.
    /// Otherwise, gets it from the incremental snapshot.
    fn into_bank_hash_info(self) -> BankHashInfo {
        let AccountsDbFields(
            _snapshot_storages,
            _snapshot_write_version,
            _snapshot_slot,
            snapshot_bank_hash_info,
            _snapshot_historical_roots,
            _snapshot_historical_roots_with_hash,
        ) = self
            .incremental_snapshot_accounts_db_fields
            .unwrap_or(self.full_snapshot_accounts_db_fields);
        snapshot_bank_hash_info
    }
}

pub(crate) fn serialize_into<W, T>(writer: W, value: &T) -> WriteResult<()>
where
    W: Write,
    T: SchemaWrite<MaxStreamSizeConfig, Src = T>,
{
    wincode::config::serialize_into(WriteAdapter::new(writer), value, MaxStreamSizeConfig::new())
}

pub(crate) fn deserialize_wincode_from<'a, R, T>(reader: R) -> ReadResult<T>
where
    R: Reader<'a>,
    T: SchemaReadOwned<MaxStreamSizeConfig, Dst = T>,
{
    wincode::config::deserialize_from(reader, MaxStreamSizeConfig::new())
}

/// Extra fields that are deserialized from the end of snapshots.
///
/// Note that this struct's fields should stay synced with the fields in
/// ExtraFieldsToSerialize with the exception that new "extra fields" should be
/// added to this struct a minor release before they are added to the serialize
/// struct.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, Serialize, SchemaWrite, StableAbi, StableAbiSample)
)]
#[derive(Clone, Debug, Deserialize, SchemaRead)]
struct ExtraFieldsToDeserialize {
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<u64>")]
    lamports_per_signature: u64,
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(
        with = "wincode_compat::DefaultOnEmptyRead<Option<UnusedIncrementalSnapshotPersistence>>"
    )]
    _unused_incremental_snapshot_persistence: Option<UnusedIncrementalSnapshotPersistence>,
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<Option<Hash>>")]
    _unused_epoch_accounts_hash: Option<Hash>,
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(
        with = "wincode_compat::DefaultOnEmptyRead<Vec<(u64, DeserializableVersionedEpochStakes)>>"
    )]
    // Match the serialize side's `HashMap<u64, VersionedEpochStakes>`, which samples `0..=1` entries.
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "stable_abi::sample_collection_sized(rng, \
                                  stable_abi::context::SequenceLenMax(1))")
    )]
    versioned_epoch_stakes: Vec<(u64, DeserializableVersionedEpochStakes)>,
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<Option<SerdeAccountsLtHash>>")]
    accounts_lt_hash: Option<SerdeAccountsLtHash>,
    #[serde(deserialize_with = "default_on_eof")]
    #[wincode(with = "wincode_compat::DefaultOnEmptyRead<Option<Hash>>")]
    block_id: Option<Hash>,
}

/// Extra fields that are serialized at the end of snapshots.
///
/// Note that this struct's fields should stay synced with the fields in
/// ExtraFieldsToDeserialize with the exception that new "extra fields" should
/// be added to the deserialize struct a minor release before they are added to
/// this one.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    // Write-only type (its deserialize counterpart is `ExtraFieldsToDeserialize`), so the abi digest
    // only verifies the serialized wire format; there is no roundtrip.
    frozen_abi(
        abi_digest = "A1hmQvmrkwy33dXMpHXTweArYefPfWtsmwXK6EbNV4K6",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "no"
    )
)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default, PartialEq))]
#[derive(Debug, Serialize, SchemaWrite)]
pub struct ExtraFieldsToSerialize {
    pub lamports_per_signature: u64,
    pub unused_incremental_snapshot_persistence: Option<UnusedIncrementalSnapshotPersistence>,
    pub unused_epoch_accounts_hash: Option<Hash>,
    pub versioned_epoch_stakes: HashMap<u64, VersionedEpochStakes>,
    pub accounts_lt_hash: Option<SerdeAccountsLtHash>,
    pub block_id: Option<Hash>,
}

/// Deserializable counterpart of [`SerializableBankSnapshot`], read as one struct (wincode reads
/// the parts sequentially, matching separate reads).
///
/// Its frozen-abi digest must equal [`SerializableBankSnapshotForAbi`]'s, so the read and write
/// wire formats can't diverge.
#[cfg_attr(
    feature = "frozen-abi",
    derive(Deserialize, Serialize, SchemaWrite, StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "2TVKjhahaEGqUZAJtMmaaagcxWzhMPUsNrVHsSoNboK7",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "wire_only"
    )
)]
#[derive(SchemaRead)]
struct DeserializableBankSnapshot {
    bank: DeserializableVersionedBank,
    accounts_db: AccountsDbFields,
    extra_fields: ExtraFieldsToDeserialize,
}

impl DeserializableBankSnapshot {
    /// Folds the extra fields into the bank fields; errors if `unused_epoch_stakes` is non-empty.
    fn into_fields(self) -> wincode::ReadResult<(BankFieldsToDeserialize, AccountsDbFields)> {
        let Self {
            bank,
            accounts_db,
            extra_fields,
        } = self;
        if !bank.unused_epoch_stakes.is_empty() {
            return Err(wincode::ReadError::InvalidValue(
                "Expected deserialized bank's unused_epoch_stakes field to be empty",
            ));
        }
        let mut bank_fields = BankFieldsToDeserialize::from(bank);
        let ExtraFieldsToDeserialize {
            lamports_per_signature,
            _unused_incremental_snapshot_persistence,
            _unused_epoch_accounts_hash,
            versioned_epoch_stakes,
            accounts_lt_hash,
            block_id,
        } = extra_fields;

        bank_fields.fee_rate_governor = bank_fields
            .fee_rate_governor
            .clone_with_lamports_per_signature(lamports_per_signature);
        bank_fields.versioned_epoch_stakes = versioned_epoch_stakes;
        bank_fields.accounts_lt_hash = accounts_lt_hash
            .expect("snapshot must have accounts_lt_hash")
            .into();
        bank_fields.block_id = block_id;

        Ok((bank_fields, accounts_db))
    }
}

pub(crate) fn fields_from_stream<R: Read>(
    snapshot_stream: &mut BufReader<R>,
) -> wincode::ReadResult<(BankFieldsToDeserialize, AccountsDbFields)> {
    deserialize_wincode_from::<_, DeserializableBankSnapshot>(snapshot_stream)?.into_fields()
}

#[cfg(feature = "dev-context-only-utils")]
pub(crate) fn fields_from_streams(
    snapshot_streams: &mut SnapshotStreams<impl Read>,
) -> wincode::ReadResult<(SnapshotBankFields, SnapshotAccountsDbFields)> {
    let (full_snapshot_bank_fields, full_snapshot_accounts_db_fields) =
        fields_from_stream(snapshot_streams.full_snapshot_stream)?;
    let (incremental_snapshot_bank_fields, incremental_snapshot_accounts_db_fields) =
        snapshot_streams
            .incremental_snapshot_stream
            .as_mut()
            .map(|stream| fields_from_stream(stream))
            .transpose()?
            .unzip();

    let snapshot_bank_fields = SnapshotBankFields {
        full: full_snapshot_bank_fields,
        incremental: incremental_snapshot_bank_fields,
    };
    let snapshot_accounts_db_fields = SnapshotAccountsDbFields {
        full_snapshot_accounts_db_fields,
        incremental_snapshot_accounts_db_fields,
    };
    Ok((snapshot_bank_fields, snapshot_accounts_db_fields))
}

/// This struct contains side-info while reconstructing the bank from streams
#[derive(Debug)]
pub struct BankFromStreamsInfo {
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub calculated_accounts_lt_hash: AccountsLtHash,
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn bank_from_streams<R>(
    snapshot_streams: &mut SnapshotStreams<R>,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> std::result::Result<(Bank, BankFromStreamsInfo), SnapshotError>
where
    R: Read,
{
    let (bank_fields, accounts_db_fields) = fields_from_streams(snapshot_streams)?;
    let (bank, info) = reconstruct_bank_from_fields(
        bank_fields,
        accounts_db_fields,
        genesis_config,
        runtime_config,
        account_paths,
        storage_and_next_append_vec_id,
        debug_keys,
        None, // leader_for_tests
        limit_load_slot_count_from_snapshot,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;
    Ok((
        bank,
        BankFromStreamsInfo {
            calculated_accounts_lt_hash: info.calculated_accounts_lt_hash,
        },
    ))
}

#[cfg(test)]
pub(crate) fn bank_to_stream<W>(
    stream: &mut io::BufWriter<W>,
    bank: &Bank,
    snapshot_storages: &[Arc<AccountStorageEntry>],
) -> wincode::WriteResult<()>
where
    W: Write,
{
    let mut bank_fields = bank.get_fields_to_serialize();
    let bank_hash_stats = bank.get_bank_hash_stats();
    let lamports_per_signature = bank_fields.fee_rate_governor.lamports_per_signature;
    let versioned_epoch_stakes = std::mem::take(&mut bank_fields.versioned_epoch_stakes);
    let accounts_lt_hash = Some(bank_fields.accounts_lt_hash.clone().into());
    let block_id = Some(bank_fields.block_id);
    serialize_bank_snapshot_into_wincode(
        stream,
        bank_fields,
        bank_hash_stats,
        snapshot_storages,
        ExtraFieldsToSerialize {
            lamports_per_signature,
            unused_incremental_snapshot_persistence: None,
            unused_epoch_accounts_hash: None,
            versioned_epoch_stakes,
            accounts_lt_hash,
            block_id,
        },
    )
}

/// Serializes bank snapshot into `stream` with bincode
pub fn serialize_bank_snapshot_into(
    stream: &mut dyn Write,
    bank_fields: BankFieldsToSerialize,
    bank_hash_stats: BankHashStats,
    account_storage_entries: &[Arc<AccountStorageEntry>],
    extra_fields: ExtraFieldsToSerialize,
) -> Result<(), Error> {
    let mut serializer = bincode::Serializer::new(
        stream,
        bincode::DefaultOptions::new().with_fixint_encoding(),
    );
    serialize_bank_snapshot_with(
        &mut serializer,
        bank_fields,
        bank_hash_stats,
        account_storage_entries,
        extra_fields,
    )
}

// The full serialized form of a bank snapshot: the bank fields, the accounts db fields, and the
// extra fields, in wire order. Generic over the account-storage-entries serializer `E` so the
// runtime can stream the storage entries lazily (see `SerializableAccountsDb`).
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Serialize, SchemaWrite)]
struct SerializableBankSnapshot<E> {
    bank: SerializableVersionedBank,
    accounts_db: SerializableAccountsDb<E>,
    extra_fields: ExtraFieldsToSerialize,
}

// Concrete instantiation of `SerializableBankSnapshot` used only to pin the wire ABI; see
// `SerializableAccountsDbForAbi`. Write-only type (its deserialize counterparts are
// `DeserializableVersionedBank`, `AccountsDbFields` and `ExtraFieldsToDeserialize`), so there is no
// roundtrip.
#[cfg(all(test, feature = "frozen-abi"))]
#[frozen_abi(
    abi_digest = "2TVKjhahaEGqUZAJtMmaaagcxWzhMPUsNrVHsSoNboK7",
    abi_serializer = ["bincode", "wincode"],
    test_roundtrip = "no"
)]
type SerializableBankSnapshotForAbi = SerializableBankSnapshot<Vec<SlotAccountStorageEntries>>;

/// Serializes bank snapshot with `serializer`
pub fn serialize_bank_snapshot_with<S>(
    serializer: S,
    bank_fields: BankFieldsToSerialize,
    bank_hash_stats: BankHashStats,
    account_storage_entries: &[Arc<AccountStorageEntry>],
    extra_fields: ExtraFieldsToSerialize,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let slot = bank_fields.slot;
    let snapshot = SerializableBankSnapshot {
        bank: SerializableVersionedBank::from(bank_fields),
        accounts_db: SerializableAccountsDb::new(slot, account_storage_entries, bank_hash_stats),
        extra_fields,
    };
    // Note: the time spent here is reported by the caller (e.g. as `bank_serialize_us` in the
    // `snapshot_bank` datapoint).
    snapshot.serialize(serializer)
}

/// Serializes bank snapshot into `stream` with wincode.
///
/// Produces byte-for-byte the same output as [`serialize_bank_snapshot_into`] (which uses bincode),
/// just through the wincode serializer.
pub fn serialize_bank_snapshot_into_wincode(
    stream: &mut dyn Write,
    bank_fields: BankFieldsToSerialize,
    bank_hash_stats: BankHashStats,
    account_storage_entries: &[Arc<AccountStorageEntry>],
    extra_fields: ExtraFieldsToSerialize,
) -> wincode::WriteResult<()> {
    let slot = bank_fields.slot;
    let snapshot = SerializableBankSnapshot {
        bank: SerializableVersionedBank::from(bank_fields),
        accounts_db: SerializableAccountsDb::new(slot, account_storage_entries, bank_hash_stats),
        extra_fields,
    };
    serialize_into(stream, &snapshot)
}

// Serializable counterpart of `AccountsDbFields`, generic over the type used to serialize the
// account storage entries so that the runtime can stream them via a lazy map-serializing iterator
// (see `SerializableAccountsDb::new`) without materializing a collection. Sync fields with
// `AccountsDbFields`!
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Serialize, SchemaWrite)]
struct SerializableAccountsDb<E> {
    /// account storage entries, serialized as a map of slot to its storage entries
    accounts_storage_entries: E,
    unused_write_version: u64, // unused, formerly write_version
    slot: Slot,
    bank_hash_info: BankHashInfo,
    /// all slots that were roots within the last epoch
    historical_roots: Vec<Slot>,
    /// slots that were roots within the last epoch for which we care about the hash value
    historical_roots_with_hash: Vec<(Slot, Hash)>,
}

/// Adapts a cloneable, exact-size iterator into a value that serializes as a length-prefixed
/// sequence under *both* serde (bincode) and wincode, re-creating the iterator via `Clone` on each
/// serialization so a locally built iterator can be written without first materializing it into a
/// collection. serde maps/sequences and bincode/wincode sequences share the same wire encoding, so
/// this matches the `slot -> [entry]` map shape read back by `AccountsDbFields`.
struct SerializableExactIteratorView<I>(I);

impl<I: Iterator> IntoIterator for SerializableExactIteratorView<I> {
    type Item = I::Item;
    type IntoIter = I;

    fn into_iter(self) -> Self::IntoIter {
        self.0
    }
}

impl<I: Iterator + Clone> IntoIterator for &SerializableExactIteratorView<I> {
    type Item = I::Item;
    type IntoIter = I;

    fn into_iter(self) -> Self::IntoIter {
        self.0.clone()
    }
}

impl<I> Serialize for SerializableExactIteratorView<I>
where
    I: ExactSizeIterator + Clone,
    I::Item: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_seq(self.0.clone())
    }
}

// Serialize the wrapped iterator as a length-prefixed sequence via `FromIntoIterator`, byte-for-byte
// identical to the serde encoding above.
unsafe impl<I, C: wincode::config::Config> SchemaWrite<C> for SerializableExactIteratorView<I>
where
    I: ExactSizeIterator + Clone,
    I::Item: SchemaWrite<C> + Borrow<<I::Item as SchemaWrite<C>>::Src>,
{
    type Src = Self;

    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        <FromIntoIterator<SerializableExactIteratorView<I>, BincodeLen> as SchemaWrite<C>>::size_of(
            src,
        )
    }

    fn write(writer: impl wincode::io::Writer, src: &Self::Src) -> WriteResult<()> {
        <FromIntoIterator<SerializableExactIteratorView<I>, BincodeLen> as SchemaWrite<C>>::write(
            writer, src,
        )
    }
}

impl SerializableAccountsDb<()> {
    fn new(
        slot: Slot,
        account_storage_entries: &[Arc<AccountStorageEntry>],
        bank_hash_stats: BankHashStats,
    ) -> SerializableAccountsDb<
        SerializableExactIteratorView<
            impl ExactSizeIterator<Item = SlotAccountStorageEntries> + Clone + '_,
        >,
    > {
        // Stream the storage entries as a `slot -> [entry]` map, each slot's single entry kept
        // inline in a `SmallVec`. `SerializableExactIteratorView` re-creates the iterator on each
        // serialization, so nothing is materialized.
        let accounts_storage_entries =
            SerializableExactIteratorView(account_storage_entries.iter().map(move |entry| {
                SlotAccountStorageEntries {
                    slot: entry.slot(),
                    entries: smallvec![SerializableAccountStorageEntry::new(entry, slot)],
                }
            }));
        let bank_hash_info = BankHashInfo {
            unused_accounts_delta_hash: [0; 32],
            unused_accounts_hash: [0; 32],
            stats: bank_hash_stats,
        };
        SerializableAccountsDb {
            accounts_storage_entries,
            unused_write_version: 0,
            slot,
            bank_hash_info,
            historical_roots: Vec::default(),
            historical_roots_with_hash: Vec::default(),
        }
    }
}

// Concrete instantiation of `SerializableAccountsDb` used only to pin the wire ABI. The runtime
// serializes the storage entries with a lazy map-serializing iterator; this alias uses a `Vec` of
// the same `(slot, entries)` shape, which serializes to identical bytes. Write-only type (its
// deserialize counterpart is `AccountsDbFields`), so there is no roundtrip.
#[cfg(all(test, feature = "frozen-abi"))]
#[frozen_abi(
    abi_digest = "2TpwtsyrverM4ius4ykX3RRCGqeLfXJQbAvYHkxdUVrs",
    abi_serializer = ["bincode", "wincode"],
    test_roundtrip = "no"
)]
type SerializableAccountsDbForAbi = SerializableAccountsDb<Vec<SlotAccountStorageEntries>>;

/// This struct contains side-info while reconstructing the bank from fields
#[derive(Debug)]
pub(crate) struct ReconstructedBankInfo {
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub(crate) calculated_accounts_lt_hash: AccountsLtHash,
    /// The capitalization, in lamports, calculated during index generation.
    pub(crate) calculated_capitalization: u64,
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn reconstruct_bank_from_fields(
    bank_fields: SnapshotBankFields,
    snapshot_accounts_db_fields: SnapshotAccountsDbFields,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    leader_for_tests: Option<SlotLeader>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<(Bank, ReconstructedBankInfo), SnapshotError> {
    let mut bank_fields = bank_fields.collapse_into();
    // Epoch stakes take several seconds to reconstruct, do it in parallel with loading accountsdb
    let deserializable_epoch_stakes = std::mem::take(&mut bank_fields.versioned_epoch_stakes);
    let epoch_stakes_handle = thread::Builder::new()
        .name("solRctEpochStk".into())
        .spawn(|| {
            deserializable_epoch_stakes
                .into_iter()
                .map(|(epoch, stakes)| (epoch, stakes.into()))
                .collect()
        })?;
    let (accounts_db, reconstructed_accounts_db_info) = reconstruct_accountsdb_from_fields(
        snapshot_accounts_db_fields,
        account_paths,
        storage_and_next_append_vec_id,
        limit_load_slot_count_from_snapshot,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;
    bank_fields.bank_hash_stats = reconstructed_accounts_db_info.bank_hash_stats;

    let bank_rc = BankRc::new(Accounts::new(Arc::new(accounts_db)));
    let runtime_config = Arc::new(runtime_config.clone());
    let epoch_stakes = epoch_stakes_handle.join().expect("calculate epoch stakes");

    let bank = Bank::new_from_snapshot(
        bank_rc,
        genesis_config,
        runtime_config,
        bank_fields,
        leader_for_tests,
        debug_keys,
        reconstructed_accounts_db_info.accounts_data_len,
        epoch_stakes,
    );

    Ok((
        bank,
        ReconstructedBankInfo {
            calculated_accounts_lt_hash: reconstructed_accounts_db_info.calculated_accounts_lt_hash,
            calculated_capitalization: reconstructed_accounts_db_info.calculated_capitalization,
        },
    ))
}

pub(crate) fn reconstruct_single_storage(
    slot: &Slot,
    append_vec_file_info: FileInfo,
    id: AccountsFileId,
    obsolete_accounts: Option<(ObsoleteAccounts, AccountsFileId, usize)>,
) -> Result<Arc<AccountStorageEntry>, SnapshotError> {
    // The storage length is taken directly from the on-disk file size (see
    // `AccountsFile::new_for_startup`). When restoring from an archive the obsolete accounts have
    // been physically removed during serialization, and when restoring from a snapshot directory
    // they are still present in the file. In both cases the file size already reflects the exact
    // number of bytes the storage spans, so there is no need to carry the length separately in the
    // snapshot fields.
    //
    // When restoring from an archive, obsolete accounts will always be `None`.
    // When restoring from fastboot, obsolete accounts will be 'Some' if the storage contained
    // accounts marked obsolete at the time the snapshot was taken.
    let obsolete_accounts =
        if let Some((obsolete_accounts, obsolete_id, _obsolete_bytes)) = obsolete_accounts {
            if obsolete_id != id {
                return Err(SnapshotError::MismatchedAccountsFileId(id, obsolete_id));
            }

            obsolete_accounts
        } else {
            ObsoleteAccounts::default()
        };

    let accounts_file = AccountsFile::new_for_startup(append_vec_file_info)?;
    Ok(Arc::new(AccountStorageEntry::new_existing(
        *slot,
        id,
        accounts_file,
        obsolete_accounts,
    )))
}

// Remap the AppendVec ID to handle any duplicate IDs that may previously existed
// due to full snapshots and incremental snapshots generated from different
// nodes
pub(crate) fn remap_append_vec_file(
    slot: Slot,
    old_append_vec_id: SerializedAccountsFileId,
    append_vec_file_info: FileInfo,
    next_append_vec_id: &AtomicAccountsFileId,
    num_collisions: &mut usize,
) -> io::Result<(AccountsFileId, FileInfo)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let append_vec_path_cstr = cstring_from_path(&append_vec_file_info.path)?;

    let mut remapped_append_vec_path = append_vec_file_info.path.clone();

    // Break out of the loop in the following situations:
    // 1. The new ID is the same as the original ID.  This means we do not need to
    //    rename the file, since the ID is the "correct" one already.
    // 2. There is not a file already at the new path.  This means it is safe to
    //    rename the file to this new path.
    let (remapped_append_vec_id, remapped_append_vec_path) = loop {
        let remapped_append_vec_id = next_append_vec_id.fetch_add(1, Ordering::AcqRel);

        // this can only happen in the first iteration of the loop
        if old_append_vec_id == remapped_append_vec_id as SerializedAccountsFileId {
            break (remapped_append_vec_id, remapped_append_vec_path);
        }

        let remapped_file_name = AccountsFile::file_name(slot, remapped_append_vec_id);
        remapped_append_vec_path = remapped_append_vec_path
            .parent()
            .unwrap()
            .join(remapped_file_name);

        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            let remapped_append_vec_path_cstr = cstring_from_path(&remapped_append_vec_path)?;

            // On linux we use renameat2(NO_REPLACE) instead of IF metadata(path).is_err() THEN
            // rename() in order to save a statx() syscall.
            match rename_no_replace(&append_vec_path_cstr, &remapped_append_vec_path_cstr) {
                // If the file was successfully renamed, break out of the loop
                Ok(_) => break (remapped_append_vec_id, remapped_append_vec_path),
                // If there's already a file at the new path, continue so we try
                // the next ID
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }

        #[cfg(any(
            not(target_os = "linux"),
            all(target_os = "linux", not(target_env = "gnu"))
        ))]
        if std::fs::metadata(&remapped_append_vec_path).is_err() {
            break (remapped_append_vec_id, remapped_append_vec_path);
        }

        // If we made it this far, a file exists at the new path.  Record the collision
        // and try again.
        *num_collisions += 1;
    };

    // Only rename the file if the new ID is actually different from the original. In the target_os
    // = linux case, we have already renamed if necessary.
    #[cfg(any(
        not(target_os = "linux"),
        all(target_os = "linux", not(target_env = "gnu"))
    ))]
    if old_append_vec_id != remapped_append_vec_id as SerializedAccountsFileId {
        std::fs::rename(&append_vec_file_info.path, &remapped_append_vec_path)?;
    }

    Ok((
        remapped_append_vec_id,
        FileInfo {
            path: remapped_append_vec_path,
            ..append_vec_file_info
        },
    ))
}

pub(crate) fn remap_and_reconstruct_single_storage(
    slot: Slot,
    old_append_vec_id: SerializedAccountsFileId,
    append_vec_file_info: FileInfo,
    next_append_vec_id: &AtomicAccountsFileId,
    num_collisions: &mut usize,
) -> Result<Arc<AccountStorageEntry>, SnapshotError> {
    let (remapped_append_vec_id, remapped_append_vec_file_info) = remap_append_vec_file(
        slot,
        old_append_vec_id,
        append_vec_file_info,
        next_append_vec_id,
        num_collisions,
    )?;
    let storage = reconstruct_single_storage(
        &slot,
        remapped_append_vec_file_info,
        remapped_append_vec_id,
        None,
    )?;
    Ok(storage)
}

/// This struct contains side-info while reconstructing the accounts DB from fields.
#[derive(Debug)]
pub struct ReconstructedAccountsDbInfo {
    pub accounts_data_len: u64,
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub calculated_accounts_lt_hash: AccountsLtHash,
    /// The capitalization, in lamports, calculated during index generation.
    pub calculated_capitalization: u64,
    pub bank_hash_stats: BankHashStats,
}

fn reconstruct_accountsdb_from_fields(
    snapshot_accounts_db_fields: SnapshotAccountsDbFields,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<(AccountsDb, ReconstructedAccountsDbInfo), SnapshotError> {
    let mut accounts_db = AccountsDb::new_with_config(
        account_paths.to_vec(),
        accounts_db_config,
        accounts_update_notifier,
        exit,
    );

    let snapshot_bank_hash_info = snapshot_accounts_db_fields.into_bank_hash_info();

    // Ensure all account paths exist
    for path in &accounts_db.paths {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|err| panic!("Failed to create directory {}: {}", path.display(), err));
    }

    let StorageAndNextAccountsFileId {
        storage,
        next_append_vec_id,
    } = storage_and_next_append_vec_id;

    assert!(
        !storage.is_empty(),
        "At least one storage entry must exist from deserializing stream"
    );

    let next_append_vec_id = next_append_vec_id.load(Ordering::Acquire);
    let max_append_vec_id = next_append_vec_id - 1;
    assert!(
        max_append_vec_id <= AccountsFileId::MAX / 2,
        "Storage id {max_append_vec_id} larger than allowed max"
    );

    // Process deserialized data, set necessary fields in self
    accounts_db.storage.initialize(storage);
    accounts_db
        .next_id
        .store(next_append_vec_id, Ordering::Release);

    info!("Building accounts index...");
    let start = Instant::now();
    let IndexGenerationInfo {
        accounts_data_len,
        calculated_accounts_lt_hash,
        calculated_capitalization,
    } = accounts_db.generate_index(limit_load_slot_count_from_snapshot, verify_index);
    info!("Building accounts index... Done in {:?}", start.elapsed());

    Ok((
        accounts_db,
        ReconstructedAccountsDbInfo {
            accounts_data_len,
            calculated_accounts_lt_hash,
            calculated_capitalization,
            bank_hash_stats: snapshot_bank_hash_info.stats,
        },
    ))
}

// Rename `src` to `dest` only if `dest` doesn't already exist.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_no_replace(src: &CStr, dest: &CStr) -> io::Result<()> {
    let ret = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            src.as_ptr() as *const _,
            libc::AT_FDCWD,
            dest.as_ptr() as *const _,
            libc::RENAME_NOREPLACE,
        )
    };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn cstring_from_path(path: &Path) -> io::Result<CString> {
    // It is better to allocate here than use the stack. Jemalloc is going to give us a chunk of a
    // preallocated small arena anyway. Instead if we used the stack since PATH_MAX=4096 it would
    // result in LLVM inserting a stack probe, see
    // https://docs.rs/compiler_builtins/latest/compiler_builtins/probestack/index.html.
    CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}
