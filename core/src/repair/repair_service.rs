//! The `repair_service` module implements the tools necessary to generate a thread which
//! regularly finds missing shreds in the ledger and sends repair requests for those shreds
use {
    super::standard_repair_handler::StandardRepairHandler,
    crate::{
        cluster_slots_service::cluster_slots::ClusterSlots,
        repair::{
            ancestor_hashes_service::{
                AncestorHashesChannels, AncestorHashesReplayUpdateReceiver, AncestorHashesService,
            },
            duplicate_repair_status::AncestorDuplicateSlotToRepair,
            outstanding_requests::OutstandingRequests,
            repair_weight::RepairWeight,
            serve_repair::{
                REPAIR_PEERS_CACHE_CAPACITY, RepairPeers, RepairProtocol, RepairRequestHeader,
                ServeRepair, ShredRepairType,
            },
        },
    },
    agave_votor_messages::{VerifiedVoterSlotsReceiver, migration::MigrationStatus},
    ahash::AHashMap,
    bytes::Bytes,
    crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender},
    lazy_lru::LruCache,
    rand::prelude::IndexedRandom as _,
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_keypair::Signer,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_meta::{BlockLocation, SlotMetaRepair},
        shred,
    },
    solana_measure::measure::Measure,
    solana_net_utils::{PinnedXdpSender, Protocol},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
    },
    solana_streamer::sendmmsg::{SendPktsError, batch_send},
    solana_time_utils::timestamp,
    std::{
        collections::{HashMap, HashSet, hash_map::Entry},
        iter::Iterator,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle, sleep},
        time::{Duration, Instant},
    },
};
#[cfg(test)]
use {crate::repair::duplicate_repair_status::DuplicateSlotRepairStatus, solana_keypair::Keypair};

// Time to defer repair requests after repair observes, or infers from a later
// FEC, that the missing shred's FEC set has started arriving. Value was derived
// empirically observing slot times vs. repair activity on mainnet nodes.
const FEC_REPAIR_DELAY: Duration = Duration::from_millis(250);

// This is the amount of time we will wait for a repair request to be fulfilled
// before making another request. Value is based on reasonable upper bound of
// expected network delays in requesting repairs and receiving shreds.
pub(crate) const REPAIR_REQUEST_TIMEOUT_MS: u64 = 150;

// When requesting repair for a specific shred through the admin RPC, we will
// request up to NUM_PEERS_TO_SAMPLE_FOR_REPAIRS in the event a specific, valid
// target node is not provided. This number was chosen to provide reasonable
// chance of sampling duplicate in the event of cluster partition.
const NUM_PEERS_TO_SAMPLE_FOR_REPAIRS: usize = 10;

// Minimum initial capacity for FEC set observations in a slot. This is to avoid
// frequent reallocations for typical mainnet blocks while still letting unusually
// large blocks grow lazily.
const MIN_FEC_SET_OBSERVATION_CAPACITY: usize = 32;

/// Returns the fixed-size FEC set ordinal containing `shred_index`.
fn fec_set_ordinal(shred_index: u64) -> usize {
    shred_index as usize / shred::DATA_SHREDS_PER_FEC_BLOCK
}

/// Per-slot repair timing tracked at FEC granularity.
///
/// This intentionally avoids per-shred timestamps. `first_observed_at_ms`
/// is indexed by FEC set ordinal and stores when repair first observed, or
/// inferred from a later FEC, that the FEC set had started arriving. Observation
/// is based on `SlotMeta::received`, which is the highest received data shred
/// index plus one.
#[derive(Debug)]
struct SlotRepairFecTimes {
    /// Timestamp each FEC set in this slot was first observed, keyed by its
    /// ordinal. Observed here means a valid shred in this FEC set or a later
    /// FEC set was received.
    first_observed_at_ms: Vec<u64>,
}

impl SlotRepairFecTimes {
    /// Creates per-slot FEC timing state with enough capacity for the expected
    /// number of observed FEC sets.
    fn with_capacity(capacity: usize) -> Self {
        Self {
            first_observed_at_ms: Vec::with_capacity(capacity),
        }
    }

    /// Records that repair observed a shred from `fec_set_ordinal` at `now_ms`.
    ///
    /// Resizing with `now_ms` is intentional: if repair observes a future FEC
    /// before an earlier one, the skipped FECs inherit the future observation
    /// time. This lets missing FEC `N` become eligible based on observing FEC
    /// `N+1` without storing anything per shred.
    fn observe_fec(&mut self, fec_set_ordinal: usize, now_ms: u64) {
        let target_len = fec_set_ordinal.saturating_add(1);
        if self.first_observed_at_ms.len() < target_len {
            self.first_observed_at_ms.resize(target_len, now_ms);
        }
    }

    /// Returns the first observed or inferred time for the FEC set containing
    /// `shred_index`.
    fn first_shred_observed_at(&self, shred_index: u64) -> Option<u64> {
        self.first_observed_at_ms
            .get(fec_set_ordinal(shred_index))
            .copied()
    }
}

/// Tracks when missing shred repairs should become eligible.
///
/// Normal missing-shred repairs wait until repair has observed, or inferred from
/// a later FEC, the missing shred's FEC set for at least `FEC_REPAIR_DELAY`.
/// Highest-shred repairs generated from normal slot traversal use the last
/// observed FEC set as their quiet-period anchor. The generic unknown-last-index
/// traversal intentionally bypasses this state to preserve legacy aggressive
/// probing.
#[derive(Debug, Default)]
pub struct RepairEligibility {
    slots: AHashMap<Slot, SlotRepairFecTimes>,
}

impl RepairEligibility {
    /// Drops timing state for slots below the current root.
    fn set_root(&mut self, root: Slot) {
        self.slots.retain(|slot, _| *slot >= root);
    }

    /// Records observed or inferred FEC timing based on `SlotMeta::received`
    /// advancing.
    ///
    /// `first_observed_at_ms.len()` is the next unseen FEC ordinal. If the
    /// highest received shred falls in an older FEC, that FEC was already
    /// timestamped; if it falls in a future FEC, resizing the vector backfills
    /// any skipped FECs with the same timestamp.
    fn observe_slot(&mut self, slot: Slot, slot_meta: &SlotMetaRepair) {
        let Some(last_received_index) = slot_meta.received.checked_sub(1) else {
            // Have not observed any shreds for this slot yet.
            return;
        };
        let last_received_fec = fec_set_ordinal(last_received_index);

        // Grab the FEC set observation meta for this slot. New slots start with
        // enough capacity for typical mainnet blocks, while unusually large
        // blocks grow lazily when a new FEC is actually observed.
        let observed_fec_sets = last_received_fec.saturating_add(1);
        let slot_repair = self.slots.entry(slot).or_insert_with(|| {
            SlotRepairFecTimes::with_capacity(
                observed_fec_sets.max(MIN_FEC_SET_OBSERVATION_CAPACITY),
            )
        });

        if slot_repair.first_observed_at_ms.len() > last_received_fec {
            // No new FECs observed.
            return;
        }
        let now_ms = timestamp();

        // Backfills skipped FEC ordinals with the same timestamp.
        slot_repair.observe_fec(last_received_fec, now_ms);
    }

    /// Returns whether a missing shred's FEC timestamp is old enough to request
    /// repair.
    pub(crate) fn is_missing_shred_eligible(
        &self,
        slot: Slot,
        shred_index: u64,
        now_ms: u64,
    ) -> bool {
        self.slots
            .get(&slot)
            .and_then(|slot_repair| slot_repair.first_shred_observed_at(shred_index))
            .map(|first_shred_time| {
                now_ms.saturating_sub(first_shred_time) >= FEC_REPAIR_DELAY.as_millis() as u64
            })
            .unwrap_or(false)
    }

    /// Returns whether normal slot traversal should request `HighestShred` for
    /// `slot`.
    ///
    /// This path only applies while the slot's true last index is unknown. Empty
    /// slots are eligible immediately so parent-slot repair can make progress;
    /// otherwise the highest observed FEC set must have been quiet for
    /// `FEC_REPAIR_DELAY`.
    pub(crate) fn is_highest_shred_eligible(
        &self,
        slot: Slot,
        slot_meta: &SlotMetaRepair,
        now_ms: u64,
    ) -> bool {
        if slot_meta.last_index.is_some() {
            return false;
        }
        if slot_meta.received == 0 {
            return true;
        }
        self.slots
            .get(&slot)
            .and_then(|slot_repair| slot_repair.first_observed_at_ms.last().copied())
            .map(|last_observed_fec_time| {
                now_ms.saturating_sub(last_observed_fec_time) >= FEC_REPAIR_DELAY.as_millis() as u64
            })
            .unwrap_or(false)
    }

