use {
    solana_packet::PACKET_DATA_WINCODE_CONFIG,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_vote_interface::instruction::VoteInstruction,
};

/// Decodes vote instruction data from at most `PACKET_DATA_SIZE` bytes, ignoring trailing bytes.
pub fn deserialize_vote_instruction(data: &[u8]) -> Option<VoteInstruction> {
    wincode::config::deserialize(data, PACKET_DATA_WINCODE_CONFIG).ok()
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

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_packet::PACKET_DATA_SIZE,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_system_transaction,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        solana_vote_interface::{
            instruction as vote_instruction,
            state::{TowerSync, VoteAuthorize, VoteAuthorizeWithSeedArgs, VoteStateUpdate},
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

    /// Pins which truncated, extended and byte-mutated copies of valid encodings decode, since the
    /// compact variants sit on hand-written varint and short_vec readers.
    #[test]
    fn test_deserialize_compact_vote_instruction_corrupted_input() {
        // Every compact encoding starts with a u32 tag, a u64 root, the lockout count and varints
        // at 12..19, a hash and the timestamp option tag at 51.
        let is_rejected = |encoded_len: usize, index: usize, byte: u8| match (index, byte) {
            // Tag 0 is InitializeAccount, which needs 97 bytes after the tag.
            (0, 0x00) => encoded_len < 4 + 97,
            (0, 0x7f | 0x80 | 0xff) => true,
            (1..=3, byte) => byte != 0,
            // Lockout counts from 127 up run past the end of the data.
            (12, 0x7f | 0x80 | 0xff) => true,
            // A continuation bit merges the varint with the next byte and shifts the rest.
            (13 | 15 | 17, 0x80 | 0xff) => true,
            (51, byte) => byte > 1,
            _ => false,
        };
        for instruction in compact_vote_instructions() {
            let encoded = wincode::serialize(&instruction).unwrap();
            for len in 0..encoded.len() {
                assert_eq!(
                    deserialize_vote_instruction(encoded.get(..len).unwrap()),
                    None,
                    "len: {len}",
                );
            }
            let mut trailing = encoded.clone();
            trailing.extend_from_slice(&[0xff; 16]);
            assert_eq!(deserialize_vote_instruction(&trailing), Some(instruction));
            for index in 0..encoded.len() {
                for byte in [0x00, 0x01, 0x02, 0x7f, 0x80, 0xff] {
                    let mut mutated = encoded.clone();
                    *mutated.get_mut(index).unwrap() = byte;
                    let decoded = deserialize_vote_instruction(&mutated);
                    assert_eq!(
                        decoded.is_none(),
                        is_rejected(encoded.len(), index, byte),
                        "index: {index}, byte: {byte:#04x}, data: {mutated:?}",
                    );
                    if let Some(decoded) = decoded {
                        assert!(
                            mutated.starts_with(&wincode::serialize(&decoded).unwrap()),
                            "index: {index}, byte: {byte:#04x}, decoded: {decoded:?}",
                        );
                    }
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
