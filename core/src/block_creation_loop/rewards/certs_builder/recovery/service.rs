//! Keeps recovery admission, verification coordination and selection off the baseline service.

use {
    super::*,
    agave_bls_sigverify::rewards::RewardInput,
    agave_votor_messages::reward_certificate::NUM_SLOTS_FOR_REWARD,
    crossbeam_channel::select_biased,
    solana_runtime::bank::Bank,
    std::sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

const INPUT_CAPACITY: usize = 64;
const FINISH_CAPACITY: usize = 16;

struct Input {
    bank: Arc<Bank>,
    input: RewardInput,
}

struct Finish {
    slot: Slot,
    shred_version: u16,
    baseline: RewardRespSucc,
    cutoff: Instant,
    reply: Sender<RewardRespSucc>,
}

#[derive(Default)]
struct ServiceMetrics {
    dropped_inputs: AtomicU64,
    dropped_finishes: AtomicU64,
}

impl ServiceMetrics {
    fn report(&self) {
        datapoint_info!(
            "reward-certs-recovery-service",
            (
                "dropped_inputs",
                self.dropped_inputs.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "dropped_finishes",
                self.dropped_finishes.swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}

/// A bounded, nonblocking handle. Its controller owns the engine and never builds direct votes.
pub(in crate::block_creation_loop::rewards) struct RecoveryService {
    inputs: Option<Sender<Input>>,
    finishes: Option<Sender<Finish>>,
    /// The first source slot that has not been requested. This also seals dropped finish commands.
    minimum_slot: Arc<AtomicU64>,
    discarded_minimum: Arc<AtomicU64>,
    metrics: Arc<ServiceMetrics>,
    shred_version: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handler: Option<JoinHandle<Recovery>>,
}

impl RecoveryService {
    pub(in crate::block_creation_loop::rewards) fn new(
        recovery: Recovery,
        shred_version: u16,
    ) -> Self {
        Self::spawn(
            recovery,
            shred_version,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(in crate::block_creation_loop::rewards) fn new_with_gates(
        recovery: Recovery,
        shred_version: u16,
        admission_gate: Option<(Sender<()>, Receiver<()>)>,
        selection_gate: Option<(Sender<()>, Receiver<()>)>,
    ) -> Self {
        Self::spawn(recovery, shred_version, admission_gate, selection_gate)
    }

    fn spawn(
        recovery: Recovery,
        shred_version: u16,
        #[cfg(test)] admission_gate: Option<(Sender<()>, Receiver<()>)>,
        #[cfg(test)] selection_gate: Option<(Sender<()>, Receiver<()>)>,
    ) -> Self {
        let (inputs, input_receiver) = bounded(INPUT_CAPACITY);
        let (finishes, finish_receiver) = bounded(FINISH_CAPACITY);
        let minimum_slot = Arc::new(AtomicU64::new(0));
        let discarded_minimum = Arc::new(AtomicU64::new(0));
        let metrics = Arc::new(ServiceMetrics::default());
        let shred_version = Arc::new(AtomicU64::new(u64::from(shred_version)));
        let stop = Arc::new(AtomicBool::new(false));
        let controller = Controller {
            recovery,
            inputs: input_receiver,
            finishes: finish_receiver,
            minimum_slot: minimum_slot.clone(),
            discarded_minimum: discarded_minimum.clone(),
            metrics: metrics.clone(),
            stop: stop.clone(),
            pending: None,
            shred_version: shred_version.clone(),
            last_report: Instant::now(),
            sealed_through: None,
            root_minimum: 0,
            #[cfg(test)]
            admission_gate,
            #[cfg(test)]
            selection_gate,
        };
        let handler = thread::Builder::new()
            .name("solConsRewCtl".into())
            .spawn(move || controller.run())
            .expect("spawn the reward recovery controller");
        Self {
            inputs: Some(inputs),
            finishes: Some(finishes),
            minimum_slot,
            discarded_minimum,
            metrics,
            shred_version,
            stop,
            handler: Some(handler),
        }
    }

    pub(in crate::block_creation_loop::rewards) fn set_shred_version(&self, shred_version: u16) {
        self.shred_version
            .store(u64::from(shred_version), Ordering::Relaxed);
    }

    /// Dropping optional evidence on saturation cannot delay the direct baseline.
    pub(in crate::block_creation_loop::rewards) fn input(
        &self,
        bank: Arc<Bank>,
        input: RewardInput,
    ) {
        if matches!(input, RewardInput::External(_)) {
            return;
        }
        if let Some(inputs) = &self.inputs
            && inputs.try_send(Input { bank, input }).is_err()
        {
            self.metrics.dropped_inputs.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The caller publishes the baseline before making this optional request. A full control queue
    /// drops only the upgrade; the shared watermark still prevents late evidence reopening its slot.
    pub(in crate::block_creation_loop::rewards) fn finish(
        &self,
        slot: Slot,
        shred_version: u16,
        baseline: RewardRespSucc,
        cutoff: Instant,
        reply: Sender<RewardRespSucc>,
    ) {
        self.set_shred_version(shred_version);
        self.minimum_slot
            .fetch_max(slot.saturating_add(1), Ordering::Relaxed);
        if let Some(finishes) = &self.finishes
            && finishes
                .try_send(Finish {
                    slot,
                    shred_version,
                    baseline,
                    cutoff,
                    reply,
                })
                .is_err()
        {
            self.metrics
                .dropped_finishes
                .fetch_add(1, Ordering::Relaxed);
            self.discarded_minimum
                .fetch_max(slot.saturating_add(1), Ordering::Relaxed);
        }
    }

    /// Stops the controller and returns its engine, including any pending verifier job.
    #[cfg(test)]
    pub(in crate::block_creation_loop::rewards) fn join(mut self) -> Recovery {
        self.shutdown().expect("the controller has not been joined")
    }

    fn shutdown(&mut self) -> Option<Recovery> {
        self.stop.store(true, Ordering::Relaxed);
        self.inputs.take();
        self.finishes.take();
        self.handler
            .take()
            .map(|handler| handler.join().expect("reward recovery controller panicked"))
    }
}

impl Drop for RecoveryService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Controller {
    recovery: Recovery,
    inputs: Receiver<Input>,
    finishes: Receiver<Finish>,
    minimum_slot: Arc<AtomicU64>,
    discarded_minimum: Arc<AtomicU64>,
    metrics: Arc<ServiceMetrics>,
    stop: Arc<AtomicBool>,
    pending: Option<(Arc<Bank>, std::vec::IntoIter<Certificate>)>,
    shred_version: Arc<AtomicU64>,
    last_report: Instant,
    sealed_through: Option<Slot>,
    root_minimum: Slot,
    #[cfg(test)]
    admission_gate: Option<(Sender<()>, Receiver<()>)>,
    #[cfg(test)]
    selection_gate: Option<(Sender<()>, Receiver<()>)>,
}

impl Controller {
    fn run(mut self) -> Recovery {
        while !self.stop.load(Ordering::Relaxed) {
            // A cutoff takes priority over queued optional evidence and result collection.
            match self.finishes.try_recv() {
                Ok(finish) => {
                    self.finish(finish);
                    continue;
                }
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => (),
            }
            // Queued successful finishes were consumed first. Dropped commands still close their
            // source slots even if no later evidence arrives to trigger root-based pruning.
            self.recovery
                .retain_from(self.discarded_minimum.load(Ordering::Relaxed));
            self.recovery
                .poll(self.shred_version.load(Ordering::Relaxed) as u16);
            if self.last_report.elapsed() >= Duration::from_secs(1) {
                std::mem::take(&mut self.recovery.metrics).report();
                self.metrics.report();
                self.last_report = Instant::now();
            }
            if let Some((bank, certificates)) = &mut self.pending {
                if let Some(cert) = certificates.next() {
                    let bank = bank.clone();
                    self.certificate(&bank, cert);
                    continue;
                }
                self.pending = None;
            }
            select_biased! {
                recv(self.finishes) -> finish => match finish {
                    Ok(finish) => self.finish(finish),
                    Err(_) => break,
                },
                recv(self.inputs) -> input => match input {
                    Ok(input) => self.input(input),
                    Err(_) => break,
                },
                default(self.recovery.poll_interval()) => (),
            }
        }
        self.recovery.retain_from(self.minimum());
        self.recovery
    }

    fn finish(&mut self, finish: Finish) {
        if self.sealed_through.is_some_and(|slot| finish.slot <= slot) {
            return;
        }
        self.sealed_through = Some(finish.slot);
        #[cfg(test)]
        if let Some((started, release)) = self.selection_gate.take() {
            let _ = started.send(());
            let _ = release.recv();
        }
        self.recovery.collect_ready();
        let response = self.recovery.finish_at(
            finish.slot,
            finish.shred_version,
            finish.baseline,
            finish.cutoff,
        );
        let _ = finish.reply.try_send(response);
    }

    fn input(&mut self, Input { bank, input }: Input) {
        let root_minimum = bank
            .slot()
            .checked_sub(NUM_SLOTS_FOR_REWARD)
            .map_or(0, |slot| slot.saturating_add(1));
        self.root_minimum = self.root_minimum.max(root_minimum);
        self.recovery.retain_from(self.root_minimum);
        match input {
            RewardInput::Certificates(certificates) => {
                self.pending = Some((bank, certificates.into_iter()));
            }
            RewardInput::VoteSignatures { vote, signatures } => {
                if vote.slot() < self.minimum() {
                    self.recovery.metrics.late_signature_components += signatures.len() as u64;
                } else if let Some(rank_map) = bank.get_rank_map(vote.slot()) {
                    self.recovery.add_signatures(vote, signatures, rank_map);
                }
            }
            RewardInput::Own(message) => {
                if message.vote.slot() < self.minimum() {
                    self.recovery.metrics.late_signature_components += 1;
                } else if let Some(rank_map) = bank.get_rank_map(message.vote.slot()) {
                    self.recovery.add_signatures(
                        message.vote,
                        [(message.rank, message.signature.into())],
                        rank_map,
                    );
                }
            }
            RewardInput::External(_) => (),
        }
    }

    fn minimum(&self) -> Slot {
        self.root_minimum
            .max(self.minimum_slot.load(Ordering::Relaxed))
    }

    fn certificate(&mut self, bank: &Bank, certificate: Certificate) {
        if certificate.cert_type.slot() < self.minimum() {
            self.recovery.metrics.late_certificates += 1;
        } else if let Some(rank_map) = bank.get_rank_map(certificate.cert_type.slot()) {
            #[cfg(test)]
            if let Some((started, release)) = self.admission_gate.take() {
                let _ = started.send(());
                let _ = release.recv();
            }
            self.recovery.add_certificate(certificate, rank_map);
        }
    }
}