    /// Marks already-observed FEC sets as old enough for repair tests.
    ///
    /// This keeps traversal tests focused on ordering and duplicate suppression
    /// without sleeping in every setup path.
    #[cfg(test)]
    pub(crate) fn observe_slot_as_elapsed_for_tests(
        &mut self,
        slot: Slot,
        slot_meta: &SlotMetaRepair,
    ) {
        self.observe_slot(slot, slot_meta);
        if let Some(slot_repair) = self.slots.get_mut(&slot) {
            let elapsed_time = timestamp().saturating_sub(FEC_REPAIR_DELAY.as_millis() as u64);
            slot_repair
                .first_observed_at_ms
                .iter_mut()
                .for_each(|first_shred_time| *first_shred_time = elapsed_time);
        }
    }

    /// Builds test eligibility state with elapsed observations for several
    /// slots.
    #[cfg(test)]
    pub(crate) fn elapsed_for_slots_for_tests(
        blockstore: &Blockstore,
        slots: impl IntoIterator<Item = Slot>,
    ) -> Self {
        let mut repair_eligibility = Self::default();
        for slot in slots {
            if let Some(slot_meta) = blockstore.meta_repair(slot).unwrap() {
                repair_eligibility.observe_slot_as_elapsed_for_tests(slot, &slot_meta);
            }
        }
        repair_eligibility
    }
}

pub type AncestorDuplicateSlotsSender = CrossbeamSender<AncestorDuplicateSlotToRepair>;
pub type AncestorDuplicateSlotsReceiver = CrossbeamReceiver<AncestorDuplicateSlotToRepair>;
pub type ConfirmedSlotsSender = CrossbeamSender<Vec<Slot>>;
pub type ConfirmedSlotsReceiver = CrossbeamReceiver<Vec<Slot>>;
pub type DumpedSlotsSender = CrossbeamSender<Vec<(Slot, Hash)>>;
pub type DumpedSlotsReceiver = CrossbeamReceiver<Vec<(Slot, Hash)>>;
pub type OutstandingShredRepairs = OutstandingRequests<ShredRepairType, BlockLocation>;
pub type PopularPrunedForksSender = CrossbeamSender<Vec<Slot>>;
pub type PopularPrunedForksReceiver = CrossbeamReceiver<Vec<Slot>>;

#[derive(Default, Debug)]
pub struct SlotRepairs {
    highest_shred_index: u64,
    // map from pubkey to total number of requests
    pubkey_repairs: HashMap<Pubkey, u64>,
}

impl SlotRepairs {
    pub fn pubkey_repairs(&self) -> &HashMap<Pubkey, u64> {
        &self.pubkey_repairs
    }
}

#[derive(Default, Debug)]
pub struct RepairStatsGroup {
    pub count: u64,
    pub min: u64,
    pub max: u64,
    pub slot_pubkeys: HashMap<Slot, SlotRepairs>,
}

impl RepairStatsGroup {
    pub fn update(&mut self, repair_peer_id: &Pubkey, slot: Slot, shred_index: u64) {
        self.count += 1;
        let slot_repairs = self.slot_pubkeys.entry(slot).or_default();
        // Increment total number of repairs of this type for this pubkey by 1
        *slot_repairs
            .pubkey_repairs
            .entry(*repair_peer_id)
            .or_default() += 1;
        // Update the max requested shred index for this slot
        slot_repairs.highest_shred_index =
            std::cmp::max(slot_repairs.highest_shred_index, shred_index);
        if self.min == 0 {
            self.min = slot;
        } else {
            self.min = std::cmp::min(self.min, slot);
        }
        self.max = std::cmp::max(self.max, slot);
    }
}

#[derive(Debug)]
pub struct RepairMetrics {
    pub stats: RepairStats,
    pub best_repairs_stats: BestRepairsStats,
    pub timing: RepairTiming,
    pub last_report: Instant,
}

impl Default for RepairMetrics {
    fn default() -> Self {
        Self {
            stats: RepairStats::default(),
            best_repairs_stats: BestRepairsStats::default(),
            timing: RepairTiming::default(),
            last_report: Instant::now(),
        }
    }
}

impl RepairMetrics {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    pub fn maybe_report(&mut self) {
        if self.last_report.elapsed() > Self::REPORT_INTERVAL {
            self.stats.report();
            self.timing.report();
            self.best_repairs_stats.report();
            *self = Self::default();
        }
    }
}

#[derive(Default, Debug)]
pub struct RepairStats {
    pub shred: RepairStatsGroup,
    pub highest_shred: RepairStatsGroup,
    pub orphan: RepairStatsGroup,
    pub get_best_orphans_us: u64,
    pub get_best_shreds_us: u64,
}

impl RepairStats {
    fn report(&self) {
        let repair_total = self.shred.count + self.highest_shred.count + self.orphan.count;
        let slot_to_count: Vec<_> = self
            .shred
            .slot_pubkeys
            .iter()
            .chain(self.highest_shred.slot_pubkeys.iter())
            .chain(self.orphan.slot_pubkeys.iter())
            .map(|(slot, slot_repairs)| (slot, slot_repairs.pubkey_repairs.values().sum::<u64>()))
            .collect();
        info!("repair_stats: {slot_to_count:?}");
        if repair_total > 0 {
            let nonzero_num = |x| if x == 0 { None } else { Some(x) };
            datapoint_info!(
                "repair_service-my_requests",
                ("repair-total", repair_total, i64),
                ("shred-count", self.shred.count, i64),
                ("highest-shred-count", self.highest_shred.count, i64),
                ("orphan-count", self.orphan.count, i64),
                ("shred-slot-max", nonzero_num(self.shred.max), Option<i64>),
                ("shred-slot-min", nonzero_num(self.shred.min), Option<i64>),
                ("highest-shred-slot-max", nonzero_num(self.highest_shred.max), Option<i64>),
                ("highest-shred-slot-min", nonzero_num(self.highest_shred.min), Option<i64>),
                ("orphan-slot-max", nonzero_num(self.orphan.max), Option<i64>),
                ("orphan-slot-min", nonzero_num(self.orphan.min), Option<i64>),
            );
        }
    }
}

#[derive(Default, Debug)]
pub struct RepairTiming {
    pub set_root_us: u64,
    pub dump_slots_us: u64,
    pub get_votes_us: u64,
    pub add_voters_us: u64,
    pub purge_outstanding_repairs_us: u64,
    pub handle_popular_pruned_forks_us: u64,
    pub get_best_orphans_us: u64,
    pub get_best_shreds_us: u64,
    pub get_unknown_last_index_us: u64,
    pub get_closest_completion_us: u64,
    pub build_batch_us: u64,
    pub send_batch_us: u64,
}

impl RepairTiming {
    fn report(&self) {
        datapoint_info!(
            "repair_service-repair_timing",
            ("set-root-us", self.set_root_us, i64),
            ("dump-slots-us", self.dump_slots_us, i64),
            ("get-votes-us", self.get_votes_us, i64),
            ("add-voters-us", self.add_voters_us, i64),
            (
                "purge-outstanding-repairs-us",
                self.purge_outstanding_repairs_us,
                i64
            ),
            (
                "handle-popular-pruned-forks-us",
                self.handle_popular_pruned_forks_us,
                i64
            ),
            ("get-best-orphans-us", self.get_best_orphans_us, i64),
            ("get-best-shreds-us", self.get_best_shreds_us, i64),
            (
                "get-unknown-last-index-us",
                self.get_unknown_last_index_us,
                i64
            ),
            (
                "get-closest-completion-us",
                self.get_closest_completion_us,
                i64
            ),
            ("build-batch-us", self.build_batch_us, i64),
            ("send-batch-us", self.send_batch_us, i64),
        );
    }
}

#[derive(Default, Debug)]
pub struct BestRepairsStats {
    pub call_count: u64,
    pub num_orphan_slots: u64,
    pub num_orphan_repairs: u64,
    pub num_best_shreds_slots: u64,
    pub num_best_shreds_repairs: u64,
    pub num_unknown_last_index_slots: u64,
    pub num_unknown_last_index_repairs: u64,
    pub num_closest_completion_slots: u64,
    pub num_closest_completion_slots_path: u64,
    pub num_closest_completion_repairs: u64,
    pub num_repair_trees: u64,
}

