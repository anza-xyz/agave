use {
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::{
        AccountMeta, Instruction, Message, VersionedMessage,
        compiled_instruction::CompiledInstruction, v0::Message as MessageV0,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{stake, system_program},
    solana_signer::Signer,
    solana_system_interface::instruction::SystemInstruction,
    solana_transaction::{Transaction, versioned::VersionedTransaction},
};

pub fn test_tx() -> Transaction {
    let keypair1 = Keypair::new();
    let pubkey1 = keypair1.pubkey();
    let zero = Hash::default();
    solana_system_transaction::transfer(&keypair1, &pubkey1, 42, zero)
}

pub fn test_multisig_tx() -> Transaction {
    let keypair0 = Keypair::new();
    let keypair1 = Keypair::new();
    let keypairs = vec![&keypair0, &keypair1];
    let lamports = 5;
    let blockhash = Hash::default();

    let transfer_instruction = SystemInstruction::Transfer { lamports };

    let program_ids = vec![system_program::id(), stake::id()];

    let instructions = vec![CompiledInstruction::new(
        3,
        &wincode::serialize(&transfer_instruction).unwrap(),
        vec![0, 1],
    )];

    Transaction::new_with_compiled_instructions(
        &keypairs,
        &[],
        blockhash,
        program_ids,
        instructions,
    )
}

/// Builds a legacy transaction with two signers and one vote program instruction.
pub fn new_test_vote_tx() -> Transaction {
    let node_keypair = Keypair::new();
    let authorized_voter_keypair = Keypair::new();
    let vote_ix = Instruction::new_with_bytes(
        solana_vote_program::id(),
        &[1, 2, 3],
        vec![
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(authorized_voter_keypair.pubkey(), true),
        ],
    );
    let blockhash = Hash::new_unique();
    let mut tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));
    tx.partial_sign(&[&node_keypair], blockhash);
    tx.partial_sign(&[&authorized_voter_keypair], blockhash);
    tx
}

/// Trait that unifies building legacy vs v0 transactions
pub trait FromTestTx: Sized {
    fn from_test_tx(payer: &Keypair, blockhash: Hash, ixs: Vec<Instruction>) -> Self;
}

impl FromTestTx for Transaction {
    fn from_test_tx(payer: &Keypair, blockhash: Hash, ixs: Vec<Instruction>) -> Self {
        let msg = Message::new(&ixs, Some(&payer.pubkey()));
        Transaction::new(&[payer], msg, blockhash)
    }
}

impl FromTestTx for VersionedTransaction {
    fn from_test_tx(payer: &Keypair, _blockhash: Hash, ixs: Vec<Instruction>) -> Self {
        let msg_v0 =
            MessageV0::try_compile(&payer.pubkey(), &ixs, &[], Hash::new_unique()).unwrap();
        let versioned = VersionedMessage::V0(msg_v0);
        VersionedTransaction::try_new(versioned, &[payer]).unwrap()
    }
}
