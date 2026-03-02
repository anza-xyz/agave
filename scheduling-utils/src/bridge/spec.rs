use {
    crate::{pubkeys_ptr::PubkeysPtr, transaction_ptr::TransactionPtr},
    agave_feature_set::FeatureSet,
    agave_scheduler_bindings::{
        ProgressMessage, tpu_message_flags,
        worker_message_types::{CheckResponse, ExecutionResponse},
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::SanitizedTransactionView,
    },
    solana_fee::FeeFeatures,
    solana_pubkey::Pubkey,
};

pub trait Bridge {
    type Worker: Worker;
    type Meta;

    /// Returns the current state of the runtime.
    fn runtime(&self) -> &RuntimeState;

    /// Returns the most recently received [`ProgressMessage`].
    fn progress(&self) -> &ProgressMessage;

    /// Returns the number of workers in agave paired with this scheduler.
    fn worker_count(&self) -> usize;

    /// Returns a mutable reference to the worker with the given ID.
    ///
    /// # Panics
    ///
    /// If the provided worker `id` does not exist.
    fn worker(&mut self, id: usize) -> &mut Self::Worker;

    /// Returns a reference to the transaction state for the given transaction
    /// key.
    ///
    /// # Panics
    ///
    /// If the provided [`TransactionKey`] does not exist.
    fn tx(&self, key: TransactionKey) -> &TransactionState;

    /// Inserts a transaction into the bridge and returns its
    /// [`TransactionKey`]. This key is used to reference the transaction in:
    ///
    /// - [`Self::tx`]
    /// - [`Self::tx_drop`]
    /// - [`Self::schedule`]
    fn tx_insert(&mut self, tx: &[u8]) -> Result<TransactionKey, TransactionViewError>;

    /// Drops the transaction with the given [`TransactionKey`].
    ///
    /// # Panics
    ///
    /// If the provided [`TransactionKey`] does not exist.
    fn tx_drop(&mut self, key: TransactionKey);

    /// Drains all pending progress messages received from agave.
    /// This updates the internal state of the bridge to reflect the progress
    /// messages so that [`Self::progress`] returns the most recently
    /// received progress message.
    ///
    /// Returns a copy of the most recently received progress message if any.
    fn drain_progress(&mut self) -> Option<ProgressMessage>;

    /// Returns the current length of the TPU queue.
    fn tpu_len(&mut self) -> usize;

    /// Drains up to `max_count` transactions from the TPU queue, passing each
    /// transaction, by key, to the provided callback. The callback returns a
    /// [`TxDecision`] indicating whether the transaction is kept or dropped.
    ///
    /// The callback must not call [`Self::tx_drop`] on the passed
    /// [`TransactionKey`].
    fn tpu_drain(
        &mut self,
        cb: impl FnMut(&mut Self, TransactionKey) -> TxDecision,
        max_count: usize,
    );

    /// Drains up to `max_count` responses (transaction batches) from the
    /// specified worker. For each response, the provided callback is called
    /// for each of the transactions, returning a [`TxDecision`] indicating
    /// whether the transaction is kept or dropped.
    ///
    /// The callback must not call [`Self::tx_drop`] on the passed
    /// [`TransactionKey`].
    fn worker_drain(
        &mut self,
        worker: usize,
        cb: impl FnMut(&mut Self, WorkerResponse<'_, Self::Meta>) -> TxDecision,
        max_count: usize,
    );

    /// Sends a batch of transactions to the specified worker for processing.
    fn schedule(&mut self, batch: ScheduleBatch<&[KeyedTransactionMeta<Self::Meta>]>);
}

pub struct RuntimeState {
    pub feature_set: FeatureSet,
    pub fee_features: FeeFeatures,
    pub lamports_per_signature: u64,
    pub burn_percent: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScheduleBatch<T> {
    pub worker: usize,
    pub transactions: T,
    pub max_working_slot: u64,
    pub flags: u16,
}

pub trait Worker {
    fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    fn len(&mut self) -> usize;

    fn rem(&mut self) -> usize;
}

#[derive(Debug, Clone)]
pub struct WorkerResponse<'a, M> {
    pub key: TransactionKey,
    pub meta: M,
    pub response: WorkerAction<'a>,
}

#[derive(Debug, Clone)]
pub enum WorkerAction<'a> {
    Unprocessed,
    Check(CheckResponse, Option<&'a PubkeysPtr>),
    Execute(ExecutionResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyedTransactionMeta<M> {
    pub key: TransactionKey,
    pub meta: M,
}

slotmap::new_key_type! {
    pub struct TransactionKey;
}

#[derive(Debug)]
pub struct TransactionState {
    pub dead: bool,
    pub borrows: u64,
    pub flags: u8,
    pub data: SanitizedTransactionView<TransactionPtr>,
    pub keys: Option<PubkeysPtr>,
}

impl TransactionState {
    #[must_use]
    pub const fn is_simple_vote(&self) -> bool {
        self.flags & tpu_message_flags::IS_SIMPLE_VOTE != 0
    }

    pub fn locks(&self) -> impl Iterator<Item = (&Pubkey, bool)> {
        self.write_locks()
            .map(|lock| (lock, true))
            .chain(self.read_locks().map(|lock| (lock, false)))
    }

    pub fn write_locks(&self) -> impl Iterator<Item = &Pubkey> {
        self.data
            .static_account_keys()
            .iter()
            .chain(self.keys.iter().flat_map(|keys| keys.as_slice().iter()))
            .enumerate()
            .filter(|(i, _)| self.is_writable(*i as u8))
            .map(|(_, key)| key)
    }

    pub fn read_locks(&self) -> impl Iterator<Item = &Pubkey> {
        self.data
            .static_account_keys()
            .iter()
            .chain(self.keys.iter().flat_map(|keys| keys.as_slice().iter()))
            .enumerate()
            .filter(|(i, _)| !self.is_writable(*i as u8))
            .map(|(_, key)| key)
    }

    fn is_writable(&self, index: u8) -> bool {
        if index >= self.data.num_static_account_keys() {
            let loaded_address_index = index.wrapping_sub(self.data.num_static_account_keys());
            loaded_address_index < self.data.total_writable_lookup_accounts() as u8
        } else {
            index
                < self
                    .data
                    .num_signatures()
                    .wrapping_sub(self.data.num_readonly_signed_static_accounts())
                || (index >= self.data.num_signatures()
                    && index
                        < (self.data.static_account_keys().len() as u8)
                            .wrapping_sub(self.data.num_readonly_unsigned_static_accounts()))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TxDecision {
    Keep,
    Drop,
}
