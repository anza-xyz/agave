//! Vortexor receiver adapter which wraps the VerifiedPacketReceiver
//! to receive packet batches from the remote and sends the packets to the
//! banking stage.

use {
    crate::banking_trace::TracedSender,
    agave_banking_stage_ingress_types::BankingPacketBatch,
    agave_verified_packet_receiver::receiver::VerifiedPacketReceiver,
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender},
    solana_perf::packet::PacketBatch,
    std::{
        net::UdpSocket,
        sync::{atomic::AtomicBool, Arc},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

#[inline]
fn send(sender: &TracedSender, batch: Arc<Vec<PacketBatch>>, count: usize) -> Result<(), String> {
    match sender.send(batch) {
        Ok(_) => {
            trace!("Sent batch: {count} received from vortexor successfully");
            Ok(())
        }
        Err(err) => Err(format!("Failed to send batch {count} down {err:?}")),
    }
}

pub struct VortexorReceiverAdapter {
    thread_hdl: JoinHandle<()>,
    receiver: VerifiedPacketReceiver,
}

const MAX_PACKET_BATCH_SIZE: usize = 8;

impl VortexorReceiverAdapter {
    pub fn new(
        sockets: Vec<Arc<UdpSocket>>,
        recv_timeout: Duration,
        packets_sender: TracedSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (batch_sender, batch_receiver) = unbounded();

        let receiver = VerifiedPacketReceiver::new(sockets, &batch_sender, None, exit.clone());

        let thread_hdl = Builder::new()
            .name("vtxRcvAdptr".to_string())
            .spawn(move || {
                if let Err(msg) = Self::recv_send(
                    batch_receiver,
                    recv_timeout,
                    MAX_PACKET_BATCH_SIZE,
                    packets_sender,
                    forward_stage_sender,
                ) {
                    info!("Quitting VortexorReceiverAdapter: {msg}");
                }
            })
            .unwrap();
        Self {
            thread_hdl,
            receiver,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()?;
        self.receiver.join()
    }

    fn recv_send(
        packet_batch_receiver: Receiver<PacketBatch>,
        recv_timeout: Duration,
        batch_size: usize,
        traced_sender: TracedSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Result<(), String> {
        loop {
            match Self::receive_until(packet_batch_receiver.clone(), recv_timeout, batch_size) {
                Ok(packet_batch) => {
                    let count = packet_batch.len();
                    // Send out packet batches
                    if let Some(forward_stage_sender) = &forward_stage_sender {
                        send(&traced_sender, packet_batch.clone(), count)?;
                        // Send out packet batches to forward stage
                        let _ = forward_stage_sender
                            .try_send((packet_batch, false /* reject non-vote */));
                    } else {
                        send(&traced_sender, packet_batch, count)?;
                    }
                }
                Err(err) => match err {
                    RecvTimeoutError::Timeout => {
                        continue;
                    }
                    RecvTimeoutError::Disconnected => {
                        return Err("Disconnected from the input channel".to_string());
                    }
                },
            }
        }
    }

    /// Receives packet batches from VerifiedPacketReceiver with a timeout
    fn receive_until(
        packet_batch_receiver: Receiver<PacketBatch>,
        recv_timeout: Duration,
        batch_size: usize,
    ) -> Result<BankingPacketBatch, RecvTimeoutError> {
        let start = Instant::now();

        let message = packet_batch_receiver.recv_timeout(recv_timeout)?;
        let mut packet_batches = Vec::new();
        packet_batches.push(message);

        while let Ok(message) = packet_batch_receiver.try_recv() {
            packet_batches.push(message);

            if start.elapsed() >= recv_timeout || packet_batches.len() >= batch_size {
                break;
            }
        }

        Ok(Arc::new(packet_batches))
    }
}
