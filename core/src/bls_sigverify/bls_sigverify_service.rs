use {
    crate::bls_sigverify::{bls_sigverifier::BLSSigVerifier, stats::BLSPacketStats},
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, TrySendError},
    solana_measure::measure::Measure,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
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

pub struct BLSSigverifyService {
    thread_hdl: JoinHandle<()>,
}

impl BLSSigverifyService {
    pub fn new(packet_receiver: Receiver<PacketBatch>, verifier: BLSSigVerifier) -> Self {
        let thread_hdl = Self::verifier_service(packet_receiver, verifier);
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

        let mut dedup_time = Measure::start("bls_sigverify_dedup_time");
        let discard_or_dedup_fail =
            deduper::dedup_packets_and_count_discards(deduper, &mut batches) as usize;
        dedup_time.stop();

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
            .dedup_packets_pp_us_hist
            .increment(dedup_time.as_us() / (num_packets as u64))
            .unwrap();
        stats.batches_hist.increment(batches_len as u64).unwrap();
        stats.packets_hist.increment(num_packets as u64).unwrap();
        stats.total_batches += batches_len;
        stats.total_packets += num_packets;
        stats.total_dedup += discard_or_dedup_fail;
        stats.total_dedup_time_us += dedup_time.as_us() as usize;
        stats.total_verify_time_us += verify_time.as_us() as usize;

        Ok(())
    }

    fn verifier_service(
        packet_receiver: Receiver<PacketBatch>,
        mut verifier: BLSSigVerifier,
    ) -> JoinHandle<()> {
        let mut stats = BLSPacketStats::default();
        let mut last_print = Instant::now();
        const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
        const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
        const DEDUPER_NUM_BITS: u64 = 63_999_979;
        Builder::new()
            .name("solSigVerAlpenglow".to_string())
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
                        stats.maybe_report();
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
