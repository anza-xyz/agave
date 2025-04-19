use {
    crate::{
        bank::Bank,
        transaction_notification::{TransactionStatusMessage, TransactionStatusSender},
    },
    crossbeam_channel::{Receiver, Sender},
    solana_sdk::clock::Slot,
    std::sync::Arc,
};

#[derive(Clone)]
pub enum BankNotification {
    OptimisticallyConfirmed(Slot),
    Frozen(Arc<Bank>),
    NewRootBank(Arc<Bank>),
    /// The newly rooted slot chain including the parent slot of the oldest bank in the rooted chain.
    NewRootedChain(Vec<Slot>),
}

impl std::fmt::Debug for BankNotification {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            BankNotification::OptimisticallyConfirmed(slot) => {
                write!(f, "OptimisticallyConfirmed({slot:?})")
            }
            BankNotification::Frozen(bank) => write!(f, "Frozen({})", bank.slot()),
            BankNotification::NewRootBank(bank) => write!(f, "Root({})", bank.slot()),
            BankNotification::NewRootedChain(chain) => write!(f, "RootedChain({chain:?})"),
        }
    }
}

pub type BankNotificationReceiver = Receiver<BankNotification>;
pub type BankNotificationSenderDirect = Sender<BankNotification>;

/// Two types of senders for sending bank notifications:
#[derive(Clone)]
pub enum BankNotificationSenderType {
    /// Directly sending bank notfications
    DirectSender(Sender<BankNotification>),
    /// Via the intermdiate transaction status sender so that the
    /// bank notifications can be ordered related to the transaction status
    /// notifcations.
    TransactionStatusSender(TransactionStatusSender),
}

#[derive(Clone)]
pub struct BankNotificationSender(BankNotificationSenderType);

#[derive(Clone)]
pub struct BankNotificationSenderConfig {
    pub sender: BankNotificationSender,
    pub should_send_parents: bool,
}

impl BankNotificationSender {
    pub fn send(&self, notitifcation: BankNotification) -> core::result::Result<(), String> {
        match &self.0 {
            BankNotificationSenderType::DirectSender(sender) => sender
                .send(notitifcation)
                .map_err(|e| format!("Error {e}").to_string()),
            BankNotificationSenderType::TransactionStatusSender(sender) => sender
                .sender
                .send(TransactionStatusMessage::BankEvent(notitifcation))
                .map_err(|e| format!("Error {e}").to_string()),
        }
    }
}

impl BankNotificationSenderConfig {
    pub fn new_direct_sender(
        sender: BankNotificationSenderDirect,
        should_send_parents: bool,
    ) -> Self {
        Self {
            sender: BankNotificationSender(BankNotificationSenderType::DirectSender(sender)),
            should_send_parents,
        }
    }

    pub fn new_indirect_sender(sender: TransactionStatusSender, should_send_parents: bool) -> Self {
        Self {
            sender: BankNotificationSender(BankNotificationSenderType::TransactionStatusSender(
                sender,
            )),
            should_send_parents,
        }
    }

    pub fn send(&self, notitifcation: BankNotification) -> core::result::Result<(), String> {
        self.sender.send(notitifcation)
    }
}
