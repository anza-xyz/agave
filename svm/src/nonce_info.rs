use solana_sdk::{account::AccountSharedData, nonce_account, pubkey::Pubkey};

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

impl NoncePartial {
    pub fn new(address: Pubkey, account: AccountSharedData) -> Self {
        Self { address, account }
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
            instruction::Instruction,
            message::{Message, SanitizedMessage},
            nonce::{self, state::DurableNonce},
            reserved_account_keys::ReservedAccountKeys,
            signature::{keypair_from_seed, Signer},
            system_instruction, system_program,
        },
    };

    fn new_sanitized_message(
        instructions: &[Instruction],
        payer: Option<&Pubkey>,
    ) -> SanitizedMessage {
        SanitizedMessage::try_from_legacy_message(
            Message::new(instructions, payer),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap()
    }

    #[test]
    fn test_nonce_info() {
        let lamports_per_signature = 42;

        let nonce_authority = keypair_from_seed(&[0; 32]).unwrap();
        let nonce_address = nonce_authority.pubkey();
        let from = keypair_from_seed(&[1; 32]).unwrap();
        let from_address = from.pubkey();
        let to_address = Pubkey::new_unique();

        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_account = AccountSharedData::new_data(
            43,
            &nonce::state::Versions::new(nonce::State::Initialized(nonce::state::Data::new(
                Pubkey::default(),
                durable_nonce,
                lamports_per_signature,
            ))),
            &system_program::id(),
        )
        .unwrap();
        let from_account = AccountSharedData::new(44, 0, &Pubkey::default());

        const TEST_RENT_DEBIT: u64 = 1;
        let rent_collected_nonce_account = {
            let mut account = nonce_account.clone();
            account.set_lamports(nonce_account.lamports() - TEST_RENT_DEBIT);
            account
        };
        let rent_collected_from_account = {
            let mut account = from_account.clone();
            account.set_lamports(from_account.lamports() - TEST_RENT_DEBIT);
            account
        };

        let instructions = vec![
            system_instruction::advance_nonce_account(&nonce_address, &nonce_authority.pubkey()),
            system_instruction::transfer(&from_address, &to_address, 42),
        ];

        // NoncePartial create + NonceInfo impl
        let partial = NoncePartial::new(nonce_address, rent_collected_nonce_account.clone());
        assert_eq!(*partial.address(), nonce_address);
        assert_eq!(*partial.account(), rent_collected_nonce_account);
        assert_eq!(
            partial.lamports_per_signature(),
            Some(lamports_per_signature)
        );
        assert_eq!(partial.fee_payer_account(), None);

        // NonceFull create + NonceInfo impl
        {
            let message = new_sanitized_message(&instructions, Some(&from_address));
            let fee_payer_address = message.account_keys().get(0).unwrap();
            let fee_payer_account = rent_collected_from_account.clone();
            let full = NonceFull::from_partial(
                partial.clone(),
                fee_payer_address,
                fee_payer_account,
                TEST_RENT_DEBIT,
            );
            assert_eq!(*full.address(), nonce_address);
            assert_eq!(*full.account(), rent_collected_nonce_account);
            assert_eq!(full.lamports_per_signature(), Some(lamports_per_signature));
            assert_eq!(
                full.fee_payer_account(),
                Some(&from_account),
                "rent debit should be refunded in captured fee account"
            );
        }

        // Nonce account is fee-payer
        {
            let message = new_sanitized_message(&instructions, Some(&nonce_address));
            let fee_payer_address = message.account_keys().get(0).unwrap();
            let fee_payer_account = rent_collected_nonce_account;
            let full = NonceFull::from_partial(
                partial,
                fee_payer_address,
                fee_payer_account,
                TEST_RENT_DEBIT,
            );
            assert_eq!(*full.address(), nonce_address);
            assert_eq!(*full.account(), nonce_account);
            assert_eq!(full.lamports_per_signature(), Some(lamports_per_signature));
            assert_eq!(full.fee_payer_account(), None);
        }
    }
}
