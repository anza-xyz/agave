//! The `fetch_stage` batches input from a UDP socket and sends it to a channel.

use {
    crate::result::{Error, Result},
    crossbeam_channel::{RecvTimeoutError, TrySendError, unbounded},
    solana_clock::{DEFAULT_TICKS_PER_SLOT, HOLD_TRANSACTIONS_SLOT_OFFSET},
    solana_metrics::{inc_new_counter_debug, inc_new_counter_info},
    solana_packet::PacketFlags,
    solana_perf::{
        packet::{PacketBatchRecycler, PacketRefMut},
        recycler::Recycler,
    },
    solana_poh::poh_recorder::PohRecorder,
    solana_streamer::streamer::{
        self, PacketBatchReceiver, PacketBatchSender, StreamerReceiveStats,
    },
    std::{
        net::UdpSocket,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle, sleep},
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
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        coalesce: Option<Duration>,
    ) -> (Self, PacketBatchReceiver, PacketBatchReceiver) {
        let (sender, receiver) = unbounded();
        let (vote_sender, vote_receiver) = unbounded();
        let (_forward_sender, forward_receiver) = unbounded();
        (
            Self::new_with_sender(
                tpu_vote_sockets,
                exit,
                &sender,
                &vote_sender,
                forward_receiver,
                poh_recorder,
                coalesce,
            ),
            receiver,
            vote_receiver,
        )
    }

    pub fn new_with_sender(
        tpu_vote_sockets: Vec<UdpSocket>,
        exit: Arc<AtomicBool>,
        sender: &PacketBatchSender,
        vote_sender: &PacketBatchSender,
        forward_receiver: PacketBatchReceiver,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        coalesce: Option<Duration>,
    ) -> Self {
        let tpu_vote_sockets = tpu_vote_sockets.into_iter().map(Arc::new).collect();
        Self::new_multi_socket(
            tpu_vote_sockets,
            exit,
            sender,
            vote_sender,
            forward_receiver,
            poh_recorder,
            coalesce,
        )
    }

    fn handle_forwarded_packets(
        recvr: &PacketBatchReceiver,
        sendr: &PacketBatchSender,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
    ) -> Result<()> {
        let mark_forwarded = |mut packet: PacketRefMut| {
            packet.meta_mut().flags |= PacketFlags::FORWARDED;
        };

        let mut packet_batch = recvr.recv()?;
        let mut num_packets = packet_batch.len();
        packet_batch.iter_mut().for_each(mark_forwarded);
        let mut packet_batches = vec![packet_batch];
        while let Ok(mut packet_batch) = recvr.try_recv() {
            packet_batch.iter_mut().for_each(mark_forwarded);
            num_packets += packet_batch.len();
            packet_batches.push(packet_batch);
            // Read at most 1K transactions in a loop
            if num_packets > 1024 {
                break;
            }
        }

        if poh_recorder
            .read()
            .unwrap()
            .would_be_leader(HOLD_TRANSACTIONS_SLOT_OFFSET.saturating_mul(DEFAULT_TICKS_PER_SLOT))
        {
            let mut packets_sent = 0usize;
            let mut packets_dropped = 0usize;
            for packet_batch in packet_batches {
                let packets_in_batch = packet_batch.len();
                match sendr.try_send(packet_batch) {
                    Ok(()) => {
                        packets_sent += packets_in_batch;
                    }
                    Err(TrySendError::Full(_)) => {
                        packets_dropped += packets_in_batch;
                    }
                    Err(TrySendError::Disconnected(_)) => return Err(Error::Send),
                };
            }
            inc_new_counter_debug!("fetch_stage-honor_forwards", packets_sent);
            if packets_dropped > 0 {
                inc_new_counter_error!("fetch_stage-dropped_forwards", packets_dropped);
            }
        } else {
            inc_new_counter_info!("fetch_stage-discard_forwards", num_packets);
        }

        Ok(())
    }

    fn new_multi_socket(
        tpu_vote_sockets: Vec<Arc<UdpSocket>>,
        exit: Arc<AtomicBool>,
        sender: &PacketBatchSender,
        vote_sender: &PacketBatchSender,
        forward_receiver: PacketBatchReceiver,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
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

        let sender = sender.clone();
        let poh_recorder = poh_recorder.clone();

        let fwd_thread_hdl = Builder::new()
            .name("solFetchStgFwRx".to_string())
            .spawn(move || {
                loop {
                    if let Err(e) =
                        Self::handle_forwarded_packets(&forward_receiver, &sender, &poh_recorder)
                    {
                        match e {
                            Error::RecvTimeout(RecvTimeoutError::Disconnected) => break,
                            Error::RecvTimeout(RecvTimeoutError::Timeout) => (),
                            Error::Recv(_) => break,
                            Error::Send => break,
                            _ => error!("{e:?}"),
                        }
                    }
                }
            })
            .unwrap();

        let metrics_thread_hdl = Builder::new()
            .name("solFetchStgMetr".to_string())
            .spawn(move || {
                loop {
                    sleep(Duration::from_secs(1));

                    tpu_vote_stats.report();

                    if exit.load(Ordering::Relaxed) {
                        return;
                    }
                }
            })
            .unwrap();

        Self {
            thread_hdls: [tpu_vote_threads, vec![fwd_thread_hdl, metrics_thread_hdl]]
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
