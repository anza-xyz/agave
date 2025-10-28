/// This is responsible for receiving the verified and deduplicated transactions
/// from the vortexor and sending down to the banking stage.
use {
    solana_perf::{packet::PacketBatchRecycler, recycler::Recycler},
    solana_streamer::streamer::{self, PacketBatchSender, StreamerReceiveStats},
    std::{
        net::UdpSocket,
        sync::{atomic::AtomicBool, Arc},
        thread::{self, JoinHandle},
    },
};

pub struct VerifiedPacketReceiver {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl VerifiedPacketReceiver {
    pub fn new(
        sockets: Vec<Arc<UdpSocket>>,
        sender: &PacketBatchSender,
        in_vote_only_mode: Option<Arc<AtomicBool>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let recycler: PacketBatchRecycler = Recycler::warmed(1000, 1024);

        let tpu_stats = Arc::new(StreamerReceiveStats::new("vortexor_receiver"));

        let thread_hdls = sockets
            .into_iter()
            .enumerate()
            .map(|(i, socket)| {
                streamer::receiver(
                    format!("solVtxRcvr{i:02}"),
                    socket,
                    exit.clone(),
                    sender.clone(),
                    recycler.clone(),
                    tpu_stats.clone(),
                    None, // coalesce
                    true,
                    in_vote_only_mode.clone(),
                    false, // is_staked_service
                )
            })
            .collect();

        Self { thread_hdls }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}
