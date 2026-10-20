use {
    crate::block_creation_loop::rewards::msg_types::{
        RewardRequest, RewardRespSucc, RewardResponse,
    },
    agave_bls_sigverify::rewards::RewardInput,
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
    crossbeam_channel::RecvError,
    entry::Entry,
    recovery::{Recovery, RecoveryMetrics},
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
mod recovery;

pub(super) use recovery::RecoveryService;

const REPORT_INTERVAL: Duration = Duration::from_secs(1);

struct Metrics {
    last_report: Instant,
    build_us: WelfordStats,
    purged_state: u64,
    queue_waited_us: WelfordStats,
    /// Direct vote aggregates for a source slot that was already built or pruned.
    late_vote_aggregates: u64,
    recovery: RecoveryMetrics,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            build_us: WelfordStats::default(),
            purged_state: 0,
            queue_waited_us: WelfordStats::default(),
            late_vote_aggregates: 0,
            recovery: RecoveryMetrics::default(),
        }
    }
}

impl Metrics {
    fn maybe_report(&mut self, recovery_enabled: bool) {
        let Self {
            last_report,
            build_us,
            purged_state,
            queue_waited_us,
            late_vote_aggregates,
            recovery,
        } = self;
        if last_report.elapsed() > REPORT_INTERVAL {
            datapoint_info!(
                "reward-certs-builder",
                ("build_us_count", build_us.count(), i64),
                ("build_us_max", build_us.maximum::<u64>(), Option<i64>),
                ("build_us_mean", build_us.mean::<u64>(), Option<i64>),
                ("queue_waited_us_count", queue_waited_us.count(), i64),
                ("queue_waited_us_max", queue_waited_us.maximum::<u64>(), Option<i64>),
                ("queue_waited_us_mean", queue_waited_us.mean::<u64>(), Option<i64>),
                ("purged_state", *purged_state, i64),
                ("late_vote_aggregates", *late_vote_aggregates, i64),
            );
            if recovery_enabled {
                recovery.report();
            }
            *self = Self::default();
        }
    }
}

/// Container to store state needed to generate reward certificates.
#[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
pub(super) struct CertsBuilder {
    /// Per [`Slot`], stores the skip and notar votes.
    aggregates: BTreeMap<Slot, Entry>,
    /// Stores the latest pubkey for the current node.
    cluster_info: Arc<ClusterInfo>,
    metrics: Metrics,
    /// Absent when reward recovery is disabled, so only direct votes are published.
    recovery: Option<Recovery>,
    /// A sealed slot must not be recreated by a late vote or worker completion.
    sealed_through: Option<Slot>,
}

impl CertsBuilder {
    /// Constructs a new instance of [`CertsBuilder`].
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn new(cluster_info: Arc<ClusterInfo>, enable_reward_recovery: bool) -> Self {
        Self {
            aggregates: BTreeMap::default(),
            cluster_info,
            metrics: Metrics::default(),
            recovery: enable_reward_recovery.then(Recovery::new),
            sealed_through: None,
        }
    }

    pub(super) fn start_recovery_service(&mut self) -> Option<RecoveryService> {
        self.recovery
            .take()
            .map(|recovery| RecoveryService::new(recovery, self.cluster_info.my_shred_version()))
    }

    #[cfg(test)]
    pub(super) fn start_recovery_service_with_gates(
        &mut self,
        admission_gate: Option<(
            crossbeam_channel::Sender<()>,
            crossbeam_channel::Receiver<()>,
        )>,
        selection_gate: Option<(
            crossbeam_channel::Sender<()>,
            crossbeam_channel::Receiver<()>,
        )>,
    ) -> Option<RecoveryService> {
        self.recovery.take().map(|recovery| {
            RecoveryService::new_with_gates(
                recovery,
                self.cluster_info.my_shred_version(),
                admission_gate,
                selection_gate,
            )
        })
    }

    #[cfg(test)]
    pub(super) fn restore_recovery_service(&mut self, recovery: RecoveryService) {
        self.recovery = Some(recovery.join());
    }

    /// Schedules recovery jobs and collects their results.
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn poll_recovery(&mut self) {
        if let Some(recovery) = &mut self.recovery {
            recovery.poll(self.cluster_info.my_shred_version());
        }
    }

    #[cfg(test)]
    pub(super) fn observe_recovery_completions(&mut self) -> crossbeam_channel::Receiver<Slot> {
        self.recovery.as_mut().unwrap().observe_completions()
    }

