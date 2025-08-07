use {
    crate::vote_transaction::VoteTransaction, solana_bincode::limited_deserialize,
    solana_clock::Slot, solana_hash::Hash, solana_pubkey::Pubkey, solana_signature::Signature,
    solana_svm_transaction::svm_transaction::SVMTransaction, solana_transaction::Transaction,
    solana_vote_interface::instruction::VoteInstruction,
    solana_votor_messages::vote::Vote as AlpenglowVote,
};

/// Represents a parsed vote transaction, which can be either a traditional Tower
/// vote or an Alpenglow vote. This enum allows unified handling of different vote types.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum ParsedVoteTransaction {
    Tower(VoteTransaction),
    Alpenglow(AlpenglowVote),
}

impl ParsedVoteTransaction {
    pub fn slots(&self) -> Vec<Slot> {
        match self {
            ParsedVoteTransaction::Tower(tx) => tx.slots(),
            ParsedVoteTransaction::Alpenglow(tx) => vec![tx.slot()],
        }
    }
    pub fn last_voted_slot(&self) -> Option<Slot> {
        match self {
            ParsedVoteTransaction::Tower(tx) => tx.last_voted_slot(),
            ParsedVoteTransaction::Alpenglow(tx) => Some(tx.slot()),
        }
    }

    pub fn last_voted_slot_hash(&self) -> Option<(Slot, Hash)> {
        match self {
            ParsedVoteTransaction::Tower(tx) => tx.last_voted_slot_hash(),
            ParsedVoteTransaction::Alpenglow(tx) => match tx {
                AlpenglowVote::Notarize(vote) => Some((vote.slot(), Hash::default())),
                AlpenglowVote::Finalize(_vote) => None,
                AlpenglowVote::NotarizeFallback(vote) => Some((vote.slot(), Hash::default())),
                AlpenglowVote::Skip(_vote) => None,
                AlpenglowVote::SkipFallback(_vote) => None,
            },
        }
    }

    pub fn is_alpenglow_vote(&self) -> bool {
        matches!(self, ParsedVoteTransaction::Alpenglow(_))
    }

    pub fn is_full_tower_vote(&self) -> bool {
        match self {
            ParsedVoteTransaction::Tower(tx) => tx.is_full_tower_vote(),
            ParsedVoteTransaction::Alpenglow(_tx) => false,
        }
    }

    pub fn as_tower_transaction(self) -> Option<VoteTransaction> {
        match self {
            ParsedVoteTransaction::Tower(tx) => Some(tx),
            ParsedVoteTransaction::Alpenglow(_tx) => None,
        }
    }

    pub fn as_alpenglow_transaction(self) -> Option<AlpenglowVote> {
        match self {
            ParsedVoteTransaction::Tower(_tx) => None,
            ParsedVoteTransaction::Alpenglow(tx) => Some(tx),
        }
    }

    pub fn as_tower_transaction_ref(&self) -> Option<&VoteTransaction> {
        match self {
            ParsedVoteTransaction::Tower(tx) => Some(tx),
            ParsedVoteTransaction::Alpenglow(_tx) => None,
        }
    }

    pub fn as_alpenglow_transaction_ref(&self) -> Option<&AlpenglowVote> {
        match self {
            ParsedVoteTransaction::Tower(_tx) => None,
            ParsedVoteTransaction::Alpenglow(tx) => Some(tx),
        }
    }
}

impl From<VoteTransaction> for ParsedVoteTransaction {
    fn from(value: VoteTransaction) -> Self {
        ParsedVoteTransaction::Tower(value)
    }
}

impl From<AlpenglowVote> for ParsedVoteTransaction {
    fn from(value: solana_votor_messages::vote::Vote) -> Self {
        ParsedVoteTransaction::Alpenglow(value)
    }
}

pub type ParsedVote = (Pubkey, ParsedVoteTransaction, Option<Hash>, Signature);

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
    Some((
        *key,
        ParsedVoteTransaction::Tower(vote),
        switch_proof_hash,
        signature,
    ))
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
    Some((
        *key,
        ParsedVoteTransaction::Tower(vote),
        switch_proof_hash,
        signature,
    ))
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
        | VoteInstruction::UpdateCommission(_)
        | VoteInstruction::UpdateValidatorIdentity
        | VoteInstruction::Withdraw(_) => None,
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
        solana_vote_interface::{instruction as vote_instruction, state::Vote},
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
        assert_eq!(
            vote,
            ParsedVoteTransaction::Tower(VoteTransaction::from(Vote::new(vec![42], bank_hash)))
        );
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
}
