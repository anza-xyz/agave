use {
    crate::{
        block_creation_loop::rewards::{
            certs_builder::{CertsBuilder, RecoveryService},
            certs_requestor::CertsRequestor,
            msg_types::RewardRequest,
        },
        tvu::MAX_ALPENGLOW_PACKET_NUM,
    },
    agave_bls_sigverify::rewards::RewardInput,
    agave_votor_messages::certificate::Certificate,
    crossbeam_channel::{Receiver, Sender, bounded, select_biased},
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
    },
};

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
        let builder = CertsBuilder::new(cluster_info.clone());
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

struct Context {
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    builder: CertsBuilder,
    /// Present until test shutdown returns recovery ownership to the builder.
    recovery: Option<RecoveryService>,
    pending_certificates: std::vec::IntoIter<Certificate>,
}

impl Context {
    fn new(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        aggregates_receiver: Receiver<RewardInput>,
        req_receiver: Receiver<RewardRequest>,
        sharable_banks: SharableBanks,
        mut builder: CertsBuilder,
    ) -> Self {
        let recovery = builder.start_recovery_service();
        Self {
            exit,
            cluster_info,
            aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
            recovery: Some(recovery),
            pending_certificates: Vec::new().into_iter(),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn new_with_gates(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        aggregates_receiver: Receiver<RewardInput>,
        req_receiver: Receiver<RewardRequest>,
        sharable_banks: SharableBanks,
        mut builder: CertsBuilder,
        admission_gate: Option<(Sender<()>, Receiver<()>)>,
        selection_gate: Option<(Sender<()>, Receiver<()>)>,
    ) -> Self {
        let recovery = builder.start_recovery_service_with_gates(admission_gate, selection_gate);
        Self {
            exit,
            cluster_info,
            aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
            recovery: Some(recovery),
            pending_certificates: Vec::new().into_iter(),
        }
    }

    #[cfg(test)]
    fn shutdown_recovery(&mut self) {
        if let Some(recovery) = self.recovery.take() {
            self.builder.restore_recovery_service(recovery);
        }
    }

    fn handle_input(&mut self, input: RewardInput) {
        let bank = self.sharable_banks.root();
        if let Some(recovery) = &self.recovery {
            recovery.set_shred_version(self.cluster_info.my_shred_version());
        }
        match input {
            RewardInput::External(_) => self.builder.handle_input(&bank, input),
            RewardInput::Own(message) => {
                if let Some(recovery) = &self.recovery {
                    recovery.input(
                        bank.clone(),
                        RewardInput::VoteSignatures {
                            vote: message.vote,
                            signatures: vec![(message.rank, message.signature.into())],
                        },
                    );
                }
                self.builder.handle_input(&bank, RewardInput::Own(message));
            }
            input => {
                if let Some(recovery) = &self.recovery {
                    recovery.input(bank, input);
                }
            }
        }
    }

    fn run(mut self) {
        self.run_loop();
    }

    fn run_loop(&mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            self.builder.maybe_report();
            if self.process_next_input().is_err() {
                break;
            }
        }
    }

    fn process_next_input(&mut self) -> Result<(), ()> {
        // Retain only the current batch and finish it before receiving another input.
        // A build request can interrupt between certificates, even under cap pressure.
        if !self.pending_certificates.as_slice().is_empty() {
            return select_biased! {
                recv(self.req_receiver) -> msg => self.builder.build_request(msg, self.recovery.as_ref()),
                default => {
                    if let Some(cert) = self.pending_certificates.next() {
                        self.handle_input(RewardInput::Certificates(vec![cert]));
                    }
                    Ok(())
                }
            };
        }
        let my_pubkey = self.cluster_info.id();
        // Bias messages to build certificates as that is on the critical path.
        select_biased! {
            recv(self.req_receiver) -> msg => self.builder.build_request(msg, self.recovery.as_ref()),
            recv(self.aggregates_receiver) -> msg => {
                match msg {
                    Ok(RewardInput::Certificates(certs)) if !certs.is_empty() => {
                        self.pending_certificates = certs.into_iter();
                        Ok(())
                    }
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
            default(self.builder.poll_interval()) => Ok(()),
        }
    }
}

#[cfg(test)]
pub(super) fn run_with_builder_for_tests(
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    builder: CertsBuilder,
) -> CertsBuilder {
    run_with_builder_and_admission_gate_for_tests(
        exit,
        cluster_info,
        aggregates_receiver,
        req_receiver,
        sharable_banks,
        builder,
        None,
    )
}

#[cfg(test)]
pub(super) fn run_with_builder_and_admission_gate_for_tests(
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    builder: CertsBuilder,
    admission_gate: Option<(Sender<()>, Receiver<()>)>,
) -> CertsBuilder {
    let mut context = Context::new_with_gates(
        exit,
        cluster_info,
        aggregates_receiver,
        req_receiver,
        sharable_banks,
        builder,
        admission_gate,
        None,
    );
    context.run_loop();
    context.shutdown_recovery();
    context.builder
}

#[cfg(test)]
mod tests;
