//! Service to clean up dead slots in accounts_db
//!
//! This can be expensive since we have to walk the append vecs being cleaned up.

mod pending_snapshot_packages;
mod stats;
pub use pending_snapshot_packages::PendingSnapshotPackages;
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bank::{Bank, BankSlotDelta, DropCallback},
        bank_forks::BankForks,
        snapshot_controller::SnapshotController,
        snapshot_package::SnapshotPackage,
    },
    agave_snapshots::{error::SnapshotError, SnapshotArchiveKind, SnapshotKind},
    crossbeam_channel::{Receiver, SendError, Sender},
    log::*,
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    solana_clock::{BankId, Slot},
    solana_measure::{measure::Measure, measure_us},
    stats::StatsManager,
    std::{
        boxed::Box,
        cmp,
        fmt::{self, Debug, Formatter},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, LazyLock, Mutex, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const INTERVAL_MS: u64 = 100;
// Set the clean interval duration to be approximately how long before the next incremental
// snapshot request is received, plus some buffer.  The default incremental snapshot interval is
// 100 slots, which ends up being 40 seconds plus buffer.
const CLEAN_INTERVAL: Duration = Duration::from_secs(50);
const SHRINK_INTERVAL: Duration = Duration::from_secs(1);

pub type SnapshotRequestSender = Sender<SnapshotRequest>;
pub type SnapshotRequestReceiver = Receiver<SnapshotRequest>;
pub type DroppedSlotsSender = Sender<(Slot, BankId)>;
pub type DroppedSlotsReceiver = Receiver<(Slot, BankId)>;

/// interval to report bank_drop queue events: 60s
const BANK_DROP_SIGNAL_CHANNEL_REPORT_INTERVAL: u64 = 60_000;
/// maximum drop bank signal queue length
const MAX_DROP_BANK_SIGNAL_QUEUE_SIZE: usize = 10_000;

#[derive(Debug, Default)]
struct PrunedBankQueueLenReporter {
    last_report_time: AtomicU64,
}

impl PrunedBankQueueLenReporter {
    fn report(&self, q_len: usize) {
        let now = solana_time_utils::timestamp();
        let last_report_time = self.last_report_time.load(Ordering::Acquire);
        if q_len > MAX_DROP_BANK_SIGNAL_QUEUE_SIZE
            && now.saturating_sub(last_report_time) > BANK_DROP_SIGNAL_CHANNEL_REPORT_INTERVAL
        {
            datapoint_warn!("excessive_pruned_bank_channel_len", ("len", q_len, i64));
            self.last_report_time.store(now, Ordering::Release);
        }
    }
}

static BANK_DROP_QUEUE_REPORTER: LazyLock<PrunedBankQueueLenReporter> =
    LazyLock::new(PrunedBankQueueLenReporter::default);

#[derive(Clone)]
pub struct SendDroppedBankCallback {
    sender: DroppedSlotsSender,
}

impl DropCallback for SendDroppedBankCallback {
    fn callback(&self, bank: &Bank) {
        BANK_DROP_QUEUE_REPORTER.report(self.sender.len());
        if let Err(SendError(_)) = self.sender.send((bank.slot(), bank.bank_id())) {
            info!("bank DropCallback signal queue disconnected.");
        }
    }

    fn clone_box(&self) -> Box<dyn DropCallback + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Debug for SendDroppedBankCallback {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "SendDroppedBankCallback({self:p})")
    }
}

impl SendDroppedBankCallback {
    pub fn new(sender: DroppedSlotsSender) -> Self {
        Self { sender }
    }
}

pub struct SnapshotRequest {
    pub snapshot_root_bank: Arc<Bank>,
    pub status_cache_slot_deltas: Vec<BankSlotDelta>,
    pub request_kind: SnapshotRequestKind,

    /// The instant this request was send to the queue.
    /// Used to track how long requests wait before processing.
    pub enqueued: Instant,
}

impl Debug for SnapshotRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotRequest")
            .field("request kind", &self.request_kind)
            .field("bank slot", &self.snapshot_root_bank.slot())
            .field("block height", &self.snapshot_root_bank.block_height())
            .finish_non_exhaustive()
    }
}

/// What kind of request is this?
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SnapshotRequestKind {
    FullSnapshot,
    IncrementalSnapshot,
}

pub struct SnapshotRequestHandler {
    pub snapshot_controller: Arc<SnapshotController>,
    pub snapshot_request_receiver: SnapshotRequestReceiver,
    pub pending_snapshot_packages: Arc<Mutex<PendingSnapshotPackages>>,
}

