#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::ffi::{CStr, CString};
use {
    crate::{
        bank::{Bank, BankFieldsToDeserialize, BankFieldsToSerialize, BankHashStats, BankRc},
        epoch_stakes::VersionedEpochStakes,
        rent_collector::RentCollector,
        runtime_config::RuntimeConfig,
        snapshot_utils::StorageAndNextAccountsFileId,
        stake_account::StakeAccount,
        stakes::{serialize_stake_accounts_to_delegation_format, Stakes},
    },
    agave_snapshots::error::SnapshotError,
    bincode::{self, config::Options, Error},
    log::*,
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    solana_accounts_db::{
        accounts::Accounts,
        accounts_db::{
            AccountStorageEntry, AccountsDb, AccountsDbConfig, AccountsFileId,
            AtomicAccountsFileId, IndexGenerationInfo,
        },
        accounts_file::{AccountsFile, StorageAccess},
        accounts_hash::AccountsLtHash,
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        ancestors::AncestorsForSerialization,
        blockhash_queue::BlockhashQueue,
        ObsoleteAccounts,
    },
    solana_clock::{Epoch, Slot, UnixTimestamp},
    solana_epoch_schedule::EpochSchedule,
    solana_fee_calculator::{FeeCalculator, FeeRateGovernor},
    solana_genesis_config::GenesisConfig,
    solana_hard_forks::HardForks,
    solana_hash::Hash,
    solana_inflation::Inflation,
    solana_lattice_hash::lt_hash::LtHash,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_serde::default_on_eof,
    solana_stake_interface::state::Delegation,
    std::{
        cell::RefCell,
        collections::{HashMap, HashSet},
        io::{self, BufReader, BufWriter, Read, Write},
        path::{Path, PathBuf},
        result::Result,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Instant,
    },
    storage::SerializableStorage,
    types::SerdeAccountsLtHash,
};

mod obsolete_accounts;
mod status_cache;
mod storage;
mod tests;
mod types;
mod utils;

pub(crate) use {
    obsolete_accounts::SerdeObsoleteAccountsMap,
    status_cache::{deserialize_status_cache, serialize_status_cache},
    storage::{SerializableAccountStorageEntry, SerializedAccountsFileId},
};

const MAX_STREAM_SIZE: u64 = 32 * 1024 * 1024 * 1024;

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountsDbFields<T>(
    HashMap<Slot, Vec<T>>,
    u64, // obsolete, formerly write_version
    Slot,
    BankHashInfo,
    /// all slots that were roots within the last epoch
    #[serde(deserialize_with = "default_on_eof")]
    Vec<Slot>,
    /// slots that were roots within the last epoch for which we care about the hash value
    #[serde(deserialize_with = "default_on_eof")]
    Vec<(Slot, Hash)>,
);

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default, PartialEq))]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ObsoleteIncrementalSnapshotPersistence {
    pub full_slot: u64,
    pub full_hash: [u8; 32],
    pub full_capitalization: u64,
    pub incremental_hash: [u8; 32],
    pub incremental_capitalization: u64,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct BankHashInfo {
    obsolete_accounts_delta_hash: [u8; 32],
    obsolete_accounts_hash: [u8; 32],
    stats: BankHashStats,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Default, Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
struct UnusedAccounts {
    unused1: HashSet<Pubkey>,
    unused2: HashSet<Pubkey>,
    unused3: HashMap<Pubkey, u64>,
}

// Deserializable version of Bank which need not be serializable,
// because it's handled by SerializableVersionedBank.
// So, sync fields with it!
#[derive(Clone, Deserialize)]
struct DeserializableVersionedBank {
    blockhash_queue: BlockhashQueue,
    ancestors: AncestorsForSerialization,
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
    epoch: Epoch,
    block_height: u64,
    collector_id: Pubkey,
    collector_fees: u64,
    _fee_calculator: FeeCalculator,
    fee_rate_governor: FeeRateGovernor,
    _collected_rent: u64,
    rent_collector: RentCollector,
    epoch_schedule: EpochSchedule,
    inflation: Inflation,
    stakes: Stakes<Delegation>,
    #[allow(dead_code)]
    unused_accounts: UnusedAccounts,
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
            ancestors: dvb.ancestors,
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
            epoch: dvb.epoch,
            block_height: dvb.block_height,
            collector_id: dvb.collector_id,
            collector_fees: dvb.collector_fees,
            fee_rate_governor: dvb.fee_rate_governor,
            rent_collector: dvb.rent_collector,
            epoch_schedule: dvb.epoch_schedule,
            inflation: dvb.inflation,
            stakes: dvb.stakes,
            is_delta: dvb.is_delta,
            versioned_epoch_stakes: HashMap::default(), // populated from ExtraFieldsToDeserialize
            accounts_lt_hash: AccountsLtHash(LT_HASH_CANARY), // populated from ExtraFieldsToDeserialize
            bank_hash_stats: BankHashStats::default(),        // populated from AccountsDbFields
        }
    }
}

