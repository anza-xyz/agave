//! The `repair_service` module implements the tools necessary to generate a thread which
//! regularly finds missing shreds in the ledger and sends repair requests for those shreds
use {
    super::standard_repair_handler::StandardRepairHandler,
    crate::{
        cluster_info_vote_listener::VerifiedVoterSlotsReceiver,
        cluster_slots_service::cluster_slots::ClusterSlots,
        repair::{
            ancestor_hashes_service::{
                AncestorHashesChannels, AncestorHashesReplayUpdateReceiver, AncestorHashesService,
            },
            duplicate_repair_status::AncestorDuplicateSlotToRepair,
            outstanding_requests::OutstandingRequests,
            repair_weight::RepairWeight,
            serve_repair::{
                self, RepairPeers, RepairProtocol, RepairRequestHeader, ServeRepair,
                ShredRepairType, REPAIR_PEERS_CACHE_CAPACITY,
            },
        },
    },
    bytes::Bytes,
    crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender},
    lru::LruCache,
    rand::seq::SliceRandom,
    solana_client::connection_cache::Protocol,
    solana_clock::{Slot, DEFAULT_TICKS_PER_SECOND, MS_PER_TICK},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_keypair::Signer,
    solana_ledger::{
        blockstore::{Blockstore, SlotMeta},
        shred,
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
    },
    solana_streamer::sendmmsg::{batch_send, SendPktsError},
    solana_time_utils::timestamp,
    std::{
        collections::{hash_map::Entry, HashMap, HashSet},
        iter::Iterator,
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::sync::mpsc::Sender as AsyncSender,
};
#[cfg(test)]
use {
    crate::repair::duplicate_repair_status::DuplicateSlotRepairStatus,
    solana_clock::DEFAULT_MS_PER_SLOT, solana_keypair::Keypair,
};

// Time to defer repair requests to allow for turbine propagation
const DEFER_REPAIR_THRESHOLD: Duration = Duration::from_millis(250);
const DEFER_REPAIR_THRESHOLD_TICKS: u64 = DEFER_REPAIR_THRESHOLD.as_millis() as u64 / MS_PER_TICK;

// This is the amount of time we will wait for a repair request to be fulfilled
// before making another request. Value is based on reasonable upper bound of
// expected network delays in requesting repairs and receiving shreds.
const REPAIR_REQUEST_TIMEOUT_MS: u64 = 150;

// When requesting repair for a specific shred through the admin RPC, we will
// request up to NUM_PEERS_TO_SAMPLE_FOR_REPAIRS in the event a specific, valid
// target node is not provided. This number was chosen to provide reasonable
// chance of sampling duplicate in the event of cluster partition.
const NUM_PEERS_TO_SAMPLE_FOR_REPAIRS: usize = 10;

pub type AncestorDuplicateSlotsSender = CrossbeamSender<AncestorDuplicateSlotToRepair>;
pub type AncestorDuplicateSlotsReceiver = CrossbeamReceiver<AncestorDuplicateSlotToRepair>;
pub type ConfirmedSlotsSender = CrossbeamSender<Vec<Slot>>;
pub type ConfirmedSlotsReceiver = CrossbeamReceiver<Vec<Slot>>;
pub type DumpedSlotsSender = CrossbeamSender<Vec<(Slot, Hash)>>;
pub type DumpedSlotsReceiver = CrossbeamReceiver<Vec<(Slot, Hash)>>;
pub type OutstandingShredRepairs = OutstandingRequests<ShredRepairType>;
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
    pub fn maybe_report(&mut self) {
        if self.last_report.elapsed().as_secs() > 2 {
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
                ("repair-highest-slot", self.highest_shred.max, i64), // deprecated
                ("highest-shred-slot-max", nonzero_num(self.highest_shred.max), Option<i64>),
                ("highest-shred-slot-min", nonzero_num(self.highest_shred.min), Option<i64>),
                ("repair-orphan", self.orphan.max, i64), // deprecated
                ("orphan-slot-max", nonzero_num(self.orphan.max), Option<i64>),
                ("orphan-slot-min", nonzero_num(self.orphan.min), Option<i64>),
            );
        }
    }
}