    /// The longest the recovery worker spent on one job collected since the last report, in
    /// microseconds.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn longest_recovery_job_us(&self) -> Option<u64> {
        self.recovery.as_ref()?.metrics.worker_us.maximum()
    }

    pub(super) fn maybe_report(&mut self) {
        if let Some(recovery) = &mut self.recovery {
            self.metrics
                .recovery
                .merge(std::mem::take(&mut recovery.metrics));
        }
        self.metrics.maybe_report(self.recovery.is_some());
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn poll_interval(&self) -> Duration {
        // Without recovery, nothing waits for a background job.
        self.recovery
            .as_ref()
            .map_or(Duration::from_secs(1), Recovery::poll_interval)
    }

    /// Builds reward certificates.
    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(RewardRespSucc::default());
        };
        if let Some(recovery) = &mut self.recovery {
            recovery.collect_ready();
        }
        self.sealed_through = Some(
            self.sealed_through
                .map_or(reward_slot, |slot| slot.max(reward_slot)),
        );
        // we assume that the block creation loop will only ever request to build reward certs in a
        // strictly increasing order so we can drop older state
        self.aggregates = self.aggregates.split_off(&reward_slot);
        let baseline = self
            .aggregates
            .remove(&reward_slot)
            .map_or_else(
                || Ok(RewardRespSucc::default()),
                |entry| entry.build_certs(reward_slot),
            )
            // The sealed slot's recovery state is not needed without a response.
            .inspect_err(|_| {
                if let Some(recovery) = &mut self.recovery {
                    recovery.retain_from(reward_slot.saturating_add(1));
                }
            })?;
        Ok(match &mut self.recovery {
            Some(recovery) => {
                recovery.finish(reward_slot, self.cluster_info.my_shred_version(), baseline)
            }
            None => baseline,
        })
    }

    pub(super) fn build_request(
        &mut self,
        request: Result<RewardRequest, RecvError>,
        recovery: Option<&RecoveryService>,
    ) -> Result<(), ()> {
        let my_pubkey = self.cluster_info.id();
        match request {
            Ok(RewardRequest {
                bank_slot,
                reply_sender,
                request_sent,
            }) => {
                let queue_waited_us = request_sent
                    .elapsed()
                    .as_micros()
                    .try_into()
                    .unwrap_or(u64::MAX);
                // Freeze the direct snapshot now. Optional admission, verification, and selection
                // live on another thread and can never delay publication of this baseline.
                let cutoff = Instant::now();
                let (result, build_us) = measure_us!(self.build_certs(bank_slot));
                let task = recovery.and_then(|recovery| {
                    let slot = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD)?;
                    let baseline = result.as_ref().cloned().unwrap_or_default();
                    let (sender, receiver) = crossbeam_channel::bounded(1);
                    Some((recovery, slot, baseline, sender, receiver))
                });
                let recovery_receiver = task
                    .as_ref()
                    .filter(|_| result.is_ok())
                    .map(|(_, _, _, _, receiver)| receiver.clone());
                let resp = RewardResponse {
                    result,
                    recovery: recovery_receiver,
                };
                let _ = reply_sender.send(resp).inspect_err(|_| {
                    info!(
                        "{my_pubkey}: channel to send reply for bank_slot={bank_slot} disconnected"
                    );
                });
                if let Some((recovery, slot, baseline, sender, _)) = task {
                    recovery.finish(
                        slot,
                        self.cluster_info.my_shred_version(),
                        baseline,
                        cutoff,
                        sender,
                    );
                }
                self.metrics.build_us.add_sample(build_us);
                self.metrics.queue_waited_us.add_sample(queue_waited_us);
                self.maybe_report();
                Ok(())
            }
            Err(_) => {
                error!("{my_pubkey}: build reward certs channel is disconnected; exiting.");
                Err(())
            }
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifier_attr::qualifiers(pub))]
    pub(super) fn handle_input(&mut self, root_bank: &Bank, input: RewardInput) {
        let root_slot = root_bank.slot();
        let minimum_slot = root_slot
            .checked_sub(NUM_SLOTS_FOR_REWARD)
            .map_or(0, |slot| slot.saturating_add(1))
            .max(self.sealed_through.map_or(0, |slot| slot.saturating_add(1)));
        // drop state that is too old based on how the root slot has progressed
        // if this actually drops state, that probably indicates that the leader missed its
        // window.
        let new_aggregates = self.aggregates.split_off(&minimum_slot);
        if !self.aggregates.is_empty() {
            self.metrics.purged_state += 1;
        }
        self.aggregates = new_aggregates;
        if let Some(recovery) = &mut self.recovery {
            recovery.retain_from(minimum_slot);
        }

        match input {
            RewardInput::External(aggregates) => {
                for aggregate in aggregates {
                    let vote = *aggregate.vote();
                    let vote_slot = vote.slot();
                    if vote_slot < minimum_slot {
                        self.metrics.late_vote_aggregates += 1;
                        continue;
                    }
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
                if vote_slot < minimum_slot {
                    self.metrics.late_vote_aggregates += 1;
                    return;
                }
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
                if let Some(recovery) = &mut self.recovery {
                    recovery.add_signatures(
                        vote,
                        [(vote_msg.rank, vote_msg.signature.into())],
                        rank_map,
                    );
                }

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
            // Only reward recovery uses raw signatures and certificates; when it is disabled, their
            // senders are too.
            RewardInput::VoteSignatures { vote, signatures } => {
                if let Some(recovery) = &mut self.recovery {
                    if vote.slot() < minimum_slot {
                        recovery.metrics.late_signature_components += signatures.len() as u64;
                    } else if let Some(rank_map) = root_bank.get_rank_map(vote.slot()) {
                        recovery.add_signatures(vote, signatures, rank_map);
                    }
                }
            }
            RewardInput::Certificates(certs) => {
                if let Some(recovery) = &mut self.recovery {
                    for cert in certs {
                        let slot = cert.cert_type.slot();
                        if slot < minimum_slot {
                            recovery.metrics.late_certificates += 1;
                        } else if let Some(rank_map) = root_bank.get_rank_map(slot) {
                            recovery.add_certificate(cert, rank_map);
                        }
                    }
                }
            }
        }
        self.poll_recovery();
        self.maybe_report();
    }
}

#[cfg(test)]
impl CertsBuilder {
    /// Tests build with reward recovery enabled.
    fn recovery_mut(&mut self) -> &mut Recovery {
        self.recovery
            .as_mut()
            .expect("tests enable reward recovery")
    }
}