// Serializable version of Bank, not Deserializable to avoid cloning by using refs.
// Sync fields with DeserializableVersionedBank!
#[derive(Serialize)]
struct SerializableVersionedBank {
    blockhash_queue: BlockhashQueue,
    ancestors: AncestorsForSerialization,
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
    epoch: Epoch,
    block_height: u64,
    collector_id: Pubkey,
    collector_fees: u64,
    fee_calculator: FeeCalculator,
    fee_rate_governor: FeeRateGovernor,
    collected_rent: u64,
    rent_collector: RentCollector,
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
            ancestors: rhs.ancestors,
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
            epoch: rhs.epoch,
            block_height: rhs.block_height,
            collector_id: rhs.collector_id,
            collector_fees: rhs.collector_fees,
            fee_calculator: FeeCalculator::default(),
            fee_rate_governor: rhs.fee_rate_governor,
            collected_rent: u64::default(),
            rent_collector: rhs.rent_collector,
            epoch_schedule: rhs.epoch_schedule,
            inflation: rhs.inflation,
            stakes: rhs.stakes,
            unused_accounts: UnusedAccounts::default(),
            unused_epoch_stakes: HashMap::default(),
            is_delta: rhs.is_delta,
        }
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::TransparentAsHelper for SerializableVersionedBank {}

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
pub struct SnapshotAccountsDbFields<T> {
    full_snapshot_accounts_db_fields: AccountsDbFields<T>,
    incremental_snapshot_accounts_db_fields: Option<AccountsDbFields<T>>,
}

impl<T> SnapshotAccountsDbFields<T> {
    pub fn new(
        full_snapshot_accounts_db_fields: AccountsDbFields<T>,
        incremental_snapshot_accounts_db_fields: Option<AccountsDbFields<T>>,
    ) -> Self {
        Self {
            full_snapshot_accounts_db_fields,
            incremental_snapshot_accounts_db_fields,
        }
    }

    /// Collapse the SnapshotAccountsDbFields into a single AccountsDbFields.  If there is no
    /// incremental snapshot, this returns the AccountsDbFields from the full snapshot.
    /// Otherwise, use the AccountsDbFields from the incremental snapshot, and a combination
    /// of the storages from both the full and incremental snapshots.
    pub fn collapse_into(self) -> Result<AccountsDbFields<T>, Error> {
        match self.incremental_snapshot_accounts_db_fields {
            None => Ok(self.full_snapshot_accounts_db_fields),
            Some(AccountsDbFields(
                mut incremental_snapshot_storages,
                incremental_snapshot_version,
                incremental_snapshot_slot,
                incremental_snapshot_bank_hash_info,
                incremental_snapshot_historical_roots,
                incremental_snapshot_historical_roots_with_hash,
            )) => {
                let full_snapshot_storages = self.full_snapshot_accounts_db_fields.0;
                let full_snapshot_slot = self.full_snapshot_accounts_db_fields.2;

                // filter out incremental snapshot storages with slot <= full snapshot slot
                incremental_snapshot_storages.retain(|slot, _| *slot > full_snapshot_slot);

                // There must not be any overlap in the slots of storages between the full snapshot and the incremental snapshot
                incremental_snapshot_storages
                    .iter()
                    .all(|storage_entry| !full_snapshot_storages.contains_key(storage_entry.0))
                    .then_some(())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Snapshots are incompatible: There are storages for the same slot in \
                             both the full snapshot and the incremental snapshot!",
                        )
                    })?;

                let mut combined_storages = full_snapshot_storages;
                combined_storages.extend(incremental_snapshot_storages);

