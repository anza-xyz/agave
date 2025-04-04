use {
    crate::bank::Bank,
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
pub type BankNotificationSender = Sender<BankNotification>;

#[derive(Clone)]
pub struct BankNotificationSenderConfig {
    pub sender: BankNotificationSender,
    pub should_send_parents: bool,
}
