use {
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_runtime::installed_scheduler_pool::{BankWithScheduler, DropBankRequest},
    std::{
        collections::HashSet,
        thread::{self, Builder, JoinHandle},
    },
};

pub struct DropBankService {
    thread_hdl: JoinHandle<()>,
}

impl DropBankService {
    pub fn new(bank_receiver: Receiver<DropBankRequest>) -> Self {
        let thread_hdl = Builder::new()
            .name("solDropBankSrvc".to_string())
            .spawn(move || {
                let mut pending_banks: Vec<BankWithScheduler> = Vec::new();
                for request in bank_receiver.iter() {
                    match request {
                        DropBankRequest::DropBanks { banks, new_root } => {
                            // Once the root reaches a pending bank's slot, that numeric slot can no
                            // longer be replayed. Release it without retiring its account state.
                            pending_banks.extend(banks);
                            Self::drop_banks(
                                pending_banks
                                    .extract_if(.., |bank| bank.slot() <= new_root)
                                    .collect(),
                            );

                            // An unfrozen bank published for transaction production can still
                            // receive legacy BankingStage commits after removal from BankForks.
                            // Keep it and all of its removed ancestors alive; the producer can
                            // still load account state inherited from those ancestors.
                            Self::retire_banks(Self::extract_unprotected_banks(&mut pending_banks));
                        }
                        DropBankRequest::Flush { slots, ack_sender } => {
                            let mut banks_to_retire = pending_banks
                                .extract_if(.., |bank| Self::matches_slots(bank, &slots))
                                .collect::<Vec<_>>();
                            banks_to_retire
                                .extend(Self::extract_unprotected_banks(&mut pending_banks));

                            // Retire the whole dependency group in one AccountsDb batch after all
                            // execution and legacy commits have quiesced.
                            Self::retire_banks(banks_to_retire);

                            // FIFO channel ordering guarantees all matching retirement requests
                            // have completed before this acknowledgement is sent.
                            let _ = ack_sender.send(());
                        }
                        DropBankRequest::Barrier { slots, ack_sender } => {
                            // Re-evaluate the dependency closure because producer banks can freeze
                            // while pending. Retire everything that is no longer protected before
                            // reporting whether a matching live producer group remains.
                            Self::retire_banks(Self::extract_unprotected_banks(&mut pending_banks));

                            let has_matching_pending = pending_banks
                                .iter()
                                .any(|bank| Self::matches_slots(bank, &slots));
                            let _ = ack_sender.send(has_matching_pending);
                        }
                    }
                }

                // Channel closure is shutdown, not proof that producer ingress has quiesced. Drop
                // the service-owned references without explicitly retiring protected account
                // state; any remaining Bank owners keep the generation alive until they stop.
                Self::drop_banks(pending_banks);
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn retire_banks(banks: Vec<BankWithScheduler>) {
        if banks.is_empty() {
            return;
        }

        // Waiting for both execution paths is required for unfrozen leader banks: unified
        // scheduler completion alone does not fence legacy BankingStage commits.
        for bank in &banks {
            let _ = bank.wait_for_completed_scheduler();
            bank.wait_for_inflight_commits();
        }

        let unrooted_slot_bank_ids = banks
            .iter()
            .map(|bank| (bank.slot(), bank.bank_id()))
            .collect::<Vec<_>>();
        banks[0].remove_unrooted_slots(&unrooted_slot_bank_ids);

        for bank in &banks {
            bank.clear_slot_signatures(bank.slot());
            bank.prune_program_cache_by_deployment_slot(bank.slot());
        }
        Self::drop_banks(banks);
    }

    fn extract_unprotected_banks(banks: &mut Vec<BankWithScheduler>) -> Vec<BankWithScheduler> {
        let mut protected_slots = HashSet::new();
        for bank in banks
            .iter()
            .filter(|bank| bank.is_transaction_producer() && !bank.is_frozen())
        {
            protected_slots.insert(bank.slot());
            protected_slots.extend(bank.ancestors.iter());
        }

        banks
            .extract_if(.., |bank| !protected_slots.contains(&bank.slot()))
            .collect()
    }

    fn matches_slots(bank: &BankWithScheduler, slots: &[Slot]) -> bool {
        slots
            .iter()
            .any(|slot| *slot == bank.slot() || bank.ancestors.contains_key(slot))
    }

    fn drop_banks(banks: Vec<BankWithScheduler>) {
        let len = banks.len();
        let mut dropped_banks_time = Measure::start("drop_banks");

        // Drop BankWithScheduler with no alive lock to avoid deadlocks. That's because
        // BankWithScheduler::drop() could block on transaction execution if unified scheduler is
        // installed. As historical context, it was dropped early inside ReplayStage rather than
        // here, which caused a deadlock for BankForks.
        drop(banks);
        dropped_banks_time.stop();
        if dropped_banks_time.as_ms() > 10 {
            datapoint_info!(
                "handle_new_root-dropped_banks",
                ("elapsed_ms", dropped_banks_time.as_ms(), i64),
                ("len", len, i64)
            );
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