                Ok(AccountsDbFields(
                    combined_storages,
                    incremental_snapshot_version,
                    incremental_snapshot_slot,
                    incremental_snapshot_bank_hash_info,
                    incremental_snapshot_historical_roots,
                    incremental_snapshot_historical_roots_with_hash,
                ))
            }
        }
    }
}

pub(crate) fn serialize_into<W, T>(writer: W, value: &T) -> bincode::Result<()>
where
    W: Write,
    T: Serialize,
{
    bincode::options()
        .with_fixint_encoding()
        .with_limit(MAX_STREAM_SIZE)
        .serialize_into(writer, value)
}

pub(crate) fn deserialize_from<R, T>(reader: R) -> bincode::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    bincode::options()
        .with_limit(MAX_STREAM_SIZE)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from::<R, T>(reader)
}

fn deserialize_accounts_db_fields<R>(
    stream: &mut BufReader<R>,
) -> Result<AccountsDbFields<SerializableAccountStorageEntry>, Error>
where
    R: Read,
{
    deserialize_from::<_, _>(stream)
}

/// Extra fields that are deserialized from the end of snapshots.
///
/// Note that this struct's fields should stay synced with the fields in
/// ExtraFieldsToSerialize with the exception that new "extra fields" should be
/// added to this struct a minor release before they are added to the serialize
/// struct.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
#[derive(Clone, Debug, Deserialize)]
struct ExtraFieldsToDeserialize {
    #[serde(deserialize_with = "default_on_eof")]
    lamports_per_signature: u64,
    #[serde(deserialize_with = "default_on_eof")]
    _obsolete_incremental_snapshot_persistence: Option<ObsoleteIncrementalSnapshotPersistence>,
    #[serde(deserialize_with = "default_on_eof")]
    _obsolete_epoch_accounts_hash: Option<Hash>,
    #[serde(deserialize_with = "default_on_eof")]
    versioned_epoch_stakes: HashMap<u64, VersionedEpochStakes>,
    #[serde(deserialize_with = "default_on_eof")]
    accounts_lt_hash: Option<SerdeAccountsLtHash>,
}

/// Extra fields that are serialized at the end of snapshots.
///
/// Note that this struct's fields should stay synced with the fields in
/// ExtraFieldsToDeserialize with the exception that new "extra fields" should
/// be added to the deserialize struct a minor release before they are added to
/// this one.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default, PartialEq))]
#[derive(Debug, Serialize)]
pub struct ExtraFieldsToSerialize {
    pub lamports_per_signature: u64,
    pub obsolete_incremental_snapshot_persistence: Option<ObsoleteIncrementalSnapshotPersistence>,
    pub obsolete_epoch_accounts_hash: Option<Hash>,
    pub versioned_epoch_stakes: HashMap<u64, VersionedEpochStakes>,
    pub accounts_lt_hash: Option<SerdeAccountsLtHash>,
}

fn deserialize_bank_fields<R>(
    mut stream: &mut BufReader<R>,
) -> Result<
    (
        BankFieldsToDeserialize,
        AccountsDbFields<SerializableAccountStorageEntry>,
    ),
    Error,
>
where
    R: Read,
{
    let deserializable_bank = deserialize_from::<_, DeserializableVersionedBank>(&mut stream)?;
    if !deserializable_bank.unused_epoch_stakes.is_empty() {
        return Err(Box::new(bincode::ErrorKind::Custom(
            "Expected deserialized bank's unused_epoch_stakes field to be empty".to_string(),
        )));
    }
    let mut bank_fields = BankFieldsToDeserialize::from(deserializable_bank);
    let accounts_db_fields = deserialize_accounts_db_fields(stream)?;
    let extra_fields = deserialize_from(stream)?;

    // Process extra fields
    let ExtraFieldsToDeserialize {
        lamports_per_signature,
        _obsolete_incremental_snapshot_persistence,
        _obsolete_epoch_accounts_hash,
        versioned_epoch_stakes,
        accounts_lt_hash,
    } = extra_fields;

    bank_fields.fee_rate_governor = bank_fields
        .fee_rate_governor
        .clone_with_lamports_per_signature(lamports_per_signature);
    bank_fields.versioned_epoch_stakes = versioned_epoch_stakes;
    bank_fields.accounts_lt_hash = accounts_lt_hash
        .expect("snapshot must have accounts_lt_hash")
        .into();

    Ok((bank_fields, accounts_db_fields))
}

