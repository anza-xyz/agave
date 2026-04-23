//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

pub use solana_perf::sigverify::{count_packets_in_batches, ed25519_verify};
use {
    crate::{banking_trace::BankingPacketSender, sigverify_stage::SigVerifyServiceError},
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded},
    solana_measure::measure::Measure,
    solana_perf::{packet::PacketBatch, sigverify},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::{Builder, JoinHandle},
    },
};

pub struct TransactionSigVerifier {
    exit: Arc<AtomicBool>,
    worker_sender: Sender<VerifyTask>,
    worker_hdls: Vec<JoinHandle<()>>,
    worker_count: usize,
}

struct VerifyTask {
    batch: PacketBatch,
    valid_packets: usize,
    in_flight_count: Arc<AtomicUsize>,
    total_valid_packets: Arc<AtomicUsize>,
    total_verify_time_us: Arc<AtomicUsize>,
}

const CAPACITY_PER_THREAD: usize = {
    15_000 // ~15k packets per second throughput
    * 2 // 2 seconds worth
};

impl TransactionSigVerifier {
    pub fn new_reject_non_vote(
        thread_pool: Arc<rayon::ThreadPool>,
        packet_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Self {
        Self::new_with_mode(thread_pool, packet_sender, forward_stage_sender, true)
    }

    pub fn new(
        thread_pool: Arc<rayon::ThreadPool>,
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Self {
        Self::new_with_mode(
            thread_pool,
            banking_stage_sender,
            forward_stage_sender,
            false,
        )
    }

    fn new_with_mode(
        thread_pool: Arc<rayon::ThreadPool>,
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        reject_non_vote: bool,
    ) -> Self {
        let worker_count = thread_pool.current_num_threads();
        let exit = Arc::new(AtomicBool::new(false));
        let (worker_sender, worker_receiver) =
            bounded(worker_count.saturating_mul(CAPACITY_PER_THREAD));
        let worker_hdls = Self::spawn_workers(
            worker_count,
            worker_receiver,
            banking_stage_sender.clone(),
            forward_stage_sender.clone(),
            exit.clone(),
            reject_non_vote,
        );
        Self {
            exit,
            worker_sender,
            worker_hdls,
            worker_count,
        }
    }

    fn spawn_workers(
        worker_count: usize,
        worker_receiver: Receiver<VerifyTask>,
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        exit: Arc<AtomicBool>,
        reject_non_vote: bool,
    ) -> Vec<JoinHandle<()>> {
        (0..worker_count)
            .map(|i| {
                let worker_receiver = worker_receiver.clone();
                let banking_stage_sender = banking_stage_sender.clone();
                let forward_stage_sender = forward_stage_sender.clone();
                let exit = exit.clone();
                Builder::new()
                    .name(format!(
                        "{}{i:02}",
                        if reject_non_vote {
                            "solSigVerVote"
                        } else {
                            "solSigVer"
                        }
                    ))
                    .spawn(move || {
                        while let Ok(task) = worker_receiver.recv() {
                            if exit.load(Ordering::Acquire) {
                                break;
                            }
                            Self::run_task(
                                task,
                                &banking_stage_sender,
                                &forward_stage_sender,
                                reject_non_vote,
                            );
                        }
                    })
                    .expect("new sigverify worker thread")
            })
            .collect()
    }

    fn run_task(
        task: VerifyTask,
        banking_stage_sender: &BankingPacketSender,
        forward_stage_sender: &Option<Sender<(BankingPacketBatch, bool)>>,
        reject_non_vote: bool,
    ) {
        let VerifyTask {
            mut batch,
            valid_packets,
            in_flight_count,
            total_valid_packets,
            total_verify_time_us,
            ..
        } = task;

        let mut verify_time = Measure::start("sigverify_batch_time");
        sigverify::ed25519_verify_serial(&mut batch, reject_non_vote, valid_packets);
        verify_time.stop();
        let num_valid_packets = sigverify::count_valid_packets(&batch);

        let banking_packet_batch = BankingPacketBatch::new(vec![batch]);
        if let Some(forward_stage_sender) = forward_stage_sender {
            if let Err(err) = banking_stage_sender.send(banking_packet_batch.clone()) {
                error!("sigverify send failed: {err:?}");
                in_flight_count.fetch_sub(valid_packets, Ordering::Release);
                return;
            }
            if let Err(TrySendError::Full(_)) =
                forward_stage_sender.try_send((banking_packet_batch, reject_non_vote))
            {
                warn!("forwarding stage channel is full, dropping packets.");
            }
        } else if let Err(err) = banking_stage_sender.send(banking_packet_batch) {
            error!("sigverify send failed: {err:?}");
            in_flight_count.fetch_sub(valid_packets, Ordering::Release);
            return;
        }

        total_valid_packets.fetch_add(num_valid_packets, Ordering::Relaxed);
        total_verify_time_us.fetch_add(verify_time.as_us() as usize, Ordering::Relaxed);
        in_flight_count.fetch_sub(valid_packets, Ordering::Release);
    }

    pub(crate) fn verify_and_send_packets(
        &mut self,
        batches: Vec<PacketBatch>,
        in_flight_count: Arc<AtomicUsize>,
        total_valid_packets: Arc<AtomicUsize>,
        total_verify_time_us: Arc<AtomicUsize>,
    ) -> Result<(), SigVerifyServiceError> {
        for batch in batches {
            let valid_packets = batch
                .iter()
                .filter(|packet| !packet.meta().discard())
                .count();
            in_flight_count.fetch_add(valid_packets, Ordering::Release);

            let task = VerifyTask {
                batch,
                valid_packets,
                in_flight_count: in_flight_count.clone(),
                total_valid_packets: total_valid_packets.clone(),
                total_verify_time_us: total_verify_time_us.clone(),
            };
            if self.worker_sender.send(task).is_err() {
                error!("sigverify worker queue closed unexpectedly");
                in_flight_count.fetch_sub(valid_packets, Ordering::Release);
            }
        }

        Ok(())
    }

    pub(crate) fn capacity(&self) -> usize {
        self.worker_count.saturating_mul(CAPACITY_PER_THREAD)
    }
}

impl Drop for TransactionSigVerifier {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Release);
        for _ in 0..self.worker_count {
            let _ = self.worker_sender.send(VerifyTask {
                batch: PacketBatch::from(solana_perf::packet::BytesPacketBatch::default()),
                valid_packets: 0,
                in_flight_count: Arc::new(AtomicUsize::new(0)),
                total_valid_packets: Arc::new(AtomicUsize::new(0)),
                total_verify_time_us: Arc::new(AtomicUsize::new(0)),
            });
        }
        for worker_hdl in self.worker_hdls.drain(..) {
            if let Err(err) = worker_hdl.join() {
                let _ = err;
                debug!("sigverify worker thread exited with panic");
            }
        }
    }
}