impl BestRepairsStats {
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        num_orphan_slots: u64,
        num_orphan_repairs: u64,
        num_best_shreds_slots: u64,
        num_best_shreds_repairs: u64,
        num_unknown_last_index_slots: u64,
        num_unknown_last_index_repairs: u64,
        num_closest_completion_slots: u64,
        num_closest_completion_slots_path: u64,
        num_closest_completion_repairs: u64,
        num_repair_trees: u64,
    ) {
        self.call_count += 1;
        self.num_orphan_slots += num_orphan_slots;
        self.num_orphan_repairs += num_orphan_repairs;
        self.num_best_shreds_slots += num_best_shreds_slots;
        self.num_best_shreds_repairs += num_best_shreds_repairs;
        self.num_unknown_last_index_slots += num_unknown_last_index_slots;
        self.num_unknown_last_index_repairs += num_unknown_last_index_repairs;
        self.num_closest_completion_slots += num_closest_completion_slots;
        self.num_closest_completion_slots_path += num_closest_completion_slots_path;
        self.num_closest_completion_repairs += num_closest_completion_repairs;
        self.num_repair_trees += num_repair_trees;
    }

    fn report(&self) {
        datapoint_info!(
            "serve_repair-best-repairs",
            ("call-count", self.call_count, i64),
            ("orphan-slots", self.num_orphan_slots, i64),
            ("orphan-repairs", self.num_orphan_repairs, i64),
            ("best-shreds-slots", self.num_best_shreds_slots, i64),
            ("best-shreds-repairs", self.num_best_shreds_repairs, i64),
            (
                "unknown-last-index-slots",
                self.num_unknown_last_index_slots,
                i64
            ),
            (
                "unknown-last-index-repairs",
                self.num_unknown_last_index_repairs,
                i64
            ),
            (
                "closest-completion-slots",
                self.num_closest_completion_slots,
                i64
            ),
            (
                "closest-completion-slots-path",
                self.num_closest_completion_slots_path,
                i64
            ),
            (
                "closest-completion-repairs",
                self.num_closest_completion_repairs,
                i64
            ),
            ("repair-trees", self.num_repair_trees, i64),
        );
    }
}

pub const MAX_REPAIR_LENGTH: usize = 512;
pub const MAX_REPAIR_PER_DUPLICATE: usize = 20;
pub const MAX_DUPLICATE_WAIT_MS: usize = 10_000;
pub const REPAIR_MS: u64 = 1;
pub const MAX_ORPHANS: usize = 5;
pub const MAX_UNKNOWN_LAST_INDEX_REPAIRS: usize = 10;
pub const MAX_CLOSEST_COMPLETION_REPAIRS: usize = 100;

#[derive(Clone)]
pub struct RepairInfo {
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub cluster_slots: Arc<ClusterSlots>,
    pub epoch_schedule: EpochSchedule,
    pub ancestor_duplicate_slots_sender: AncestorDuplicateSlotsSender,
    // Validators from which repairs are requested
    pub repair_validators: Option<HashSet<Pubkey>>,
    // Validators which should be given priority when serving
    pub repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
}

pub struct RepairSlotRange {
    pub start: Slot,
    pub end: Slot,
}

impl Default for RepairSlotRange {
    fn default() -> Self {
        RepairSlotRange {
            start: 0,
            end: u64::MAX,
        }
    }
}

struct RepairChannels {
    verified_voter_slots_receiver: VerifiedVoterSlotsReceiver,
    dumped_slots_receiver: DumpedSlotsReceiver,
    popular_pruned_forks_sender: PopularPrunedForksSender,
}

pub struct RepairServiceChannels {
    repair_channels: RepairChannels,
    ancestors_hashes_channels: AncestorHashesChannels,
}

impl RepairServiceChannels {
    pub fn new(
        verified_voter_slots_receiver: VerifiedVoterSlotsReceiver,
        dumped_slots_receiver: DumpedSlotsReceiver,
        popular_pruned_forks_sender: PopularPrunedForksSender,
        ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
    ) -> Self {
        Self {
            repair_channels: RepairChannels {
                verified_voter_slots_receiver,
                dumped_slots_receiver,
                popular_pruned_forks_sender,
            },
            ancestors_hashes_channels: AncestorHashesChannels {
                ancestor_hashes_replay_update_receiver,
            },
        }
    }
}

struct RepairTracker {
    sharable_banks: SharableBanks,
    repair_weight: RepairWeight,
    serve_repair: ServeRepair,
    repair_metrics: RepairMetrics,
    peers_cache: LruCache<u64, RepairPeers>,
    popular_pruned_forks_requests: HashSet<Slot>,
    // Maps a repair that may still be outstanding to the timestamp it was requested.
    outstanding_repairs: HashMap<ShredRepairType, u64>,
    repair_eligibility: RepairEligibility,
}

pub struct RepairService {
    t_repair: JoinHandle<()>,
    ancestor_hashes_service: Option<AncestorHashesService>,
}

impl RepairService {
    pub fn new(
        blockstore: Arc<Blockstore>,
        exit: Arc<AtomicBool>,
        repair_socket: Arc<UdpSocket>,
        ancestor_hashes_socket: Arc<UdpSocket>,
        repair_info: RepairInfo,
        outstanding_requests: Arc<RwLock<OutstandingShredRepairs>>,
        repair_service_channels: RepairServiceChannels,
        xdp_sender: Option<PinnedXdpSender>,
    ) -> Self {
        let t_repair = {
            let blockstore = blockstore.clone();
            let exit = exit.clone();
            let repair_info = repair_info.clone();
            Builder::new()
                .name("solRepairSvc".to_string())
                .spawn(move || {
                    Self::run(
                        blockstore,
                        &exit,
                        &repair_socket,
                        repair_service_channels.repair_channels,
                        repair_info,
                        &outstanding_requests,
                        xdp_sender,
                    )
                })
                .unwrap()
        };

        let ancestor_hashes_service = AncestorHashesService::new(
            exit,
            blockstore,
            ancestor_hashes_socket,
            repair_service_channels.ancestors_hashes_channels,
            repair_info,
        );

        RepairService {
            t_repair,
            ancestor_hashes_service,
        }
    }

    fn update_weighting_heuristic(
        blockstore: &Blockstore,
        root_bank: Arc<Bank>,
        repair_weight: &mut RepairWeight,
        popular_pruned_forks_requests: &mut HashSet<Slot>,
        dumped_slots_receiver: &DumpedSlotsReceiver,
        verified_voter_slots_receiver: &VerifiedVoterSlotsReceiver,
        migration_status: &MigrationStatus,
        repair_metrics: &mut RepairMetrics,
    ) {
        if repair_weight.is_pruned_tree_tracking_enabled()
            && migration_status.is_alpenglow_enabled()
        {
            repair_weight.disable_pruned_tree_tracking();
            popular_pruned_forks_requests.clear();
        }

        // Purge outdated slots from the weighting heuristic
        let mut set_root_us = Measure::start("set_root_us");
        repair_weight.set_root(root_bank.slot());
        set_root_us.stop();

        // Remove dumped slots from the weighting heuristic
        let mut dump_slots_us = Measure::start("dump_slots_us");
        dumped_slots_receiver
            .try_iter()
            .for_each(|slot_hash_keys_to_dump| {
                // Currently we don't use the correct_hash in repair. Since this dumped
                // slot is DuplicateConfirmed, we have a >= 52% chance on receiving the
                // correct version.
                for (slot, _correct_hash) in slot_hash_keys_to_dump {
                    // `slot` is dumped in blockstore wanting to be repaired, we orphan it along with
                    // descendants while copying the weighting heuristic so that it can be
                    // repaired with correct ancestor information
                    //
                    // We still check to see if this slot is too old, as bank forks root
                    // might have been updated in between the send and our receive. If that
                    // is the case we can safely ignore this dump request as the slot in
                    // question would have already been purged in `repair_weight.set_root`
                    // and there is no chance of it being part of the rooted path.
                    if slot >= repair_weight.root() {
                        let dumped_slots = repair_weight.split_off(slot);
                        // Remove from outstanding ancestor hashes requests. Also clean any
                        // requests that might have been since fixed
                        popular_pruned_forks_requests.retain(|slot| {
                            !dumped_slots.contains(slot) && repair_weight.is_pruned(*slot)
                        });
                    }
                }
            });
        dump_slots_us.stop();

        // Add new votes to the weighting heuristic
        let mut get_votes_us = Measure::start("get_votes_us");
        let mut slot_to_vote_pubkeys: HashMap<Slot, Vec<Pubkey>> = HashMap::new();
        verified_voter_slots_receiver
            .try_iter()
            .for_each(|(vote_pubkey, vote_slots)| {
                for slot in vote_slots {
                    slot_to_vote_pubkeys
                        .entry(slot)
                        .or_default()
                        .push(vote_pubkey);
                }
            });
        get_votes_us.stop();

        let mut add_voters_us = Measure::start("add_voters_us");
        repair_weight.add_voters(
            blockstore,
            slot_to_vote_pubkeys.into_iter(),
            root_bank.epoch_stakes_map(),
            root_bank.epoch_schedule(),
        );
        add_voters_us.stop();

        repair_metrics.timing.set_root_us += set_root_us.as_us();
        repair_metrics.timing.dump_slots_us += dump_slots_us.as_us();
        repair_metrics.timing.get_votes_us += get_votes_us.as_us();
        repair_metrics.timing.add_voters_us += add_voters_us.as_us();
    }

