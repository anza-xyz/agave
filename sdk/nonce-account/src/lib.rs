//! Functions related to nonce accounts.

use {
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount},
    solana_hash::Hash,
    solana_nonce::{
        state::{Data, State},
        versions::Versions,
    },
    solana_sdk_ids::system_program,
    std::cell::RefCell,
};

pub fn create_account(lamports: u64) -> RefCell<AccountSharedData> {
    RefCell::new(
        AccountSharedData::new_data_with_space(
            lamports,
            &Versions::new(State::Uninitialized),
            State::size(),
            &system_program::id(),
        )
        .expect("nonce_account"),
    )
}

/// Checks if the recent_blockhash field in Transaction verifies, and returns
/// nonce account data if so.
pub fn verify_nonce_account(
    account: &AccountSharedData,
    recent_blockhash: &Hash, // Transaction.message.recent_blockhash
) -> Option<Data> {
    (account.owner() == &system_program::id())
        .then(|| {
            StateMut::<Versions>::state(account)
                .ok()?
                .verify_recent_blockhash(recent_blockhash)
                .cloned()
        })
        .flatten()
}

pub fn lamports_per_signature_of(account: &AccountSharedData) -> Option<u64> {
    match StateMut::<Versions>::state(account).ok()?.state() {
        State::Initialized(data) => Some(data.fee_calculator.lamports_per_signature),
        State::Uninitialized => None,
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SystemAccountKind {
    System,
    Nonce,
}

pub fn get_system_account_kind(account: &AccountSharedData) -> Option<SystemAccountKind> {
    if system_program::check_id(account.owner()) {
        if account.data().is_empty() {
            Some(SystemAccountKind::System)
        } else if account.data().len() == State::size() {
            let nonce_versions: Versions = account.state().ok()?;
            match nonce_versions.state() {
                State::Uninitialized => None,
                State::Initialized(_) => Some(SystemAccountKind::Nonce),
            }
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_fee_calculator::FeeCalculator,
        solana_nonce::state::{Data, DurableNonce},
        solana_pubkey::Pubkey,
    };

    #[test]
    fn test_verify_bad_account_owner_fails() {
        let program_id = Pubkey::new_unique();
        assert_ne!(program_id, system_program::id());
        let account = AccountSharedData::new_data_with_space(
            42,
            &Versions::new(State::Uninitialized),
            State::size(),
            &program_id,
        )
        .expect("nonce_account");
        assert_eq!(verify_nonce_account(&account, &Hash::default()), None);
    }

    fn new_nonce_account(versions: Versions) -> AccountSharedData {
        AccountSharedData::new_data(
            1_000_000,             // lamports
            &versions,             // state
            &system_program::id(), // owner
        )
        .unwrap()
    }

    #[test]
    fn test_verify_nonce_account() {
        let blockhash = Hash::from([171; 32]);
        let versions = Versions::Legacy(Box::new(State::Uninitialized));
        let account = new_nonce_account(versions);
        assert_eq!(verify_nonce_account(&account, &blockhash), None);
        assert_eq!(verify_nonce_account(&account, &Hash::default()), None);
        let versions = Versions::Current(Box::new(State::Uninitialized));
        let account = new_nonce_account(versions);
        assert_eq!(verify_nonce_account(&account, &blockhash), None);
        assert_eq!(verify_nonce_account(&account, &Hash::default()), None);
        let durable_nonce = DurableNonce::from_blockhash(&blockhash);
        let data = Data {
            authority: Pubkey::new_unique(),
            durable_nonce,
            fee_calculator: FeeCalculator {
                lamports_per_signature: 2718,
            },
        };
        let versions = Versions::Legacy(Box::new(State::Initialized(data.clone())));
        let account = new_nonce_account(versions);
        assert_eq!(verify_nonce_account(&account, &blockhash), None);
        assert_eq!(verify_nonce_account(&account, &Hash::default()), None);
        assert_eq!(verify_nonce_account(&account, &data.blockhash()), None);
        assert_eq!(
            verify_nonce_account(&account, durable_nonce.as_hash()),
            None
        );
        let durable_nonce = DurableNonce::from_blockhash(durable_nonce.as_hash());
        assert_ne!(data.durable_nonce, durable_nonce);
        let data = Data {
            durable_nonce,
            ..data
        };
        let versions = Versions::Current(Box::new(State::Initialized(data.clone())));
        let account = new_nonce_account(versions);
        assert_eq!(verify_nonce_account(&account, &blockhash), None);
        assert_eq!(verify_nonce_account(&account, &Hash::default()), None);
        assert_eq!(
            verify_nonce_account(&account, &data.blockhash()),
            Some(data.clone())
        );
        assert_eq!(
            verify_nonce_account(&account, durable_nonce.as_hash()),
            Some(data)
        );
    }

    #[test]
    fn test_get_system_account_kind_system_ok() {
        let system_account = AccountSharedData::default();
        assert_eq!(
            get_system_account_kind(&system_account),
            Some(SystemAccountKind::System)
        );
    }

    #[test]
    fn test_get_system_account_kind_nonce_ok() {
        let nonce_account = AccountSharedData::new_data(
            42,
            &Versions::new(State::Initialized(Data::default())),
            &system_program::id(),
        )
        .unwrap();
        assert_eq!(
            get_system_account_kind(&nonce_account),
            Some(SystemAccountKind::Nonce)
        );
    }

    #[test]
    fn test_get_system_account_kind_uninitialized_nonce_account_fail() {
        assert_eq!(
            get_system_account_kind(&crate::create_account(42).borrow()),
            None
        );
    }

    #[test]
    fn test_get_system_account_kind_system_owner_nonzero_nonnonce_data_fail() {
        let other_data_account =
            AccountSharedData::new_data(42, b"other", &Pubkey::default()).unwrap();
        assert_eq!(get_system_account_kind(&other_data_account), None);
    }

    #[test]
    fn test_get_system_account_kind_nonsystem_owner_with_nonce_data_fail() {
        let nonce_account = AccountSharedData::new_data(
            42,
            &Versions::new(State::Initialized(Data::default())),
            &Pubkey::new_unique(),
        )
        .unwrap();
        assert_eq!(get_system_account_kind(&nonce_account), None);
    }
}
