use crate::remote_wallet::RemoteWalletInfo;
use crate::wallet::ledger::ledger::LedgerWallet;
use std::rc::Rc;

#[derive(Debug)]
pub struct Device {
    pub(crate) path: String,
    pub(crate) info: RemoteWalletInfo,
    pub wallet_type: RemoteWalletType,
}

#[derive(Debug)]
pub enum RemoteWalletType {
    Ledger(Rc<LedgerWallet>),
}