    fn identify_repairs(
        blockstore: &Blockstore,
        root_bank: Arc<Bank>,
        _repair_info: &RepairInfo,
        repair_weight: &mut RepairWeight,
        repair_eligibility: &mut RepairEligibility,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
        repair_metrics: &mut RepairMetrics,
    ) -> Vec<ShredRepairType> {
        let mut purge_outstanding_repairs_us = Measure::start("purge_outstanding_repairs_us");
        // Purge old entries. They've either completed or need to be retried.
        outstanding_repairs.retain(|_repair_request, time| {
            timestamp().saturating_sub(*time) < REPAIR_REQUEST_TIMEOUT_MS
        });
        purge_outstanding_repairs_us.stop();
        repair_metrics.timing.purge_outstanding_repairs_us += purge_outstanding_repairs_us.as_us();
        repair_eligibility.set_root(root_bank.slot());

        repair_weight.get_best_weighted_repairs(
            blockstore,
            root_bank.epoch_stakes_map(),
            root_bank.epoch_schedule(),
            MAX_ORPHANS,
            MAX_REPAIR_LENGTH,
            MAX_UNKNOWN_LAST_INDEX_REPAIRS,
            MAX_CLOSEST_COMPLETION_REPAIRS,
            repair_eligibility,
            repair_metrics,
            outstanding_repairs,
        )
    }

    fn handle_popular_pruned_forks(
        root_bank: Arc<Bank>,
        repair_weight: &mut RepairWeight,
        popular_pruned_forks_requests: &mut HashSet<Slot>,
        popular_pruned_forks_sender: &PopularPrunedForksSender,
        repair_metrics: &mut RepairMetrics,
    ) {
        let mut handle_popular_pruned_forks_us = Measure::start("handle_popular_pruned_forks_us");
        let mut popular_pruned_forks = repair_weight
            .get_popular_pruned_forks(root_bank.epoch_stakes_map(), root_bank.epoch_schedule());
        // Check if we've already sent a request along this pruned fork
        popular_pruned_forks.retain(|slot| {
            if popular_pruned_forks_requests
                .iter()
                .any(|prev_req_slot| repair_weight.same_tree(*slot, *prev_req_slot))
            {
                false
            } else {
                popular_pruned_forks_requests.insert(*slot);
                true
            }
        });
        if !popular_pruned_forks.is_empty() {
            warn!("Notifying repair of popular pruned forks {popular_pruned_forks:?}");
            popular_pruned_forks_sender
                .send(popular_pruned_forks)
                .unwrap_or_else(|err| error!("failed to send popular pruned forks {err}"));
        }
        handle_popular_pruned_forks_us.stop();
        repair_metrics.timing.handle_popular_pruned_forks_us +=
            handle_popular_pruned_forks_us.as_us();
    }

    fn build_and_send_repair_batch(
        serve_repair: &mut ServeRepair,
        peers_cache: &mut LruCache<u64, RepairPeers>,
        repairs: Vec<ShredRepairType>,
        repair_info: &RepairInfo,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        repair_socket: &UdpSocket,
        xdp_sender: Option<&PinnedXdpSender>,
        repair_metrics: &mut RepairMetrics,
    ) {
        let mut build_batch_us = Measure::start("build_batch_us");
        let batch: Vec<(Vec<u8>, SocketAddr)> = {
            let mut outstanding_requests = outstanding_requests.write().unwrap();
            repairs
                .into_iter()
                .filter_map(|repair_request| {
                    let (to, req) = serve_repair
                        .repair_request(
                            repair_info,
                            repair_request,
                            peers_cache,
                            &mut repair_metrics.stats,
                            &mut outstanding_requests,
                        )
                        .ok()??;
                    Some((req, to))
                })
                .collect()
        };
        build_batch_us.stop();

        let mut send_batch_us = Measure::start("send_batch_us");
        if !batch.is_empty() {
            let num_pkts = batch.len();
            if let Some(xdp) = xdp_sender {
                for (i, (bytes, addr)) in batch.into_iter().enumerate() {
                    if let Err(e) = xdp.try_send(i, addr, Bytes::from(bytes)) {
                        warn!("repair xdp send failed: {e:?}");
                    }
                }
            } else {
                let batch = batch.iter().map(|(bytes, addr)| (bytes, addr));
                match batch_send(repair_socket, batch) {
                    Ok(()) => (),
                    Err(SendPktsError::IoError(err, num_failed)) => {
                        error!(
                            "{} batch_send failed to send {num_failed}/{num_pkts} packets first \
                             error {err:?}",
                            repair_info.cluster_info.id()
                        );
                    }
                }
            }
        }
        send_batch_us.stop();

        repair_metrics.timing.build_batch_us += build_batch_us.as_us();
        repair_metrics.timing.send_batch_us += send_batch_us.as_us();
    }

    fn run_repair_iteration(
        blockstore: &Blockstore,
        repair_channels: &RepairChannels,
        repair_info: &RepairInfo,
        repair_tracker: &mut RepairTracker,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        repair_socket: &UdpSocket,
        xdp_sender: Option<&PinnedXdpSender>,
        migration_status: &MigrationStatus,
    ) {
        let RepairChannels {
            verified_voter_slots_receiver,
            dumped_slots_receiver,
            popular_pruned_forks_sender,
        } = repair_channels;
        let RepairTracker {
            sharable_banks,
            repair_weight,
            serve_repair,
            repair_metrics,
            peers_cache,
            popular_pruned_forks_requests,
            outstanding_repairs,
            repair_eligibility,
        } = repair_tracker;
        let root_bank = sharable_banks.root();

        Self::update_weighting_heuristic(
            blockstore,
            root_bank.clone(),
            repair_weight,
            popular_pruned_forks_requests,
            dumped_slots_receiver,
            verified_voter_slots_receiver,
            migration_status,
            repair_metrics,
        );

        let repairs = Self::identify_repairs(
            blockstore,
            root_bank.clone(),
            repair_info,
            repair_weight,
            repair_eligibility,
            outstanding_repairs,
            repair_metrics,
        );

        if !migration_status.is_alpenglow_enabled() {
            Self::handle_popular_pruned_forks(
                root_bank.clone(),
                repair_weight,
                popular_pruned_forks_requests,
                popular_pruned_forks_sender,
                repair_metrics,
            );
        }

        Self::build_and_send_repair_batch(
            serve_repair,
            peers_cache,
            repairs,
            repair_info,
            outstanding_requests,
            repair_socket,
            xdp_sender,
            repair_metrics,
        );
    }

    fn run(
        blockstore: Arc<Blockstore>,
        exit: &AtomicBool,
        repair_socket: &UdpSocket,
        repair_channels: RepairChannels,
        repair_info: RepairInfo,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        xdp_sender: Option<PinnedXdpSender>,
    ) {
        let (sharable_banks, migration_status) = {
            let bank_forks_r = repair_info.bank_forks.read().unwrap();
            (
                bank_forks_r.sharable_banks(),
                bank_forks_r.migration_status(),
            )
        };
        let root_bank_slot = sharable_banks.root().slot();
        let mut repair_tracker = RepairTracker {
            sharable_banks: sharable_banks.clone(),
            repair_weight: RepairWeight::new(root_bank_slot),
            serve_repair: {
                ServeRepair::new(
                    repair_info.cluster_info.clone(),
                    sharable_banks,
                    repair_info.repair_whitelist.clone(),
                    Box::new(StandardRepairHandler::new(blockstore.clone())),
                    migration_status.clone(),
                )
            },
            repair_metrics: RepairMetrics::default(),
            peers_cache: LruCache::new(REPAIR_PEERS_CACHE_CAPACITY),
            popular_pruned_forks_requests: HashSet::new(),
            outstanding_repairs: HashMap::new(),
            repair_eligibility: RepairEligibility::default(),
        };

        while !exit.load(Ordering::Relaxed) {
            Self::run_repair_iteration(
                blockstore.as_ref(),
                &repair_channels,
                &repair_info,
                &mut repair_tracker,
                outstanding_requests,
                repair_socket,
                xdp_sender.as_ref(),
                migration_status.as_ref(),
            );
            repair_tracker.repair_metrics.maybe_report();
            sleep(Duration::from_millis(REPAIR_MS));
        }
    }

