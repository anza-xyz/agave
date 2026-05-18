use {
    crate::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded},
    log::warn,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    std::{
        fmt,
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
    SetRoot {
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
        response_sender: Sender<()>,
    },
    ClearBank {
        slot: Slot,
        response_sender: Sender<()>,
    },
}

impl BankForksCommand {
    fn metric_slot(&self) -> Slot {
        match self {
            Self::InsertBank { bank, .. } => bank.slot(),
            Self::SetRoot { new_root, .. } => *new_root,
            Self::ClearBank { slot, .. } => *slot,
        }
    }
}

impl fmt::Display for BankForksCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsertBank { .. } => write!(f, "insert_bank"),
            Self::SetRoot { .. } => write!(f, "set_root"),
            Self::ClearBank { .. } => write!(f, "clear_bank"),
        }
    }
}

pub trait BankForksController: Send + Sync {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError>;

    fn set_root(
        &self,
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> Result<(), BankForksControllerError>;

    fn clear_bank(&self, slot: Slot) -> Result<(), BankForksControllerError>;
}

/// Handle used by non-replay threads to serialize BankForks writes onto ReplayStage.
#[derive(Clone)]
pub struct BankForksControllerHandle {
    sender: Sender<BankForksCommand>,
}

impl BankForksControllerHandle {
    pub fn new() -> (Self, BankForksCommandReceiver) {
        let (sender, receiver) = bounded(CHANNEL_SIZE);
        (Self { sender }, BankForksCommandReceiver { receiver })
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

    fn set_root(
        &self,
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> Result<(), BankForksControllerError> {
        let (response_sender, response_receiver) = bounded(1);
        self.send_command(
            BankForksCommand::SetRoot {
                my_pubkey,
                parent_slot,
                new_root,
                highest_super_majority_root,
                response_sender,
            },
            response_receiver,
        )
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
}

impl BankForksCommandReceiver {
    pub fn receiver(&self) -> &Receiver<BankForksCommand> {
        &self.receiver
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{bank::SlotLeader, bank_forks::BankForks, genesis_utils::create_genesis_config},
        std::thread,
    };

    #[test]
    fn test_bank_forks_controller_insert_and_set_root() {
        let genesis = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let (controller, receiver) = BankForksControllerHandle::new();
        let replay_bank_forks = bank_forks.clone();
        let replay_thread = thread::spawn(move || {
            loop {
                let Ok(command) = receiver.receiver().recv() else {
                    break;
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
                    BankForksCommand::SetRoot {
                        new_root,
                        response_sender,
                        ..
                    } => {
                        {
                            let mut bank_forks = replay_bank_forks.write().unwrap();
                            bank_forks.set_root(new_root, None, None);
                        }
                        response_sender.send(()).unwrap();
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

        controller.set_root(Pubkey::default(), 1, 1, None).unwrap();
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
