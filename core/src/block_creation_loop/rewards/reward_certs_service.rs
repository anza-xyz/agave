use {
    crate::{
        block_creation_loop::rewards::{
            certs_builder::{CertsBuilder, recovery::RecoveryService},
            certs_requestor::CertsRequestor,
            msg_types::{RewardRequest, RewardResponse},
        },
        tvu::MAX_ALPENGLOW_PACKET_NUM,
    },
    agave_bls_sigverify::rewards::RewardInput,
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::reward_certificate::NUM_SLOTS_FOR_REWARD,
    crossbeam_channel::{Receiver, RecvError, Sender, bounded, select_biased},
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure_us,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct Metrics {
    pub(super) last_report: Instant,
    build_us: WelfordStats,
    purged_state: u64,
    queue_waited_us: WelfordStats,
    /// Direct vote aggregates for a source slot that was already built or pruned.
    pub(super) late_vote_aggregates: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            build_us: WelfordStats::default(),
            purged_state: 0,
            queue_waited_us: WelfordStats::default(),
            late_vote_aggregates: 0,
        }
    }
}

impl Metrics {
    fn maybe_report(&mut self) {
        let Self {
            last_report,
            build_us,
            purged_state,
            queue_waited_us,
            late_vote_aggregates,
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
            *self = Self::default();
        }
    }
}

pub(crate) struct RewardCertsService {
    handler: JoinHandle<()>,
}

impl RewardCertsService {
    pub(crate) fn new(
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        exit: Arc<AtomicBool>,
    ) -> (Self, CertsRequestor, Sender<RewardInput>) {
        let (reward_aggregates_sender, reward_aggregates_receiver) =
            bounded(MAX_ALPENGLOW_PACKET_NUM);
        let (certs_requestor, req_receiver) = CertsRequestor::new();
        let builder = CertsBuilder::new();
        let ctx = Context::new(
            exit,
            cluster_info,
            reward_aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
        );
        let handler = Builder::new()
            .name("solConsRew".to_string())
            .spawn(move || {
                ctx.run();
            })
            .unwrap();
        (Self { handler }, certs_requestor, reward_aggregates_sender)
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.handler.join()
    }
}

pub(super) struct Context {
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    pub(super) builder: CertsBuilder,
    pub(super) recovery: RecoveryService,
    pub(super) metrics: Metrics,
}

impl Context {
    fn new(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        aggregates_receiver: Receiver<RewardInput>,
        req_receiver: Receiver<RewardRequest>,
        sharable_banks: SharableBanks,
        builder: CertsBuilder,
    ) -> Self {
        let recovery = RecoveryService::new(cluster_info.my_shred_version());
        Self {
            exit,
            cluster_info,
            aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
            recovery,
            metrics: Metrics::default(),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_gates(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        aggregates_receiver: Receiver<RewardInput>,
        req_receiver: Receiver<RewardRequest>,
        sharable_banks: SharableBanks,
        builder: CertsBuilder,
        recovery: super::certs_builder::recovery::Recovery,
        admission_gate: Option<(Sender<()>, Receiver<()>)>,
        selection_gate: Option<(Sender<()>, Receiver<()>)>,
    ) -> Self {
        let recovery = RecoveryService::new_with_gates(
            recovery,
            cluster_info.my_shred_version(),
            admission_gate,
            selection_gate,
        );
        Self {
            exit,
            cluster_info,
            aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
            recovery,
            metrics: Metrics::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn into_parts(self) -> (CertsBuilder, super::certs_builder::recovery::Recovery) {
        (self.builder, self.recovery.join())
    }

    pub(super) fn handle_input(&mut self, input: RewardInput) {
        let bank = self.sharable_banks.root();
        self.recovery
            .set_shred_version(self.cluster_info.my_shred_version());
        if matches!(&input, RewardInput::External(_) | RewardInput::Own(_)) {
            let stats = self.builder.handle_input(&bank, &input);
            self.metrics.purged_state += stats.purged_state;
            self.metrics.late_vote_aggregates += stats.late_vote_aggregates;
        }
        self.metrics.maybe_report();
        self.recovery.input(bank, input);
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
                let (result, build_us) = measure_us!(self.builder.build_certs(bank_slot));
                let task = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD).map(|slot| {
                    let baseline = result.as_ref().cloned().unwrap_or_default();
                    let (sender, receiver) = crossbeam_channel::bounded(1);
                    (slot, baseline, sender, receiver)
                });
                let recovery_receiver = task
                    .as_ref()
                    .filter(|_| result.is_ok())
                    .map(|(_, _, _, receiver)| receiver.clone());
                let resp = RewardResponse {
                    result,
                    recovery: recovery_receiver,
                };
                let _ = reply_sender.send(resp).inspect_err(|_| {
                    info!(
                        "{my_pubkey}: channel to send reply for bank_slot={bank_slot} disconnected"
                    );
                });
                if let Some((slot, baseline, sender, _)) = task {
                    self.recovery.finish(
                        slot,
                        self.cluster_info.my_shred_version(),
                        baseline,
                        cutoff,
                        sender,
                    );
                }
                self.metrics.build_us.add_sample(build_us);
                self.metrics.queue_waited_us.add_sample(queue_waited_us);
                self.metrics.maybe_report();
                Ok(())
            }
            Err(_) => {
                error!("{my_pubkey}: build reward certs channel is disconnected; exiting.");
                Err(())
            }
        }
    }

    fn run(mut self) {
        self.run_loop();
    }

    fn run_loop(&mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            self.metrics.maybe_report();
            if self.process_next_input().is_err() {
                break;
            }
        }
    }

    fn process_next_input(&mut self) -> Result<(), ()> {
        let my_pubkey = self.cluster_info.id();
        // Bias messages to build certificates as that is on the critical path.
        select_biased! {
            recv(self.req_receiver) -> msg => self.build_request(msg),
            recv(self.aggregates_receiver) -> msg => {
                match msg {
                    Ok(reward_input) => {
                        self.handle_input(reward_input);
                        Ok(())
                    }
                    Err(_) => {
                        error!("{my_pubkey}: aggregates receiver channel is disconnected; exiting.");
                        Err(())
                    }
                }
            }
            default(Duration::from_secs(1)) => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_recovery_for_tests(
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    builder: CertsBuilder,
    recovery: super::certs_builder::recovery::Recovery,
    admission_gate: Option<(Sender<()>, Receiver<()>)>,
) -> (CertsBuilder, super::certs_builder::recovery::Recovery) {
    let mut context = Context::new_with_gates(
        exit,
        cluster_info,
        aggregates_receiver,
        req_receiver,
        sharable_banks,
        builder,
        recovery,
        admission_gate,
        None,
    );
    context.run_loop();
    context.into_parts()
}

#[cfg(test)]
mod tests;