/// Get snapshot storage lengths from accounts_db_fields
pub(crate) fn snapshot_storage_lengths_from_fields(
    accounts_db_fields: &AccountsDbFields<SerializableAccountStorageEntry>,
) -> HashMap<Slot, HashMap<SerializedAccountsFileId, usize>> {
    let AccountsDbFields(snapshot_storage, ..) = &accounts_db_fields;
    snapshot_storage
        .iter()
        .map(|(slot, slot_storage)| {
            (
                *slot,
                slot_storage
                    .iter()
                    .map(|storage_entry| (storage_entry.id(), storage_entry.current_len()))
                    .collect(),
            )
        })
        .collect()
}

pub(crate) fn fields_from_stream<R: Read>(
    snapshot_stream: &mut BufReader<R>,
) -> std::result::Result<
    (
        BankFieldsToDeserialize,
        AccountsDbFields<SerializableAccountStorageEntry>,
    ),
    Error,
> {
    deserialize_bank_fields(snapshot_stream)
}

#[cfg(feature = "dev-context-only-utils")]
pub(crate) fn fields_from_streams(
    snapshot_streams: &mut SnapshotStreams<impl Read>,
) -> std::result::Result<
    (
        SnapshotBankFields,
        SnapshotAccountsDbFields<SerializableAccountStorageEntry>,
    ),
    Error,
> {
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
) -> std::result::Result<(Bank, BankFromStreamsInfo), Error>
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
    stream: &mut BufWriter<W>,
    bank: &Bank,
    snapshot_storages: &[Vec<Arc<AccountStorageEntry>>],
) -> Result<(), Error>
where
    W: Write,
{
    bincode::serialize_into(
        stream,
        &SerializableBankAndStorage {
            bank,
            snapshot_storages,
        },
    )
}

/// Serializes bank snapshot into `stream` with bincode
pub fn serialize_bank_snapshot_into<W>(
    stream: &mut BufWriter<W>,
    bank_fields: BankFieldsToSerialize,
    bank_hash_stats: BankHashStats,
    account_storage_entries: &[Vec<Arc<AccountStorageEntry>>],
    extra_fields: ExtraFieldsToSerialize,
    write_version: u64,
) -> Result<(), Error>
where
    W: Write,
{
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
        write_version,
    )
}

/// Serializes bank snapshot with `serializer`
pub fn serialize_bank_snapshot_with<S>(
    serializer: S,
    bank_fields: BankFieldsToSerialize,
    bank_hash_stats: BankHashStats,
    account_storage_entries: &[Vec<Arc<AccountStorageEntry>>],
    extra_fields: ExtraFieldsToSerialize,
    write_version: u64,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let slot = bank_fields.slot;
    let serializable_bank = SerializableVersionedBank::from(bank_fields);
    let serializable_accounts_db = SerializableAccountsDb::<'_> {
        slot,
        account_storage_entries,
        bank_hash_stats,
        write_version,
    };
    (serializable_bank, serializable_accounts_db, extra_fields).serialize(serializer)
}

#[cfg(test)]
struct SerializableBankAndStorage<'a> {
    bank: &'a Bank,
    snapshot_storages: &'a [Vec<Arc<AccountStorageEntry>>],
}

#[cfg(test)]
impl Serialize for SerializableBankAndStorage<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        let slot = self.bank.slot();
        let mut bank_fields = self.bank.get_fields_to_serialize();
        let accounts_db = &self.bank.rc.accounts.accounts_db;
        let bank_hash_stats = self.bank.get_bank_hash_stats();
        let write_version = accounts_db.write_version.load(Ordering::Acquire);
        let lamports_per_signature = bank_fields.fee_rate_governor.lamports_per_signature;
        let versioned_epoch_stakes = std::mem::take(&mut bank_fields.versioned_epoch_stakes);
        let accounts_lt_hash = Some(bank_fields.accounts_lt_hash.clone().into());
        let bank_fields_to_serialize = (
            SerializableVersionedBank::from(bank_fields),
            SerializableAccountsDb::<'_> {
                slot,
                account_storage_entries: self.snapshot_storages,
                bank_hash_stats,
                write_version,
            },
            ExtraFieldsToSerialize {
                lamports_per_signature,
                obsolete_incremental_snapshot_persistence: None,
                obsolete_epoch_accounts_hash: None,
                versioned_epoch_stakes,
                accounts_lt_hash,
            },
        );
        bank_fields_to_serialize.serialize(serializer)
    }
}

