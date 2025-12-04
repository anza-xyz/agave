//! The `poh_service` module implements a service that records the passing of
//! "ticks", a measure of time in the PoH stream
use {
    crate::{
        poh_controller::{PohServiceMessage, PohServiceMessageGuard, PohServiceMessageReceiver},
        poh_recorder::{PohRecorder, Record},
        record_channels::RecordReceiver,
    },
    log::*,
    solana_clock::DEFAULT_HASHES_PER_SECOND,
    solana_entry::poh::Poh,
    solana_measure::measure::Measure,
    solana_poh_config::PohConfig,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

pub struct PohService {
    tick_producer: JoinHandle<()>,
}

// Amount of time to hash continuously.
//
// * If this number is too small, PoH hash rate will suffer.
// * If this number is too large, PoH will be less responsive to record requests.
//
// Can use test_poh_service to calibrate this
const TARGET_HASH_BATCH_TIME_US: u64 = 50;
pub const DEFAULT_HASHES_PER_BATCH: u64 =
    TARGET_HASH_BATCH_TIME_US * DEFAULT_HASHES_PER_SECOND / 1_000_000;

pub const DEFAULT_PINNED_CPU_CORE: usize = 0;

const TARGET_SLOT_ADJUSTMENT_NS: u64 = 50_000_000;

#[derive(Debug)]
struct PohTiming {
    num_ticks: u64,
    num_hashes: u64,
    total_sleep_us: u64,
    total_lock_time_ns: u64,
    total_hash_time_ns: u64,
    total_tick_time_ns: u64,
    last_metric: Instant,
    total_record_time_us: u64,
}

impl PohTiming {
    fn new() -> Self {
        Self {
            num_ticks: 0,
            num_hashes: 0,
            total_sleep_us: 0,
            total_lock_time_ns: 0,
            total_hash_time_ns: 0,
            total_tick_time_ns: 0,
            last_metric: Instant::now(),
            total_record_time_us: 0,
        }
    }
    fn report(&mut self, ticks_per_slot: u64) {
        if self.last_metric.elapsed().as_millis() > 1000 {
            let elapsed_us = self.last_metric.elapsed().as_micros() as u64;
            let us_per_slot = (elapsed_us * ticks_per_slot) / self.num_ticks;
            datapoint_info!(
                "poh-service",
                ("ticks", self.num_ticks as i64, i64),
                ("hashes", self.num_hashes as i64, i64),
                ("elapsed_us", us_per_slot, i64),
                ("total_sleep_us", self.total_sleep_us, i64),
                ("total_tick_time_us", self.total_tick_time_ns / 1000, i64),
                ("total_lock_time_us", self.total_lock_time_ns / 1000, i64),
                ("total_hash_time_us", self.total_hash_time_ns / 1000, i64),
                ("total_record_time_us", self.total_record_time_us, i64),
            );
            self.total_sleep_us = 0;
            self.num_ticks = 0;
            self.num_hashes = 0;
            self.total_tick_time_ns = 0;
            self.total_lock_time_ns = 0;
            self.total_hash_time_ns = 0;
            self.last_metric = Instant::now();
            self.total_record_time_us = 0;
        }
    }
}

impl PohService {
    pub fn new(
        poh_recorder: Arc<RwLock<PohRecorder>>,
        poh_config: &PohConfig,
        poh_exit: Arc<AtomicBool>,
        ticks_per_slot: u64,
        pinned_cpu_core: usize,
        hashes_per_batch: u64,
        record_receiver: RecordReceiver,
        poh_service_receiver: PohServiceMessageReceiver,
    ) -> Self {
        let poh_config = poh_config.clone();
        let tick_producer = Builder::new()
            .name("solPohTickProd".to_string())
            .spawn(move || {
                if poh_config.hashes_per_tick.is_none() {
                    if poh_config.target_tick_count.is_none() {
                        Self::low_power_tick_producer(
                            poh_recorder,
                            &poh_config,
                            &poh_exit,
                            record_receiver,
                            poh_service_receiver,
                            ticks_per_slot,
                        );
                    } else {
                        Self::short_lived_low_power_tick_producer(
                            poh_recorder,
                            &poh_config,
                            &poh_exit,
                            record_receiver,
                            poh_service_receiver,
                            ticks_per_slot,
                        );
                    }
                } else {
                    // PoH service runs in a tight loop, generating hashes as fast as possible.
                    // Let's dedicate one of the CPU cores to this thread so that it can gain
                    // from cache performance.
                    if let Some(cores) = core_affinity::get_core_ids() {
                        core_affinity::set_for_current(cores[pinned_cpu_core]);
                    }
                    Self::tick_producer(
                        poh_recorder,
                        &poh_exit,
                        ticks_per_slot,
                        hashes_per_batch,
                        record_receiver,
                        poh_service_receiver,
                        Self::target_ns_per_tick(
                            ticks_per_slot,
                            poh_config.target_tick_duration.as_nanos() as u64,
                        ),
                    );
                }
                poh_exit.store(true, Ordering::Relaxed);
            })
            .unwrap();

        Self { tick_producer }
    }

    pub fn target_ns_per_tick(ticks_per_slot: u64, target_tick_duration_ns: u64) -> u64 {
        // Account for some extra time outside of PoH generation to account
        // for processing time outside PoH.
        let adjustment_per_tick = if ticks_per_slot > 0 {
            TARGET_SLOT_ADJUSTMENT_NS / ticks_per_slot
        } else {
            0
        };
        target_tick_duration_ns.saturating_sub(adjustment_per_tick)
    }

    fn low_power_tick_producer(
        poh_recorder: Arc<RwLock<PohRecorder>>,
        poh_config: &PohConfig,
        poh_exit: &AtomicBool,
        mut record_receiver: RecordReceiver,
        poh_service_receiver: PohServiceMessageReceiver,
        ticks_per_slot: u64,
    ) {
        let poh = poh_recorder.read().unwrap().poh.clone();
        let mut last_tick = Instant::now();
        let mut should_shutdown_for_test_producers =
            Self::should_shutdown_for_test_producers(&poh_recorder);
        if should_shutdown_for_test_producers {
            record_receiver.shutdown();
        }
        while !poh_exit.load(Ordering::Relaxed) {
            let service_message =
                Self::check_for_service_message(&poh_service_receiver, &mut record_receiver);
            loop {
                let remaining_tick_time = poh_config
                    .target_tick_duration
                    .saturating_sub(last_tick.elapsed());
                Self::read_record_receiver_and_process(
                    &poh_recorder,
                    &mut record_receiver,
                    remaining_tick_time,
                    ticks_per_slot,
                );

                // Only perform the last tick of the slot if there are no records.
                // This ensures we don't tick, ending the slot, and cause recording
                // to fail, unless all records have been processed.
                // If we are on the last tick, the channel is shutdown so no more
                // records can be received - we will just process the ones that
                // have already been received.
                debug_assert!(
                    !should_shutdown_for_test_producers || record_receiver.is_shutdown(),
                    "channel should be shutdown if last tick of slot"
                );
                if remaining_tick_time.is_zero()
                    && (!should_shutdown_for_test_producers || record_receiver.is_safe_to_restart())
                {
                    last_tick = Instant::now();
                    poh_recorder.write().unwrap().tick();

                    should_shutdown_for_test_producers =
                        Self::should_shutdown_for_test_producers(&poh_recorder);
                    if should_shutdown_for_test_producers
                        || record_receiver.should_shutdown(
                            poh.lock().unwrap().remaining_hashes_in_slot(ticks_per_slot),
                            ticks_per_slot,
                        )
                    {
                        record_receiver.shutdown();
                    }

                    // Check if we can break the inner loop to handle a service message.
                    if Self::can_process_service_message(&service_message, &record_receiver) {
                        break;
                    }
                }
            }

            if let Some(service_message) = service_message {
                Self::handle_service_message(&poh_recorder, service_message, &mut record_receiver);
                should_shutdown_for_test_producers =
                    Self::should_shutdown_for_test_producers(&poh_recorder);
                if should_shutdown_for_test_producers {
                    record_receiver.shutdown();
                }
            }
        }

        record_receiver.shutdown();
        while !record_receiver.is_safe_to_restart() {
            Self::read_record_receiver_and_process(
                &poh_recorder,
                &mut record_receiver,
                Duration::ZERO,
                ticks_per_slot,
            );
        }
    }

    pub fn read_record_receiver_and_process(
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        record_receiver: &mut RecordReceiver,
        timeout: Duration,
        ticks_per_slot: u64,
    ) {
        let record = record_receiver.recv_timeout(timeout);
        if let Ok(record) = record {
            match poh_recorder.write().unwrap().record(
                record.bank_id,
                record.mixins,
                record.transaction_batches,
            ) {
                Ok(record_summary) => {
                    if record_receiver
                        .should_shutdown(record_summary.remaining_hashes_in_slot, ticks_per_slot)
                    {
                        record_receiver.shutdown();
                    }
                }
                Err(err) => {
                    panic!("PohRecorder::record failed: {err:?}");
                }
            }
        }
    }

    fn short_lived_low_power_tick_producer(
        poh_recorder: Arc<RwLock<PohRecorder>>,
        poh_config: &PohConfig,
        poh_exit: &AtomicBool,
        mut record_receiver: RecordReceiver,
        poh_service_receiver: PohServiceMessageReceiver,
        ticks_per_slot: u64,
    ) {
        let mut warned = false;
        let mut elapsed_ticks = 0;
        let mut last_tick = Instant::now();
        let num_ticks = poh_config.target_tick_count.unwrap();
        let poh = poh_recorder.read().unwrap().poh.clone();
        let mut should_shutdown_for_test_producers =
            Self::should_shutdown_for_test_producers(&poh_recorder);
        if should_shutdown_for_test_producers {
            record_receiver.shutdown();
        }

        while elapsed_ticks < num_ticks {
            let service_message =
                Self::check_for_service_message(&poh_service_receiver, &mut record_receiver);

            loop {
                let remaining_tick_time = poh_config
                    .target_tick_duration
                    .saturating_sub(last_tick.elapsed());
                Self::read_record_receiver_and_process(
                    &poh_recorder,
                    &mut record_receiver,
                    Duration::from_millis(0),
                    ticks_per_slot,
                );

                // Only perform the last tick of the slot if there are no records.
                // This ensures we don't tick, ending the slot, and cause recording
                // to fail, unless all records have been processed.
                // If we are on the last tick, the channel is shutdown so no more
                // records can be received - we will just process the ones that
                // have already been received.
                debug_assert!(
                    !should_shutdown_for_test_producers || record_receiver.is_shutdown(),
                    "channel should be shutdown if last tick of slot"
                );
                if remaining_tick_time.is_zero()
                    && (!should_shutdown_for_test_producers || record_receiver.is_safe_to_restart())
                {
                    last_tick = Instant::now();
                    poh_recorder.write().unwrap().tick();
                    elapsed_ticks += 1;

                    should_shutdown_for_test_producers =
                        Self::should_shutdown_for_test_producers(&poh_recorder);
                    if should_shutdown_for_test_producers
                        || record_receiver.should_shutdown(
                            poh.lock().unwrap().remaining_hashes_in_slot(ticks_per_slot),
                            ticks_per_slot,
                        )
                    {
                        record_receiver.shutdown();
                    }
                }

                // Check if we can break the inner loop to handle a service message.
                if Self::can_process_service_message(&service_message, &record_receiver) {
                    break;
                }
            }

            if poh_exit.load(Ordering::Relaxed) && !warned {
                warned = true;
                warn!("exit signal is ignored because PohService is scheduled to exit soon");
            }
            if let Some(service_message) = service_message {
                Self::handle_service_message(&poh_recorder, service_message, &mut record_receiver);
                should_shutdown_for_test_producers =
                    Self::should_shutdown_for_test_producers(&poh_recorder);
                if should_shutdown_for_test_producers {
                    record_receiver.shutdown();
                }
            }
        }

        record_receiver.shutdown();
        while !record_receiver.is_safe_to_restart() {
            Self::read_record_receiver_and_process(
                &poh_recorder,
                &mut record_receiver,
                Duration::ZERO,
                ticks_per_slot,
            );
        }
    }

    /// Returns true if the receiver should be shutdown. This is for test variants of the poh service.
    fn should_shutdown_for_test_producers(poh_recorder: &RwLock<PohRecorder>) -> bool {
        let poh_recorder = poh_recorder.read().unwrap();
        poh_recorder
            .bank()
            .map(|bank| bank.max_tick_height().wrapping_sub(1) <= poh_recorder.tick_height())
            .unwrap_or(false)
    }

    // returns true if we need to tick
    fn record_or_hash(
        next_record: &mut Option<Record>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        timing: &mut PohTiming,
        record_receiver: &mut RecordReceiver,
        hashes_per_batch: u64,
        poh: &Arc<Mutex<Poh>>,
        target_ns_per_tick: u64,
        ticks_per_slot: u64,
    ) -> bool {
        match next_record.take() {
            Some(mut record) => {
                // received message to record
                // so, record for as long as we have queued up record requests
                let mut lock_time = Measure::start("lock");
                let mut poh_recorder_l = poh_recorder.write().unwrap();
                lock_time.stop();
                timing.total_lock_time_ns += lock_time.as_ns();
                let mut record_time = Measure::start("record");
                loop {
                    match poh_recorder_l.record(
                        record.bank_id,
                        record.mixins,
                        std::mem::take(&mut record.transaction_batches),
                    ) {
                        Ok(record_summary) => {
                            if record_receiver.should_shutdown(
                                record_summary.remaining_hashes_in_slot,
                                ticks_per_slot,
                            ) {
                                record_receiver.shutdown();
                            }
                        }
                        Err(err) => {
                            panic!("PohRecorder::record failed: {err:?}");
                        }
                    }

                    timing.num_hashes += 1; // note: may have also ticked inside record
                    if let Ok(new_record) = record_receiver.try_recv() {
                        // we already have second request to record, so record again while we still have the mutex
                        record = new_record;
                    } else {
                        break;
                    }
                }
                record_time.stop();
                timing.total_record_time_us += record_time.as_us();
                // PohRecorder.record would have ticked if it needed to, so should_tick will be false
            }
            None => {
                // did not receive instructions to record, so hash until we notice we've been asked to record (or we need to tick) and then remember what to record
                let mut lock_time = Measure::start("lock");
                let mut poh_l = poh.lock().unwrap();
                lock_time.stop();
                timing.total_lock_time_ns += lock_time.as_ns();
                loop {
                    timing.num_hashes += hashes_per_batch;
                    let mut hash_time = Measure::start("hash");
                    let should_tick = poh_l.hash(hashes_per_batch);
                    let ideal_time = poh_l.target_poh_time(target_ns_per_tick);
                    hash_time.stop();

                    // shutdown if another batch would push us over the shutdown threshold.
                    let remaining_hashes_in_slot = poh_l.remaining_hashes_in_slot(ticks_per_slot);
                    let remaining_hashes_after_next_batch =
                        remaining_hashes_in_slot.saturating_sub(hashes_per_batch);
                    if record_receiver
                        .should_shutdown(remaining_hashes_after_next_batch, ticks_per_slot)
                    {
                        record_receiver.shutdown();
                    }

                    timing.total_hash_time_ns += hash_time.as_ns();
                    if should_tick {
                        // nothing else can be done. tick required.
                        return true;
                    }
                    // check to see if a record request has been sent
                    if let Ok(record) = record_receiver.try_recv() {
                        // remember the record we just received as the next record to occur
                        *next_record = Some(record);
                        break;
                    }
                    // check to see if we need to wait to catch up to ideal
                    let wait_start = Instant::now();
                    if ideal_time <= wait_start {
                        // no, keep hashing. We still hold the lock.
                        continue;
                    }

                    // busy wait, polling for new records and after dropping poh lock (reset can occur, for example)
                    drop(poh_l);
                    while ideal_time > Instant::now() {
                        // check to see if a record request has been sent
                        if let Ok(record) = record_receiver.try_recv() {
                            // remember the record we just received as the next record to occur
                            *next_record = Some(record);
                            break;
                        }
                    }
                    timing.total_sleep_us += wait_start.elapsed().as_micros() as u64;
                    break;
                }
            }
        };
        false // should_tick = false for all code that reaches here
    }

    fn tick_producer(
        poh_recorder: Arc<RwLock<PohRecorder>>,
        poh_exit: &AtomicBool,
        ticks_per_slot: u64,
        hashes_per_batch: u64,
        mut record_receiver: RecordReceiver,
        poh_service_receiver: PohServiceMessageReceiver,
        target_ns_per_tick: u64,
    ) {
        let poh = poh_recorder.read().unwrap().poh.clone();
        let mut timing = PohTiming::new();
        let mut next_record = None;
        let mut should_exit = poh_exit.load(Ordering::Relaxed);

        loop {
            // If we should exit, close the channel so no more records are accepted,
            // but we still want to process any pending records.
            // We should **not** however process any service messages once we have detected
            // the exit signal.
            should_exit |= poh_exit.load(Ordering::Relaxed); // once set, stay set.
            if should_exit {
                record_receiver.shutdown();
            }

            let service_message =
                Self::check_for_service_message(&poh_service_receiver, &mut record_receiver);
            loop {
                let should_tick = Self::record_or_hash(
                    &mut next_record,
                    &poh_recorder,
                    &mut timing,
                    &mut record_receiver,
                    hashes_per_batch,
                    &poh,
                    target_ns_per_tick,
                    ticks_per_slot,
                );
                if should_tick {
                    // Lock PohRecorder only for the final hash. record_or_hash will lock PohRecorder for record calls but not for hashing.
                    {
                        let mut lock_time = Measure::start("lock");
                        let mut poh_recorder_l = poh_recorder.write().unwrap();
                        lock_time.stop();
                        timing.total_lock_time_ns += lock_time.as_ns();
                        let mut tick_time = Measure::start("tick");
                        poh_recorder_l.tick();
                        tick_time.stop();
                        timing.total_tick_time_ns += tick_time.as_ns();
                    }
                    timing.num_ticks += 1;

                    timing.report(ticks_per_slot);
                }

                // Check if we can break the inner loop to handle a service message.
                if next_record.is_none()
                    && Self::can_process_service_message(&service_message, &record_receiver)
                {
                    break;
                }
            }

            if let Some(service_message) = service_message {
                if !should_exit {
                    Self::handle_service_message(
                        &poh_recorder,
                        service_message,
                        &mut record_receiver,
                    );
                }
            }

            // If exit signal is set and there are no more records to process, exit.
            if should_exit && record_receiver.is_safe_to_restart() {
                break;
            }
        }
    }

    /// Check for a service message and shutdown the channel if there is one.
    fn check_for_service_message<'a>(
        service_message_receiver: &'a PohServiceMessageReceiver,
        record_receiver: &mut RecordReceiver,
    ) -> Option<PohServiceMessageGuard<'a>> {
        match service_message_receiver.try_recv() {
            Ok(bank_message) => {
                record_receiver.shutdown();
                Some(bank_message)
            }
            Err(_) => None,
        }
    }

    fn handle_service_message(
        poh_recorder: &RwLock<PohRecorder>,
        mut service_message: PohServiceMessageGuard,
        record_receiver: &mut RecordReceiver,
    ) {
        {
            let mut recorder = poh_recorder.write().unwrap();
            match service_message.take() {
                PohServiceMessage::Reset {
                    reset_bank,
                    next_leader_slot,
                } => {
                    recorder.reset(reset_bank, next_leader_slot);
                }
                PohServiceMessage::SetBank { bank } => {
                    let bank_id = bank.bank_id();
                    let bank_max_tick_height = bank.max_tick_height();
                    recorder.set_bank(bank);
                    let should_restart =
                        recorder.tick_height() < bank_max_tick_height.saturating_sub(1);
                    if should_restart {
                        record_receiver.restart(bank_id);
                    }
                }
            }
        }
    }

    /// If we have a service message and there are no more records to process,
    /// we can break inner recording loops and handle the service message.
    /// However, if there are still records to process, we must continue processing
    /// records before handling the service message, to ensure we do not lose
    /// any records.
    fn can_process_service_message(
        service_message: &Option<PohServiceMessageGuard>,
        record_receiver: &RecordReceiver,
    ) -> bool {
        service_message.is_none() || record_receiver.is_safe_to_restart()
    }

    pub fn join(self) -> thread::Result<()> {
        self.tick_producer.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            poh_controller::PohController, poh_recorder::PohRecorderError::MaxHeightReached,
            record_channels::record_channels,
        },
        rand::{rng, Rng},
        solana_clock::{DEFAULT_HASHES_PER_TICK, DEFAULT_MS_PER_SLOT, DEFAULT_TICKS_PER_SLOT},
        solana_hash::Hash,
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_measure::measure::Measure,
        solana_perf::test_tx::test_tx,
        solana_runtime::bank::Bank,
        solana_sha256_hasher::hash,
        solana_transaction::versioned::VersionedTransaction,
        std::{thread::sleep, time::Duration},
    };

    #[test]
    #[ignore]
    fn test_poh_service() {
        agave_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        let hashes_per_tick = Some(DEFAULT_HASHES_PER_TICK);
        genesis_config.poh_config.hashes_per_tick = hashes_per_tick;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let prev_hash = bank.last_blockhash();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        let default_target_tick_duration =
            PohConfig::default().target_tick_duration.as_micros() as u64;
        let target_tick_duration = Duration::from_micros(default_target_tick_duration);
        let poh_config = PohConfig {
            hashes_per_tick,
            target_tick_duration,
            target_tick_count: None,
        };
        let exit = Arc::new(AtomicBool::new(false));

        let ticks_per_slot = bank.ticks_per_slot();
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let blockstore = Arc::new(blockstore);
        // Just set something very far in the future that we won't reach.
        let next_leader_slot = Some((1_000_000, 1_000_000));
        let (poh_recorder, entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            prev_hash,
            bank.clone(),
            next_leader_slot,
            ticks_per_slot,
            blockstore,
            &leader_schedule_cache,
            &poh_config,
            exit.clone(),
        );
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let ticks_per_slot = bank.ticks_per_slot();

        // specify RUN_TIME to run in a benchmark-like mode
        // to calibrate batch size
        let run_time = std::env::var("RUN_TIME")
            .map(|x| x.parse().unwrap())
            .unwrap_or(0);
        let is_test_run = run_time == 0;

        let entry_producer = {
            let poh_recorder = poh_recorder.clone();
            let exit = exit.clone();
            let mut bank = bank.clone();

            Builder::new()
                .name("solPohEntryProd".to_string())
                .spawn(move || {
                    let now = Instant::now();
                    let mut total_us = 0;
                    let mut total_times = 0;
                    let h1 = hash(b"hello world!");
                    let tx = VersionedTransaction::from(test_tx());
                    loop {
                        // send some data
                        let mut time = Measure::start("record");
                        let res = poh_recorder.write().unwrap().record(
                            bank.slot(),
                            vec![h1],
                            vec![vec![tx.clone()]],
                        );
                        if let Err(MaxHeightReached) = res {
                            // Advance to the next slot.
                            poh_recorder
                                .write()
                                .unwrap()
                                .reset(bank.clone(), next_leader_slot);
                            bank = Arc::new(Bank::new_from_parent(
                                bank.clone(),
                                &solana_pubkey::new_rand(),
                                bank.slot() + 1,
                            ));
                            poh_recorder
                                .write()
                                .unwrap()
                                .set_bank_for_test(bank.clone());
                        }
                        time.stop();
                        total_us += time.as_us();
                        total_times += 1;
                        if is_test_run && rng().random_ratio(1, 4) {
                            sleep(Duration::from_millis(200));
                        }

                        if exit.load(Ordering::Relaxed) {
                            info!(
                                "spent:{}ms record: {}ms entries recorded: {}",
                                now.elapsed().as_millis(),
                                total_us / 1000,
                                total_times,
                            );
                            break;
                        }
                    }
                })
                .unwrap()
        };

        let hashes_per_batch = std::env::var("HASHES_PER_BATCH")
            .map(|x| x.parse().unwrap())
            .unwrap_or(DEFAULT_HASHES_PER_BATCH);
        let (_record_sender, record_receiver) = record_channels(false);
        let (_poh_controller, poh_service_message_receiver) = PohController::new();
        let poh_service = PohService::new(
            poh_recorder.clone(),
            &poh_config,
            exit.clone(),
            0,
            DEFAULT_PINNED_CPU_CORE,
            hashes_per_batch,
            record_receiver,
            poh_service_message_receiver,
        );
        poh_recorder.write().unwrap().set_bank_for_test(bank);

        // get some events
        let mut hashes = 0;
        let mut need_tick = true;
        let mut need_entry = true;
        let mut need_partial = true;
        let mut num_ticks = 0;

        let time = Instant::now();
        while run_time != 0 || need_tick || need_entry || need_partial {
            let (_bank, (entry, _tick_height)) = entry_receiver
                .recv_timeout(Duration::from_millis(DEFAULT_MS_PER_SLOT))
                .expect("Expected to receive an entry");

            if entry.is_tick() {
                num_ticks += 1;
                assert!(
                    entry.num_hashes <= poh_config.hashes_per_tick.unwrap(),
                    "{} <= {}",
                    entry.num_hashes,
                    poh_config.hashes_per_tick.unwrap()
                );

                if entry.num_hashes == poh_config.hashes_per_tick.unwrap() {
                    need_tick = false;
                } else {
                    need_partial = false;
                }

                hashes += entry.num_hashes;

                assert_eq!(hashes, poh_config.hashes_per_tick.unwrap());

                hashes = 0;
            } else {
                assert!(entry.num_hashes >= 1);
                need_entry = false;
                hashes += entry.num_hashes;
            }

            if run_time != 0 {
                if time.elapsed().as_millis() > run_time {
                    break;
                }
            } else {
                assert!(
                    time.elapsed().as_secs() < 60,
                    "Test should not run for this long! {}s tick {} entry {} partial {}",
                    time.elapsed().as_secs(),
                    need_tick,
                    need_entry,
                    need_partial,
                );
            }
        }
        info!(
            "target_tick_duration: {} ticks_per_slot: {}",
            poh_config.target_tick_duration.as_nanos(),
            ticks_per_slot
        );
        let elapsed = time.elapsed();
        info!(
            "{} ticks in {}ms {}us/tick",
            num_ticks,
            elapsed.as_millis(),
            elapsed.as_micros() / num_ticks
        );

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
        entry_producer.join().unwrap();
    }

    #[test]
    fn test_poh_service_record_race() {
        agave_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        let hashes_per_tick = Some(DEFAULT_HASHES_PER_TICK);
        genesis_config.poh_config.hashes_per_tick = hashes_per_tick;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let prev_hash = bank.last_blockhash();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        let default_target_tick_duration =
            PohConfig::default().target_tick_duration.as_micros() as u64;
        let target_tick_duration = Duration::from_micros(default_target_tick_duration);
        let poh_config = PohConfig {
            hashes_per_tick,
            target_tick_duration,
            target_tick_count: None,
        };
        let exit = Arc::new(AtomicBool::new(false));

        let ticks_per_slot = bank.ticks_per_slot();
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let blockstore = Arc::new(blockstore);
        let next_leader_slot = Some((1_000_000, 1_000_000));
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            prev_hash,
            bank.clone(),
            next_leader_slot,
            ticks_per_slot,
            blockstore,
            &leader_schedule_cache,
            &poh_config,
            exit.clone(),
        );
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        // Queue a new record & service message at the same time.
        let (record_sender, mut record_receiver) = record_channels(false);
        record_receiver.restart(bank.bank_id());
        let (mut poh_controller, poh_service_message_receiver) = PohController::new();
        poh_controller.reset(bank.clone(), None).unwrap();
        record_sender
            .try_send(Record {
                mixins: vec![Hash::new_unique()],
                transaction_batches: vec![vec![VersionedTransaction::from(test_tx())]],
                bank_id: bank.bank_id(),
            })
            .unwrap();

        // Spawn the poh service.
        poh_recorder.write().unwrap().set_bank_for_test(bank);
        let poh_service = PohService::new(
            poh_recorder.clone(),
            &poh_config,
            exit.clone(),
            DEFAULT_TICKS_PER_SLOT,
            DEFAULT_PINNED_CPU_CORE,
            DEFAULT_HASHES_PER_BATCH,
            record_receiver,
            poh_service_message_receiver,
        );

        // Let poh service process the messages.
        let start = Instant::now();
        while !record_sender.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(1));
        }

        // Shutdown.
        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }
}
