use {
    crate::{
        block_creation_loop::rewards::certs_builder::{self, CertsBuilder},
        tvu::MAX_ALPENGLOW_PACKET_NUM,
    },
    agave_bls_sigverify::rewards::RewardInput,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded},
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        sync::{
            Arc, Mutex,
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
    ) -> (Self, Arc<Mutex<CertsBuilder>>, Sender<RewardInput>) {
        let (reward_aggregates_sender, reward_aggregates_receiver) =
            bounded(MAX_ALPENGLOW_PACKET_NUM);
        let builder = Arc::<Mutex<CertsBuilder>>::default();
        let ctx = Context {
            exit,
            cluster_info,
            aggregates_receiver: reward_aggregates_receiver,
            sharable_banks,
            builder: builder.clone(),
        };
        let handler = Builder::new()
            .name("solConsRew".to_string())
            .spawn(move || {
                ctx.run();
            })
            .unwrap();
        (Self { handler }, builder, reward_aggregates_sender)
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.handler.join()
    }
}

struct Context {
    exit: Arc<AtomicBool>,
    cluster_info: Arc<ClusterInfo>,
    aggregates_receiver: Receiver<RewardInput>,
    sharable_banks: SharableBanks,
    builder: Arc<Mutex<CertsBuilder>>,
}

impl Context {
    fn run(self) {
        while !self.exit.load(Ordering::Relaxed) {
            match self
                .aggregates_receiver
                .recv_timeout(Duration::from_secs(1))
            {
                Ok(reward_input) => {
                    let bank = self.sharable_banks.root();
                    certs_builder::handle_input(&self.builder, &bank, reward_input);
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.builder.lock().unwrap().maybe_report();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    error!(
                        "{}: aggregates receiver channel is disconnected; exiting.",
                        self.cluster_info.id()
                    );
                    break;
                }
            }
        }
    }
}
