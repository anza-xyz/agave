use {
    crate::{
        block_creation_loop::rewards::{
            certs_builder::CertsBuilder, certs_requestor::CertsRequestor, msg_types::RewardRequest,
        },
        tvu::MAX_ALPENGLOW_PACKET_NUM,
    },
    agave_bls_sigverify::rewards::RewardInput,
    crossbeam_channel::{Receiver, Sender, bounded, select_biased},
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
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
        Self {
            exit,
            cluster_info,
            aggregates_receiver,
            req_receiver,
            sharable_banks,
            builder,
        }
    }

    fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let my_pubkey = self.cluster_info.id();
            // bias messages to build certificates as that is on the critical path
            select_biased! {
                recv(self.req_receiver) -> msg => {
                    if let Err(()) = self.builder.build_request(msg) {
                        break;
                    }
                }
                recv(self.aggregates_receiver) -> msg => {
                    match msg {
                        Ok(reward_input) => {
                            let bank = self.sharable_banks.root();
                            self.builder.handle_input(&bank, reward_input);
                        }
                        Err(_) => {
                            error!("{my_pubkey}: aggregates receiver channel is disconnected; exiting.");
                            break;
                        }
                    }
                }
                default(Duration::from_secs(1)) => {
                    continue;
                }
            }
        }
    }
}