impl SnapshotRequestHandler {
    // Returns the latest requested snapshot slot and storages
    #[allow(clippy::type_complexity)]
    pub fn handle_snapshot_requests(
        &self,
        non_snapshot_time_us: u128,
    ) -> Option<Result<Slot, SnapshotError>> {
        let (snapshot_request, num_outstanding_requests, num_re_enqueued_requests) =
            self.get_next_snapshot_request()?;

        datapoint_info!(
            "handle_snapshot_requests",
            ("num_outstanding_requests", num_outstanding_requests, i64),
            ("num_re_enqueued_requests", num_re_enqueued_requests, i64),
            (
                "enqueued_time_us",
                snapshot_request.enqueued.elapsed().as_micros(),
                i64
            ),
        );

        let snapshot_kind = new_snapshot_kind(&snapshot_request)?;
        Some(self.handle_snapshot_request(non_snapshot_time_us, snapshot_request, snapshot_kind))
    }

    /// Get the next snapshot request to handle
    ///
    /// Look through the snapshot request channel to find the highest priority one to handle next.
    /// If there are no snapshot requests in the channel, return None.  Otherwise return the
    /// highest priority one.  Unhandled snapshot requests with slots GREATER-THAN the handled one
    /// will be re-enqueued.  The remaining will be dropped.
    ///
    /// Also return the number of snapshot requests initially in the channel, and the number of
    /// ones re-enqueued.
    fn get_next_snapshot_request(
        &self,
    ) -> Option<(
        SnapshotRequest,
        /*num outstanding snapshot requests*/ usize,
        /*num re-enqueued snapshot requests*/ usize,
    )> {
        let mut requests: Vec<_> = self.snapshot_request_receiver.try_iter().collect();
        let requests_len = requests.len();
        debug!("outstanding snapshot requests ({requests_len}): {requests:?}");

        match requests_len {
            0 => None,
            1 => {
                // SAFETY: We know the len is 1, so `pop` will return `Some`
                let snapshot_request = requests.pop().unwrap();
                Some((snapshot_request, 1, 0))
            }
            _ => {
                let max_idx = requests
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| cmp_requests_by_priority(a, b))
                    .map(|(idx, _)| idx)
                    .unwrap(); // SAFETY: We know len > 1
                let snapshot_request = requests.swap_remove(max_idx);
                let handled_request_slot = snapshot_request.snapshot_root_bank.slot();
                // re-enqueue any remaining requests for slots GREATER-THAN the one that will be handled
                let num_re_enqueued_requests = requests
                    .into_iter()
                    .filter(|snapshot_request| {
                        snapshot_request.snapshot_root_bank.slot() > handled_request_slot
                    })
                    .map(|snapshot_request| {
                        self.snapshot_controller
                            .request_sender()
                            .try_send(snapshot_request)
                            .expect("re-enqueue snapshot request");
                    })
                    .count();

                Some((snapshot_request, requests_len, num_re_enqueued_requests))
            }
        }
    }

    fn handle_snapshot_request(
        &self,
        non_snapshot_time_us: u128,
        snapshot_request: SnapshotRequest,
        snapshot_kind: SnapshotKind,
    ) -> Result<Slot, SnapshotError> {
        info!("handling snapshot request: {snapshot_request:?}, {snapshot_kind:?}");
        let mut total_time = Measure::start("snapshot_request_receiver_total_time");
        let SnapshotRequest {
            snapshot_root_bank,
            status_cache_slot_deltas,
            request_kind: _,
            enqueued: _,
        } = snapshot_request;

        if snapshot_kind.is_full_snapshot() {
            // The latest full snapshot slot is what accounts-db uses to properly handle
            // zero lamport accounts.  We are handling a full snapshot request here, and
            // since taking a snapshot is not allowed to fail, we can update accounts-db now.
            snapshot_root_bank
                .rc
                .accounts
                .accounts_db
                .set_latest_full_snapshot_slot(snapshot_root_bank.slot());
        }

        let mut flush_accounts_cache_time = Measure::start("flush_accounts_cache_time");
        // Forced cache flushing MUST flush all roots <= snapshot_root_bank.slot().
        // That's because `snapshot_root_bank.slot()` must be root at this point,
        // and contains relevant updates because each bank has at least 1 account update due
        // to sysvar maintenance. Otherwise, this would cause missing storages in the snapshot
        snapshot_root_bank.force_flush_accounts_cache();
        // Ensure all roots <= `self.slot()` have been flushed.
        // Note `max_flush_root` could be larger than self.slot() if there are
        // `> MAX_CACHE_SLOT` cached and rooted slots which triggered earlier flushes.
        assert!(
            snapshot_root_bank.slot()
                <= snapshot_root_bank
                    .rc
                    .accounts
                    .accounts_db
                    .accounts_cache
                    .fetch_max_flush_root()
        );
        flush_accounts_cache_time.stop();

        let mut clean_time = Measure::start("clean_time");
        snapshot_root_bank.clean_accounts();
        clean_time.stop();

        let (_, shrink_ancient_time_us) = measure_us!(snapshot_root_bank.shrink_ancient_slots());

        let mut shrink_time = Measure::start("shrink_time");
        snapshot_root_bank.shrink_candidate_slots();
        shrink_time.stop();

        // Snapshot the bank and send over a snapshot package
        let mut snapshot_time = Measure::start("snapshot_time");
        let snapshot_package = SnapshotPackage::new(
            snapshot_kind,
            &snapshot_root_bank,
            snapshot_root_bank.get_snapshot_storages(None),
            status_cache_slot_deltas,
        );
        self.pending_snapshot_packages
            .lock()
            .unwrap()
            .push(snapshot_package);
        snapshot_time.stop();
        info!(
            "Handled snapshot request. snapshot kind: {:?}, slot: {}, bank hash: {}",
            snapshot_kind,
            snapshot_root_bank.slot(),
            snapshot_root_bank.hash(),
        );

        total_time.stop();

        datapoint_info!(
            "handle_snapshot_requests-timing",
            (
                "flush_accounts_cache_time",
                flush_accounts_cache_time.as_us(),
                i64
            ),
            ("shrink_time", shrink_time.as_us(), i64),
            ("clean_time", clean_time.as_us(), i64),
            ("snapshot_time", snapshot_time.as_us(), i64),
            ("total_us", total_time.as_us(), i64),
            ("non_snapshot_time_us", non_snapshot_time_us, i64),
            ("shrink_ancient_time_us", shrink_ancient_time_us, i64),
        );
        Ok(snapshot_root_bank.slot())
    }

    /// Returns the slot of the next snapshot request to be handled
    fn peek_next_snapshot_request_slot(&self) -> Option<Slot> {
        // We reuse `get_next_snapshot_request()` here, since it already implements all the logic
        // for getting the highest priority request, *AND* we leverage its test coverage.
        // Additionally, since `get_next_snapshot_request()` drops old requests, we might get to
        // proactively clean up old banks earlier as well!
        let (next_request, _, _) = self.get_next_snapshot_request()?;
        let next_slot = next_request.snapshot_root_bank.slot();

        // make sure to re-enqueue the request, otherwise we'd lose it!
        self.snapshot_controller
            .request_sender()
            .try_send(next_request)
            .expect("re-enqueue snapshot request");

        Some(next_slot)
    }
}