    /// Generates deferred repairs for missing shreds, or for the highest shred
    /// when the slot has no known gaps but its last index is still unknown.
    pub(crate) fn generate_repairs_for_slot(
        blockstore: &Blockstore,
        slot: Slot,
        slot_meta: &SlotMetaRepair,
        repair_eligibility: &mut RepairEligibility,
        max_repairs: usize,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        if max_repairs == 0 || slot_meta.is_full() {
            vec![]
        } else if slot_meta.consumed == slot_meta.received {
            // No gaps. Request the highest shred if eligible.
            repair_eligibility.observe_slot(slot, slot_meta);
            let now_ms = timestamp();
            if !repair_eligibility.is_highest_shred_eligible(slot, slot_meta, now_ms) {
                return vec![];
            }
            match RepairService::request_repair_if_needed(
                outstanding_repairs,
                ShredRepairType::HighestShred(slot, slot_meta.received),
            ) {
                Some(repair_request) => vec![repair_request],
                None => vec![],
            }
        } else {
            // There are gaps. Request missing shreds if eligible.
            repair_eligibility.observe_slot(slot, slot_meta);
            let now_ms = timestamp();
            blockstore
                .find_missing_data_indexes(
                    slot,
                    slot_meta.consumed,
                    slot_meta.received,
                    max_repairs,
                )
                .into_iter()
                .filter(|i| repair_eligibility.is_missing_shred_eligible(slot, *i, now_ms))
                .filter_map(|i| {
                    RepairService::request_repair_if_needed(
                        outstanding_repairs,
                        ShredRepairType::Shred(slot, i),
                    )
                })
                .collect()
        }
    }

    /// Repairs any fork starting at the input slot (uses blockstore for fork info)
    pub fn generate_repairs_for_fork(
        blockstore: &Blockstore,
        repairs: &mut Vec<ShredRepairType>,
        max_repairs: usize,
        slot: Slot,
        repair_eligibility: &mut RepairEligibility,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) {
        let mut pending_slots = vec![slot];
        while repairs.len() < max_repairs && !pending_slots.is_empty() {
            let slot = pending_slots.pop().unwrap();
            if let Some(slot_meta) = blockstore.meta_repair(slot).unwrap() {
                let new_repairs = Self::generate_repairs_for_slot(
                    blockstore,
                    slot,
                    &slot_meta,
                    repair_eligibility,
                    max_repairs - repairs.len(),
                    outstanding_repairs,
                );
                repairs.extend(new_repairs);
                let next_slots = slot_meta.next_slots;
                pending_slots.extend(next_slots);
            } else {
                break;
            }
        }
    }

    fn get_repair_peers(
        cluster_info: Arc<ClusterInfo>,
        cluster_slots: Arc<ClusterSlots>,
        slot: u64,
    ) -> Vec<(Pubkey, SocketAddr)> {
        // Find the repair peers that have this slot frozen.
        let Some(peers_with_slot) = cluster_slots.lookup(slot) else {
            warn!("No repair peers have frozen slot: {slot}");
            return vec![];
        };

        // Filter out any peers that don't have a valid repair socket.
        let repair_peers: Vec<(Pubkey, SocketAddr, u32)> = peers_with_slot
            .iter()
            .filter_map(|(pubkey, stake)| {
                let peer_repair_addr = cluster_info
                    .lookup_contact_info(pubkey, |node| node.serve_repair(Protocol::UDP));
                if let Some(Some(peer_repair_addr)) = peer_repair_addr {
                    trace!("Repair peer {pubkey} has a valid repair socket: {peer_repair_addr:?}");
                    Some((
                        *pubkey,
                        peer_repair_addr,
                        (stake / solana_native_token::LAMPORTS_PER_SOL) as u32,
                    ))
                } else {
                    None
                }
            })
            .collect();

        // Sample a subset of the repair peers weighted by stake.
        let mut rng = rand::rng();
        let Ok(weighted_sample_repair_peers) = repair_peers.choose_multiple_weighted(
            &mut rng,
            NUM_PEERS_TO_SAMPLE_FOR_REPAIRS,
            |(_, _, stake)| *stake,
        ) else {
            return vec![];
        };

        // Return the pubkey and repair socket address for the sampled peers.
        weighted_sample_repair_peers
            .collect::<Vec<_>>()
            .iter()
            .map(|(pubkey, addr, _)| (*pubkey, *addr))
            .collect()
    }

    pub fn request_repair_for_shred_from_peer(
        cluster_info: Arc<ClusterInfo>,
        cluster_slots: Arc<ClusterSlots>,
        pubkey: Option<Pubkey>,
        slot: u64,
        shred_index: u64,
        repair_socket: &UdpSocket,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
    ) {
        let mut repair_peers = vec![];

        // Check validity of passed in peer.
        if let Some(pubkey) = pubkey {
            let peer_repair_addr =
                cluster_info.lookup_contact_info(&pubkey, |node| node.serve_repair(Protocol::UDP));
            if let Some(Some(peer_repair_addr)) = peer_repair_addr {
                trace!("Repair peer {pubkey} has valid repair socket: {peer_repair_addr:?}");
                repair_peers.push((pubkey, peer_repair_addr));
            }
        };

        // Select weighted sample of valid peers if no valid peer was passed in.
        if repair_peers.is_empty() {
            debug!(
                "No pubkey was provided or no valid repair socket was found. Sampling a set of \
                 repair peers instead."
            );
            repair_peers = Self::get_repair_peers(cluster_info.clone(), cluster_slots, slot);
        }

        // Send repair request to each peer.
        for (pubkey, peer_repair_addr) in repair_peers {
            Self::request_repair_for_shred_from_address(
                cluster_info.clone(),
                pubkey,
                peer_repair_addr,
                slot,
                shred_index,
                repair_socket,
                outstanding_repair_requests.clone(),
            );
        }
    }

    fn request_repair_for_shred_from_address(
        cluster_info: Arc<ClusterInfo>,
        pubkey: Pubkey,
        address: SocketAddr,
        slot: u64,
        shred_index: u64,
        repair_socket: &UdpSocket,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
    ) {
        // Setup repair request
        let identity_keypair = cluster_info.keypair();
        let repair_request = ShredRepairType::Shred(slot, shred_index);
        let nonce = outstanding_repair_requests
            .write()
            .unwrap()
            .add_request_with_metadata(repair_request, timestamp(), Some(BlockLocation::Original));

        // Create repair request
        let header =
            RepairRequestHeader::new(identity_keypair.pubkey(), pubkey, timestamp(), nonce);
        let request_proto = RepairProtocol::WindowIndex {
            header,
            slot,
            shred_index,
        };
        let packet_buf =
            ServeRepair::repair_proto_to_bytes(&request_proto, &identity_keypair).unwrap();

        // Prepare packet batch to send
        let reqs = [(&packet_buf, address)];

        // Send packet batch
        match batch_send(repair_socket, reqs) {
            Ok(()) => {
                debug!("successfully sent repair request to {pubkey} / {address}!");
            }
            Err(SendPktsError::IoError(err, _num_failed)) => {
                error!("batch_send failed to send packet - error = {err:?}");
            }
        }
    }

    pub fn request_repair_if_needed(
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
        repair_request: ShredRepairType,
    ) -> Option<ShredRepairType> {
        if let Entry::Vacant(entry) = outstanding_repairs.entry(repair_request) {
            entry.insert(timestamp());
            Some(repair_request)
        } else {
            None
        }
    }

