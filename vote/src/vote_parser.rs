use {
    crate::vote_transaction::VoteTransaction,
    solana_bincode::limited_deserialize,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_svm_transaction::{instruction::SVMInstruction, svm_transaction::SVMTransaction},
    solana_transaction::Transaction,
    solana_vote_interface::instruction::VoteInstruction,
};

pub type ParsedVote = (Pubkey, VoteTransaction, Option<Hash>, Signature);

/// Check if a transaction is a valid vote-only transaction.
/// A valid vote-only transaction must:
/// 1. Have 1 to 3 instructions
/// 2. The first instruction must be to the vote program
/// 3. The first instruction must be a single vote state update (UpdateVoteState, TowerSync, etc.)
/// If `is_expanded_version` is true, then:
/// 4. The optional second instruction must be `SetComputeUnitLimit`
/// 5. The optional third instruction must be `SetLoadedAccountsDataSizeLimit`
pub fn is_valid_vote_only_transaction(tx: &impl SVMTransaction, is_expanded_version: bool) -> bool {
    let mut instructions = tx.program_instructions_iter();

    let Some((program_id, instruction)) = instructions.next() else {
        return false;
    };

    if !solana_sdk_ids::vote::check_id(program_id) {
        return false;
    }

    if !limited_deserialize::<VoteInstruction>(
        instruction.data,
        solana_packet::PACKET_DATA_SIZE as u64,
    )
    .map(|ix| ix.is_single_vote_state_update())
    .unwrap_or(false)
    {
        return false;
    }

    if !is_expanded_version {
        return instructions.next().is_none();
    }

    let Some((program_id, instruction)) = instructions.next() else {
        return true;
    };
    if !is_compute_budget_instruction(
        program_id,
        &instruction,
        SET_COMPUTE_UNIT_LIMIT_DISCRIMINATOR,
    ) {
        return false;
    }

    let Some((program_id, instruction)) = instructions.next() else {
        return true;
    };
    is_compute_budget_instruction(
        program_id,
        &instruction,
        SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT_DISCRIMINATOR,
    ) && instructions.next().is_none()
}

// Local wire-format discriminators for the compute-budget instructions accepted
// by the temporary vote-only simple-vote layout. These mirror
// `solana_compute_budget_interface::ComputeBudgetInstruction` serialization.
const SET_COMPUTE_UNIT_LIMIT_DISCRIMINATOR: u8 = 2;
const SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT_DISCRIMINATOR: u8 = 4;

fn is_compute_budget_instruction(
    program_id: &Pubkey,
    instruction: &SVMInstruction,
    discriminator: u8,
) -> bool {
    // Both SetComputeUnitLimit and SetLoadedAccountsDataSizeLimit have `u32` data.
    const COMPUTE_BUDGET_INSTRUCTION_DATA_LEN: usize = 1 + core::mem::size_of::<u32>();

    solana_sdk_ids::compute_budget::check_id(program_id)
        && instruction.data.len() == COMPUTE_BUDGET_INSTRUCTION_DATA_LEN
        && instruction.data[0] == discriminator
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
    match limited_deserialize(
        vote_instruction_data,
        solana_packet::PACKET_DATA_SIZE as u64,
    )
    .ok()?
    {
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
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_sha256_hasher::hash,
        solana_signer::Signer,
        solana_system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_vote_interface::{
            instruction as vote_instruction,
            state::{TowerSync, Vote, VoteAuthorize},
        },
    };

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
            is_valid_vote_only_transaction(&sanitized, false),
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
            is_valid_vote_only_transaction(&sanitized, false),
            "TowerSyncSwitch transaction should be valid"
        );

        // Non-vote transaction (system transfer) should fail
        let from_keypair = Keypair::new();
        let to_pubkey = Pubkey::new_unique();
        let transfer_tx =
            solana_system_transaction::transfer(&from_keypair, &to_pubkey, 1, blockhash);
        let sanitized = SanitizedTransaction::from_transaction_for_tests(transfer_tx);
        assert!(
            !is_valid_vote_only_transaction(&sanitized, false),
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
            !is_valid_vote_only_transaction(&sanitized, false),
            "Transaction with multiple vote instructions should not be valid"
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
            !is_valid_vote_only_transaction(&sanitized, false),
            "Vote Authorize instruction should not be valid vote-only transaction"
        );
    }

    fn compute_budget_instruction(discriminator: u8, value: u32) -> Instruction {
        let mut data = vec![discriminator];
        data.extend_from_slice(&value.to_le_bytes());
        Instruction {
            program_id: solana_sdk_ids::compute_budget::id(),
            accounts: vec![],
            data,
        }
    }

    fn tower_sync_instruction(vote_keypair: &Keypair) -> Instruction {
        vote_instruction::tower_sync(
            &vote_keypair.pubkey(),
            &vote_keypair.pubkey(),
            TowerSync::new_from_slot(1, Hash::default()),
        )
    }

    fn is_valid_vote_only_instructions(
        vote_keypair: &Keypair,
        instructions: &[Instruction],
        is_expanded_version: bool,
    ) -> bool {
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&vote_keypair.pubkey()),
            &[&vote_keypair],
            Hash::default(),
        );
        let sanitized = SanitizedTransaction::from_transaction_for_tests(tx);
        is_valid_vote_only_transaction(&sanitized, is_expanded_version)
    }

    #[test]
    fn test_is_valid_vote_only_transaction_with_compute_budget_instructions() {
        let vote_keypair = Keypair::new();
        let vote_ix = tower_sync_instruction(&vote_keypair);
        let set_compute_unit_limit_ix =
            compute_budget_instruction(SET_COMPUTE_UNIT_LIMIT_DISCRIMINATOR, 1);
        let set_loaded_accounts_data_size_limit_ix =
            compute_budget_instruction(SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT_DISCRIMINATOR, 1);

        assert!(
            is_valid_vote_only_instructions(
                &vote_keypair,
                &[vote_ix.clone(), set_compute_unit_limit_ix.clone()],
                true,
            ),
            "vote followed by SetComputeUnitLimit should be valid"
        );
        assert!(
            !is_valid_vote_only_instructions(
                &vote_keypair,
                &[vote_ix.clone(), set_compute_unit_limit_ix.clone()],
                false,
            ),
            "compute-budget instructions should require the expanded vote-only gate"
        );
        assert!(
            is_valid_vote_only_instructions(
                &vote_keypair,
                &[
                    vote_ix.clone(),
                    set_compute_unit_limit_ix.clone(),
                    set_loaded_accounts_data_size_limit_ix.clone(),
                ],
                true,
            ),
            "vote followed by accepted compute-budget layout should be valid"
        );
        assert!(
            !is_valid_vote_only_instructions(
                &vote_keypair,
                &[
                    vote_ix.clone(),
                    set_loaded_accounts_data_size_limit_ix.clone(),
                ],
                true,
            ),
            "vote followed by SetLoadedAccountsDataSizeLimit without SetComputeUnitLimit should \
             not be valid"
        );
        assert!(
            !is_valid_vote_only_instructions(
                &vote_keypair,
                &[
                    vote_ix.clone(),
                    set_loaded_accounts_data_size_limit_ix,
                    set_compute_unit_limit_ix.clone(),
                ],
                true,
            ),
            "reordered compute-budget instructions should not be valid"
        );
        assert!(
            !is_valid_vote_only_instructions(
                &vote_keypair,
                &[
                    vote_ix.clone(),
                    set_compute_unit_limit_ix.clone(),
                    set_compute_unit_limit_ix,
                ],
                true,
            ),
            "duplicate SetComputeUnitLimit instructions should not be valid"
        );
    }
}