#[cfg(test)]
struct SerializableBankAndStorageNoExtra<'a> {
    bank: &'a Bank,
    snapshot_storages: &'a [Vec<Arc<AccountStorageEntry>>],
}

#[cfg(test)]
impl Serialize for SerializableBankAndStorageNoExtra<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        let slot = self.bank.slot();
        let bank_fields = self.bank.get_fields_to_serialize();
        let accounts_db = &self.bank.rc.accounts.accounts_db;
        let bank_hash_stats = self.bank.get_bank_hash_stats();
        let write_version = accounts_db.write_version.load(Ordering::Acquire);
        (
            SerializableVersionedBank::from(bank_fields),
            SerializableAccountsDb::<'_> {
                slot,
                account_storage_entries: self.snapshot_storages,
                bank_hash_stats,
                write_version,
            },
        )
            .serialize(serializer)
    }
}

#[cfg(test)]
impl<'a> From<SerializableBankAndStorageNoExtra<'a>> for SerializableBankAndStorage<'a> {
    fn from(s: SerializableBankAndStorageNoExtra<'a>) -> SerializableBankAndStorage<'a> {
        let SerializableBankAndStorageNoExtra {
            bank,
            snapshot_storages,
        } = s;
        SerializableBankAndStorage {
            bank,
            snapshot_storages,
        }
    }
}

struct SerializableAccountsDb<'a> {
    slot: Slot,
    account_storage_entries: &'a [Vec<Arc<AccountStorageEntry>>],
    bank_hash_stats: BankHashStats,
    write_version: u64,
}

impl Serialize for SerializableAccountsDb<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        // (1st of 3 elements) write the list of account storage entry lists out as a map
        let entry_count = RefCell::<usize>::new(0);
        let entries = utils::serialize_iter_as_map(self.account_storage_entries.iter().map(|x| {
            *entry_count.borrow_mut() += x.len();
            (
                x.first().unwrap().slot(),
                utils::serialize_iter_as_seq(
                    x.iter()
                        .map(|x| SerializableAccountStorageEntry::new(x.as_ref(), self.slot)),
                ),
            )
        }));
        let bank_hash_info = BankHashInfo {
            obsolete_accounts_delta_hash: [0; 32],
            obsolete_accounts_hash: [0; 32],
            stats: self.bank_hash_stats.clone(),
        };

        let historical_roots = Vec::<Slot>::default();
        let historical_roots_with_hash = Vec::<(Slot, Hash)>::default();

        let mut serialize_account_storage_timer = Measure::start("serialize_account_storage_ms");
        let result = (
            entries,
            self.write_version,
            self.slot,
            bank_hash_info,
            historical_roots,
            historical_roots_with_hash,
        )
            .serialize(serializer);
        serialize_account_storage_timer.stop();
        datapoint_info!(
            "serialize_account_storage_ms",
            ("duration", serialize_account_storage_timer.as_ms(), i64),
            ("num_entries", *entry_count.borrow(), i64),
        );
        result
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::TransparentAsHelper for SerializableAccountsDb<'_> {}