    /// Generate repairs for all slots `x` in the repair_range.start <= x <= repair_range.end
    #[cfg(test)]
    pub fn generate_repairs_in_range(
        blockstore: &Blockstore,
        max_repairs: usize,
        repair_range: &RepairSlotRange,
    ) -> Vec<ShredRepairType> {
        // Slot height and shred indexes for shreds we want to repair
        let mut repairs: Vec<ShredRepairType> = vec![];
        let mut repair_eligibility = RepairEligibility::default();
        for slot in repair_range.start..=repair_range.end {
            if repairs.len() >= max_repairs {
                break;
            }

            let meta = blockstore
                .meta_repair(slot)
                .expect("Unable to lookup slot meta")
                .unwrap_or(SlotMetaRepair {
                    slot,
                    ..SlotMetaRepair::default()
                });
            repair_eligibility.observe_slot_as_elapsed_for_tests(slot, &meta);

            let new_repairs = Self::generate_repairs_for_slot(
                blockstore,
                slot,
                &meta,
                &mut repair_eligibility,
                max_repairs - repairs.len(),
                &mut HashMap::default(),
            );
            repairs.extend(new_repairs);
        }

        repairs
    }

    #[cfg(test)]
    fn generate_duplicate_repairs_for_slot(
        blockstore: &Blockstore,
        slot: Slot,
    ) -> Option<Vec<ShredRepairType>> {
        if let Some(slot_meta) = blockstore.meta_repair(slot).unwrap() {
            if slot_meta.is_full() {
                // If the slot is full, no further need to repair this slot
                None
            } else {
                let mut repair_eligibility = RepairEligibility::default();
                repair_eligibility.observe_slot_as_elapsed_for_tests(slot, &slot_meta);
                Some(Self::generate_repairs_for_slot(
                    blockstore,
                    slot,
                    &slot_meta,
                    &mut repair_eligibility,
                    MAX_REPAIR_PER_DUPLICATE,
                    &mut HashMap::default(),
                ))
            }
        } else {
            error!("Slot meta for duplicate slot does not exist, cannot generate repairs");
            // Filter out this slot from the set of duplicates to be repaired as
            // the SlotMeta has to exist for duplicates to be generated
            None
        }
    }

    #[cfg(test)]
    fn generate_and_send_duplicate_repairs(
        duplicate_slot_repair_statuses: &mut HashMap<Slot, DuplicateSlotRepairStatus>,
        cluster_slots: &ClusterSlots,
        blockstore: &Blockstore,
        serve_repair: &ServeRepair,
        repair_stats: &mut RepairStats,
        repair_socket: &UdpSocket,
        repair_validators: &Option<HashSet<Pubkey>>,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        identity_keypair: &Keypair,
    ) {
        duplicate_slot_repair_statuses.retain(|slot, status| {
            Self::update_duplicate_slot_repair_addr(
                *slot,
                status,
                cluster_slots,
                serve_repair,
                repair_validators,
                &identity_keypair.pubkey(),
            );
            if let Some((repair_pubkey, repair_addr)) = status.repair_pubkey_and_addr {
                let repairs = Self::generate_duplicate_repairs_for_slot(blockstore, *slot);

                if let Some(repairs) = repairs {
                    let mut outstanding_requests = outstanding_requests.write().unwrap();
                    for repair_type in repairs {
                        let location = repair_type
                            .block_id()
                            .map_or(BlockLocation::Original, |block_id| {
                                BlockLocation::Alternate { block_id }
                            });
                        let nonce = outstanding_requests.add_request_with_metadata(
                            repair_type,
                            timestamp(),
                            Some(location),
                        );

                        match serve_repair.map_repair_request(
                            &repair_type,
                            &repair_pubkey,
                            repair_stats,
                            nonce,
                            identity_keypair,
                        ) {
                            Ok(req) => {
                                if let Err(e) = repair_socket.send_to(&req, repair_addr) {
                                    info!(
                                        "repair req send_to {repair_pubkey} ({repair_addr}) error \
                                         {e:?}"
                                    );
                                }
                            }
                            Err(e) => info!("map_repair_request err={e}"),
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                true
            }
        })
    }

    #[cfg(test)]
    fn update_duplicate_slot_repair_addr(
        slot: Slot,
        status: &mut DuplicateSlotRepairStatus,
        cluster_slots: &ClusterSlots,
        serve_repair: &ServeRepair,
        repair_validators: &Option<HashSet<Pubkey>>,
        my_pubkey: &Pubkey,
    ) {
        let now = timestamp();
        if status.repair_pubkey_and_addr.is_none()
            || now.saturating_sub(status.start_ts) >= MAX_DUPLICATE_WAIT_MS as u64
        {
            status.repair_pubkey_and_addr = serve_repair
                .repair_request_duplicate_compute_best_peer(
                    slot,
                    cluster_slots,
                    repair_validators,
                    my_pubkey,
                );
            status.start_ts = timestamp();
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn initiate_repair_for_duplicate_slot(
        slot: Slot,
        duplicate_slot_repair_statuses: &mut HashMap<Slot, DuplicateSlotRepairStatus>,
        cluster_slots: &ClusterSlots,
        serve_repair: &ServeRepair,
        repair_validators: &Option<HashSet<Pubkey>>,
        my_pubkey: &Pubkey,
    ) {
        // If we're already in the middle of repairing this, ignore the signal.
        if duplicate_slot_repair_statuses.contains_key(&slot) {
            return;
        }
        // Mark this slot as special repair, try to download from single
        // validator to avoid corruption
        let repair_pubkey_and_addr = serve_repair.repair_request_duplicate_compute_best_peer(
            slot,
            cluster_slots,
            repair_validators,
            my_pubkey,
        );
        let new_duplicate_slot_repair_status = DuplicateSlotRepairStatus {
            correct_ancestor_to_repair: (slot, Hash::default()),
            repair_pubkey_and_addr,
            start_ts: timestamp(),
        };
        duplicate_slot_repair_statuses.insert(slot, new_duplicate_slot_repair_status);
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_repair.join()?;
        if let Some(ancestor_hashes_service) = self.ancestor_hashes_service {
            ancestor_hashes_service.join()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn sleep_shred_deferment_period() {
    // sleep to bypass shred deferment window
    sleep(FEC_REPAIR_DELAY);
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::repair::serve_repair,
        solana_gossip::{contact_info::ContactInfo, node::Node},
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{
                Blockstore, make_chaining_slot_entries, make_many_slot_entries, make_slot_entries,
            },
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
            get_tmp_ledger_path_auto_delete,
            shred::max_ticks_per_n_shreds,
        },
        solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
        solana_perf::packet::PacketRef,
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        std::{collections::HashSet, sync::Arc},
    };

    fn new_test_cluster_info() -> ClusterInfo {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified)
    }

    #[test]
    pub fn test_request_repair_for_shred_from_address() {
        // Setup cluster and repair info
        let cluster_info = Arc::new(new_test_cluster_info());
        let pubkey = cluster_info.id();
        let slot = 100;
        let shred_index = 50;
        let reader = bind_to_localhost_unique().expect("should bind");
        let address = reader.local_addr().unwrap();
        let sender = bind_to_localhost_unique().expect("should bind");
        let outstanding_repair_requests = Arc::new(RwLock::new(OutstandingShredRepairs::default()));

        // Send a repair request
        RepairService::request_repair_for_shred_from_address(
            cluster_info,
            pubkey,
            address,
            slot,
            shred_index,
            &sender,
            outstanding_repair_requests,
        );

        // Receive and translate repair packet
        let mut packets = vec![solana_packet::Packet::default(); 1];
        let _recv_count = solana_streamer::recvmmsg::recv_mmsg(&reader, &mut packets[..]).unwrap();
        let packet = &packets[0];

        let remote_request = PacketRef::from(packet).to_bytes_packet();
        // Deserialize and check the request
        let deserialized =
            serve_repair::deserialize_request::<RepairProtocol>(&remote_request).unwrap();
        match deserialized {
            RepairProtocol::WindowIndex {
                slot: deserialized_slot,
                shred_index: deserialized_shred_index,
                ..
            } => {
                assert_eq!(deserialized_slot, slot);
                assert_eq!(deserialized_shred_index, shred_index);
            }
            _ => panic!("unexpected repair protocol"),
        }
    }

    #[test]
    pub fn test_repair_orphan() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Create some orphan slots
        let (mut shreds, _) = make_slot_entries(1, 0, 1);
        let (shreds2, _) = make_slot_entries(5, 2, 1);
        shreds.extend(shreds2);
        blockstore.insert_shreds(shreds, false).unwrap();
        let mut repair_weight = RepairWeight::new(0);
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut RepairEligibility::default(),
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            vec![
                ShredRepairType::Orphan(2),
                ShredRepairType::HighestShred(0, 0)
            ]
        );
    }

    #[test]
    pub fn test_repair_empty_slot() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let (shreds, _) = make_slot_entries(2, 0, 1);

        // Write this shred to slot 2, should chain to slot 0, which we haven't received
        // any shreds for
        blockstore.insert_shreds(shreds, false).unwrap();
        let mut repair_weight = RepairWeight::new(0);

        // Check that repair tries to patch the empty slot
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut RepairEligibility::default(),
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            vec![ShredRepairType::HighestShred(0, 0)]
        );
    }

    #[test]
    pub fn test_generate_repairs() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let nth = 3;
        let num_slots = 2;

        // Create some shreds
        let (mut shreds, _) = make_many_slot_entries(0, num_slots, 150);
        let num_shreds = shreds.len() as u64;
        let num_shreds_per_slot = num_shreds / num_slots;

        // write every nth shred
        let mut shreds_to_write = vec![];
        let mut missing_indexes_per_slot = vec![];
        for i in (0..num_shreds).rev() {
            let index = i % num_shreds_per_slot;
            // get_best_repair_shreds only returns missing shreds in
            // between shreds received; So this should either insert the
            // last shred in each slot, or exclude missing shreds after the
            // last inserted shred from expected repairs.
            if index.is_multiple_of(nth) || index + 1 == num_shreds_per_slot {
                shreds_to_write.insert(0, shreds.remove(i as usize));
            } else if i < num_shreds_per_slot {
                missing_indexes_per_slot.insert(0, index);
            }
        }
        blockstore.insert_shreds(shreds_to_write, false).unwrap();
        let expected: Vec<ShredRepairType> = (0..num_slots)
            .flat_map(|slot| {
                missing_indexes_per_slot
                    .iter()
                    .map(move |shred_index| ShredRepairType::Shred(slot, *shred_index))
            })
            .collect();

        let mut repair_weight = RepairWeight::new(0);
        let mut repair_eligibility = RepairEligibility::default();
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut repair_eligibility,
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            vec![]
        );

