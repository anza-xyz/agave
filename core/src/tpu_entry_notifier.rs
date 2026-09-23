use {
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender, TrySendError},
    solana_clock::BankId,
    solana_entry::{
        block_component::VersionedBlockMarker, entry::EntrySummary,
        recorder_message::RecorderMessage,
    },
    solana_ledger::entry_notifier_service::{
        EntryNotification, EntryNotifierSender, send_entry_notification,
    },
    solana_poh::poh_recorder::WorkingBankMessage,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub(crate) struct TpuEntryNotifier {
    thread_hdl: JoinHandle<()>,
}

/// Try the nonblocking fast path first so saturation is observable, then block
/// rather than dropping a message on its way to BroadcastStage.
fn send_broadcast_message(
    sender: &Sender<WorkingBankMessage>,
    message: WorkingBankMessage,
) -> Result<(), Box<SendError<WorkingBankMessage>>> {
    match sender.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(message)) => {
            log::error!(
                "TPU entry notifier to BroadcastStage channel is full; blocking to preserve the \
                 message"
            );
            sender.send(message).map_err(Box::new)
        }
        Err(TrySendError::Disconnected(message)) => Err(Box::new(SendError(message))),
    }
}

impl TpuEntryNotifier {
    pub(crate) fn new(
        entry_receiver: Receiver<WorkingBankMessage>,
        entry_notification_sender: EntryNotifierSender,
        broadcast_message_sender: Sender<WorkingBankMessage>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solTpuEntry".to_string())
            .spawn(move || {
                let mut current_slot = 0;
                let mut current_bank_id = BankId::default();
                let mut current_index = 0;
                let mut current_transaction_index = 0;
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(RecvTimeoutError::Disconnected) = Self::send_entry_notification(
                        exit.clone(),
                        &entry_receiver,
                        &entry_notification_sender,
                        &broadcast_message_sender,
                        &mut current_slot,
                        &mut current_bank_id,
                        &mut current_index,
                        &mut current_transaction_index,
                    ) {
                        break;
                    }
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    pub(crate) fn send_entry_notification(
        exit: Arc<AtomicBool>,
        entry_receiver: &Receiver<WorkingBankMessage>,
        entry_notification_sender: &EntryNotifierSender,
        broadcast_message_sender: &Sender<WorkingBankMessage>,
        current_slot: &mut u64,
        current_bank_id: &mut BankId,
        current_index: &mut usize,
        current_transaction_index: &mut usize,
    ) -> Result<(), RecvTimeoutError> {
        let (bank, (message, tick_height)) = entry_receiver.recv_timeout(Duration::from_secs(1))?;
        let slot = bank.slot();
        let bank_id = bank.bank_id();
        if slot != *current_slot || bank_id != *current_bank_id {
            *current_index = 0;
            *current_transaction_index = 0;
            *current_slot = slot;
            *current_bank_id = bank_id;
        };
        let index = *current_index;

        match &message {
            RecorderMessage::SlotStart => {}
            RecorderMessage::Entry(entry) => {
                let entry_summary = EntrySummary {
                    num_hashes: entry.num_hashes,
                    hash: entry.hash,
                    num_transactions: entry.transactions.len() as u64,
                };
                if let Err(err) = send_entry_notification(
                    entry_notification_sender,
                    EntryNotification::Entry {
                        slot,
                        bank_id,
                        index,
                        entry: entry_summary,
                        starting_transaction_index: *current_transaction_index,
                    },
                ) {
                    warn!(
                        "Failed to send slot {slot:?} entry {index:?} from Tpu to \
                         EntryNotifierService, error {err:?}",
                    );
                }
                *current_index += 1;
                *current_transaction_index += entry.transactions.len();
            }
            RecorderMessage::Marker(VersionedBlockMarker::V1(marker)) => {
                if let Some(block_footer) = marker.as_block_footer()
                    && let Err(err) = send_entry_notification(
                        entry_notification_sender,
                        EntryNotification::BlockFooter {
                            slot,
                            bank_id,
                            block_footer: Box::new(block_footer.clone()),
                        },
                    )
                {
                    warn!(
                        "Failed to send slot {slot:?} block footer from Tpu to \
                         EntryNotifierService, error {err:?}",
                    );
                }
            }
        }

        if let Err(err) =
            send_broadcast_message(broadcast_message_sender, (bank, (message, tick_height)))
        {
            warn!(
                "Failed to send slot {slot:?} recorder message {index:?} from Tpu to \
                 BroadcastStage, error {err:?}",
            );
            // If the BroadcastStage channel is closed, the validator has halted. Try to exit
            // gracefully.
            exit.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_entry::block_component::{
            BlockFooterV1, VersionedBlockFooter, VersionedBlockMarker,
        },
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_runtime::bank::{Bank, SlotLeader},
    };

    #[test]
    fn test_block_footer_notification_and_forwarding() {
        let (parent, _bank_forks) = Bank::new_with_bank_forks_for_tests(&GenesisConfig::default());
        let bank = Arc::new(Bank::new_from_parent(parent, SlotLeader::default(), 42));
        let slot = bank.slot();
        let bank_id = bank.bank_id();
        let block_footer = BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 1_234_567_890,
            block_user_agent: b"test-validator/1.0".to_vec(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        };
        let expected_block_footer = VersionedBlockFooter::V1(block_footer.clone());
        let marker = VersionedBlockMarker::from_block_footer(block_footer);
        let tick_height = 123;

        let (entry_sender, entry_receiver) = unbounded();
        let (entry_notification_sender, entry_notification_receiver) = unbounded();
        let (broadcast_message_sender, broadcast_message_receiver) = unbounded();
        entry_sender
            .send((
                bank.clone(),
                (RecorderMessage::Marker(marker.clone()), tick_height),
            ))
            .unwrap();

        let mut current_slot = 0;
        let mut current_bank_id = BankId::default();
        let mut current_index = 0;
        let mut current_transaction_index = 0;
        TpuEntryNotifier::send_entry_notification(
            Arc::new(AtomicBool::new(false)),
            &entry_receiver,
            &entry_notification_sender,
            &broadcast_message_sender,
            &mut current_slot,
            &mut current_bank_id,
            &mut current_index,
            &mut current_transaction_index,
        )
        .unwrap();

        let EntryNotification::BlockFooter {
            slot: notified_slot,
            bank_id: notified_bank_id,
            block_footer: notified_block_footer,
        } = entry_notification_receiver.try_recv().unwrap()
        else {
            panic!("expected block footer notification");
        };
        assert_eq!(notified_slot, slot);
        assert_eq!(notified_bank_id, bank_id);
        assert_eq!(*notified_block_footer, expected_block_footer);
        assert!(entry_notification_receiver.try_recv().is_err());

        let (forwarded_bank, (forwarded_message, forwarded_tick_height)) =
            broadcast_message_receiver.try_recv().unwrap();
        assert!(Arc::ptr_eq(&forwarded_bank, &bank));
        assert_eq!(forwarded_tick_height, tick_height);
        let RecorderMessage::Marker(forwarded_marker) = forwarded_message else {
            panic!("expected forwarded block footer marker");
        };
        assert_eq!(forwarded_marker, marker);
        assert!(broadcast_message_receiver.try_recv().is_err());
    }
}
