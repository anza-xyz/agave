use {
    crate::bls_sigverify::{bls_sigverifier::BLSSigVerifier, stats::BLSPacketStats},
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, TrySendError},
    itertools::Itertools,
    solana_measure::measure::Measure,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
        sigverify::{count_discarded_packets, count_packets_in_batches, shrink_batches},
    },
    solana_streamer::streamer::{self, StreamerError},
    solana_time_utils as timing,
    solana_votor_messages::consensus_message::ConsensusMessage,
    std::{
        thread::{self, Builder, JoinHandle},
        time::Instant,
    },
    thiserror::Error,
};

// Try to target 50ms, rough timings from mainnet machines
//
// 50ms/(300ns/packet) = 166666 packets ~ 1300 batches
const BLS_MAX_DEDUP_BATCH: usize = 165_000;

// 50ms/(10us/packet) = 5000 packets
const BLS_MAX_SIGVERIFY_BATCH: usize = 5_000;

// Packet batch shrinker will reorganize packets into compacted batches if 10%
// or more of the packets in a group of packet batches have been discarded.
const BLS_MAX_DISCARDED_PACKET_RATE: f64 = 0.10;

#[derive(Error, Debug)]
pub enum BLSSigVerifyServiceError<SendType> {
    #[error("send packets batch error")]
    Send(Box<SendError<SendType>>),

    #[error("try_send packet errror")]
    TrySend(Box<TrySendError<SendType>>),

    #[error("streamer error")]
    Streamer(Box<StreamerError>),
}

impl<SendType> From<SendError<SendType>> for BLSSigVerifyServiceError<SendType> {
    fn from(e: SendError<SendType>) -> Self {
        Self::Send(Box::new(e))
    }
}

impl<SendType> From<TrySendError<SendType>> for BLSSigVerifyServiceError<SendType> {
    fn from(e: TrySendError<SendType>) -> Self {
        Self::TrySend(Box::new(e))
    }
}

impl<SendType> From<StreamerError> for BLSSigVerifyServiceError<SendType> {
    fn from(e: StreamerError) -> Self {
        Self::Streamer(Box::new(e))
    }
}

type Result<T, SendType> = std::result::Result<T, BLSSigVerifyServiceError<SendType>>;

pub struct BLSSigVerifyStage {
    thread_hdl: JoinHandle<()>,
}

impl BLSSigVerifyStage {
    // TODO(sam): This is inherited from regular sigverify for now
    pub fn discard_excess_packets(batches: &mut [PacketBatch], mut max_packets: usize) {
        // Group packets by their incoming IP address.
        let mut addrs = batches
            .iter_mut()
            .rev()
            .flat_map(|batch| batch.iter_mut().rev())
            .filter(|packet| !packet.meta().discard())
            .map(|packet| (packet.meta().addr, packet))
            .into_group_map();
        // Allocate max_packets evenly across addresses.
        while max_packets > 0 && !addrs.is_empty() {
            let num_addrs = addrs.len();
            addrs.retain(|_, packets| {
                let cap = max_packets.div_ceil(num_addrs);
                max_packets -= packets.len().min(cap);
                packets.truncate(packets.len().saturating_sub(cap));
                !packets.is_empty()
            });
        }
        // Discard excess packets from each address.
        for mut packet in addrs.into_values().flatten() {
            packet.meta_mut().set_discard(true);
        }
    }

    /// make this function public so that it is available for benchmarking
    // TODO(sam): This is inherited from regular sigverify for now
    pub fn maybe_shrink_batches(
        packet_batches: Vec<PacketBatch>,
    ) -> (u64, usize, Vec<PacketBatch>) {
        let mut shrink_time = Measure::start("sigverify_shrink_time");
        let num_packets = count_packets_in_batches(&packet_batches);
        let num_discarded_packets = count_discarded_packets(&packet_batches);
        let pre_packet_batches_len = packet_batches.len();
        let discarded_packet_rate = (num_discarded_packets as f64) / (num_packets as f64);
        let packet_batches = if discarded_packet_rate >= BLS_MAX_DISCARDED_PACKET_RATE {
            shrink_batches(packet_batches)
        } else {
            packet_batches
        };
        let post_packet_batches_len = packet_batches.len();
        let shrink_total = pre_packet_batches_len.saturating_sub(post_packet_batches_len);
        shrink_time.stop();
        (shrink_time.as_us(), shrink_total, packet_batches)
    }

    pub fn new(
        packet_receiver: Receiver<PacketBatch>,
        verifier: BLSSigVerifier,
        thread_name: &'static str,
        metrics_name: &'static str,
    ) -> Self {
        let thread_hdl =
            Self::verifier_service(packet_receiver, verifier, thread_name, metrics_name);
        Self { thread_hdl }
    }

