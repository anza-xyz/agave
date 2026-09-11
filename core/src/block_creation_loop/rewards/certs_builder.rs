use {
    crate::block_creation_loop::rewards::msg_types::{
        RewardRequest, RewardRespSucc, RewardResponse,
    },
    agave_bls_sigverify::rewards::RewardInput,
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
    crossbeam_channel::RecvError,
    entry::Entry,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure_us,
    solana_runtime::bank::Bank,
    std::{
        collections::BTreeMap,
        sync::Arc,
        time::{Duration, Instant},
    },
};

mod entry;

const REPORT_INTERVAL: Duration = Duration::from_secs(1);

struct Metrics {
    last_report: Instant,
    build_us: WelfordStats,
    purged_state: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            build_us: WelfordStats::default(),
            purged_state: 0,
        }
    }
}

impl Metrics {
    fn maybe_report(&mut self) {
        let Self {
            last_report,
            build_us,
            purged_state,
        } = self;
        if last_report.elapsed() > REPORT_INTERVAL {
            datapoint_info!(
                "reward-certs-builder",
                ("build_us_count", build_us.count(), i64),
                ("build_us_max", build_us.maximum::<u64>(), Option<i64>),
                ("build_us_mean", build_us.mean::<u64>(), Option<i64>),
                ("purged_state", *purged_state, i64),
            );
            *self = Self::default();
        }
    }
}

/// Container to store state needed to generate reward certificates.
pub(super) struct CertsBuilder {
    /// Per [`Slot`], stores the skip and notar votes.
    aggregates: BTreeMap<Slot, Entry>,
    /// Stores the latest pubkey for the current node.
    cluster_info: Arc<ClusterInfo>,
    metrics: Metrics,
}

impl CertsBuilder {
    /// Constructs a new instance of [`CertsBuilder`].
    pub(super) fn new(cluster_info: Arc<ClusterInfo>) -> Self {
        Self {
            aggregates: BTreeMap::default(),
            cluster_info,
            metrics: Metrics::default(),
        }
    }

    /// Builds reward certificates.
    fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(RewardRespSucc::default());
        };
        // we assume that the block creation loop will only ever request to build reward certs in a
        // strictly increasing order so we can drop older state
        self.aggregates = self.aggregates.split_off(&reward_slot);
        match self.aggregates.remove(&reward_slot) {
            None => Ok(RewardRespSucc::default()),
            Some(entry) => entry.build_certs(reward_slot),
        }
    }

    pub(super) fn build_request(
        &mut self,
        request: Result<RewardRequest, RecvError>,
    ) -> Result<(), ()> {
        let my_pubkey = self.cluster_info.id();
        match request {
            Ok(RewardRequest {
                bank_slot,
                reply_sender,
            }) => {
                let (result, build_us) = measure_us!(self.build_certs(bank_slot));
                let resp = RewardResponse { result };
                let _ = reply_sender.send(resp).inspect_err(|_| {
                    info!(
                        "{my_pubkey}: channel to send reply for bank_slot={bank_slot} disconnected"
                    );
                });
                self.metrics.build_us.add_sample(build_us);
                self.metrics.maybe_report();
                Ok(())
            }
            Err(_) => {
                error!("{my_pubkey}: build reward certs channel is disconnected; exiting.");
                Err(())
            }
        }
    }

    pub(super) fn handle_input(&mut self, root_bank: &Bank, input: RewardInput) {
        let root_slot = root_bank.slot();
        // drop state that is too old based on how the root slot has progressed
        // if this actually drops state, that probably indicates that the leader missed its
        // window.
        let new_aggregates = self
            .aggregates
            .split_off(&root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD));
        if !self.aggregates.is_empty() {
            self.metrics.purged_state += 1;
        }
        self.aggregates = new_aggregates;

        match input {
            RewardInput::External(aggregates) => {
                for aggregate in aggregates {
                    let vote = *aggregate.vote();
                    let vote_slot = vote.slot();
                    let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                        warn!(
                            "failed to look up rank_map for slot {vote_slot} using bank for slot \
                             {}",
                            root_bank.slot()
                        );
                        return;
                    };
                    let max_validators = rank_map.len();
                    let mut vote_account_pubkeys = vec![];
                    for rank in aggregate.ranks().iter_ones() {
                        let Some(stake_entry) = rank_map.get_pubkey_stake_entry(rank) else {
                            return;
                        };
                        vote_account_pubkeys.push(stake_entry.vote_account_pubkey);
                    }

                    match self
                        .aggregates
                        .entry(vote_slot)
                        .or_insert_with(|| Entry::new(max_validators))
                        .add_aggregate(aggregate, vote_account_pubkeys)
                    {
                        Ok(()) => (),
                        Err(e) => {
                            warn!("Adding aggregate with vote {vote:?} failed with {e}");
                        }
                    }
                }
            }
            RewardInput::Own(vote_msg) => {
                let vote = vote_msg.vote;
                let vote_slot = vote.slot();
                let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                    warn!(
                        "failed to look up rank_map for slot {vote_slot} using bank for slot {}",
                        root_bank.slot()
                    );
                    return;
                };
                let max_validators = rank_map.len();
                let Some(stake_entry) = rank_map.get_pubkey_stake_entry(vote_msg.rank as usize)
                else {
                    return;
                };

                match self
                    .aggregates
                    .entry(vote_msg.vote.slot())
                    .or_insert_with(|| Entry::new(max_validators))
                    .add_own_msg(vote_msg, stake_entry.vote_account_pubkey)
                {
                    Ok(()) => (),
                    Err(e) => {
                        warn!("Adding aggregate with vote {vote:?} failed with {e}");
                    }
                }
            }
        }
        self.metrics.maybe_report();
    }
}