        sleep_shred_deferment_period();
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut repair_eligibility,
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            expected
        );

        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                expected.len() - 2,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut repair_eligibility,
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            )[..],
            expected[0..expected.len() - 2]
        );
    }

    #[test]
    pub fn test_repair_empty_middle_fec() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let make_shreds_by_index = |slot, parent_slot, min_index| {
            let mut num_entries = 1_000;
            loop {
                let (shreds, _) = make_slot_entries(slot, parent_slot, num_entries);
                let shreds_by_index: HashMap<_, _> = shreds
                    .into_iter()
                    .map(|shred| (u64::from(shred.index()), shred))
                    .collect();
                if shreds_by_index.contains_key(&min_index) {
                    break shreds_by_index;
                }
                num_entries *= 2;
            }
        };

        let slot = 0;
        let missing_index = 1;
        let shreds_by_index = make_shreds_by_index(slot, 0, missing_index + 1);
        blockstore
            .insert_shreds(
                vec![
                    shreds_by_index.get(&0).unwrap().clone(),
                    shreds_by_index.get(&(missing_index + 1)).unwrap().clone(),
                ],
                false,
            )
            .unwrap();
        let slot_meta = blockstore.meta_repair(slot).unwrap().unwrap();
        let mut repair_eligibility = RepairEligibility::default();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                slot,
                &slot_meta,
                &mut repair_eligibility,
                usize::MAX,
                &mut HashMap::default(),
            ),
            vec![]
        );

        sleep_shred_deferment_period();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                slot,
                &slot_meta,
                &mut repair_eligibility,
                usize::MAX,
                &mut HashMap::default(),
            ),
            vec![ShredRepairType::Shred(slot, missing_index)]
        );

        let slot = 1;
        let missing_fec_start = shred::DATA_SHREDS_PER_FEC_BLOCK as u64;
        let future_fec_start = 2 * missing_fec_start;
        let shreds_by_index = make_shreds_by_index(slot, 0, future_fec_start);
        let shreds_to_insert: Vec<_> = (0..missing_fec_start)
            .chain(std::iter::once(future_fec_start))
            .map(|index| shreds_by_index.get(&index).unwrap().clone())
            .collect();
        blockstore.insert_shreds(shreds_to_insert, false).unwrap();

        let slot_meta = blockstore.meta_repair(slot).unwrap().unwrap();
        let mut repair_eligibility = RepairEligibility::default();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                slot,
                &slot_meta,
                &mut repair_eligibility,
                1,
                &mut HashMap::default(),
            ),
            vec![]
        );

        sleep_shred_deferment_period();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                slot,
                &slot_meta,
                &mut repair_eligibility,
                1,
                &mut HashMap::default(),
            ),
            vec![ShredRepairType::Shred(slot, missing_fec_start)]
        );
    }

    #[test]
    pub fn test_generate_highest_repair() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let num_entries_per_slot = 100;

        // Create some shreds
        let (mut shreds, _) = make_slot_entries(
            0, // slot
            0, // parent_slot
            num_entries_per_slot as u64,
        );
        let num_shreds_per_slot = shreds.len() as u64;

        // Remove last shred (which is also last in slot) so that slot is not complete
        shreds.pop();

        blockstore.insert_shreds(shreds, false).unwrap();

        // We didn't get the last shred for this slot, so ask for the highest shred for that slot
        let expected: Vec<ShredRepairType> =
            vec![ShredRepairType::HighestShred(0, num_shreds_per_slot - 1)];

        let mut repair_weight = RepairWeight::new(0);
        let mut repair_eligibility = RepairEligibility::default();
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut repair_eligibility,
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            vec![]
        );

        sleep_shred_deferment_period();
        assert_eq!(
            repair_weight.get_best_weighted_repairs(
                &blockstore,
                &HashMap::new(),
                &EpochSchedule::default(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                &mut repair_eligibility,
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            expected
        );
    }

    #[test]
    pub fn test_highest_repair_waits() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (shreds, _) = make_slot_entries(0, 0, 100);
        let shreds_by_index: HashMap<_, _> = shreds
            .into_iter()
            .map(|shred| (u64::from(shred.index()), shred))
            .collect();

        blockstore
            .insert_shreds(vec![shreds_by_index.get(&0).unwrap().clone()], false)
            .unwrap();
        let mut repair_eligibility = RepairEligibility::default();
        let slot_meta = blockstore.meta_repair(0).unwrap().unwrap();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                0,
                &slot_meta,
                &mut repair_eligibility,
                usize::MAX,
                &mut HashMap::default(),
            ),
            vec![]
        );

        blockstore
            .insert_shreds(vec![shreds_by_index.get(&1).unwrap().clone()], false)
            .unwrap();
        let slot_meta = blockstore.meta_repair(0).unwrap().unwrap();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                0,
                &slot_meta,
                &mut repair_eligibility,
                usize::MAX,
                &mut HashMap::default(),
            ),
            vec![]
        );

        sleep_shred_deferment_period();
        assert_eq!(
            RepairService::generate_repairs_for_slot(
                &blockstore,
                0,
                &slot_meta,
                &mut repair_eligibility,
                usize::MAX,
                &mut HashMap::default(),
            ),
            vec![ShredRepairType::HighestShred(0, 2)]
        );
    }

    #[test]
    pub fn test_repair_range() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let slots: Vec<u64> = vec![1, 3, 5, 7, 8];
        let num_entries_per_slot = max_ticks_per_n_shreds(1, None) + 1;

        let shreds = make_chaining_slot_entries(&slots, num_entries_per_slot, 0);
        for (mut slot_shreds, _) in shreds.into_iter() {
            slot_shreds.remove(0);
            blockstore.insert_shreds(slot_shreds, false).unwrap();
        }

        // Iterate through all possible combinations of start..end (inclusive on both
        // sides of the range)
        for start in 0..slots.len() {
            for end in start..slots.len() {
                let repair_slot_range = RepairSlotRange {
                    start: slots[start],
                    end: slots[end],
                };
                let expected: Vec<ShredRepairType> = (repair_slot_range.start
                    ..=repair_slot_range.end)
                    .map(|slot_index| {
                        if slots.contains(&slot_index) {
                            ShredRepairType::Shred(slot_index, 0)
                        } else {
                            ShredRepairType::HighestShred(slot_index, 0)
                        }
                    })
                    .collect();

                sleep_shred_deferment_period();
                assert_eq!(
                    RepairService::generate_repairs_in_range(
                        &blockstore,
                        usize::MAX,
                        &repair_slot_range,
                    ),
                    expected
                );
            }
        }
    }

    #[test]
    pub fn test_repair_range_highest() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let num_entries_per_slot = 10;

        let num_slots = 1;
        let start = 5;

        // Create some shreds in slots 0..num_slots
        for i in start..start + num_slots {
            let parent = if i > 0 { i - 1 } else { 0 };
            let (shreds, _) = make_slot_entries(
                i, // slot
                parent,
                num_entries_per_slot as u64,
            );

            blockstore.insert_shreds(shreds, false).unwrap();
        }

        let end = 4;
        let expected: Vec<ShredRepairType> = vec![
            ShredRepairType::HighestShred(end - 2, 0),
            ShredRepairType::HighestShred(end - 1, 0),
            ShredRepairType::HighestShred(end, 0),
        ];

        let repair_slot_range = RepairSlotRange { start: 2, end };

        assert_eq!(
            RepairService::generate_repairs_in_range(&blockstore, usize::MAX, &repair_slot_range,),
            expected
        );
    }

    #[test]
    pub fn test_generate_duplicate_repairs_for_slot() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let dead_slot = 9;

        // SlotMeta doesn't exist, should make no repairs
        assert!(
            RepairService::generate_duplicate_repairs_for_slot(&blockstore, dead_slot,).is_none()
        );

        // Insert some shreds to create a SlotMeta, should make repairs
        let num_entries_per_slot = max_ticks_per_n_shreds(1, None) + 1;
        let (mut shreds, _) = make_slot_entries(
            dead_slot,     // slot
            dead_slot - 1, // parent_slot
            num_entries_per_slot,
        );
        blockstore
            .insert_shreds(shreds[..shreds.len() - 1].to_vec(), false)
            .unwrap();
        assert!(
            RepairService::generate_duplicate_repairs_for_slot(&blockstore, dead_slot,).is_some()
        );

        // SlotMeta is full, should make no repairs
        blockstore
            .insert_shreds(vec![shreds.pop().unwrap()], false)
            .unwrap();
        assert!(
            RepairService::generate_duplicate_repairs_for_slot(&blockstore, dead_slot,).is_none()
        );
    }

    #[test]
    pub fn test_generate_and_send_duplicate_repairs() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let cluster_slots = ClusterSlots::default_for_tests();
        let cluster_info = Arc::new(new_test_cluster_info());
        let identity_keypair = cluster_info.keypair();
        let serve_repair = {
            let bank_forks_r = bank_forks.read().unwrap();
            ServeRepair::new(
                cluster_info,
                bank_forks_r.sharable_banks(),
                Arc::new(RwLock::new(HashSet::default())),
                Box::new(StandardRepairHandler::new(blockstore.clone())),
                bank_forks_r.migration_status(),
            )
        };
        let mut duplicate_slot_repair_statuses = HashMap::new();
        let dead_slot = 9;
        let receive_socket = &bind_to_localhost_unique().expect("should bind - receive socket");
        let duplicate_status = DuplicateSlotRepairStatus {
            correct_ancestor_to_repair: (dead_slot, Hash::default()),
            start_ts: u64::MAX,
            repair_pubkey_and_addr: None,
        };

        // Insert some shreds to create a SlotMeta,
        let num_entries_per_slot = max_ticks_per_n_shreds(1, None) + 1;
        let (mut shreds, _) = make_slot_entries(dead_slot, dead_slot - 1, num_entries_per_slot);
        blockstore
            .insert_shreds(shreds[..shreds.len() - 1].to_vec(), false)
            .unwrap();

        duplicate_slot_repair_statuses.insert(dead_slot, duplicate_status);

        // There is no repair_addr, so should not get filtered because the timeout
        // `u64::MAX` has not expired
        RepairService::generate_and_send_duplicate_repairs(
            &mut duplicate_slot_repair_statuses,
            &cluster_slots,
            &blockstore,
            &serve_repair,
            &mut RepairStats::default(),
            &bind_to_localhost_unique().expect("should bind - repair socket"),
            &None,
            &RwLock::new(OutstandingRequests::default()),
            &identity_keypair,
        );
        assert!(
            duplicate_slot_repair_statuses
                .get(&dead_slot)
                .unwrap()
                .repair_pubkey_and_addr
                .is_none()
        );
        assert!(duplicate_slot_repair_statuses.contains_key(&dead_slot));

        // Give the slot a repair address
        duplicate_slot_repair_statuses
            .get_mut(&dead_slot)
            .unwrap()
            .repair_pubkey_and_addr =
            Some((Pubkey::default(), receive_socket.local_addr().unwrap()));

        // Slot is not yet full, should not get filtered from `duplicate_slot_repair_statuses`
        RepairService::generate_and_send_duplicate_repairs(
            &mut duplicate_slot_repair_statuses,
            &cluster_slots,
            &blockstore,
            &serve_repair,
            &mut RepairStats::default(),
            &bind_to_localhost_unique().expect("should bind - repair socket"),
            &None,
            &RwLock::new(OutstandingRequests::default()),
            &identity_keypair,
        );
        assert_eq!(duplicate_slot_repair_statuses.len(), 1);
        assert!(duplicate_slot_repair_statuses.contains_key(&dead_slot));

        // Insert rest of shreds. Slot is full, should get filtered from
        // `duplicate_slot_repair_statuses`
        blockstore
            .insert_shreds(vec![shreds.pop().unwrap()], false)
            .unwrap();
        RepairService::generate_and_send_duplicate_repairs(
            &mut duplicate_slot_repair_statuses,
            &cluster_slots,
            &blockstore,
            &serve_repair,
            &mut RepairStats::default(),
            &bind_to_localhost_unique().expect("should bind - repair socket"),
            &None,
            &RwLock::new(OutstandingRequests::default()),
            &identity_keypair,
        );
        assert!(duplicate_slot_repair_statuses.is_empty());
    }

    #[test]
    pub fn test_update_duplicate_slot_repair_addr() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let dummy_addr = Some((
            Pubkey::default(),
            bind_to_localhost_unique()
                .expect("should bind - dummy socket")
                .local_addr()
                .unwrap(),
        ));
        let cluster_info = Arc::new(new_test_cluster_info());
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let serve_repair = {
            let bank_forks_r = bank_forks.read().unwrap();
            ServeRepair::new(
                cluster_info.clone(),
                bank_forks_r.sharable_banks(),
                Arc::new(RwLock::new(HashSet::default())),
                Box::new(StandardRepairHandler::new(blockstore)),
                bank_forks_r.migration_status(),
            )
        };
        let valid_repair_peer = Node::new_localhost().info;
        let my_pubkey = cluster_info.id();

        // Signal that this peer has confirmed the dead slot, and is thus
        // a valid target for repair
        let dead_slot = 9;
        let cluster_slots = ClusterSlots::default_for_tests();
        cluster_slots.fake_epoch_info_for_tests(HashMap::from([(*valid_repair_peer.pubkey(), 42)]));
        cluster_slots.insert_node_id(dead_slot, *valid_repair_peer.pubkey());
        cluster_info.insert_info(valid_repair_peer);

        // Not enough time has passed, should not update the
        // address
        let mut duplicate_status = DuplicateSlotRepairStatus {
            correct_ancestor_to_repair: (dead_slot, Hash::default()),
            start_ts: u64::MAX,
            repair_pubkey_and_addr: dummy_addr,
        };
        RepairService::update_duplicate_slot_repair_addr(
            dead_slot,
            &mut duplicate_status,
            &cluster_slots,
            &serve_repair,
            &None,
            &my_pubkey,
        );
        assert_eq!(duplicate_status.repair_pubkey_and_addr, dummy_addr);

        // If the repair address is None, should try to update
        let mut duplicate_status = DuplicateSlotRepairStatus {
            correct_ancestor_to_repair: (dead_slot, Hash::default()),
            start_ts: u64::MAX,
            repair_pubkey_and_addr: None,
        };
        RepairService::update_duplicate_slot_repair_addr(
            dead_slot,
            &mut duplicate_status,
            &cluster_slots,
            &serve_repair,
            &None,
            &my_pubkey,
        );
        assert!(duplicate_status.repair_pubkey_and_addr.is_some());

        // If sufficient time has passed, should try to update
        let mut duplicate_status = DuplicateSlotRepairStatus {
            correct_ancestor_to_repair: (dead_slot, Hash::default()),
            start_ts: timestamp() - MAX_DUPLICATE_WAIT_MS as u64,
            repair_pubkey_and_addr: dummy_addr,
        };
        RepairService::update_duplicate_slot_repair_addr(
            dead_slot,
            &mut duplicate_status,
            &cluster_slots,
            &serve_repair,
            &None,
            &my_pubkey,
        );
        assert_ne!(duplicate_status.repair_pubkey_and_addr, dummy_addr);
    }
}
