//! The `fetch_stage` batches input from a UDP socket and sends it to a channel.

use {
    crossbeam_channel::unbounded,
    solana_perf::{packet::PacketBatchRecycler, recycler::Recycler},
    solana_streamer::streamer::{
        self, PacketBatchReceiver, PacketBatchSender, StreamerReceiveStats,
    },
    std::{
        net::UdpSocket,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct FetchStage {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl FetchStage {
    pub fn new(
        tpu_vote_sockets: Vec<UdpSocket>,
        exit: Arc<AtomicBool>,
        coalesce: Option<Duration>,
    ) -> (Self, PacketBatchReceiver) {
        let (vote_sender, vote_receiver) = unbounded();
        (
            Self::new_with_sender(tpu_vote_sockets, exit, &vote_sender, coalesce),
            vote_receiver,
        )
    }

    pub fn new_with_sender(
        tpu_vote_sockets: Vec<UdpSocket>,
        exit: Arc<AtomicBool>,
        vote_sender: &PacketBatchSender,
        coalesce: Option<Duration>,
    ) -> Self {
        let tpu_vote_sockets = tpu_vote_sockets.into_iter().map(Arc::new).collect();
        Self::new_multi_socket(tpu_vote_sockets, exit, vote_sender, coalesce)
    }

    fn new_multi_socket(
        tpu_vote_sockets: Vec<Arc<UdpSocket>>,
        exit: Arc<AtomicBool>,
        vote_sender: &PacketBatchSender,
        coalesce: Option<Duration>,
    ) -> Self {
        let recycler: PacketBatchRecycler = Recycler::warmed(1000, 1024);

        let tpu_vote_stats = Arc::new(StreamerReceiveStats::new("tpu_vote_receiver"));
        let tpu_vote_threads: Vec<_> = tpu_vote_sockets
            .into_iter()
            .enumerate()
            .map(|(i, socket)| {
                streamer::receiver(
                    format!("solRcvrTpuVot{i:02}"),
                    socket,
                    exit.clone(),
                    vote_sender.clone(),
                    recycler.clone(),
                    tpu_vote_stats.clone(),
                    coalesce,
                    true,
                    true, // only staked connections should be voting
                )
            })
            .collect();

        let metrics_thread_hdl = Builder::new()
            .name("solFetchStgMetr".to_string())
            .spawn(move || loop {
                sleep(Duration::from_secs(1));

                tpu_vote_stats.report();

                if exit.load(Ordering::Relaxed) {
                    return;
                }
            })
            .unwrap();

        Self {
            thread_hdls: [tpu_vote_threads, vec![metrics_thread_hdl]]
                .into_iter()
                .flatten()
                .collect(),
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}
