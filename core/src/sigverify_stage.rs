//! The `sigverify_stage` implements the signature verification stage of the TPU. It
//! receives a list of lists of packets and outputs the same list, but tags each
//! top-level list with a list of booleans, telling the next stage whether the
//! signature in that packet is valid. It assumes each packet contains one
//! transaction. All processing is done on the CPU by default.

use {
    crate::sigverify,
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError},
    itertools::Itertools,
    rayon::ThreadPool,
    solana_measure::measure::Measure,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
        sigverify::count_valid_packets,
    },
    solana_streamer::streamer::{self, StreamerError},
    solana_time_utils as timing,
    std::{
        sync::Arc,
        thread::{self, Builder, JoinHandle},
        time::Instant,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum SigVerifyServiceError<SendType> {
    #[error("send packets batch error")]
    Send(#[from] SendError<SendType>),

    #[error("streamer error")]
    Streamer(#[from] StreamerError),
}

type Result<T, SendType> = std::result::Result<T, SigVerifyServiceError<SendType>>;

pub struct SigVerifyStage {
    thread_hdl: JoinHandle<()>,
}

pub trait SigVerifier {
    type SendType: std::fmt::Debug;
    fn verify_batches(&self, batches: Vec<PacketBatch>, valid_packets: usize) -> Vec<PacketBatch>;
    fn send_packets(&mut self, packet_batches: Vec<PacketBatch>) -> Result<(), Self::SendType>;
}

#[derive(Clone)]
pub struct DisabledSigVerifier {
    pub thread_pool: Arc<ThreadPool>,
}

#[derive(Default)]
struct SigVerifierStats {
    recv_batches_us_hist: histogram::Histogram, // time to call recv_batch
    verify_batches_pp_us_hist: histogram::Histogram, // per-packet time to call verify_batch
    dedup_packets_pp_us_hist: histogram::Histogram, // per-packet time to call verify_batch
    batches_hist: histogram::Histogram,         // number of packet batches per verify call
    packets_hist: histogram::Histogram,         // number of packets per verify call
    num_deduper_saturations: usize,
    total_batches: usize,
    total_packets: usize,
    total_dedup: usize,
    total_valid_packets: usize,
    total_dedup_time_us: usize,
    total_verify_time_us: usize,
}

impl SigVerifierStats {
    fn maybe_report(&self, name: &'static str) {
        // No need to report a datapoint if no batches/packets received
        if self.total_batches == 0 {
            return;
        }

        datapoint_info!(
            name,
            (
                "recv_batches_us_90pct",
                self.recv_batches_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_min",
                self.recv_batches_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_max",
                self.recv_batches_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_mean",
                self.recv_batches_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_90pct",
                self.verify_batches_pp_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_min",
                self.verify_batches_pp_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_max",
                self.verify_batches_pp_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_mean",
                self.verify_batches_pp_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_90pct",
                self.dedup_packets_pp_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_min",
                self.dedup_packets_pp_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_max",
                self.dedup_packets_pp_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_mean",
                self.dedup_packets_pp_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "batches_90pct",
                self.batches_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("batches_min", self.batches_hist.minimum().unwrap_or(0), i64),
            ("batches_max", self.batches_hist.maximum().unwrap_or(0), i64),
            ("batches_mean", self.batches_hist.mean().unwrap_or(0), i64),
            (
                "packets_90pct",
                self.packets_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("packets_min", self.packets_hist.minimum().unwrap_or(0), i64),
            ("packets_max", self.packets_hist.maximum().unwrap_or(0), i64),
            ("packets_mean", self.packets_hist.mean().unwrap_or(0), i64),
            ("num_deduper_saturations", self.num_deduper_saturations, i64),
            ("total_batches", self.total_batches, i64),
            ("total_packets", self.total_packets, i64),
            ("total_dedup", self.total_dedup, i64),
            ("total_valid_packets", self.total_valid_packets, i64),
            ("total_dedup_time_us", self.total_dedup_time_us, i64),
            ("total_verify_time_us", self.total_verify_time_us, i64),
        );
    }
}

impl SigVerifier for DisabledSigVerifier {
    type SendType = ();
    fn verify_batches(
        &self,
        mut batches: Vec<PacketBatch>,
        _valid_packets: usize,
    ) -> Vec<PacketBatch> {
        sigverify::ed25519_verify_disabled(&self.thread_pool, &mut batches);
        batches
    }

    fn send_packets(&mut self, _packet_batches: Vec<PacketBatch>) -> Result<(), Self::SendType> {
        Ok(())
    }
}

impl SigVerifyStage {
    pub fn new<T: SigVerifier + 'static + Send>(
        packet_receiver: Receiver<PacketBatch>,
        verifier: T,
        thread_name: &'static str,
        metrics_name: &'static str,
    ) -> Self {
        let thread_hdl =
            Self::verifier_service(packet_receiver, verifier, thread_name, metrics_name);
        Self { thread_hdl }
    }

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

    fn verifier<const K: usize, T: SigVerifier>(
        deduper: &Deduper<K, [u8]>,
        recvr: &Receiver<PacketBatch>,
        verifier: &mut T,
        stats: &mut SigVerifierStats,
    ) -> Result<(), T::SendType> {
        const SOFT_RECEIVE_CAP: usize = 5_000;
        let (mut batches, num_packets, recv_duration) =
            streamer::recv_packet_batches(recvr, SOFT_RECEIVE_CAP)?;

        let batches_len = batches.len();
        debug!(
            "@{:?} verifier: verifying: {}",
            timing::timestamp(),
            num_packets,
        );

        let mut dedup_time = Measure::start("sigverify_dedup_time");
        let discard_or_dedup_fail =
            deduper::dedup_packets_and_count_discards(deduper, &mut batches) as usize;
        dedup_time.stop();
        let num_unique = num_packets.saturating_sub(discard_or_dedup_fail);
        let num_packets_to_verify = num_unique;

        let mut verify_time = Measure::start("sigverify_batch_time");
        let batches = verifier.verify_batches(batches, num_packets_to_verify);
        let num_valid_packets = count_valid_packets(&batches);
        verify_time.stop();

        verifier.send_packets(batches)?;

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
            .dedup_packets_pp_us_hist
            .increment(dedup_time.as_us() / (num_packets as u64))
            .unwrap();
        stats.batches_hist.increment(batches_len as u64).unwrap();
        stats.packets_hist.increment(num_packets as u64).unwrap();
        stats.total_batches += batches_len;
        stats.total_packets += num_packets;
        stats.total_dedup += discard_or_dedup_fail;
        stats.total_valid_packets += num_valid_packets;
        stats.total_dedup_time_us += dedup_time.as_us() as usize;
        stats.total_verify_time_us += verify_time.as_us() as usize;

        Ok(())
    }

    fn verifier_service<T: SigVerifier + 'static + Send>(
        packet_receiver: Receiver<PacketBatch>,
        mut verifier: T,
        thread_name: &'static str,
        metrics_name: &'static str,
    ) -> JoinHandle<()> {
        let mut stats = SigVerifierStats::default();
        let mut last_print = Instant::now();
        const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
        const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
        const DEDUPER_NUM_BITS: u64 = 63_999_979;
        Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let mut rng = rand::rng();
                let mut deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
                loop {
                    if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, MAX_DEDUPER_AGE) {
                        stats.num_deduper_saturations += 1;
                    }
                    if let Err(e) =
                        Self::verifier(&deduper, &packet_receiver, &mut verifier, &mut stats)
                    {
                        match e {
                            SigVerifyServiceError::Streamer(StreamerError::RecvTimeout(
                                RecvTimeoutError::Disconnected,
                            )) => break,
                            SigVerifyServiceError::Streamer(StreamerError::RecvTimeout(
                                RecvTimeoutError::Timeout,
                            )) => (),
                            SigVerifyServiceError::Send(_) => {
                                break;
                            }
                            _ => error!("{e:?}"),
                        }
                    }
                    if last_print.elapsed().as_secs() > 2 {
                        stats.maybe_report(metrics_name);
                        stats = SigVerifierStats::default();
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{banking_trace::BankingTracer, sigverify::TransactionSigVerifier},
        crossbeam_channel::unbounded,
        solana_perf::{
            packet::{Packet, RecycledPacketBatch, to_packet_batches},
            sigverify,
            test_tx::test_tx,
        },
        std::sync::Arc,
    };

    fn count_non_discard(packet_batches: &[PacketBatch]) -> usize {
        packet_batches
            .iter()
            .flatten()
            .filter(|p| !p.meta().discard())
            .count()
    }

    #[test]
    fn test_packet_discard() {
        agave_logger::setup();
        let batch_size = 10;
        let mut batch = RecycledPacketBatch::with_capacity(batch_size);
        let packet = Packet::default();
        batch.resize(batch_size, packet);
        batch[3].meta_mut().addr = std::net::IpAddr::from([1u16; 8]);
        batch[3].meta_mut().set_discard(true);
        batch[4].meta_mut().addr = std::net::IpAddr::from([2u16; 8]);
        let mut batches = vec![PacketBatch::from(batch)];
        let max = 3;
        SigVerifyStage::discard_excess_packets(&mut batches, max);
        let total_non_discard = count_non_discard(&batches);
        assert_eq!(total_non_discard, max);
        assert!(!batches[0].get(0).unwrap().meta().discard());
        assert!(batches[0].get(3).unwrap().meta().discard());
        assert!(!batches[0].get(4).unwrap().meta().discard());
    }

    fn gen_batches(
        use_same_tx: bool,
        packets_per_batch: usize,
        total_packets: usize,
    ) -> Vec<PacketBatch> {
        if use_same_tx {
            let tx = test_tx();
            to_packet_batches(&vec![tx; total_packets], packets_per_batch)
        } else {
            let txs: Vec<_> = (0..total_packets).map(|_| test_tx()).collect();
            to_packet_batches(&txs, packets_per_batch)
        }
    }

    #[test]
    fn test_sigverify_stage_with_same_tx() {
        test_sigverify_stage(true)
    }

    #[test]
    fn test_sigverify_stage_without_same_tx() {
        test_sigverify_stage(false)
    }

    fn test_sigverify_stage(use_same_tx: bool) {
        agave_logger::setup();
        trace!("start");
        let (packet_s, packet_r) = unbounded();
        let (verified_s, verified_r) = BankingTracer::channel_for_test();
        let threadpool = Arc::new(sigverify::threadpool_for_tests());
        let verifier = TransactionSigVerifier::new(threadpool, verified_s, None);
        let stage = SigVerifyStage::new(packet_r, verifier, "solSigVerTest", "test");

        let now = Instant::now();
        let packets_per_batch = 128;
        let total_packets = 1920;

        let batches = gen_batches(use_same_tx, packets_per_batch, total_packets);
        trace!(
            "starting... generation took: {} ms batches: {}",
            now.elapsed().as_millis(),
            batches.len()
        );

        let mut sent_len = 0;
        for batch in batches.into_iter() {
            sent_len += batch.len();
            assert_eq!(batch.len(), packets_per_batch);
            packet_s.send(batch).unwrap();
        }
        let mut packet_s = Some(packet_s);
        let mut valid_received = 0;
        trace!("sent: {sent_len}");
        loop {
            if let Ok(verifieds) = verified_r.recv() {
                valid_received += verifieds
                    .iter()
                    .map(|batch| batch.iter().filter(|p| !p.meta().discard()).count())
                    .sum::<usize>();
            } else {
                break;
            }

            // Check if all the sent batches have been picked up by sigverify stage.
            // Drop sender to exit the loop on next receive call, once the channel is
            // drained.
            if packet_s.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                packet_s.take();
            }
        }
        trace!("received: {valid_received}");

        if use_same_tx {
            assert_eq!(valid_received, 1);
        } else {
            assert_eq!(valid_received, total_packets);
        }
        stage.join().unwrap();
    }
}