/// This struct contains side-info while reconstructing the bank from fields
#[derive(Debug)]
pub(crate) struct ReconstructedBankInfo {
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub(crate) calculated_accounts_lt_hash: AccountsLtHash,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct_bank_from_fields<E>(
    bank_fields: SnapshotBankFields,
    snapshot_accounts_db_fields: SnapshotAccountsDbFields<E>,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<(Bank, ReconstructedBankInfo), Error>
where
    E: SerializableStorage + std::marker::Sync,
{
    let mut bank_fields = bank_fields.collapse_into();
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

    let bank = Bank::new_from_snapshot(
        bank_rc,
        genesis_config,
        runtime_config,
        bank_fields,
        debug_keys,
        reconstructed_accounts_db_info.accounts_data_len,
    );

    info!("rent_collector: {:?}", bank.rent_collector());
    Ok((
        bank,
        ReconstructedBankInfo {
            calculated_accounts_lt_hash: reconstructed_accounts_db_info.calculated_accounts_lt_hash,
        },
    ))
}

pub(crate) fn reconstruct_single_storage(
    slot: &Slot,
    append_vec_path: &Path,
    current_len: usize,
    id: AccountsFileId,
    storage_access: StorageAccess,
    obsolete_accounts: Option<(ObsoleteAccounts, AccountsFileId, usize)>,
) -> Result<Arc<AccountStorageEntry>, SnapshotError> {
    // When restoring from an archive, obsolete accounts will always be `None`
    // When restoring from fastboot, obsolete accounts will be 'Some' if the storage contained
    // accounts marked obsolete at the time the snapshot was taken.
    let (current_len, obsolete_accounts) = if let Some(obsolete_accounts) = obsolete_accounts {
        let updated_len = current_len + obsolete_accounts.2;
        if obsolete_accounts.1 != id {
            return Err(SnapshotError::MismatchedAccountsFileId(
                id,
                obsolete_accounts.1,
            ));
        }

        (updated_len, obsolete_accounts.0)
    } else {
        (current_len, ObsoleteAccounts::default())
    };

    let accounts_file =
        AccountsFile::new_for_startup(append_vec_path, current_len, storage_access)?;
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
    append_vec_path: &Path,
    next_append_vec_id: &AtomicAccountsFileId,
    num_collisions: &AtomicUsize,
) -> io::Result<(AccountsFileId, PathBuf)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let append_vec_path_cstr = cstring_from_path(append_vec_path)?;

    let mut remapped_append_vec_path = append_vec_path.to_path_buf();

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
        remapped_append_vec_path = append_vec_path.parent().unwrap().join(remapped_file_name);

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
        num_collisions.fetch_add(1, Ordering::Relaxed);
    };

    // Only rename the file if the new ID is actually different from the original. In the target_os
    // = linux case, we have already renamed if necessary.
    #[cfg(any(
        not(target_os = "linux"),
        all(target_os = "linux", not(target_env = "gnu"))
    ))]
    if old_append_vec_id != remapped_append_vec_id as SerializedAccountsFileId {
        std::fs::rename(append_vec_path, &remapped_append_vec_path)?;
    }

    Ok((remapped_append_vec_id, remapped_append_vec_path))
}

pub(crate) fn remap_and_reconstruct_single_storage(
    slot: Slot,
    old_append_vec_id: SerializedAccountsFileId,
    current_len: usize,
    append_vec_path: &Path,
    next_append_vec_id: &AtomicAccountsFileId,
    num_collisions: &AtomicUsize,
    storage_access: StorageAccess,
) -> Result<Arc<AccountStorageEntry>, SnapshotError> {
    let (remapped_append_vec_id, remapped_append_vec_path) = remap_append_vec_file(
        slot,
        old_append_vec_id,
        append_vec_path,
        next_append_vec_id,
        num_collisions,
    )?;
    let storage = reconstruct_single_storage(
        &slot,
        &remapped_append_vec_path,
        current_len,
        remapped_append_vec_id,
        storage_access,
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
    pub bank_hash_stats: BankHashStats,
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_accountsdb_from_fields<E>(
    snapshot_accounts_db_fields: SnapshotAccountsDbFields<E>,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<(AccountsDb, ReconstructedAccountsDbInfo), Error>
where
    E: SerializableStorage + std::marker::Sync,
{
    let mut accounts_db = AccountsDb::new_with_config(
        account_paths.to_vec(),
        accounts_db_config,
        accounts_update_notifier,
        exit,
    );

    let AccountsDbFields(
        _snapshot_storages,
        snapshot_version,
        _snapshot_slot,
        snapshot_bank_hash_info,
        _snapshot_historical_roots,
        _snapshot_historical_roots_with_hash,
    ) = snapshot_accounts_db_fields.collapse_into()?;

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
    accounts_db
        .write_version
        .fetch_add(snapshot_version, Ordering::Release);

    info!("Building accounts index...");
    let start = Instant::now();
    let IndexGenerationInfo {
        accounts_data_len,
        calculated_accounts_lt_hash,
    } = accounts_db.generate_index(limit_load_slot_count_from_snapshot, verify_index);
    info!("Building accounts index... Done in {:?}", start.elapsed());

    Ok((
        accounts_db,
        ReconstructedAccountsDbInfo {
            accounts_data_len,
            calculated_accounts_lt_hash,
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
