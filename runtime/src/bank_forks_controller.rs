use {
    crate::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded},
    log::warn,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::{
        fmt,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

const CHANNEL_SIZE: usize = 16;

#[derive(Debug, Error)]
pub enum BankForksControllerError {
    #[error("bank forks controller is disconnected")]
    Disconnected,
    #[error("bank to insert for slot {0} was stale, failed to insert")]
    UnableToInsertStaleBank(Slot),
}

pub enum BankForksCommand {
    InsertBank {
        bank: Box<Bank>,
        response_sender: Sender<Option<BankWithScheduler>>,
    },
    ClearBank {
        slot: Slot,
        response_sender: Sender<()>,
    },
}

pub struct SetRootCommand {
    pub parent_slot: Slot,
    pub new_root: Slot,
    pub highest_super_majority_root: Option<Slot>,
}

impl BankForksCommand {
    fn metric_slot(&self) -> Slot {
        match self {
            Self::InsertBank { bank, .. } => bank.slot(),
            Self::ClearBank { slot, .. } => *slot,
        }
    }
}

impl fmt::Display for BankForksCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsertBank { .. } => write!(f, "insert_bank"),
            Self::ClearBank { .. } => write!(f, "clear_bank"),
        }
    }
}

pub trait BankForksController: Send + Sync {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError>;

    fn enqueue_set_root(
        &self,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    );

    fn clear_bank(&self, slot: Slot) -> Result<(), BankForksControllerError>;
}

/// Handle used by non-replay threads to serialize BankForks writes onto ReplayStage.
#[derive(Clone)]
pub struct BankForksControllerHandle {
    sender: Sender<BankForksCommand>,
    pending_set_root: Arc<Mutex<Option<SetRootCommand>>>,
    set_root_signal_sender: Sender<()>,
}

impl BankForksControllerHandle {
    pub fn new() -> (Self, BankForksCommandReceiver) {
        let (sender, receiver) = bounded(CHANNEL_SIZE);
        let (set_root_signal_sender, set_root_signal_receiver) = bounded(1);
        let pending_set_root = Arc::new(Mutex::new(None));
        (
            Self {
                sender,
                pending_set_root: pending_set_root.clone(),
                set_root_signal_sender,
            },
            BankForksCommandReceiver {
                receiver,
                pending_set_root,
                set_root_signal_receiver,
            },
        )
    }

    fn send_command<T>(
        &self,
        command: BankForksCommand,
        response_receiver: Receiver<T>,
    ) -> Result<T, BankForksControllerError> {
        let command_name = command.to_string();
        let slot = command.metric_slot();
        let queue_len_before_send = self.sender.len();
        let total_start = Instant::now();
        let send_start = Instant::now();
        if self.sender.send(command).is_err() {
            return Err(BankForksControllerError::Disconnected);
        }
        let send_us = send_start.elapsed().as_micros() as i64;

        let response_wait_start = Instant::now();
        let response = loop {
            match response_receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(response) => break response,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(BankForksControllerError::Disconnected);
                }
                Err(RecvTimeoutError::Timeout) => (),
            }
            warn!(
                "Replay is stuck, waiting for {}ms no response to {command_name} for {slot}",
                response_wait_start.elapsed().as_millis()
            );
        };

        let response_wait_us = response_wait_start.elapsed().as_micros() as i64;
        let total_us = total_start.elapsed().as_micros() as i64;
        datapoint_info!(
            "bank_forks_controller-command",
            ("command", command_name, String),
            ("slot", slot as i64, i64),
            ("queue_len_before_send", queue_len_before_send as i64, i64),
            ("send_us", send_us, i64),
            ("response_wait_us", response_wait_us, i64),
            ("total_us", total_us, i64),
        );

        Ok(response)
    }
}

impl BankForksController for BankForksControllerHandle {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError> {
        let slot = bank.slot();
        let (response_sender, response_receiver) = bounded(1);
        let bank = self.send_command(
            BankForksCommand::InsertBank {
                bank: Box::new(bank),
                response_sender,
            },
            response_receiver,
        )?;
        bank.ok_or(BankForksControllerError::UnableToInsertStaleBank(slot))
    }

    fn enqueue_set_root(
        &self,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) {
        let total_start = Instant::now();
        let command = SetRootCommand {
            parent_slot,
            new_root,
            highest_super_majority_root,
        };

        {
            let mut pending_set_root = self.pending_set_root.lock().unwrap();
            // Replay only needs to process the highest pending root.
            if pending_set_root
                .as_ref()
                .is_none_or(|pending| command.new_root > pending.new_root)
            {
                *pending_set_root = Some(command);
            }
        }
        let _ = self.set_root_signal_sender.try_send(());

        let total_us = total_start.elapsed().as_micros() as i64;

        datapoint_info!(
            "bank_forks_controller-command",
            ("command", "set_root", String),
            ("slot", new_root as i64, i64),
            ("total_us", total_us, i64),
        );
    }

