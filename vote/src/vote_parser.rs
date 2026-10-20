use {
    crate::vote_transaction::VoteTransaction,
    solana_hash::Hash,
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_svm_transaction::{svm_message::SVMStaticMessage, svm_transaction::SVMTransaction},
    solana_transaction::Transaction,
    solana_vote_interface::instruction::VoteInstruction,
};

pub type ParsedVote = (Pubkey, VoteTransaction, Option<Hash>, Signature);

/// Decodes vote instruction data from at most `PACKET_DATA_SIZE` bytes, ignoring trailing bytes.
pub fn deserialize_vote_instruction(data: &[u8]) -> Option<VoteInstruction> {
    wincode::config::deserialize(
        data,
        wincode::config::Configuration::default()
            .with_deserialization_size_limit::<PACKET_DATA_SIZE>(),
    )
    .ok()
}

/// Check if a transaction is a valid vote-only transaction.
/// A valid vote-only transaction must:
/// 1. Have exactly one instruction
/// 2. That instruction must be to the vote program
/// 3. That instruction must be a single vote state update (UpdateVoteState, TowerSync, etc.)
pub fn is_valid_vote_only_transaction(tx: &impl SVMStaticMessage) -> bool {
    let mut instructions = tx.program_instructions_iter();

    let Some((program_id, instruction)) = instructions.next() else {
        return false;
    };

    if instructions.next().is_some() {
        return false;
    }

    if !solana_sdk_ids::vote::check_id(program_id) {
        return false;
    }

    deserialize_vote_instruction(instruction.data)
        .is_some_and(|ix| ix.is_single_vote_state_update())
}

// Used for locally forwarding processed vote transactions to consensus
pub fn parse_sanitized_vote_transaction(tx: &impl SVMTransaction) -> Option<ParsedVote> {
    // Check first instruction for a vote
    let (program_id, first_instruction) = tx.program_instructions_iter().next()?;
    if !solana_sdk_ids::vote::check_id(program_id) {
        return None;
    }
    let first_account = usize::from(*first_instruction.accounts.first()?);
    let key = tx.account_keys().get(first_account)?;
    let (vote, switch_proof_hash) = parse_vote_instruction_data(first_instruction.data)?;
    let signature = tx.signatures().first().cloned().unwrap_or_default();
    Some((*key, vote, switch_proof_hash, signature))
}

// Used for parsing gossip vote transactions
pub fn parse_vote_transaction(tx: &Transaction) -> Option<ParsedVote> {
    // Check first instruction for a vote
    let message = tx.message();
    let first_instruction = message.instructions.first()?;
    let program_id_index = usize::from(first_instruction.program_id_index);
    let program_id = message.account_keys.get(program_id_index)?;
    if !solana_sdk_ids::vote::check_id(program_id) {
        return None;
    }
    let first_account = usize::from(*first_instruction.accounts.first()?);
    let key = message.account_keys.get(first_account)?;
    let (vote, switch_proof_hash) = parse_vote_instruction_data(&first_instruction.data)?;
    let signature = tx.signatures.first().cloned().unwrap_or_default();
    Some((*key, vote, switch_proof_hash, signature))
}

