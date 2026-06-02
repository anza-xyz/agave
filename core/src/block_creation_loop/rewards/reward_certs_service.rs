use {
    crate::{
        block_creation_loop::rewards::{
            certs_builder::CertsBuilder,
            certs_requestor::CertsRequestor,
            msg_types::{AddVoteMessage, RewardRequest},
        },
        tvu::MAX_ALPENGLOW_PACKET_NUM,
    },
    crossbeam_channel::{Receiver, Sender, bounded, select_biased},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
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
        leader_schedule: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
        exit: Arc<AtomicBool>,
    ) -> (Self, CertsRequestor, Sender<AddVoteMessage>) {
        let (votes_sender, votes_receiver) = bounded(MAX_ALPENGLOW_PACKET_NUM);
        let (certs_requestor, req_receiver) = CertsRequestor::new();
        let builder = CertsBuilder::new(cluster_info.clone(), leader_schedule);
        let ctx = Context::new(
            exit,
            cluster_info,
            votes_receiver,
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
        (Self { handler }, certs_requestor, votes_sender)
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.handler.join()
    }
}

struct Context {
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    votes_receiver: Receiver<AddVoteMessage>,
    req_receiver: Receiver<RewardRequest>,
    sharable_banks: SharableBanks,
    builder: CertsBuilder,
}

impl Context {
    fn new(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        votes_receiver: Receiver<AddVoteMessage>,
        req_receiver: Receiver<RewardRequest>,
        sharable_banks: SharableBanks,
        builder: CertsBuilder,
    ) -> Self {
        Self {
            exit,
            cluster_info,
            votes_receiver,
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
                recv(self.votes_receiver) -> msg => {
                    match msg {
                        Ok(msg) => {
                            let bank = self.sharable_banks.root();
                            for vote in msg.votes {
                                self.builder.add_vote(&bank, &vote);
                            }
                        }
                        Err(_) => {
                            error!("{my_pubkey}: votes receiver channel is disconnected; exiting.");
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