#[derive(Debug)]
pub struct PrunedBanksRequestHandler {
    pub pruned_banks_receiver: DroppedSlotsReceiver,
}

impl PrunedBanksRequestHandler {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn handle_request(&self, bank: &Bank) -> usize {
        let mut banks_to_purge: Vec<_> = self.pruned_banks_receiver.try_iter().collect();
        // We need a stable sort to ensure we purge banks—with the same slot—in the same order
        // they were sent into the channel.
        banks_to_purge.sort_by_key(|(slot, _id)| *slot);
        let num_banks_to_purge = banks_to_purge.len();

        // Group the banks into slices with the same slot
        let grouped_banks_to_purge: Vec<_> = banks_to_purge.chunk_by(|a, b| a.0 == b.0).collect();

        // Log whenever we need to handle banks with the same slot.  Purposely do this *before* we
        // call `purge_slot()` to ensure we get the datapoint (in case there's an assert/panic).
        let num_banks_with_same_slot =
            num_banks_to_purge.saturating_sub(grouped_banks_to_purge.len());
        if num_banks_with_same_slot > 0 {
            datapoint_info!(
                "pruned_banks_request_handler",
                ("num_pruned_banks", num_banks_to_purge, i64),
                ("num_banks_with_same_slot", num_banks_with_same_slot, i64),
            );
        }

