use {
    crate::transaction_meta::TransactionMeta,
    lazy_lru::LruCache,
    solana_loader_v3_interface::get_program_data_address,
    solana_message::AccountKeys,
    solana_pubkey::Pubkey,
    solana_svm_transaction::{
        svm_message::{SVMMessage, SVMStaticMessage},
        svm_transaction::{SVMStaticTransaction, SVMTransaction},
    },
    solana_transaction::{sanitized::SanitizedTransaction, versioned::VersionedTransaction},
    std::{
        borrow::Cow,
        sync::{LazyLock, RwLock},
    },
};

// Derivations are immutable and can be shared across transactions and banks.
// 64 shards of 1024 entries; lazy-lru allows each shard to grow to twice its capacity.
static PROGRAMDATA_ADDRESSES: LazyLock<[RwLock<LruCache<Pubkey, Pubkey>>; 64]> =
    LazyLock::new(|| std::array::from_fn(|_| RwLock::new(LruCache::new(1024))));

static PROGRAMDATA_SHARD_BYTE: LazyLock<usize> = LazyLock::new(|| rand::random_range(0..32));

pub(crate) fn cached_programdata_address(program: &Pubkey) -> Pubkey {
    let cache = &PROGRAMDATA_ADDRESSES[usize::from(program.as_ref()[*PROGRAMDATA_SHARD_BYTE]) & 63];
    if let Some(address) = cache.read().unwrap().get(program).copied() {
        return address;
    }
    // Derive outside the lock; concurrent misses may harmlessly compute the same address.
    let address = get_program_data_address(program);
    cache.write().unwrap().put(*program, address);
    address
}

pub trait StaticMessageWithMeta: TransactionMeta + SVMStaticMessage {}
impl<T: TransactionMeta + SVMStaticMessage> StaticMessageWithMeta for T {}

/// Additional readonly locks for loader-v3 ProgramData accounts, including potential CPI programs.
/// Explicit account locks take precedence. Lookup table addresses are not account keys.
pub fn implicit_read_locks(account_keys: AccountKeys<'_>) -> impl Iterator<Item = Pubkey> + '_ {
    account_keys
        .iter()
        .map(cached_programdata_address)
        .filter(move |address| !account_keys.iter().any(|key| key == address))
}

pub fn writable_accounts(transaction: &impl SVMMessage) -> impl Iterator<Item = &Pubkey> + Clone {
    transaction
        .account_keys()
        .iter()
        .enumerate()
        .filter_map(|(index, key)| transaction.is_writable(index).then_some(key))
}

pub trait StaticTransactionWithMeta: TransactionMeta + SVMStaticTransaction {
    /// Required to interact with several legacy interfaces that require
    /// `VersionedTransaction`. This should not be used unless necessary, as it
    /// performs numerous allocations that negatively impact performance.
    fn to_versioned_transaction(&self) -> VersionedTransaction;

    /// Returns the serialized transaction size in bytes.
    /// Runtime metadata is not included.
    fn serialized_size(&self) -> usize;
}

pub trait TransactionWithMeta: StaticTransactionWithMeta + SVMTransaction {
    /// Required to interact with geyser plugins.
    /// This function should not be used except for interacting with geyser.
    /// It may do numerous allocations that negatively impact performance.
    fn as_sanitized_transaction(&self) -> Cow<'_, SanitizedTransaction>;
}
