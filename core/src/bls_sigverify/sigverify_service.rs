use {
    crate::bls_sigverify::{sigverifier::BLSSigVerifier, stats::BLSPreVerifyStats},
    agave_votor_messages::consensus_message::ConsensusMessage,
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, TrySendError},
    solana_measure::measure::Measure,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
    },
    solana_streamer::streamer::{self, StreamerError},
    solana_time_utils as timing,
    std::thread::{self, Builder, JoinHandle},
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum BLSSigVerifyServiceError {
    #[error("try_send packet errror {0:?}")]
    TrySendError(String),

    #[error("streamer error")]
    Streamer(#[from] StreamerError),
}

impl From<TrySendError<ConsensusMessage>> for BLSSigVerifyServiceError {
    fn from(e: TrySendError<ConsensusMessage>) -> Self {
        // We don't send TrySendError because it's big
        match e {
            TrySendError::Full(_) => {
                BLSSigVerifyServiceError::TrySendError("channel full".to_string())
            }
            TrySendError::Disconnected(_) => {
                BLSSigVerifyServiceError::TrySendError("channel disconnected".to_string())
            }
        }
    }
}

type Result<T> = std::result::Result<T, BLSSigVerifyServiceError>;

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
        stats: &mut BLSPreVerifyStats,
    ) -> Result<()> {
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
        verifier.verify_and_send_batches(batches, None)?;
        verify_time.stop();

        debug!(
            "@{:?} verifier: done. batches: {} total verify time: {:?} verified: {} v/s {}",
            timing::timestamp(),
            batches_len,
            verify_time.as_ms(),
            num_packets,
            (num_packets as f32 / verify_time.as_s())
        );

        stats.increase_stats(
            &recv_duration,
            &verify_time,
            &dedup_time,
            batches_len,
            num_packets,
            discard_or_dedup_fail,
        );
        Ok(())
    }

    fn verifier_service(
        packet_receiver: Receiver<PacketBatch>,
        mut verifier: BLSSigVerifier,
    ) -> JoinHandle<()> {
        let mut stats = BLSPreVerifyStats::new();
        const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
        const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
        const DEDUPER_NUM_BITS: u64 = 63_999_979;
        Builder::new()
            .name("solAlpnSigVer".to_string())
            .spawn(move || {
                info!("Alpenglow sigverify service started");
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
                                match streamer_error_box {
                                    StreamerError::RecvTimeout(RecvTimeoutError::Disconnected) => {
                                        break
                                    }
                                    StreamerError::RecvTimeout(RecvTimeoutError::Timeout) => (),
                                    _ => error!("{streamer_error_box}"),
                                }
                            }
                            BLSSigVerifyServiceError::TrySendError(e) => {
                                error!("consensus pool receiver: {e}");
                                break;
                            }
                        }
                    }
                    stats.maybe_report();
                }
                info!("Alpenglow sigverify service stopped");
            })
            .unwrap()
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
