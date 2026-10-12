//! Filters forwarded transaction packets before sending them to sigverify.

use {
    crate::result::{Error, Result},
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    crossbeam_channel::{RecvTimeoutError, TrySendError},
    solana_clock::HOLD_TRANSACTIONS_SLOT_OFFSET,
    solana_packet::PacketFlags,
    solana_perf::packet::BytesPacket,
    solana_poh::poh_recorder::SharedLeaderState,
    solana_streamer::streamer::{PacketBatchReceiver, PacketBatchSender},
    std::{
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

struct ForwardingStats {
    last_report: Instant,
    /// The number of packets sent along to the next stage
    num_packets_sent: usize,
    /// The number of packets discarded because we are not leader soon
    num_packets_discarded: usize,
    /// The number of packets dropped because the channel is already full
    num_packets_dropped: usize,
}

impl ForwardingStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    fn new() -> Self {
        Self {
            last_report: Instant::now(),
            num_packets_sent: 0,
            num_packets_discarded: 0,
            num_packets_dropped: 0,
        }
    }

    fn maybe_report_and_reset(&mut self) {
        if self.last_report.elapsed() < Self::REPORT_INTERVAL {
            return;
        }

        if !self.is_empty() {
            datapoint_info!(
                "fetch_stage-forwards",
                ("num_packets_sent", self.num_packets_sent, i64),
                ("num_packets_discarded", self.num_packets_discarded, i64),
                ("num_packets_dropped", self.num_packets_dropped, i64)
            );
        }

        *self = Self::new();
    }

    fn is_empty(&self) -> bool {
        (self.num_packets_sent + self.num_packets_discarded + self.num_packets_dropped) == 0
    }
}

pub struct FetchStage {
    thread_hdl: JoinHandle<()>,
}

impl FetchStage {
    pub fn new_with_sender(
        sender: &PacketBatchSender,
        forward_receiver: PacketBatchReceiver,
        shared_leader_state: SharedLeaderState,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
    ) -> Self {
        let sender = sender.clone();
        let thread_hdl = Builder::new()
            .name("solFetchStgFwRx".to_string())
            .spawn(move || {
                let mut stats = ForwardingStats::new();
                loop {
                    if let Err(e) = Self::handle_forwarded_packets(
                        &forward_receiver,
                        &sender,
                        &shared_leader_state,
                        &alpenglow_slot_clock,
                        &mut stats,
                    ) {
                        match e {
                            Error::RecvTimeout(RecvTimeoutError::Disconnected) => break,
                            Error::RecvTimeout(RecvTimeoutError::Timeout) => (),
                            Error::Recv(_) | Error::Send => break,
                            _ => error!("{e:?}"),
                        }
                    }
                    stats.maybe_report_and_reset();
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn handle_forwarded_packets(
        recvr: &PacketBatchReceiver,
        sendr: &PacketBatchSender,
        shared_leader_state: &SharedLeaderState,
        alpenglow_slot_clock: &SharedAlpenglowSlotClock,
        stats: &mut ForwardingStats,
    ) -> Result<()> {
        let mark_forwarded = |packet: &mut BytesPacket| {
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

        let leader_state = shared_leader_state.load();
        let would_be_leader = leader_state.working_bank().is_some()
            || leader_state.next_leader_slot_range().is_some_and(
                |(first_leader_slot, last_leader_slot)| {
                    alpenglow_slot_clock
                        .current_slot()
                        .is_some_and(|current_slot| {
                            current_slot <= last_leader_slot
                                && current_slot.saturating_add(HOLD_TRANSACTIONS_SLOT_OFFSET)
                                    >= first_leader_slot
                        })
                },
            );
        if would_be_leader {
            for packet_batch in packet_batches {
                let packets_in_batch = packet_batch.len();
                match sendr.try_send(packet_batch) {
                    Ok(()) => {
                        stats.num_packets_sent += packets_in_batch;
                    }
                    Err(TrySendError::Full(_)) => {
                        stats.num_packets_dropped += packets_in_batch;
                    }
                    Err(TrySendError::Disconnected(_)) => return Err(Error::Send),
                };
            }
        } else {
            stats.num_packets_discarded += num_packets;
        }

        Ok(())
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
