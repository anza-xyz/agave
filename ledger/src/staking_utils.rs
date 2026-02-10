#[cfg(test)]
pub(crate) mod tests {
    use {
        rand::Rng,
        solana_account::AccountSharedData,
        solana_clock::Clock,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_runtime::bank::Bank,
        solana_signer::{signers::Signers, Signer},
        solana_stake_interface::{
            program as stake_program,
            stake_flags::StakeFlags,
            state::{Authorized, Delegation, Meta, Stake, StakeStateV2},
        },
        solana_transaction::Transaction,
        solana_vote::vote_account::{VoteAccount, VoteAccounts},
        solana_vote_program::{
            vote_instruction,
            vote_state::{VoteInit, VoteStateV4, VoteStateVersions},
        },
        std::sync::Arc,
    };

    pub(crate) fn setup_vote_and_stake_accounts(
        bank: &Bank,
        from_account: &Keypair,
        vote_account: &Keypair,
        validator_identity_account: &Keypair,
        amount: u64,
    ) {
        let vote_pubkey = vote_account.pubkey();
        fn process_instructions<T: Signers>(bank: &Bank, keypairs: &T, ixs: &[Instruction]) {
            let tx = Transaction::new_signed_with_payer(
                ixs,
                Some(&keypairs.pubkeys()[0]),
                keypairs,
                bank.last_blockhash(),
            );
            bank.process_transaction(&tx).unwrap();
        }

        process_instructions(
            bank,
            &[from_account, vote_account, validator_identity_account],
            &vote_instruction::create_account_with_config(
                &from_account.pubkey(),
                &vote_pubkey,
                &VoteInit {
                    node_pubkey: validator_identity_account.pubkey(),
                    authorized_voter: vote_pubkey,
                    authorized_withdrawer: vote_pubkey,
                    commission: 0,
                },
                amount,
                vote_instruction::CreateVoteAccountConfig {
                    space: VoteStateV4::size_of() as u64,
                    ..vote_instruction::CreateVoteAccountConfig::default()
                },
            ),
        );

        let stake_account_keypair = Keypair::new();
        let stake_account_pubkey = stake_account_keypair.pubkey();

        let stake_account = StakeStateV2::Stake(
            Meta {
                authorized: Authorized::auto(&stake_account_pubkey),
                ..Meta::default()
            },
            Stake {
                delegation: Delegation {
                    voter_pubkey: vote_pubkey,
                    stake: amount,
                    ..Delegation::default()
                },
                ..Stake::default()
            },
            StakeFlags::default(),
        );

        let account = AccountSharedData::create_from_existing_shared_data(
            1,
            Arc::new(bincode::serialize(&stake_account).unwrap()),
            stake_program::id(),
            false,
            u64::MAX,
        );

        bank.store_account(&stake_account_pubkey, &account);
    }

    #[test]
    fn test_to_staked_nodes() {
        let mut stakes = Vec::new();
        let node1 = solana_pubkey::new_rand();
        let vote_pubkey1 = solana_pubkey::new_rand();

        // Node 1 has stake of 3
        for i in 0..3 {
            stakes.push((
                i,
                VoteStateV4::new_with_defaults(
                    &vote_pubkey1,
                    &VoteInit {
                        node_pubkey: node1,
                        ..VoteInit::default()
                    },
                    &Clock::default(),
                ),
            ));
        }

        // Node 1 has stake of 5
        let node2 = solana_pubkey::new_rand();
        let vote_pubkey2 = solana_pubkey::new_rand();

        stakes.push((
            5,
            VoteStateV4::new_with_defaults(
                &vote_pubkey2,
                &VoteInit {
                    node_pubkey: node2,
                    ..VoteInit::default()
                },
                &Clock::default(),
            ),
        ));
        let mut rng = rand::rng();
        let vote_accounts = stakes.into_iter().map(|(stake, vote_state)| {
            let account = AccountSharedData::new_data(
                rng.random(), // lamports
                &VoteStateVersions::new_v4(vote_state),
                &solana_vote_program::id(), // owner
            )
            .unwrap();
            let vote_pubkey = Pubkey::new_unique();
            let vote_account = VoteAccount::try_from(account).unwrap();
            (vote_pubkey, (stake, vote_account))
        });
        let result = vote_accounts.collect::<VoteAccounts>().staked_nodes();
        assert_eq!(result.len(), 2);
        assert_eq!(result[&node1], 3);
        assert_eq!(result[&node2], 5);
    }
}
