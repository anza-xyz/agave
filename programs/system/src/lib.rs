#![allow(clippy::arithmetic_side_effects)]
pub mod system_instruction;
pub mod system_processor;

pub use system_program::id;
use {
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount},
    solana_nonce as nonce,
    solana_sdk_ids::system_program,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SystemAccountKind {
    System,
    Nonce,
}

pub fn get_system_account_kind(account: &AccountSharedData) -> Option<SystemAccountKind> {
    if system_program::check_id(account.owner()) {
        if account.data().is_empty() {
            Some(SystemAccountKind::System)
        } else if account.data().len() == nonce::state::State::size() {
            let nonce_versions: nonce::versions::Versions = account.state().ok()?;
            match nonce_versions.state() {
                nonce::state::State::Uninitialized => None,
                nonce::state::State::Initialized(_) => Some(SystemAccountKind::Nonce),
            }
        } else {
            None
        }
    } else {
        None
    }
}