    fn verifier<const K: usize>(
        deduper: &Deduper<K, [u8]>,
        recvr: &Receiver<PacketBatch>,
        verifier: &mut BLSSigVerifier,
        stats: &mut BLSPacketStats,
    ) -> Result<(), ConsensusMessage> {
        let (mut batches, num_packets, recv_duration) = streamer::recv_packet_batches(recvr)?;

        let batches_len = batches.len();
        debug!(
            "@{:?} bls_verifier: verifying: {}",
            timing::timestamp(),
            num_packets,
        );

        let mut discard_random_time = Measure::start("bls_sigverify_discard_random_time");
        let non_discarded_packets = solana_perf::discard::discard_batches_randomly(
            &mut batches,
            BLS_MAX_DEDUP_BATCH,
            num_packets,
        );
        let num_discarded_randomly = num_packets.saturating_sub(non_discarded_packets);
        discard_random_time.stop();

        let mut dedup_time = Measure::start("bls_sigverify_dedup_time");
        let discard_or_dedup_fail =
            deduper::dedup_packets_and_count_discards(deduper, &mut batches) as usize;
        dedup_time.stop();
        let num_unique = non_discarded_packets.saturating_sub(discard_or_dedup_fail);

        let mut discard_time = Measure::start("bls_sigverify_discard_time");
        if num_unique > BLS_MAX_SIGVERIFY_BATCH {
            Self::discard_excess_packets(&mut batches, BLS_MAX_SIGVERIFY_BATCH);
        }
        let excess_fail = num_unique.saturating_sub(BLS_MAX_SIGVERIFY_BATCH);
        discard_time.stop();

        // Pre-shrink packet batches if many packets are discarded from dedup / discard
        let (pre_shrink_time_us, pre_shrink_total, batches) = Self::maybe_shrink_batches(batches);

        let mut verify_time = Measure::start("sigverify_batch_time");
        verifier.verify_and_send_batches(batches)?;
        verify_time.stop();

        debug!(
            "@{:?} verifier: done. batches: {} total verify time: {:?} verified: {} v/s {}",
            timing::timestamp(),
            batches_len,
            verify_time.as_ms(),
            num_packets,
            (num_packets as f32 / verify_time.as_s())
        );

        stats
            .recv_batches_us_hist
            .increment(recv_duration.as_micros() as u64)
            .unwrap();
        stats
            .verify_batches_pp_us_hist
            .increment(verify_time.as_us() / (num_packets as u64))
            .unwrap();
        stats
            .discard_packets_pp_us_hist
            .increment(discard_time.as_us() / (num_packets as u64))
            .unwrap();
        stats
            .dedup_packets_pp_us_hist
            .increment(dedup_time.as_us() / (num_packets as u64))
            .unwrap();
        stats.batches_hist.increment(batches_len as u64).unwrap();
        stats.packets_hist.increment(num_packets as u64).unwrap();
        stats.total_batches += batches_len;
        stats.total_packets += num_packets;
        stats.total_dedup += discard_or_dedup_fail;
        stats.total_discard_random_time_us += discard_random_time.as_us() as usize;
        stats.total_discard_random += num_discarded_randomly;
        stats.total_excess_fail += excess_fail;
        stats.total_shrinks += pre_shrink_total;
        stats.total_dedup_time_us += dedup_time.as_us() as usize;
        stats.total_discard_time_us += discard_time.as_us() as usize;
        stats.total_verify_time_us += verify_time.as_us() as usize;
        stats.total_shrink_time_us += pre_shrink_time_us as usize;

        Ok(())
    }

    fn verifier_service(
        packet_receiver: Receiver<PacketBatch>,
        mut verifier: BLSSigVerifier,
        thread_name: &'static str,
        metrics_name: &'static str,
    ) -> JoinHandle<()> {
        let mut stats = BLSPacketStats::default();
        let mut last_print = Instant::now();
        const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
        const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
        const DEDUPER_NUM_BITS: u64 = 63_999_979;
        Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let mut rng = rand::thread_rng();
                let mut deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
                loop {
                    if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, MAX_DEDUPER_AGE) {
                        stats.num_deduper_saturations += 1;
                    }
                    if let Err(e) =
                        Self::verifier(&deduper, &packet_receiver, &mut verifier, &mut stats)
                    {
                        match e {
                            BLSSigVerifyServiceError::Streamer(streamer_error_box) => {
                                match *streamer_error_box {
                                    StreamerError::RecvTimeout(RecvTimeoutError::Disconnected) => {
                                        break
                                    }
                                    StreamerError::RecvTimeout(RecvTimeoutError::Timeout) => (),
                                    _ => error!("{streamer_error_box}"),
                                }
                            }
                            BLSSigVerifyServiceError::Send(_)
                            | BLSSigVerifyServiceError::TrySend(_) => {
                                break;
                            }
                        }
                    }
                    if last_print.elapsed().as_secs() > 2 {
                        stats.maybe_report(metrics_name);
                        stats = BLSPacketStats::default();
                        last_print = Instant::now();
                    }
                }
            })
            .unwrap()
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