    fn clear_bank(&self, slot: Slot) -> Result<(), BankForksControllerError> {
        let (response_sender, response_receiver) = bounded(1);
        self.send_command(
            BankForksCommand::ClearBank {
                slot,
                response_sender,
            },
            response_receiver,
        )
    }
}

pub struct BankForksCommandReceiver {
    receiver: Receiver<BankForksCommand>,
    pending_set_root: Arc<Mutex<Option<SetRootCommand>>>,
    set_root_signal_receiver: Receiver<()>,
}

impl BankForksCommandReceiver {
    pub fn receiver(&self) -> &Receiver<BankForksCommand> {
        &self.receiver
    }

    pub fn set_root_signal_receiver(&self) -> &Receiver<()> {
        &self.set_root_signal_receiver
    }

    pub fn take_set_root_command(&self) -> Option<SetRootCommand> {
        self.pending_set_root.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{bank::SlotLeader, bank_forks::BankForks, genesis_utils::create_genesis_config},
        std::{thread, time::Duration},
    };

    #[test]
    fn test_bank_forks_controller_keeps_highest_pending_set_root() {
        let (controller, receiver) = BankForksControllerHandle::new();

        controller.enqueue_set_root(5, 5, Some(5));
        controller.enqueue_set_root(3, 3, Some(3));
        assert_eq!(receiver.take_set_root_command().unwrap().new_root, 5);
        assert!(receiver.take_set_root_command().is_none());

        controller.enqueue_set_root(3, 3, Some(3));
        controller.enqueue_set_root(5, 5, Some(5));
        assert_eq!(receiver.take_set_root_command().unwrap().new_root, 5);
    }

    #[test]
    fn test_bank_forks_controller_signals_pending_set_root() {
        let (controller, receiver) = BankForksControllerHandle::new();

        controller.enqueue_set_root(1, 1, Some(1));
        receiver
            .set_root_signal_receiver()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(receiver.take_set_root_command().unwrap().new_root, 1);

        controller.enqueue_set_root(2, 2, Some(2));
        controller.enqueue_set_root(3, 3, Some(3));
        receiver
            .set_root_signal_receiver()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(receiver.set_root_signal_receiver().try_recv().is_err());
        assert_eq!(receiver.take_set_root_command().unwrap().new_root, 3);
    }

    #[test]
    fn test_bank_forks_controller_insert_and_set_root() {
        let genesis = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let (controller, receiver) = BankForksControllerHandle::new();
        let replay_bank_forks = bank_forks.clone();
        let (root_sender, root_receiver) = bounded(1);
        let replay_thread = thread::spawn(move || {
            loop {
                if let Some(command) = receiver.take_set_root_command() {
                    {
                        let mut bank_forks = replay_bank_forks.write().unwrap();
                        bank_forks.set_root(command.new_root, None, None);
                    }
                    root_sender.send(command.new_root).unwrap();
                }
                let command = match receiver.receiver().recv_timeout(Duration::from_millis(10)) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                match command {
                    BankForksCommand::InsertBank {
                        bank,
                        response_sender,
                    } => {
                        let bank = {
                            let mut bank_forks = replay_bank_forks.write().unwrap();
                            bank_forks.insert(*bank)
                        };
                        response_sender.send(Some(bank)).unwrap();
                    }
                    BankForksCommand::ClearBank {
                        slot,
                        response_sender,
                    } => {
                        let bank_to_clear =
                            replay_bank_forks.read().unwrap().get_with_scheduler(slot);
                        if let Some(bank) = bank_to_clear {
                            let _ = bank.wait_for_completed_scheduler();
                        }

                        {
                            let mut bank_forks = replay_bank_forks.write().unwrap();
                            bank_forks.clear_bank(slot, false);
                        }
                        response_sender.send(()).unwrap();
                    }
                }
            }
        });

        let parent_bank = bank_forks.read().unwrap().root_bank();
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), 1);
        bank.freeze();
        let inserted_bank = controller.insert_bank(bank).unwrap();
        assert_eq!(inserted_bank.slot(), 1);
        assert!(bank_forks.read().unwrap().get(1).is_some());

        controller.enqueue_set_root(1, 1, None);
        assert_eq!(root_receiver.recv().unwrap(), 1);
        assert_eq!(bank_forks.read().unwrap().root(), 1);

        let parent_bank = bank_forks.read().unwrap().root_bank();
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), 2);
        bank.freeze();
        let inserted_bank = controller.insert_bank(bank).unwrap();
        assert_eq!(inserted_bank.slot(), 2);
        assert!(bank_forks.read().unwrap().get(2).is_some());

        controller.clear_bank(2).unwrap();
        assert!(bank_forks.read().unwrap().get(2).is_none());

        drop(controller);
        replay_thread.join().unwrap();
    }
}
