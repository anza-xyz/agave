use {
    crate::rpc_subscriptions::{NotificationEntry, RpcNotification, TimestampedNotificationEntry},
    dashmap::{DashMap, mapref::entry::Entry as DashEntry},
    serde::{Deserialize, Serialize},
    solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig},
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_metrics::{CounterToken, TokenCounter},
    solana_pubkey::Pubkey,
    solana_rpc_client_api::filter::RpcFilterType,
    solana_runtime::{
        bank::{TransactionLogCollectorConfig, TransactionLogCollectorFilter},
        bank_forks::BankForks,
    },
    solana_signature::Signature,
    solana_transaction_status::{TransactionDetails, UiTransactionEncoding},
    std::{
        collections::hash_map::{Entry, HashMap},
        fmt,
        sync::{
            Arc, RwLock, Weak,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
    },
    thiserror::Error,
    tokio::sync::broadcast,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubscriptionId(u64);

impl From<u64> for SubscriptionId {
    fn from(value: u64) -> Self {
        SubscriptionId(value)
    }
}

impl From<SubscriptionId> for u64 {
    fn from(value: SubscriptionId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubscriptionParams {
    Account(AccountSubscriptionParams),
    Block(BlockSubscriptionParams),
    Logs(LogsSubscriptionParams),
    Program(ProgramSubscriptionParams),
    Signature(SignatureSubscriptionParams),
    Slot,
    SlotsUpdates,
    Root,
    Vote,
}

impl SubscriptionParams {
    fn method(&self) -> &'static str {
        match self {
            SubscriptionParams::Account(_) => "accountNotification",
            SubscriptionParams::Logs(_) => "logsNotification",
            SubscriptionParams::Program(_) => "programNotification",
            SubscriptionParams::Signature(_) => "signatureNotification",
            SubscriptionParams::Slot => "slotNotification",
            SubscriptionParams::SlotsUpdates => "slotsUpdatesNotification",
            SubscriptionParams::Block(_) => "blockNotification",
            SubscriptionParams::Root => "rootNotification",
            SubscriptionParams::Vote => "voteNotification",
        }
    }

    fn commitment(&self) -> Option<CommitmentConfig> {
        match self {
            SubscriptionParams::Account(params) => Some(params.commitment),
            SubscriptionParams::Logs(params) => Some(params.commitment),
            SubscriptionParams::Program(params) => Some(params.commitment),
            SubscriptionParams::Signature(params) => Some(params.commitment),
            SubscriptionParams::Block(params) => Some(params.commitment),
            SubscriptionParams::Slot
            | SubscriptionParams::SlotsUpdates
            | SubscriptionParams::Root
            | SubscriptionParams::Vote => None,
        }
    }

    fn is_commitment_watcher(&self) -> bool {
        let commitment = match self {
            SubscriptionParams::Account(params) => &params.commitment,
            SubscriptionParams::Block(params) => &params.commitment,
            SubscriptionParams::Logs(params) => &params.commitment,
            SubscriptionParams::Program(params) => &params.commitment,
            SubscriptionParams::Signature(params) => &params.commitment,
            SubscriptionParams::Root
            | SubscriptionParams::Slot
            | SubscriptionParams::SlotsUpdates
            | SubscriptionParams::Vote => return false,
        };
        !commitment.is_confirmed()
    }

    fn is_gossip_watcher(&self) -> bool {
        let commitment = match self {
            SubscriptionParams::Account(params) => &params.commitment,
            SubscriptionParams::Block(params) => &params.commitment,
            SubscriptionParams::Logs(params) => &params.commitment,
            SubscriptionParams::Program(params) => &params.commitment,
            SubscriptionParams::Signature(params) => &params.commitment,
            SubscriptionParams::Root
            | SubscriptionParams::Slot
            | SubscriptionParams::SlotsUpdates
            | SubscriptionParams::Vote => return false,
        };
        commitment.is_confirmed()
    }

    fn is_node_progress_watcher(&self) -> bool {
        matches!(
            self,
            SubscriptionParams::Slot
                | SubscriptionParams::SlotsUpdates
                | SubscriptionParams::Root
                | SubscriptionParams::Vote
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountSubscriptionParams {
    pub pubkey: Pubkey,
    pub encoding: UiAccountEncoding,
    pub data_slice: Option<UiDataSliceConfig>,
    pub commitment: CommitmentConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockSubscriptionParams {
    pub commitment: CommitmentConfig,
    pub encoding: UiTransactionEncoding,
    pub kind: BlockSubscriptionKind,
    pub transaction_details: TransactionDetails,
    pub show_rewards: bool,
    pub max_supported_transaction_version: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BlockSubscriptionKind {
    All,
    MentionsAccountOrProgram(Pubkey),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogsSubscriptionParams {
    pub kind: LogsSubscriptionKind,
    pub commitment: CommitmentConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LogsSubscriptionKind {
    All,
    AllWithVotes,
    Single(Pubkey),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProgramSubscriptionParams {
    pub pubkey: Pubkey,
    pub filters: Vec<RpcFilterType>,
    pub encoding: UiAccountEncoding,
    pub data_slice: Option<UiDataSliceConfig>,
    pub commitment: CommitmentConfig,
    pub with_context: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SignatureSubscriptionParams {
    pub signature: Signature,
    pub commitment: CommitmentConfig,
    pub enable_received_notification: bool,
}

#[derive(Clone)]
pub struct SubscriptionControl(Arc<SubscriptionControlInner>);
pub struct WeakSubscriptionTokenRef(Weak<SubscriptionTokenInner>, SubscriptionId);

struct SubscriptionControlInner {
    subscriptions: DashMap<SubscriptionParams, WeakSubscriptionTokenRef>,
    next_id: AtomicU64,
    max_active_subscriptions: usize,
    // Number of live `SubscriptionToken`s issued by `subscribe()`. Counts
    // duplicate subscribers to the same params separately so the configured
    // cap is enforced per subscriber, not per deduplicated upstream stream.
    subscriber_count: AtomicUsize,
    sender: crossbeam_channel::Sender<TimestampedNotificationEntry>,
    broadcast_sender: broadcast::Sender<RpcNotification>,
    counter: TokenCounter,
}

impl SubscriptionControl {
    pub fn new(
        max_active_subscriptions: usize,
        sender: crossbeam_channel::Sender<TimestampedNotificationEntry>,
        broadcast_sender: broadcast::Sender<RpcNotification>,
    ) -> Self {
        Self(Arc::new(SubscriptionControlInner {
            subscriptions: DashMap::new(),
            next_id: AtomicU64::new(0),
            max_active_subscriptions,
            subscriber_count: AtomicUsize::new(0),
            sender,
            broadcast_sender,
            counter: TokenCounter::new("rpc_pubsub_total_subscriptions"),
        }))
    }

    pub fn broadcast_receiver(&self) -> broadcast::Receiver<RpcNotification> {
        self.0.broadcast_sender.subscribe()
    }

    pub fn subscribe(&self, params: SubscriptionParams) -> Result<SubscriptionToken, Error> {
        debug!(
            "Total existing subscriptions: {}",
            self.0.subscriptions.len()
        );
        // Reserve a subscriber slot up front. The cap is per live token holder,
        // not per deduplicated upstream stream, so duplicate subscribes to the
        // same params each consume their own slot.
        let subscriber_guard = SubscriberCountGuard::try_reserve(&self.0).ok_or_else(|| {
            inc_new_counter_info!("rpc-subscription-refused-limit-reached", 1);
            Error::TooManySubscriptions
        })?;

        let create_inner = |id, params| {
            Arc::new(SubscriptionTokenInner {
                control: Arc::clone(&self.0),
                params,
                id,
            })
        };

        match self.0.subscriptions.entry(params) {
            DashEntry::Occupied(mut entry) => match entry.get().0.upgrade() {
                Some(inner) => Ok(SubscriptionToken(
                    inner,
                    self.0.counter.create_token(),
                    subscriber_guard,
                )),
                // This means the last Arc for this Weak pointer entered the drop just before us,
                // but could not remove the entry since we are holding the write lock.
                // See `Drop` implementation for `SubscriptionTokenInner` for further info.
                None => {
                    let inner = create_inner(entry.get().1, entry.key().clone());
                    let weak_ref = WeakSubscriptionTokenRef(Arc::downgrade(&inner), inner.id);
                    entry.insert(weak_ref);
                    Ok(SubscriptionToken(
                        inner,
                        self.0.counter.create_token(),
                        subscriber_guard,
                    ))
                }
            },
            DashEntry::Vacant(entry) => {
                let id = SubscriptionId::from(self.0.next_id.fetch_add(1, Ordering::AcqRel));
                let inner = create_inner(id, entry.key().clone());
                let weak_ref = WeakSubscriptionTokenRef(Arc::downgrade(&inner), id);
                let _ = self
                    .0
                    .sender
                    .send(NotificationEntry::Subscribed(inner.params.clone(), id).into());
                entry.insert(weak_ref);
                datapoint_info!(
                    "rpc-subscription",
                    ("total", self.0.subscriptions.len(), i64)
                );
                Ok(SubscriptionToken(
                    inner,
                    self.0.counter.create_token(),
                    subscriber_guard,
                ))
            }
        }
    }

    pub fn subscriber_total(&self) -> usize {
        self.0.subscriber_count.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> usize {
        self.0.subscriptions.len()
    }

    #[cfg(test)]
    pub fn assert_subscribed(&self, params: &SubscriptionParams) {
        assert!(self.0.subscriptions.contains_key(params));
    }

    #[cfg(test)]
    pub fn assert_unsubscribed(&self, params: &SubscriptionParams) {
        assert!(!self.0.subscriptions.contains_key(params));
    }

    #[cfg(test)]
    pub fn account_subscribed(&self, pubkey: &Pubkey) -> bool {
        self.0.subscriptions.iter().any(|item| {
            if let SubscriptionParams::Account(params) = item.key() {
                &params.pubkey == pubkey
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    pub fn logs_subscribed(&self, pubkey: Option<&Pubkey>) -> bool {
        self.0.subscriptions.iter().any(|item| {
            if let SubscriptionParams::Logs(params) = item.key() {
                let subscribed_pubkey = match &params.kind {
                    LogsSubscriptionKind::All | LogsSubscriptionKind::AllWithVotes => None,
                    LogsSubscriptionKind::Single(pubkey) => Some(pubkey),
                };
                subscribed_pubkey == pubkey
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    pub fn signature_subscribed(&self, signature: &Signature) -> bool {
        self.0.subscriptions.iter().any(|item| {
            if let SubscriptionParams::Signature(params) = item.key() {
                &params.signature == signature
            } else {
                false
            }
        })
    }
}

#[derive(Debug)]
pub struct SubscriptionInfo {
    id: SubscriptionId,
    params: SubscriptionParams,
    method: &'static str,
    pub last_notified_slot: RwLock<Slot>,
    commitment: Option<CommitmentConfig>,
}

impl SubscriptionInfo {
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    pub fn method(&self) -> &'static str {
        self.method
    }

    pub fn params(&self) -> &SubscriptionParams {
        &self.params
    }

    pub fn commitment(&self) -> Option<CommitmentConfig> {
        self.commitment
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("node subscription limit reached")]
    TooManySubscriptions,
}

struct LogsSubscriptionsIndex {
    all_count: usize,
    all_with_votes_count: usize,
    single_count: HashMap<Pubkey, usize>,

    bank_forks: Arc<RwLock<BankForks>>,
}

impl LogsSubscriptionsIndex {
    fn add(&mut self, params: &LogsSubscriptionParams) {
        match params.kind {
            LogsSubscriptionKind::All => self.all_count += 1,
            LogsSubscriptionKind::AllWithVotes => self.all_with_votes_count += 1,
            LogsSubscriptionKind::Single(key) => {
                *self.single_count.entry(key).or_default() += 1;
            }
        }
        self.update_config();
    }

    fn remove(&mut self, params: &LogsSubscriptionParams) {
        match params.kind {
            LogsSubscriptionKind::All => self.all_count -= 1,
            LogsSubscriptionKind::AllWithVotes => self.all_with_votes_count -= 1,
            LogsSubscriptionKind::Single(key) => match self.single_count.entry(key) {
                Entry::Occupied(mut entry) => {
                    *entry.get_mut() -= 1;
                    if *entry.get() == 0 {
                        entry.remove();
                    }
                }
                Entry::Vacant(_) => error!("missing entry in single_count"),
            },
        }
        self.update_config();
    }

    fn update_config(&self) {
        let mentioned_addresses = self.single_count.keys().copied().collect();
        let config = if self.all_with_votes_count > 0 {
            TransactionLogCollectorConfig {
                filter: TransactionLogCollectorFilter::AllWithVotes,
                mentioned_addresses,
            }
        } else if self.all_count > 0 {
            TransactionLogCollectorConfig {
                filter: TransactionLogCollectorFilter::All,
                mentioned_addresses,
            }
        } else {
            TransactionLogCollectorConfig {
                filter: TransactionLogCollectorFilter::OnlyMentionedAddresses,
                mentioned_addresses,
            }
        };

        *self
            .bank_forks
            .read()
            .unwrap()
            .root_bank()
            .transaction_log_collector_config
            .write()
            .unwrap() = config;
    }
}

pub struct SubscriptionsTracker {
    logs_subscriptions_index: LogsSubscriptionsIndex,
    by_signature: HashMap<Signature, HashMap<SubscriptionId, Arc<SubscriptionInfo>>>,
    // Accounts, logs, programs, signatures (not gossip)
    commitment_watchers: HashMap<SubscriptionId, Arc<SubscriptionInfo>>,
    // Accounts, logs, programs, signatures (gossip)
    gossip_watchers: HashMap<SubscriptionId, Arc<SubscriptionInfo>>,
    // Slots, slots updates, roots, votes.
    node_progress_watchers: HashMap<SubscriptionParams, Arc<SubscriptionInfo>>,
}

impl SubscriptionsTracker {
    pub fn new(bank_forks: Arc<RwLock<BankForks>>) -> Self {
        SubscriptionsTracker {
            logs_subscriptions_index: LogsSubscriptionsIndex {
                all_count: 0,
                all_with_votes_count: 0,
                single_count: HashMap::new(),
                bank_forks,
            },
            by_signature: HashMap::new(),
            commitment_watchers: HashMap::new(),
            gossip_watchers: HashMap::new(),
            node_progress_watchers: HashMap::new(),
        }
    }

    pub fn subscribe(
        &mut self,
        params: SubscriptionParams,
        id: SubscriptionId,
        last_notified_slot: impl FnOnce() -> Slot,
    ) {
        let info = Arc::new(SubscriptionInfo {
            last_notified_slot: RwLock::new(last_notified_slot()),
            id,
            commitment: params.commitment(),
            method: params.method(),
            params: params.clone(),
        });
        match &params {
            SubscriptionParams::Logs(params) => {
                self.logs_subscriptions_index.add(params);
            }
            SubscriptionParams::Signature(params) => {
                self.by_signature
                    .entry(params.signature)
                    .or_default()
                    .insert(id, Arc::clone(&info));
            }
            _ => {}
        }
        if info.params.is_commitment_watcher() {
            self.commitment_watchers.insert(id, Arc::clone(&info));
        }
        if info.params.is_gossip_watcher() {
            self.gossip_watchers.insert(id, Arc::clone(&info));
        }
        if info.params.is_node_progress_watcher() {
            self.node_progress_watchers
                .insert(info.params.clone(), Arc::clone(&info));
        }
    }

    #[allow(clippy::collapsible_if)]
    pub fn unsubscribe(&mut self, params: SubscriptionParams, id: SubscriptionId) {
        match &params {
            SubscriptionParams::Logs(params) => {
                self.logs_subscriptions_index.remove(params);
            }
            SubscriptionParams::Signature(params) => {
                if let Entry::Occupied(mut entry) = self.by_signature.entry(params.signature) {
                    if entry.get_mut().remove(&id).is_none() {
                        warn!("Subscriptions inconsistency (missing entry in by_signature)");
                    }
                    if entry.get_mut().is_empty() {
                        entry.remove();
                    }
                } else {
                    warn!("Subscriptions inconsistency (missing entry in by_signature)");
                }
            }
            _ => {}
        }
        if params.is_commitment_watcher() {
            if self.commitment_watchers.remove(&id).is_none() {
                warn!("Subscriptions inconsistency (missing entry in commitment_watchers)");
            }
        }
        if params.is_gossip_watcher() {
            if self.gossip_watchers.remove(&id).is_none() {
                warn!("Subscriptions inconsistency (missing entry in gossip_watchers)");
            }
        }
        if params.is_node_progress_watcher() {
            if self.node_progress_watchers.remove(&params).is_none() {
                warn!("Subscriptions inconsistency (missing entry in node_progress_watchers)");
            }
        }
    }

    pub fn by_signature(
        &self,
    ) -> &HashMap<Signature, HashMap<SubscriptionId, Arc<SubscriptionInfo>>> {
        &self.by_signature
    }

    pub fn commitment_watchers(&self) -> &HashMap<SubscriptionId, Arc<SubscriptionInfo>> {
        &self.commitment_watchers
    }

    pub fn gossip_watchers(&self) -> &HashMap<SubscriptionId, Arc<SubscriptionInfo>> {
        &self.gossip_watchers
    }

    pub fn node_progress_watchers(&self) -> &HashMap<SubscriptionParams, Arc<SubscriptionInfo>> {
        &self.node_progress_watchers
    }
}

struct SubscriptionTokenInner {
    control: Arc<SubscriptionControlInner>,
    params: SubscriptionParams,
    id: SubscriptionId,
}

impl fmt::Debug for SubscriptionTokenInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionTokenInner")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for SubscriptionTokenInner {
    #[allow(clippy::collapsible_if)]
    fn drop(&mut self) {
        match self.control.subscriptions.entry(self.params.clone()) {
            DashEntry::Vacant(_) => {
                warn!("Subscriptions inconsistency (missing entry in by_params)");
            }
            // Check the strong refs count to ensure no other thread recreated this subscription (not token)
            // while we were acquiring the lock.
            DashEntry::Occupied(entry) if entry.get().0.strong_count() == 0 => {
                let _ = self
                    .control
                    .sender
                    .send(NotificationEntry::Unsubscribed(self.params.clone(), self.id).into());
                entry.remove();
                datapoint_info!(
                    "rpc-subscription",
                    ("total", self.control.subscriptions.len(), i64)
                );
            }
            // This branch handles the case in which this entry got recreated
            // while we were waiting for the lock (inside the `DashMap::entry` method).
            DashEntry::Occupied(_entry) /* if _entry.get().0.strong_count() > 0 */ => (),
        }
    }
}

// RAII guard for one subscriber slot. The slot is reserved atomically against
// the configured cap by `try_reserve` (returns `None` if the cap is reached)
// and released on drop. Intentionally not `Clone`: each holder must go through
// `try_reserve` so duplicates cannot bypass the cap.
struct SubscriberCountGuard(Arc<SubscriptionControlInner>);

impl SubscriberCountGuard {
    fn try_reserve(control: &Arc<SubscriptionControlInner>) -> Option<Self> {
        let max = control.max_active_subscriptions;
        control
            .subscriber_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current < max).then_some(current + 1)
            })
            .ok()?;
        Some(Self(Arc::clone(control)))
    }
}

impl Drop for SubscriberCountGuard {
    fn drop(&mut self) {
        self.0.subscriber_count.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct SubscriptionToken(
    Arc<SubscriptionTokenInner>,
    #[allow(dead_code)] CounterToken,
    #[allow(dead_code)] SubscriberCountGuard,
);

impl SubscriptionToken {
    pub fn id(&self) -> SubscriptionId {
        self.0.id
    }

    pub fn params(&self) -> &SubscriptionParams {
        &self.0.params
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::rpc_pubsub_service::PubSubConfig,
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_runtime::bank::Bank,
    };

    struct ControlWrapper {
        control: SubscriptionControl,
        receiver: crossbeam_channel::Receiver<TimestampedNotificationEntry>,
    }

    impl ControlWrapper {
        fn new() -> Self {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let (broadcast_sender, _broadcast_receiver) = broadcast::channel(42);

            let control = SubscriptionControl::new(
                PubSubConfig::default_for_tests().max_active_subscriptions,
                sender,
                broadcast_sender,
            );
            Self { control, receiver }
        }

        fn assert_subscribed(&self, expected_params: &SubscriptionParams, expected_id: u64) {
            if let NotificationEntry::Subscribed(params, id) = self.receiver.recv().unwrap().entry {
                assert_eq!(&params, expected_params);
                assert_eq!(id, SubscriptionId::from(expected_id));
            } else {
                panic!("unexpected notification");
            }
            self.assert_silence();
        }

        fn assert_unsubscribed(&self, expected_params: &SubscriptionParams, expected_id: u64) {
            if let NotificationEntry::Unsubscribed(params, id) = self.receiver.recv().unwrap().entry
            {
                assert_eq!(&params, expected_params);
                assert_eq!(id, SubscriptionId::from(expected_id));
            } else {
                panic!("unexpected notification");
            }
            self.assert_silence();
        }

        fn assert_silence(&self) {
            assert!(self.receiver.try_recv().is_err());
        }
    }

    #[test]
    fn notify_subscribe() {
        let control = ControlWrapper::new();
        let token1 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        control.assert_subscribed(&SubscriptionParams::Slot, 0);
        drop(token1);
        control.assert_unsubscribed(&SubscriptionParams::Slot, 0);
    }

    #[test]
    fn recreates_inner_when_weak_upgrade_fails() {
        // Exercise the race-window branch in `subscribe()` where a stale
        // `WeakSubscriptionTokenRef` is still in the DashMap because the
        // previous holder's `SubscriptionTokenInner::drop` is blocked on the
        // entry lock. A fresh subscribe must recreate the inner and reuse the
        // stored SubscriptionId rather than allocating a new one.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (broadcast_sender, _broadcast_receiver) = broadcast::channel(42);
        let control = SubscriptionControl::new(2, sender, broadcast_sender);

        let stored_id = SubscriptionId::from(42);
        let dead_weak = {
            let arc = Arc::new(SubscriptionTokenInner {
                control: Arc::clone(&control.0),
                params: SubscriptionParams::Slot,
                id: stored_id,
            });
            let weak = Arc::downgrade(&arc);
            WeakSubscriptionTokenRef(weak, stored_id)
            // `arc` drops here; its Drop sees Vacant and logs a warning.
        };
        control
            .0
            .subscriptions
            .insert(SubscriptionParams::Slot, dead_weak);

        let token = control.subscribe(SubscriptionParams::Slot).unwrap();
        assert_eq!(token.id(), stored_id);
        assert_eq!(control.total(), 1);
        assert_eq!(control.subscriber_total(), 1);
        // Drain notifications emitted by the test (Drop-on-vacant warning is
        // log-only; the only entry should be the Subscribed from the recreate).
        while receiver.try_recv().is_ok() {}
        drop(token);
    }

    #[test]
    fn duplicate_params_consume_separate_subscriber_slots() {
        // Cap of 1 must reject a second subscriber even when params match an
        // existing subscription (GHSA: duplicate-params cap bypass).
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let (broadcast_sender, _broadcast_receiver) = broadcast::channel(42);
        let control = SubscriptionControl::new(1, sender, broadcast_sender);

        let token1 = control.subscribe(SubscriptionParams::Slot).unwrap();
        assert_eq!(control.total(), 1);
        assert_eq!(control.subscriber_total(), 1);

        assert!(matches!(
            control.subscribe(SubscriptionParams::Slot),
            Err(Error::TooManySubscriptions)
        ));
        assert_eq!(control.subscriber_total(), 1);

        drop(token1);
        // After the only holder drops, a fresh subscriber should fit again.
        let token2 = control.subscribe(SubscriptionParams::Slot).unwrap();
        assert_eq!(control.subscriber_total(), 1);
        drop(token2);
        assert_eq!(control.subscriber_total(), 0);
    }

    #[test]
    fn notify_subscribe_multiple() {
        let control = ControlWrapper::new();
        let token1 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        control.assert_subscribed(&SubscriptionParams::Slot, 0);
        let token2 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        drop(token1);
        let token3 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        drop(token3);
        control.assert_silence();
        drop(token2);
        control.assert_unsubscribed(&SubscriptionParams::Slot, 0);
    }

    #[test]
    fn notify_subscribe_two_subscriptions() {
        let control = ControlWrapper::new();
        let token_slot1 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        control.assert_subscribed(&SubscriptionParams::Slot, 0);

        let signature_params = SubscriptionParams::Signature(SignatureSubscriptionParams {
            signature: Signature::default(),
            commitment: CommitmentConfig::processed(),
            enable_received_notification: false,
        });
        let token_signature1 = control.control.subscribe(signature_params.clone()).unwrap();
        control.assert_subscribed(&signature_params, 1);

        let token_slot2 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        let token_signature2 = control.control.subscribe(signature_params.clone()).unwrap();
        drop(token_slot1);
        control.assert_silence();
        drop(token_slot2);
        control.assert_unsubscribed(&SubscriptionParams::Slot, 0);
        drop(token_signature2);
        control.assert_silence();
        drop(token_signature1);
        control.assert_unsubscribed(&signature_params, 1);

        let token_slot3 = control.control.subscribe(SubscriptionParams::Slot).unwrap();
        control.assert_subscribed(&SubscriptionParams::Slot, 2);
        drop(token_slot3);
        control.assert_unsubscribed(&SubscriptionParams::Slot, 2);
    }

    #[test]
    fn subscription_info() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut tracker = SubscriptionsTracker::new(bank_forks);

        tracker.subscribe(SubscriptionParams::Slot, 0.into(), || 0);
        let info = tracker
            .node_progress_watchers
            .get(&SubscriptionParams::Slot)
            .unwrap();
        assert_eq!(info.commitment, None);
        assert_eq!(info.params, SubscriptionParams::Slot);
        assert_eq!(info.method, SubscriptionParams::Slot.method());
        assert_eq!(info.id, SubscriptionId::from(0));
        assert_eq!(*info.last_notified_slot.read().unwrap(), 0);

        let account_params = SubscriptionParams::Account(AccountSubscriptionParams {
            pubkey: spl_generic_token::token::id(),
            commitment: CommitmentConfig::finalized(),
            encoding: UiAccountEncoding::Base64Zstd,
            data_slice: None,
        });
        tracker.subscribe(account_params.clone(), 1.into(), || 42);

        let info = tracker
            .commitment_watchers
            .get(&SubscriptionId::from(1))
            .unwrap();
        assert_eq!(info.commitment, Some(CommitmentConfig::finalized()));
        assert_eq!(info.params, account_params);
        assert_eq!(info.method, account_params.method());
        assert_eq!(info.id, SubscriptionId::from(1));
        assert_eq!(*info.last_notified_slot.read().unwrap(), 42);
    }

    #[test]
    fn subscription_indexes() {
        fn counts(tracker: &SubscriptionsTracker) -> (usize, usize, usize, usize) {
            (
                tracker.by_signature.len(),
                tracker.commitment_watchers.len(),
                tracker.gossip_watchers.len(),
                tracker.node_progress_watchers.len(),
            )
        }

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut tracker = SubscriptionsTracker::new(bank_forks);

        tracker.subscribe(SubscriptionParams::Slot, 0.into(), || 0);
        assert_eq!(counts(&tracker), (0, 0, 0, 1));
        tracker.unsubscribe(SubscriptionParams::Slot, 0.into());
        assert_eq!(counts(&tracker), (0, 0, 0, 0));

        let account_params = SubscriptionParams::Account(AccountSubscriptionParams {
            pubkey: spl_generic_token::token::id(),
            commitment: CommitmentConfig::finalized(),
            encoding: UiAccountEncoding::Base64Zstd,
            data_slice: None,
        });
        tracker.subscribe(account_params.clone(), 1.into(), || 0);
        assert_eq!(counts(&tracker), (0, 1, 0, 0));
        tracker.unsubscribe(account_params, 1.into());
        assert_eq!(counts(&tracker), (0, 0, 0, 0));

        let account_params2 = SubscriptionParams::Account(AccountSubscriptionParams {
            pubkey: spl_generic_token::token::id(),
            commitment: CommitmentConfig::confirmed(),
            encoding: UiAccountEncoding::Base64Zstd,
            data_slice: None,
        });
        tracker.subscribe(account_params2.clone(), 2.into(), || 0);
        assert_eq!(counts(&tracker), (0, 0, 1, 0));
        tracker.unsubscribe(account_params2, 2.into());
        assert_eq!(counts(&tracker), (0, 0, 0, 0));

        let signature_params = SubscriptionParams::Signature(SignatureSubscriptionParams {
            signature: Signature::default(),
            commitment: CommitmentConfig::processed(),
            enable_received_notification: false,
        });
        tracker.subscribe(signature_params.clone(), 3.into(), || 0);
        assert_eq!(counts(&tracker), (1, 1, 0, 0));
        tracker.unsubscribe(signature_params, 3.into());
        assert_eq!(counts(&tracker), (0, 0, 0, 0));
    }
}