#[derive(Default, Debug)]
pub struct RepairTiming {
    pub set_root_elapsed: u64,
    pub dump_slots_elapsed: u64,
    pub get_votes_elapsed: u64,
    pub add_voters_elapsed: u64,
    pub purge_outstanding_repairs: u64,
    pub handle_popular_pruned_forks: u64,
    pub get_best_orphans_elapsed: u64,
    pub get_best_shreds_elapsed: u64,
    pub get_unknown_last_index_elapsed: u64,
    pub get_closest_completion_elapsed: u64,
    pub send_repairs_elapsed: u64,
    pub build_repairs_batch_elapsed: u64,
    pub batch_send_repairs_elapsed: u64,
}

impl RepairTiming {
    fn report(&self) {
        datapoint_info!(
            "repair_service-repair_timing",
            ("set-root-elapsed", self.set_root_elapsed, i64),
            ("dump-slots-elapsed", self.dump_slots_elapsed, i64),
            ("get-votes-elapsed", self.get_votes_elapsed, i64),
            ("add-voters-elapsed", self.add_voters_elapsed, i64),
            (
                "purge-outstanding-repairs",
                self.purge_outstanding_repairs,
                i64
            ),
            (
                "handle-popular-pruned-forks",
                self.handle_popular_pruned_forks,
                i64
            ),
            (
                "get-best-orphans-elapsed",
                self.get_best_orphans_elapsed,
                i64
            ),
            ("get-best-shreds-elapsed", self.get_best_shreds_elapsed, i64),
            (
                "get-unknown-last-index-elapsed",
                self.get_unknown_last_index_elapsed,
                i64
            ),
            (
                "get-closest-completion-elapsed",
                self.get_closest_completion_elapsed,
                i64
            ),
            ("send-repairs-elapsed", self.send_repairs_elapsed, i64),
            (
                "build-repairs-batch-elapsed",
                self.build_repairs_batch_elapsed,
                i64
            ),
            (
                "batch-send-repairs-elapsed",
                self.batch_send_repairs_elapsed,
                i64
            ),
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
    // A given list of slots to repair when in wen_restart
    pub wen_restart_repair_slots: Option<Arc<RwLock<Vec<Slot>>>>,
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
    repair_request_quic_sender: AsyncSender<(SocketAddr, Bytes)>,
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
        repair_request_quic_sender: AsyncSender<(SocketAddr, Bytes)>,
        verified_voter_slots_receiver: VerifiedVoterSlotsReceiver,
        dumped_slots_receiver: DumpedSlotsReceiver,
        popular_pruned_forks_sender: PopularPrunedForksSender,
        ancestor_hashes_request_quic_sender: AsyncSender<(SocketAddr, Bytes)>,
        ancestor_hashes_response_quic_receiver: CrossbeamReceiver<(Pubkey, SocketAddr, Bytes)>,
        ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
    ) -> Self {
        Self {
            repair_channels: RepairChannels {
                repair_request_quic_sender,
                verified_voter_slots_receiver,
                dumped_slots_receiver,
                popular_pruned_forks_sender,
            },
            ancestors_hashes_channels: AncestorHashesChannels {
                ancestor_hashes_request_quic_sender,
                ancestor_hashes_response_quic_receiver,
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
}

pub struct RepairService {
    t_repair: JoinHandle<()>,
    ancestor_hashes_service: AncestorHashesService,
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
        repair_metrics: &mut RepairMetrics,
    ) {
        // Purge outdated slots from the weighting heuristic
        let mut set_root_elapsed = Measure::start("set_root_elapsed");
        repair_weight.set_root(root_bank.slot());
        set_root_elapsed.stop();

        // Remove dumped slots from the weighting heuristic
        let mut dump_slots_elapsed = Measure::start("dump_slots_elapsed");
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
        dump_slots_elapsed.stop();

        // Add new votes to the weighting heuristic
        let mut get_votes_elapsed = Measure::start("get_votes_elapsed");
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
        get_votes_elapsed.stop();

        let mut add_voters_elapsed = Measure::start("add_voters");
        repair_weight.add_voters(
            blockstore,
            slot_to_vote_pubkeys.into_iter(),
            root_bank.epoch_stakes_map(),
            root_bank.epoch_schedule(),
        );
        add_voters_elapsed.stop();

        repair_metrics.timing.set_root_elapsed += set_root_elapsed.as_us();
        repair_metrics.timing.dump_slots_elapsed += dump_slots_elapsed.as_us();
        repair_metrics.timing.get_votes_elapsed += get_votes_elapsed.as_us();
        repair_metrics.timing.add_voters_elapsed += add_voters_elapsed.as_us();
    }

    fn identify_repairs(
        blockstore: &Blockstore,
        root_bank: Arc<Bank>,
        repair_info: &RepairInfo,
        repair_weight: &mut RepairWeight,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
        repair_metrics: &mut RepairMetrics,
    ) -> Vec<ShredRepairType> {
        let mut purge_outstanding_repairs = Measure::start("purge_outstanding_repairs");
        // Purge old entries. They've either completed or need to be retried.
        outstanding_repairs.retain(|_repair_request, time| {
            timestamp().saturating_sub(*time) < REPAIR_REQUEST_TIMEOUT_MS
        });
        purge_outstanding_repairs.stop();
        repair_metrics.timing.purge_outstanding_repairs = purge_outstanding_repairs.as_us();

        match repair_info.wen_restart_repair_slots.clone() {
            Some(slots_to_repair) => Self::generate_repairs_for_wen_restart(
                blockstore,
                MAX_REPAIR_LENGTH,
                &slots_to_repair.read().unwrap(),
                outstanding_repairs,
            ),
            None => repair_weight.get_best_weighted_repairs(
                blockstore,
                root_bank.epoch_stakes_map(),
                root_bank.epoch_schedule(),
                MAX_ORPHANS,
                MAX_REPAIR_LENGTH,
                MAX_UNKNOWN_LAST_INDEX_REPAIRS,
                MAX_CLOSEST_COMPLETION_REPAIRS,
                repair_metrics,
                outstanding_repairs,
            ),
        }
    }

    fn handle_popular_pruned_forks(
        root_bank: Arc<Bank>,
        repair_weight: &mut RepairWeight,
        popular_pruned_forks_requests: &mut HashSet<Slot>,
        popular_pruned_forks_sender: &PopularPrunedForksSender,
        repair_metrics: &mut RepairMetrics,
    ) {
        let mut handle_popular_pruned_forks = Measure::start("handle_popular_pruned_forks");
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
        handle_popular_pruned_forks.stop();
        repair_metrics.timing.handle_popular_pruned_forks = handle_popular_pruned_forks.as_us();
    }

    fn build_and_send_repair_batch(
        serve_repair: &mut ServeRepair,
        peers_cache: &mut LruCache<u64, RepairPeers>,
        repair_request_quic_sender: &AsyncSender<(SocketAddr, Bytes)>,
        repairs: Vec<ShredRepairType>,
        repair_info: &RepairInfo,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        repair_socket: &UdpSocket,
        repair_protocol: Protocol,
        repair_metrics: &mut RepairMetrics,
    ) {
        let mut build_repairs_batch_elapsed = Measure::start("build_repairs_batch_elapsed");
        let identity_keypair = repair_info.cluster_info.keypair();
        let batch: Vec<(Vec<u8>, SocketAddr)> = {
            let mut outstanding_requests = outstanding_requests.write().unwrap();
            repairs
                .into_iter()
                .filter_map(|repair_request| {
                    let (to, req) = serve_repair
                        .repair_request(
                            &repair_info.cluster_slots,
                            repair_request,
                            peers_cache,
                            &mut repair_metrics.stats,
                            &repair_info.repair_validators,
                            &mut outstanding_requests,
                            &identity_keypair,
                            repair_request_quic_sender,
                            repair_protocol,
                        )
                        .ok()??;
                    Some((req, to))
                })
                .collect()
        };
        build_repairs_batch_elapsed.stop();

        let mut batch_send_repairs_elapsed = Measure::start("batch_send_repairs_elapsed");
        if !batch.is_empty() {
            let num_pkts = batch.len();
            let batch = batch.iter().map(|(bytes, addr)| (bytes, addr));
            match batch_send(repair_socket, batch) {
                Ok(()) => (),
                Err(SendPktsError::IoError(err, num_failed)) => {
                    error!(
                        "{} batch_send failed to send {num_failed}/{num_pkts} packets first error \
                         {err:?}",
                        identity_keypair.pubkey()
                    );
                }
            }
        }
        batch_send_repairs_elapsed.stop();

        repair_metrics.timing.build_repairs_batch_elapsed = build_repairs_batch_elapsed.as_us();
        repair_metrics.timing.batch_send_repairs_elapsed = batch_send_repairs_elapsed.as_us();
    }

    fn run_repair_iteration(
        blockstore: &Blockstore,
        repair_channels: &RepairChannels,
        repair_info: &RepairInfo,
        repair_tracker: &mut RepairTracker,
        outstanding_requests: &RwLock<OutstandingShredRepairs>,
        repair_socket: &UdpSocket,
    ) {
        let RepairChannels {
            repair_request_quic_sender,
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
        } = repair_tracker;
        let root_bank = sharable_banks.root();

        Self::update_weighting_heuristic(
            blockstore,
            root_bank.clone(),
            repair_weight,
            popular_pruned_forks_requests,
            dumped_slots_receiver,
            verified_voter_slots_receiver,
            repair_metrics,
        );

        let repairs = Self::identify_repairs(
            blockstore,
            root_bank.clone(),
            repair_info,
            repair_weight,
            outstanding_repairs,
            repair_metrics,
        );

        Self::handle_popular_pruned_forks(
            root_bank.clone(),
            repair_weight,
            popular_pruned_forks_requests,
            popular_pruned_forks_sender,
            repair_metrics,
        );

        Self::build_and_send_repair_batch(
            serve_repair,
            peers_cache,
            repair_request_quic_sender,
            repairs,
            repair_info,
            outstanding_requests,
            repair_socket,
            serve_repair::get_repair_protocol(root_bank.cluster_type()),
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
    ) {
        let sharable_banks = repair_info.bank_forks.read().unwrap().sharable_banks();
        let root_bank_slot = sharable_banks.root().slot();
        let mut repair_tracker = RepairTracker {
            sharable_banks,
            repair_weight: RepairWeight::new(root_bank_slot),
            serve_repair: {
                ServeRepair::new(
                    repair_info.cluster_info.clone(),
                    repair_info.bank_forks.read().unwrap().sharable_banks(),
                    repair_info.repair_whitelist.clone(),
                    Box::new(StandardRepairHandler::new(blockstore.clone())),
                )
            },
            repair_metrics: RepairMetrics::default(),
            peers_cache: LruCache::new(REPAIR_PEERS_CACHE_CAPACITY),
            popular_pruned_forks_requests: HashSet::new(),
            outstanding_repairs: HashMap::new(),
        };

        while !exit.load(Ordering::Relaxed) {
            Self::run_repair_iteration(
                blockstore.as_ref(),
                &repair_channels,
                &repair_info,
                &mut repair_tracker,
                outstanding_requests,
                repair_socket,
            );
            repair_tracker.repair_metrics.maybe_report();
            sleep(Duration::from_millis(REPAIR_MS));
        }
    }

    pub fn generate_repairs_for_slot_throttled_by_tick(
        blockstore: &Blockstore,
        slot: Slot,
        slot_meta: &SlotMeta,
        max_repairs: usize,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        Self::generate_repairs_for_slot(
            blockstore,
            slot,
            slot_meta,
            max_repairs,
            true,
            outstanding_repairs,
        )
    }

    pub fn generate_repairs_for_slot_not_throttled_by_tick(
        blockstore: &Blockstore,
        slot: Slot,
        slot_meta: &SlotMeta,
        max_repairs: usize,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        Self::generate_repairs_for_slot(
            blockstore,
            slot,
            slot_meta,
            max_repairs,
            false,
            outstanding_repairs,
        )
    }

    /// If this slot is missing shreds generate repairs
    fn generate_repairs_for_slot(
        blockstore: &Blockstore,
        slot: Slot,
        slot_meta: &SlotMeta,
        max_repairs: usize,
        throttle_requests_by_shred_tick: bool,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        let defer_repair_threshold_ticks = if throttle_requests_by_shred_tick {
            DEFER_REPAIR_THRESHOLD_TICKS
        } else {
            0
        };
        if max_repairs == 0 || slot_meta.is_full() {
            vec![]
        } else if slot_meta.consumed == slot_meta.received {
            if throttle_requests_by_shred_tick {
                // check delay time of last shred
                if let Some(reference_tick) = slot_meta
                    .received
                    .checked_sub(1)
                    .and_then(|index| blockstore.get_data_shred(slot, index).ok()?)
                    .and_then(|shred| shred::layout::get_reference_tick(&shred).ok())
                    .map(u64::from)
                {
                    // System time is not monotonic
                    let ticks_since_first_insert = DEFAULT_TICKS_PER_SECOND
                        * timestamp().saturating_sub(slot_meta.first_shred_timestamp)
                        / 1_000;
                    if ticks_since_first_insert
                        < reference_tick.saturating_add(defer_repair_threshold_ticks)
                    {
                        return vec![];
                    }
                }
            }

            match RepairService::request_repair_if_needed(
                outstanding_repairs,
                ShredRepairType::HighestShred(slot, slot_meta.received),
            ) {
                Some(repair_request) => vec![repair_request],
                None => vec![],
            }
        } else {
            blockstore
                .find_missing_data_indexes(
                    slot,
                    slot_meta.first_shred_timestamp,
                    defer_repair_threshold_ticks,
                    slot_meta.consumed,
                    slot_meta.received,
                    max_repairs,
                )
                .into_iter()
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
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) {
        let mut pending_slots = vec![slot];
        while repairs.len() < max_repairs && !pending_slots.is_empty() {
            let slot = pending_slots.pop().unwrap();
            if let Some(slot_meta) = blockstore.meta(slot).unwrap() {
                let new_repairs = Self::generate_repairs_for_slot_throttled_by_tick(
                    blockstore,
                    slot,
                    &slot_meta,
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

    pub(crate) fn generate_repairs_for_wen_restart(
        blockstore: &Blockstore,
        max_repairs: usize,
        slots: &Vec<Slot>,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        let mut repairs: Vec<ShredRepairType> = Vec::new();
        for slot in slots {
            if let Some(slot_meta) = blockstore.meta(*slot).unwrap() {
                // When in wen_restart, turbine is not running, so there is
                // no need to wait after first shred.
                let new_repairs = Self::generate_repairs_for_slot_not_throttled_by_tick(
                    blockstore,
                    *slot,
                    &slot_meta,
                    max_repairs - repairs.len(),
                    outstanding_repairs,
                );
                repairs.extend(new_repairs);
            } else {
                repairs.push(ShredRepairType::HighestShred(*slot, 0));
            }
            if repairs.len() >= max_repairs {
                break;
            }
        }
        repairs
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
        let mut rng = rand::thread_rng();
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
            .add_request(repair_request, timestamp());

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
        for slot in repair_range.start..=repair_range.end {
            if repairs.len() >= max_repairs {
                break;
            }

            let meta = blockstore
                .meta(slot)
                .expect("Unable to lookup slot meta")
                .unwrap_or(SlotMeta {
                    slot,
                    ..SlotMeta::default()
                });

            let new_repairs = Self::generate_repairs_for_slot_throttled_by_tick(
                blockstore,
                slot,
                &meta,
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
        if let Some(slot_meta) = blockstore.meta(slot).unwrap() {
            if slot_meta.is_full() {
                // If the slot is full, no further need to repair this slot
                None
            } else {
                Some(Self::generate_repairs_for_slot_throttled_by_tick(
                    blockstore,
                    slot,
                    &slot_meta,
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
                        let nonce = outstanding_requests.add_request(repair_type, timestamp());

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
        self.ancestor_hashes_service.join()
    }
}

#[cfg(test)]
pub(crate) fn sleep_shred_deferment_period() {
    // sleep to bypass shred deferment window
    sleep(Duration::from_millis(
        DEFAULT_MS_PER_SLOT + DEFER_REPAIR_THRESHOLD.as_millis() as u64,
    ));
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::repair::quic_endpoint::RemoteRequest,
        solana_gossip::{contact_info::ContactInfo, node::Node},
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{
                make_chaining_slot_entries, make_many_slot_entries, make_slot_entries, Blockstore,
            },
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            get_tmp_ledger_path_auto_delete,
            shred::max_ticks_per_n_shreds,
        },
        solana_net_utils::{sockets::bind_to_localhost_unique, SocketAddrSpace},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        std::collections::HashSet,
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
            cluster_info.clone(),
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
        let Some(bytes) = packet.data(..).map(Vec::from) else {
            panic!("packet data not found");
        };
        let remote_request = RemoteRequest {
            remote_pubkey: None,
            remote_address: packet.meta().socket_addr(),
            bytes: Bytes::from(bytes),
        };

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
        blockstore.insert_shreds(shreds, None, false).unwrap();
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
        blockstore.insert_shreds(shreds, None, false).unwrap();
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
        blockstore
            .insert_shreds(shreds_to_write, None, false)
            .unwrap();
        let expected: Vec<ShredRepairType> = (0..num_slots)
            .flat_map(|slot| {
                missing_indexes_per_slot
                    .iter()
                    .map(move |shred_index| ShredRepairType::Shred(slot, *shred_index))
            })
            .collect();

        let mut repair_weight = RepairWeight::new(0);
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
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            )[..],
            expected[0..expected.len() - 2]
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

        blockstore.insert_shreds(shreds, None, false).unwrap();

        // We didn't get the last shred for this slot, so ask for the highest shred for that slot
        let expected: Vec<ShredRepairType> =
            vec![ShredRepairType::HighestShred(0, num_shreds_per_slot - 1)];

        sleep_shred_deferment_period();
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
                &mut RepairMetrics::default(),
                &mut HashMap::default(),
            ),
            expected
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
            blockstore.insert_shreds(slot_shreds, None, false).unwrap();
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

            blockstore.insert_shreds(shreds, None, false).unwrap();
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
            .insert_shreds(shreds[..shreds.len() - 1].to_vec(), None, false)
            .unwrap();
        assert!(
            RepairService::generate_duplicate_repairs_for_slot(&blockstore, dead_slot,).is_some()
        );

        // SlotMeta is full, should make no repairs
        blockstore
            .insert_shreds(vec![shreds.pop().unwrap()], None, false)
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
            ServeRepair::new(
                cluster_info,
                bank_forks.read().unwrap().sharable_banks(),
                Arc::new(RwLock::new(HashSet::default())),
                Box::new(StandardRepairHandler::new(blockstore.clone())),
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
            .insert_shreds(shreds[..shreds.len() - 1].to_vec(), None, false)
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
        assert!(duplicate_slot_repair_statuses
            .get(&dead_slot)
            .unwrap()
            .repair_pubkey_and_addr
            .is_none());
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
            .insert_shreds(vec![shreds.pop().unwrap()], None, false)
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
            ServeRepair::new(
                cluster_info.clone(),
                bank_forks.read().unwrap().sharable_banks(),
                Arc::new(RwLock::new(HashSet::default())),
                Box::new(StandardRepairHandler::new(blockstore)),
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

    #[test]
    fn test_generate_repairs_for_wen_restart() {
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let max_repairs = 3;

        let slots: Vec<u64> = vec![2, 3, 5, 7];
        let num_entries_per_slot = max_ticks_per_n_shreds(3, None) + 1;

        let shreds = make_chaining_slot_entries(&slots, num_entries_per_slot, 0);
        for (i, (mut slot_shreds, _)) in shreds.into_iter().enumerate() {
            slot_shreds.remove(i);
            blockstore.insert_shreds(slot_shreds, None, false).unwrap();
        }

        let mut slots_to_repair: Vec<Slot> = vec![];

        // When slots_to_repair is empty, ignore all and return empty result.
        let result = RepairService::generate_repairs_for_wen_restart(
            &blockstore,
            max_repairs,
            &slots_to_repair,
            &mut HashMap::default(),
        );
        assert!(result.is_empty());

        // When asked to repair slot with missing shreds and some unknown slot, return correct results.
        slots_to_repair = vec![3, 81];
        let result = RepairService::generate_repairs_for_wen_restart(
            &blockstore,
            max_repairs,
            &slots_to_repair,
            &mut HashMap::default(),
        );
        assert_eq!(
            result,
            vec![
                ShredRepairType::Shred(3, 1),
                ShredRepairType::HighestShred(81, 0),
            ],
        );

        // Test that it will not generate more than max_repairs.e().unwrap();
        slots_to_repair = vec![2, 82, 7, 83, 84];
        let result = RepairService::generate_repairs_for_wen_restart(
            &blockstore,
            max_repairs,
            &slots_to_repair,
            &mut HashMap::default(),
        );
        assert_eq!(result.len(), max_repairs);
        assert_eq!(
            result,
            vec![
                ShredRepairType::Shred(2, 0),
                ShredRepairType::HighestShred(82, 0),
                ShredRepairType::Shred(7, 3),
            ],
        );
    }
}
