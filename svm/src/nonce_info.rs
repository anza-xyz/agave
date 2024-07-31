use {
    solana_sdk::{
        account::AccountSharedData,
        account_utils::StateMut,
        nonce::state::{DurableNonce, State as NonceState, Versions as NonceVersions},
        nonce_account,
        pubkey::Pubkey,
    },
    thiserror::Error,
};

pub trait NonceInfo {
    fn address(&self) -> &Pubkey;
    fn account(&self) -> &AccountSharedData;
    fn lamports_per_signature(&self) -> Option<u64>;
    fn fee_payer_account(&self) -> Option<&AccountSharedData>;
}

/// Holds limited nonce info available during transaction checks
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoncePartial {
    address: Pubkey,
    account: AccountSharedData,
}

#[derive(Error, Debug, PartialEq)]
pub enum AdvanceNonceError {
    #[error("Invalid account")]
    Invalid,
    #[error("Uninitialized nonce")]
    Uninitialized,
}

impl NoncePartial {
    pub fn new(address: Pubkey, account: AccountSharedData) -> Self {
        Self { address, account }
    }

    // Advance the stored blockhash to prevent fee theft by someone
    // replaying nonce transactions that have failed with an
    // `InstructionError`.
    pub fn try_advance_nonce(
        &mut self,
        durable_nonce: DurableNonce,
        lamports_per_signature: u64,
    ) -> Result<(), AdvanceNonceError> {
        let nonce_versions = StateMut::<NonceVersions>::state(&self.account)
            .map_err(|_| AdvanceNonceError::Invalid)?;
        if let NonceState::Initialized(ref data) = nonce_versions.state() {
            let nonce_state =
                NonceState::new_initialized(&data.authority, durable_nonce, lamports_per_signature);
            let nonce_versions = NonceVersions::new(nonce_state);
            self.account.set_state(&nonce_versions).unwrap();
            Ok(())
        } else {
            Err(AdvanceNonceError::Uninitialized)
        }
    }
}

impl NonceInfo for NoncePartial {
    fn address(&self) -> &Pubkey {
        &self.address
    }
    fn account(&self) -> &AccountSharedData {
        &self.account
    }
    fn lamports_per_signature(&self) -> Option<u64> {
        nonce_account::lamports_per_signature_of(&self.account)
    }
    fn fee_payer_account(&self) -> Option<&AccountSharedData> {
        None
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{
            hash::Hash,
            nonce::state::{
                Data as NonceData, DurableNonce, State as NonceState, Versions as NonceVersions,
            },
            system_program,
        },
    };

    fn create_nonce_account(state: NonceState) -> AccountSharedData {
        AccountSharedData::new_data(1_000_000, &NonceVersions::new(state), &system_program::id())
            .unwrap()
    }

    #[test]
    fn test_nonce_info() {
        let nonce_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let lamports_per_signature = 42;
        let nonce_account = create_nonce_account(NonceState::Initialized(NonceData::new(
            Pubkey::default(),
            durable_nonce,
            lamports_per_signature,
        )));

        // NoncePartial create + NonceInfo impl
        let partial = NoncePartial::new(nonce_address, nonce_account.clone());
        assert_eq!(*partial.address(), nonce_address);
        assert_eq!(*partial.account(), nonce_account);
        assert_eq!(
            partial.lamports_per_signature(),
            Some(lamports_per_signature)
        );
        assert_eq!(partial.fee_payer_account(), None);
    }

    #[test]
    fn test_try_advance_nonce_success() {
        let authority = Pubkey::new_unique();
        let mut nonce_partial = NoncePartial::new(
            Pubkey::new_unique(),
            create_nonce_account(NonceState::Initialized(NonceData::new(
                authority,
                DurableNonce::from_blockhash(&Hash::new_unique()),
                42,
            ))),
        );

        let new_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let new_lamports_per_signature = 100;
        let result = nonce_partial.try_advance_nonce(new_nonce, new_lamports_per_signature);
        assert_eq!(result, Ok(()));

        let nonce_versions = StateMut::<NonceVersions>::state(&nonce_partial.account).unwrap();
        assert_eq!(
            &NonceState::Initialized(NonceData::new(
                authority,
                new_nonce,
                new_lamports_per_signature
            )),
            nonce_versions.state()
        );
    }

    #[test]
    fn test_try_advance_nonce_invalid() {
        let mut nonce_partial = NoncePartial::new(
            Pubkey::new_unique(),
            AccountSharedData::new(1_000_000, 0, &Pubkey::default()),
        );

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let result = nonce_partial.try_advance_nonce(durable_nonce, 5000);
        assert_eq!(result, Err(AdvanceNonceError::Invalid));
    }

    #[test]
    fn test_try_advance_nonce_uninitialized() {
        let mut nonce_partial = NoncePartial::new(
            Pubkey::new_unique(),
            create_nonce_account(NonceState::Uninitialized),
        );

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let result = nonce_partial.try_advance_nonce(durable_nonce, 5000);
        assert_eq!(result, Err(AdvanceNonceError::Uninitialized));
    }
}
