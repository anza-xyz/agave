//! Vortexor receiver adapter which wraps the VerifiedPacketReceiver
//! to receive packet batches from the remote and sends the packets to the
//! banking stage.

use {
    crate::banking_trace::TracedSender,
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError},
    solana_perf::packet::PacketBatch,
    solana_vortexor_receiver::receiver::VerifiedPacketReceiver,
    std::{
        net::UdpSocket,
        sync::{atomic::AtomicBool, Arc},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

pub struct VortexorReceiverAdapter {
    thread_hdl: JoinHandle<()>,
    receiver: VerifiedPacketReceiver,
}

impl VortexorReceiverAdapter {
    pub fn new(
        sockets: Vec<Arc<UdpSocket>>,
        recv_timeout: Duration,
        tpu_coalesce: Duration,
        packets_sender: TracedSender,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (batch_sender, batch_receiver) = unbounded();

        let receiver =
            VerifiedPacketReceiver::new(sockets, &batch_sender, tpu_coalesce, None, exit.clone());

        let thread_hdl = Builder::new()
            .name("vtxRcvAdptr".to_string())
            .spawn(move || {
                Self::recv_send(batch_receiver, recv_timeout, 8, packets_sender);
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
    ) {
        loop {
            match Self::receive_until(packet_batch_receiver.clone(), recv_timeout, batch_size) {
                Ok(packet_batch) => {
                    let count = packet_batch.len();
                    // Send out packet batches
                    match traced_sender.send(packet_batch) {
                        Ok(_) => {
                            trace!("Sent batch: {count} received from vortexor successfully");
                            continue;
                        }
                        Err(err) => {
                            debug!("Failed to send batch {count} {err:?}");
                            break;
                        }
                    }
                }
                Err(err) => match err {
                    RecvTimeoutError::Timeout => {
                        continue;
                    }
                    RecvTimeoutError::Disconnected => {
                        break;
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