fn parse_vote_instruction_data(
    vote_instruction_data: &[u8],
) -> Option<(VoteTransaction, Option<Hash>)> {
    match deserialize_vote_instruction(vote_instruction_data)? {
        VoteInstruction::Vote(vote) => Some((VoteTransaction::from(vote), None)),
        VoteInstruction::VoteSwitch(vote, hash) => Some((VoteTransaction::from(vote), Some(hash))),
        VoteInstruction::UpdateVoteState(vote_state_update) => {
            Some((VoteTransaction::from(vote_state_update), None))
        }
        VoteInstruction::UpdateVoteStateSwitch(vote_state_update, hash) => {
            Some((VoteTransaction::from(vote_state_update), Some(hash)))
        }
        VoteInstruction::CompactUpdateVoteState(vote_state_update) => {
            Some((VoteTransaction::from(vote_state_update), None))
        }
        VoteInstruction::CompactUpdateVoteStateSwitch(vote_state_update, hash) => {
            Some((VoteTransaction::from(vote_state_update), Some(hash)))
        }
        VoteInstruction::TowerSync(tower_sync) => Some((VoteTransaction::from(tower_sync), None)),
        VoteInstruction::TowerSyncSwitch(tower_sync, hash) => {
            Some((VoteTransaction::from(tower_sync), Some(hash)))
        }
        VoteInstruction::Authorize(_, _)
        | VoteInstruction::AuthorizeChecked(_)
        | VoteInstruction::AuthorizeWithSeed(_)
        | VoteInstruction::AuthorizeCheckedWithSeed(_)
        | VoteInstruction::InitializeAccount(_)
        | VoteInstruction::InitializeAccountV2(_)
        | VoteInstruction::UpdateCommission(_)
        | VoteInstruction::UpdateCommissionCollector(_)
        | VoteInstruction::UpdateCommissionBps { .. }
        | VoteInstruction::UpdateValidatorIdentity
        | VoteInstruction::Withdraw(_)
        | VoteInstruction::DepositDelegatorRewards { .. } => None,
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_clock::Slot,
        solana_keypair::Keypair,
        solana_sha256_hasher::hash,
        solana_signer::Signer,
        solana_system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_vote_interface::{
            instruction as vote_instruction,
            state::{TowerSync, Vote, VoteAuthorize, VoteAuthorizeWithSeedArgs, VoteStateUpdate},
        },
    };

    fn compact_vote_instructions() -> [VoteInstruction; 4] {
        let hash = Hash::new_unique();
        let mut vote_state_update = VoteStateUpdate::from(vec![(1, 3), (2, 2), (3, 1)]);
        vote_state_update.root = Some(0);
        vote_state_update.timestamp = Some(9);
        let mut tower_sync = TowerSync::from(vec![(1, 3), (2, 2), (3, 1)]);
        tower_sync.root = Some(0);
        tower_sync.timestamp = Some(9);
        tower_sync.block_id = Hash::new_unique();
        [
            VoteInstruction::CompactUpdateVoteState(vote_state_update.clone()),
            VoteInstruction::CompactUpdateVoteStateSwitch(vote_state_update, hash),
            VoteInstruction::TowerSync(tower_sync.clone()),
            VoteInstruction::TowerSyncSwitch(tower_sync, hash),
        ]
    }

    /// Checks the compact variants, whose hand-written bincode and wincode decoders sit on top of
    /// the shared varint and short_vec primitives, on corrupted copies of valid encodings.
    #[test]
    fn test_deserialize_compact_vote_instruction_matches_bincode_on_corrupted_input() {
        let assert_decoders_agree = |data: &[u8]| {
            assert_eq!(
                deserialize_vote_instruction(data),
                solana_bincode::limited_deserialize(data, PACKET_DATA_SIZE as u64).ok(),
                "data: {data:?}",
            );
        };
        for instruction in compact_vote_instructions() {
            let encoded = wincode::serialize(&instruction).unwrap();
            let mut trailing = encoded.clone();
            trailing.extend_from_slice(&[0xff; 16]);
            assert_decoders_agree(&trailing);
            for len in 0..encoded.len() {
                assert_decoders_agree(encoded.get(..len).unwrap());
            }
            for index in 0..encoded.len() {
                for byte in [0x00, 0x01, 0x02, 0x7f, 0x80, 0xff] {
                    let mut mutated = encoded.clone();
                    *mutated.get_mut(index).unwrap() = byte;
                    assert_decoders_agree(&mutated);
                }
            }
        }
    }

    #[test]
    fn test_deserialize_vote_instruction_size_limit() {
        // Every byte outside the seed: two u32 tags, two pubkeys and the u64 seed length.
        const FIXED_LEN: usize = 4 + 4 + 32 + 8 + 32;
        for encoded_len in [PACKET_DATA_SIZE - 1, PACKET_DATA_SIZE, PACKET_DATA_SIZE + 1] {
            let instruction = VoteInstruction::AuthorizeWithSeed(VoteAuthorizeWithSeedArgs {
                authorization_type: VoteAuthorize::Voter,
                current_authority_derived_key_owner: Pubkey::new_unique(),
                current_authority_derived_key_seed: "x".repeat(encoded_len - FIXED_LEN),
                new_authority: Pubkey::new_unique(),
            });
            let mut encoded = wincode::serialize(&instruction).unwrap();
            assert_eq!(encoded.len(), encoded_len);
            let expected = (encoded_len <= PACKET_DATA_SIZE).then_some(instruction);
            assert_eq!(deserialize_vote_instruction(&encoded), expected);
            encoded.push(0);
            assert_eq!(deserialize_vote_instruction(&encoded), expected);
        }
    }

    // Reimplemented locally from Vote program.
    fn new_vote_transaction(
        slots: Vec<Slot>,
        bank_hash: Hash,
        blockhash: Hash,
        node_keypair: &Keypair,
        vote_keypair: &Keypair,
        authorized_voter_keypair: &Keypair,
        switch_proof_hash: Option<Hash>,
    ) -> Transaction {
        let votes = Vote::new(slots, bank_hash);
        let vote_ix = if let Some(switch_proof_hash) = switch_proof_hash {
            vote_instruction::vote_switch(
                &vote_keypair.pubkey(),
                &authorized_voter_keypair.pubkey(),
                votes,
                switch_proof_hash,
            )
        } else {
            vote_instruction::vote(
                &vote_keypair.pubkey(),
                &authorized_voter_keypair.pubkey(),
                votes,
            )
        };

        let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

        vote_tx.partial_sign(&[node_keypair], blockhash);
        vote_tx.partial_sign(&[authorized_voter_keypair], blockhash);
        vote_tx
    }

    fn run_test_parse_vote_transaction(input_hash: Option<Hash>) {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let auth_voter_keypair = Keypair::new();
        let bank_hash = Hash::default();
        let vote_tx = new_vote_transaction(
            vec![42],
            bank_hash,
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &auth_voter_keypair,
            input_hash,
        );
        let (key, vote, hash, signature) = parse_vote_transaction(&vote_tx).unwrap();
        assert_eq!(hash, input_hash);
        assert_eq!(vote, VoteTransaction::from(Vote::new(vec![42], bank_hash)));
        assert_eq!(key, vote_keypair.pubkey());
        assert_eq!(signature, vote_tx.signatures[0]);

        // Test bad program id fails
        let mut vote_ix = vote_instruction::vote(
            &vote_keypair.pubkey(),
            &auth_voter_keypair.pubkey(),
            Vote::new(vec![1, 2], Hash::default()),
        );
        vote_ix.program_id = Pubkey::default();
        let vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));
        assert!(parse_vote_transaction(&vote_tx).is_none());
    }

    #[test]
    fn test_parse_vote_transaction() {
        run_test_parse_vote_transaction(None);
        run_test_parse_vote_transaction(Some(hash(&[42u8])));
    }

    #[test]
    fn test_is_valid_vote_only_transaction() {
        let vote_keypair = Keypair::new();
        let blockhash = Hash::default();

        // Valid TowerSync transaction should pass
        let tower_sync = TowerSync::new_from_slot(1, Hash::default());
        let vote_ix = vote_instruction::tower_sync(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            tower_sync,
        );
        let vote_tx = Transaction::new_signed_with_payer(
            &[vote_ix],
            Some(&vote_keypair.pubkey()),
            &[&vote_keypair],
            blockhash,
        );
        let sanitized = SanitizedTransaction::from_transaction_for_tests(vote_tx);
        assert!(
            is_valid_vote_only_transaction(&sanitized),
            "TowerSync transaction should be valid"
        );

        // Valid TowerSyncSwitch transaction should pass
        let tower_sync = TowerSync::new_from_slot(1, Hash::default());
        let vote_ix = vote_instruction::tower_sync_switch(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            tower_sync,
            Hash::new_unique(),
        );
        let vote_tx = Transaction::new_signed_with_payer(
            &[vote_ix],
            Some(&vote_keypair.pubkey()),
            &[&vote_keypair],
            blockhash,
        );
        let sanitized = SanitizedTransaction::from_transaction_for_tests(vote_tx);
        assert!(
            is_valid_vote_only_transaction(&sanitized),
            "TowerSyncSwitch transaction should be valid"
        );

        // Non-vote transaction (system transfer) should fail
        let from_keypair = Keypair::new();
        let to_pubkey = Pubkey::new_unique();
        let transfer_tx =
            solana_system_transaction::transfer(&from_keypair, &to_pubkey, 1, blockhash);
        let sanitized = SanitizedTransaction::from_transaction_for_tests(transfer_tx);
        assert!(
            !is_valid_vote_only_transaction(&sanitized),
            "System transfer should not be valid vote-only transaction"
        );

        // Transaction with multiple instructions should fail
        let ix1 = vote_instruction::tower_sync(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            TowerSync::new_from_slot(1, Hash::default()),
        );
        let ix2 = vote_instruction::tower_sync(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            TowerSync::new_from_slot(2, Hash::default()),
        );
        let multi_ix_tx = Transaction::new_signed_with_payer(
            &[ix1, ix2],
            Some(&vote_keypair.pubkey()),
            &[&vote_keypair],
            blockhash,
        );
        let sanitized = SanitizedTransaction::from_transaction_for_tests(multi_ix_tx);
        assert!(
            !is_valid_vote_only_transaction(&sanitized),
            "Transaction with multiple instructions should not be valid"
        );

        // Vote program accounting instructions should fail
        let authorize_ix = vote_instruction::authorize(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            &Pubkey::new_unique(),
            VoteAuthorize::Voter,
        );
        let authorize_tx = Transaction::new_signed_with_payer(
            &[authorize_ix],
            Some(&vote_keypair.pubkey()),
            &[&vote_keypair],
            blockhash,
        );
        let sanitized = SanitizedTransaction::from_transaction_for_tests(authorize_tx);
        assert!(
            !is_valid_vote_only_transaction(&sanitized),
            "Vote Authorize instruction should not be valid vote-only transaction"
        );
    }
}
