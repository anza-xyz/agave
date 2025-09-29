use {
    crate::{
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
        },
        leader_updater::LeaderUpdater,
        ConnectionWorkersScheduler,
    },
    log::warn,
    solana_keypair::Keypair,
    std::net::{SocketAddr, UdpSocket},
    tokio::{
        runtime::Handle,
        sync::{mpsc, watch},
    },
    tokio_util::{sync::CancellationToken, task::TaskTracker},
};

// TODO(klykov): temp name
trait Transactions {}

/// [`TpuClientNextClient`]
#[derive(Clone)]
pub struct TpuClientNextClient<TransactionBatch: Transactions> {
    sender: mpsc::Sender<TransactionBatch>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
    tasks: TaskTracker,
    cancel: CancellationToken,
}

pub struct TpuClientNextClientBuilder {
    leader_updater: Box<dyn LeaderUpdater>,
    bind_target: BindTarget,
    identity: Option<StakeIdentity>,
    num_connections: Option<usize>,
    leader_send_fanout: Option<usize>,
    metrics_reporting: Option<(String, std::time::Duration)>,
    cancel: CancellationToken,
}

impl TpuClientNextClientBuilder {
    pub fn new(leader_updater: Box<dyn LeaderUpdater>, cancel: CancellationToken) -> Self {
        Self {
            leader_updater,
            bind_target,
            identity: None,
            num_connections: None,
            leader_send_fanout: None,
            metrics_reporting: None,
            cancel,
        }
    }

    pub fn set_bind_socket(mut self, bind_socket: UdpSocket) -> Self {
        self.bind_target = BindTarget::Socket(bind_socket);
        self
    }

    pub fn set_bind_addr(mut self, bind_addr: SocketAddr) -> Self {
        self.bind_target = BindTarget::Address(bind_addr);
        self
    }

    pub fn set_leader_send_fanout(mut self, fanout: usize) -> Self {
        self.leader_send_fanout = Some(fanout);
        self
    }

    pub fn set_identity(mut self, identity: &Keypair) -> Self {
        self.identity = Some(StakeIdentity::new(identity));
        self
    }

    pub fn set_max_cache_size(mut self, num_connections: usize) -> Self {
        self.num_connections = Some(num_connections);
        self
    }

    pub fn set_metrics_reporting(mut self, name: &str, interval: std::time::Duration) -> Self {
        self.metrics_reporting = Some((name.to_string(), interval));
        self
    }

    async fn build<TransactionBatch: Transactions>(self) -> TpuClientNextClient<TransactionBatch> {
        // todo for this 128 add set as well
        let (sender, receiver) = mpsc::channel(128);

        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);

        let config = ConnectionWorkersSchedulerConfig {
            bind: self.bind_target,
            stake_identity: self.identity,
            num_connections: self.num_connections.unwrap(),
            skip_check_transaction_age: true,
            // experimentally found parameter values
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            // We open connection to one more leader in advance, which time-wise means ~1.6s
            leaders_fanout: Fanout {
                connect: self.leader_send_fanout.unwrap() + 1,
                send: self.leader_send_fanout.unwrap(),
            },
        };

        let scheduler = ConnectionWorkersScheduler::new(
            self.leader_updater,
            receiver,
            update_certificate_receiver,
            self.cancel.clone(),
        );
        let mut tasks = TaskTracker::new();
        // leaking handle to this task, as it will run until the cancel signal is received
        tasks.spawn(scheduler.get_stats().report_to_influxdb(
            &self.metrics_reporting.as_ref().unwrap().0,
            self.metrics_reporting.as_ref().unwrap().1,
            self.cancel.clone(),
        ));
        tasks.spawn(scheduler.run(config));
        TpuClientNextClient::<TransactionBatch> {
            sender,
            update_certificate_sender,
            tasks,
            cancel,
        }
    }
}

impl<TransactionBatch: Transactions> TpuClientNextClient<TransactionBatch> {
    pub async fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let res = self
            .sender
            .send(TransactionBatch::new(wire_transactions))
            .await;
        if res.is_err() {
            warn!("Failed to send transaction to channel: it is closed.");
        }
    }

    fn update_identity(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