        // Purge all the slots in parallel
        // Banks for the same slot are purged sequentially
        let accounts_db = bank.rc.accounts.accounts_db.as_ref();
        accounts_db.thread_pool_background.install(|| {
            grouped_banks_to_purge.into_par_iter().for_each(|group| {
                group.iter().for_each(|(slot, bank_id)| {
                    accounts_db.purge_slot(*slot, *bank_id, true);
                })
            });
        });

        num_banks_to_purge
    }

    fn remove_dead_slots(
        &self,
        bank: &Bank,
        removed_slots_count: &mut usize,
        total_remove_slots_time: &mut u64,
    ) {
        let mut remove_slots_time = Measure::start("remove_slots_time");
        *removed_slots_count += self.handle_request(bank);
        remove_slots_time.stop();
        *total_remove_slots_time += remove_slots_time.as_us();

        if *removed_slots_count >= 100 {
            datapoint_info!(
                "remove_slots_timing",
                ("remove_slots_time", *total_remove_slots_time, i64),
                ("removed_slots_count", *removed_slots_count, i64),
            );
            *total_remove_slots_time = 0;
            *removed_slots_count = 0;
        }
    }
}

pub struct AbsRequestHandlers {
    pub snapshot_request_handler: SnapshotRequestHandler,
    pub pruned_banks_request_handler: PrunedBanksRequestHandler,
}

impl AbsRequestHandlers {
    // Returns the latest requested snapshot slot, if one exists
    #[allow(clippy::type_complexity)]
    pub fn handle_snapshot_requests(
        &self,
        non_snapshot_time_us: u128,
    ) -> Option<Result<Slot, SnapshotError>> {
        self.snapshot_request_handler
            .handle_snapshot_requests(non_snapshot_time_us)
    }
}

pub struct AccountsBackgroundService {
    t_background: JoinHandle<()>,
    status: AbsStatus,
}

impl AccountsBackgroundService {
    pub fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        exit: Arc<AtomicBool>,
        request_handlers: AbsRequestHandlers,
    ) -> Self {
        let is_running = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let mut last_cleaned_slot = 0;
        let mut removed_slots_count = 0;
        let mut total_remove_slots_time = 0;
        let t_background = Builder::new()
            .name("solAcctsBgSvc".to_string())
            .spawn({
                let is_running = is_running.clone();
                let stop = stop.clone();

                move || {
                    info!("AccountsBackgroundService has started");
                    let mut stats = StatsManager::new();
                    let mut last_snapshot_end_time = None;
                    let mut previous_clean_time = Instant::now();
                    let mut previous_shrink_time = Instant::now();

                    loop {
                        if exit.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let start_time = Instant::now();

                        // Grab the current root bank
                        let bank = bank_forks.read().unwrap().root_bank();

                        // Purge accounts of any dead slots
                        request_handlers
                            .pruned_banks_request_handler
                            .remove_dead_slots(
                                &bank,
                                &mut removed_slots_count,
                                &mut total_remove_slots_time,
                            );

                        let non_snapshot_time = last_snapshot_end_time
                            .map(|last_snapshot_end_time: Instant| {
                                last_snapshot_end_time.elapsed().as_micros()
                            })
                            .unwrap_or_default();

                        // Check to see if there were any requests for snapshotting banks
                        // < the current root bank `bank` above.
                        //
                        // Claim: Any snapshot request for slot `N` found here implies that the
                        // last cleanup slot `M` satisfies `M < N`
                        //
                        // Proof: Assume for contradiction that we find a snapshot request for slot
                        // `N` here, but cleanup has already happened on some slot `M >= N`.
                        // Because the call to `bank.clean_accounts(true)` (in the code below)
                        // implies we only clean slots `<= bank - 1`, then that means in some
                        // *previous* iteration of this loop, we must have gotten a root bank for
                        // slot some slot `R` where `R > N`, but did not see the snapshot for `N`
                        // in the snapshot request channel.
                        //
                        // However, this is impossible because BankForks.set_root() will always
                        // flush the snapshot request for `N` to the snapshot request channel
                        // before setting a root `R > N`, and
                        // snapshot_request_handler.handle_requests() will always look for the
                        // latest available snapshot in the channel.
                        let snapshot_handle_result =
                            request_handlers.handle_snapshot_requests(non_snapshot_time);

                        if let Some(snapshot_handle_result) = snapshot_handle_result {
                            // Safe, see proof above

                            last_snapshot_end_time = Some(Instant::now());
                            match snapshot_handle_result {
                                Ok(snapshot_slot) => {
                                    assert!(
                                        last_cleaned_slot <= snapshot_slot,
                                        "last cleaned slot: {last_cleaned_slot}, snapshot request \
                                         slot: {snapshot_slot}, enqueued snapshot requests: {:?}",
                                        request_handlers
                                            .snapshot_request_handler
                                            .snapshot_request_receiver
                                            .try_iter()
                                            .collect::<Vec<_>>(),
                                    );
                                    last_cleaned_slot = snapshot_slot;
                                    previous_clean_time = Instant::now();
                                    previous_shrink_time = Instant::now();
                                }
                                Err(err) => {
                                    error!(
                                        "Stopping AccountsBackgroundService! Fatal error while \
                                         handling snapshot requests: {err}",
                                    );
                                    exit.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        } else {
                            // we didn't handle a snapshot request, so do flush/clean/shrink

                            let next_snapshot_request_slot = request_handlers
                                .snapshot_request_handler
                                .peek_next_snapshot_request_slot();

                            // We cannot clean past the next snapshot request slot because it may
                            // have zero-lamport accounts.  See the comments in
                            // Bank::clean_accounts() for more information.
                            let max_clean_slot_inclusive = cmp::min(
                                next_snapshot_request_slot.unwrap_or(Slot::MAX),
                                bank.slot(),
                            )
                            .saturating_sub(1);

                            let duration_since_previous_clean = previous_clean_time.elapsed();
                            let should_clean = duration_since_previous_clean > CLEAN_INTERVAL;

                            // if we're cleaning, then force flush, otherwise be lazy
                            let force_flush = should_clean;
                            bank.rc
                                .accounts
                                .accounts_db
                                .flush_accounts_cache(force_flush, Some(max_clean_slot_inclusive));

                            if should_clean {
                                bank.rc.accounts.accounts_db.clean_accounts(
                                    Some(max_clean_slot_inclusive),
                                    false,
                                    bank.epoch_schedule(),
                                );
                                last_cleaned_slot = max_clean_slot_inclusive;
                                previous_clean_time = Instant::now();
                            }

                            let duration_since_previous_shrink = previous_shrink_time.elapsed();
                            let should_shrink = duration_since_previous_shrink > SHRINK_INTERVAL;
                            // To avoid pathological interactions between the clean and shrink
                            // timers, call shrink for either should_shrink or should_clean.
                            if should_shrink || should_clean {
                                if should_clean {
                                    // We used to only squash (aka shrink ancients) when we also
                                    // cleaned, so keep that same behavior here for now.
                                    bank.shrink_ancient_slots();
                                }
                                bank.shrink_candidate_slots();
                                previous_shrink_time = Instant::now();
                            }
                        }
                        stats.record_and_maybe_submit(start_time.elapsed());
                        sleep(Duration::from_millis(INTERVAL_MS));
                    }
                    info!("AccountsBackgroundService has stopped");
                    is_running.store(false, Ordering::Relaxed);
                }
            })
            .unwrap();

        Self {
            t_background,
            status: AbsStatus { is_running, stop },
        }
    }

    /// Should be called immediately after bank_fork_utils::load_bank_forks(), and as such, there
    /// should only be one bank, the root bank, in `bank_forks`
    /// All banks added to `bank_forks` will be descended from the root bank, and thus will inherit
    /// the bank drop callback.
    pub fn setup_bank_drop_callback(bank_forks: Arc<RwLock<BankForks>>) -> DroppedSlotsReceiver {
        assert_eq!(bank_forks.read().unwrap().banks().len(), 1);

        let (pruned_banks_sender, pruned_banks_receiver) = crossbeam_channel::unbounded();
        {
            let root_bank = bank_forks.read().unwrap().root_bank();

            root_bank
                .rc
                .accounts
                .accounts_db
                .enable_bank_drop_callback();
            root_bank.set_callback(Some(Box::new(SendDroppedBankCallback::new(
                pruned_banks_sender,
            ))));
        }
        pruned_banks_receiver
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_background.join()
    }

    /// Returns an object to query/manage the status of ABS
    pub fn status(&self) -> &AbsStatus {
        &self.status
    }
}

/// Query and manage the status of AccountsBackgroundService
#[derive(Debug, Clone)]
pub struct AbsStatus {
    /// Flag to query if ABS is running
    is_running: Arc<AtomicBool>,
    /// Flag to set to stop ABS
    stop: Arc<AtomicBool>,
}

impl AbsStatus {
    /// Returns if ABS is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Raises the flag for ABS to stop
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests() -> Self {
        Self {
            is_running: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Get the SnapshotKind from a given SnapshotRequest
#[must_use]
fn new_snapshot_kind(snapshot_request: &SnapshotRequest) -> Option<SnapshotKind> {
    match snapshot_request.request_kind {
        SnapshotRequestKind::FullSnapshot => Some(SnapshotKind::Archive(SnapshotArchiveKind::Full)),
        SnapshotRequestKind::IncrementalSnapshot => {
            if let Some(latest_full_snapshot_slot) = snapshot_request
                .snapshot_root_bank
                .rc
                .accounts
                .accounts_db
                .latest_full_snapshot_slot()
            {
                Some(SnapshotKind::Archive(SnapshotArchiveKind::Incremental(
                    latest_full_snapshot_slot,
                )))
            } else {
                warn!(
                    "Ignoring IncrementalSnapshot request for slot {} because there is no latest \
                     full snapshot",
                    snapshot_request.snapshot_root_bank.slot()
                );
                None
            }
        }
    }
}

/// Compare snapshot requests; used to pick the highest priority request to handle.
///
/// Priority, from highest to lowest:
/// - Epoch Accounts Hash
/// - Full Snapshot
/// - Incremental Snapshot
///
/// If two requests of the same kind are being compared, their bank slots are the tiebreaker.
#[must_use]
fn cmp_requests_by_priority(a: &SnapshotRequest, b: &SnapshotRequest) -> cmp::Ordering {
    let slot_a = a.snapshot_root_bank.slot();
    let slot_b = b.snapshot_root_bank.slot();
    cmp_snapshot_request_kinds_by_priority(&a.request_kind, &b.request_kind)
        .then(slot_a.cmp(&slot_b))
}

/// Compare snapshot request kinds by priority
///
/// Priority, from highest to lowest:
/// - Full Snapshot
/// - Incremental Snapshot
#[must_use]
fn cmp_snapshot_request_kinds_by_priority(
    a: &SnapshotRequestKind,
    b: &SnapshotRequestKind,
) -> cmp::Ordering {
    use {
        cmp::Ordering::{Equal, Greater, Less},
        SnapshotRequestKind as Kind,
    };
    match (a, b) {
        (Kind::FullSnapshot, Kind::FullSnapshot) => Equal,
        (Kind::FullSnapshot, Kind::IncrementalSnapshot) => Greater,
        (Kind::IncrementalSnapshot, Kind::FullSnapshot) => Less,
        (Kind::IncrementalSnapshot, Kind::IncrementalSnapshot) => Equal,
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::genesis_utils::create_genesis_config,
        agave_snapshots::{snapshot_config::SnapshotConfig, SnapshotInterval},
        crossbeam_channel::unbounded,
        solana_account::AccountSharedData,
        solana_epoch_schedule::EpochSchedule,
        solana_pubkey::Pubkey,
        std::num::NonZeroU64,
    };

    #[test]
    fn test_accounts_background_service_remove_dead_slots() {
        let genesis = create_genesis_config(10);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis.genesis_config));
        let (pruned_banks_sender, pruned_banks_receiver) = unbounded();
        let pruned_banks_request_handler = PrunedBanksRequestHandler {
            pruned_banks_receiver,
        };

        // Store an account in slot 0
        let account_key = Pubkey::new_unique();
        bank0.store_account(
            &account_key,
            &AccountSharedData::new(264, 0, &Pubkey::default()),
        );
        assert!(bank0.get_account(&account_key).is_some());
        pruned_banks_sender.send((0, 0)).unwrap();

        assert!(!bank0.rc.accounts.scan_slot(0, |_| Some(())).is_empty());

        pruned_banks_request_handler.remove_dead_slots(&bank0, &mut 0, &mut 0);

        assert!(bank0.rc.accounts.scan_slot(0, |_| Some(())).is_empty());
    }

    /// Ensure that unhandled snapshot requests are properly re-enqueued or dropped
    ///
    /// The snapshot request handler should be flexible and handle re-queueing unhandled snapshot
    /// requests, if those unhandled requests are for slots GREATER-THAN the last request handled.
    #[test]
    fn test_get_next_snapshot_request() {
        // These constants were picked to ensure the desired snapshot requests were sent to the
        // channel.  Ensure there are multiple requests of each kind.
        const SLOTS_PER_EPOCH: Slot = 400;
        const FULL_SNAPSHOT_INTERVAL: Slot = 80;
        const INCREMENTAL_SNAPSHOT_INTERVAL: Slot = 30;

        let snapshot_config = SnapshotConfig {
            full_snapshot_archive_interval: SnapshotInterval::Slots(
                NonZeroU64::new(FULL_SNAPSHOT_INTERVAL).unwrap(),
            ),
            incremental_snapshot_archive_interval: SnapshotInterval::Slots(
                NonZeroU64::new(INCREMENTAL_SNAPSHOT_INTERVAL).unwrap(),
            ),
            ..SnapshotConfig::default()
        };

        let pending_snapshot_packages = Arc::new(Mutex::new(PendingSnapshotPackages::default()));
        let (snapshot_request_sender, snapshot_request_receiver) = crossbeam_channel::unbounded();
        let snapshot_controller = Arc::new(SnapshotController::new(
            snapshot_request_sender.clone(),
            snapshot_config,
            0,
        ));
        let snapshot_request_handler = SnapshotRequestHandler {
            snapshot_controller,
            snapshot_request_receiver,
            pending_snapshot_packages,
        };

        let send_snapshot_request = |snapshot_root_bank, request_kind| {
            let snapshot_request = SnapshotRequest {
                snapshot_root_bank,
                status_cache_slot_deltas: Vec::default(),
                request_kind,
                enqueued: Instant::now(),
            };
            snapshot_request_sender.send(snapshot_request).unwrap();
        };

        let mut genesis_config_info = create_genesis_config(10);
        genesis_config_info.genesis_config.epoch_schedule =
            EpochSchedule::custom(SLOTS_PER_EPOCH, SLOTS_PER_EPOCH, false);
        let mut bank = Arc::new(Bank::new_for_tests(&genesis_config_info.genesis_config));

        // We need to get and set accounts-db's latest full snapshot slot to test
        // get_next_snapshot_request().  To workaround potential borrowing issues
        // caused by make_banks() below, Arc::clone bank0 and add helper functions.
        let bank0 = bank.clone();
        fn latest_full_snapshot_slot(bank: &Bank) -> Option<Slot> {
            bank.rc.accounts.accounts_db.latest_full_snapshot_slot()
        }
        fn set_latest_full_snapshot_slot(bank: &Bank, slot: Slot) {
            bank.rc
                .accounts
                .accounts_db
                .set_latest_full_snapshot_slot(slot);
        }

        // Create new banks and send snapshot requests so that the following requests will be in
        // the channel before handling the requests:
        //
        // fss  80
        // iss  90
        // iss 120
        // iss 150
        // fss 160
        // iss 180
        // iss 210
        // fss 240 <-- handled 1st
        // iss 270
        // iss 300 <-- handled 2nd
        //
        // Also, incremental snapshots before slot 240 (the first full snapshot handled), will
        // actually be skipped since the latest full snapshot slot will be `None`.
        let mut make_banks = |num_banks| {
            for _ in 0..num_banks {
                let slot = bank.slot() + 1;
                bank = Arc::new(Bank::new_from_parent(
                    bank.clone(),
                    &Pubkey::new_unique(),
                    slot,
                ));

                // Since we're not using `BankForks::set_root()`, we have to handle sending the
                // correct snapshot requests ourself.
                if bank.block_height().is_multiple_of(FULL_SNAPSHOT_INTERVAL) {
                    send_snapshot_request(Arc::clone(&bank), SnapshotRequestKind::FullSnapshot);
                } else if bank
                    .block_height()
                    .is_multiple_of(INCREMENTAL_SNAPSHOT_INTERVAL)
                {
                    send_snapshot_request(
                        Arc::clone(&bank),
                        SnapshotRequestKind::IncrementalSnapshot,
                    );
                }
            }
        };
        make_banks(303);

        // Ensure the full snapshot from slot 240 is handled 1st
        // (the older full snapshots are skipped and dropped)
        assert_eq!(latest_full_snapshot_slot(&bank0), None);
        let (snapshot_request, ..) = snapshot_request_handler
            .get_next_snapshot_request()
            .unwrap();
        assert_eq!(
            snapshot_request.request_kind,
            SnapshotRequestKind::FullSnapshot
        );
        assert_eq!(snapshot_request.snapshot_root_bank.slot(), 240);
        set_latest_full_snapshot_slot(&bank0, 240);

        // Ensure the incremental snapshot from slot 300 is handled 2nd
        // (the older incremental snapshots are skipped and dropped)
        assert_eq!(latest_full_snapshot_slot(&bank0), Some(240));
        let (snapshot_request, ..) = snapshot_request_handler
            .get_next_snapshot_request()
            .unwrap();
        assert_eq!(
            snapshot_request.request_kind,
            SnapshotRequestKind::IncrementalSnapshot
        );
        assert_eq!(snapshot_request.snapshot_root_bank.slot(), 300);

        // And now ensure the snapshot request channel is empty!
        assert_eq!(latest_full_snapshot_slot(&bank0), Some(240));
        assert!(snapshot_request_handler
            .get_next_snapshot_request()
            .is_none());
    }

    /// Ensure that we can prune banks with the same slot (if they were on different forks)
    #[test]
    fn test_pruned_banks_request_handler_handle_request() {
        let (pruned_banks_sender, pruned_banks_receiver) = crossbeam_channel::unbounded();
        let pruned_banks_request_handler = PrunedBanksRequestHandler {
            pruned_banks_receiver,
        };
        let genesis_config_info = create_genesis_config(10);
        let bank = Bank::new_for_tests(&genesis_config_info.genesis_config);
        bank.rc.accounts.accounts_db.enable_bank_drop_callback();
        bank.set_callback(Some(Box::new(SendDroppedBankCallback::new(
            pruned_banks_sender,
        ))));

        let fork0_bank0 = Arc::new(bank);
        let fork0_bank1 = Arc::new(Bank::new_from_parent(
            fork0_bank0.clone(),
            &Pubkey::new_unique(),
            fork0_bank0.slot() + 1,
        ));
        let fork1_bank1 = Arc::new(Bank::new_from_parent(
            fork0_bank0.clone(),
            &Pubkey::new_unique(),
            fork0_bank0.slot() + 1,
        ));
        let fork2_bank1 = Arc::new(Bank::new_from_parent(
            fork0_bank0.clone(),
            &Pubkey::new_unique(),
            fork0_bank0.slot() + 1,
        ));
        let fork0_bank2 = Arc::new(Bank::new_from_parent(
            fork0_bank1.clone(),
            &Pubkey::new_unique(),
            fork0_bank1.slot() + 1,
        ));
        let fork1_bank2 = Arc::new(Bank::new_from_parent(
            fork1_bank1.clone(),
            &Pubkey::new_unique(),
            fork1_bank1.slot() + 1,
        ));
        let fork0_bank3 = Arc::new(Bank::new_from_parent(
            fork0_bank2.clone(),
            &Pubkey::new_unique(),
            fork0_bank2.slot() + 1,
        ));
        let fork3_bank3 = Arc::new(Bank::new_from_parent(
            fork0_bank2.clone(),
            &Pubkey::new_unique(),
            fork0_bank2.slot() + 1,
        ));
        fork0_bank3.squash();

        drop(fork3_bank3);
        drop(fork1_bank2);
        drop(fork0_bank2);
        drop(fork1_bank1);
        drop(fork2_bank1);
        drop(fork0_bank1);
        drop(fork0_bank0);
        let num_banks_purged = pruned_banks_request_handler.handle_request(&fork0_bank3);
        assert_eq!(num_banks_purged, 7);
    }

    #[test]
    fn test_cmp_snapshot_request_kinds_by_priority() {
        use cmp::Ordering::{Equal, Greater, Less};
        for (snapshot_request_kind_a, snapshot_request_kind_b, expected_result) in [
            (
                SnapshotRequestKind::FullSnapshot,
                SnapshotRequestKind::FullSnapshot,
                Equal,
            ),
            (
                SnapshotRequestKind::FullSnapshot,
                SnapshotRequestKind::IncrementalSnapshot,
                Greater,
            ),
            (
                SnapshotRequestKind::IncrementalSnapshot,
                SnapshotRequestKind::FullSnapshot,
                Less,
            ),
            (
                SnapshotRequestKind::IncrementalSnapshot,
                SnapshotRequestKind::IncrementalSnapshot,
                Equal,
            ),
        ] {
            let actual_result = cmp_snapshot_request_kinds_by_priority(
                &snapshot_request_kind_a,
                &snapshot_request_kind_b,
            );
            assert_eq!(expected_result, actual_result);
        }
    }
}
