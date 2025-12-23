use {
    rand::{CryptoRng, Rng, RngCore},
    solana_clock::Slot,
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
    solana_vote::vote_transaction,
    solana_vote_program::vote_state::TowerSync,
};

pub fn test_tx() -> Transaction {
    let keypair1 = Keypair::new();
    let pubkey1 = keypair1.pubkey();
    let zero = Hash::default();
    solana_system_transaction::transfer(&keypair1, &pubkey1, 42, zero)
}

pub fn test_invalid_tx() -> Transaction {
    let mut tx = test_tx();
    tx.signatures = vec![Transaction::get_invalid_signature()];
    tx
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
        0,
        &transfer_instruction,
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

pub fn new_test_vote_tx<R>(rng: &mut R) -> Transaction
where
    R: CryptoRng + RngCore,
{
    let mut slots: Vec<Slot> = std::iter::repeat_with(|| rng.random()).take(5).collect();
    slots.sort_unstable();
    slots.dedup();
    let switch_proof_hash = rng.random_bool(0.5).then(Hash::new_unique);
    let tower_sync = TowerSync::new_from_slots(slots, Hash::default(), None);
    vote_transaction::new_tower_sync_transaction(
        tower_sync,
        Hash::new_unique(), // blockhash
        &Keypair::new(),    // node_keypair
        &Keypair::new(),    // vote_keypair
        &Keypair::new(),    // authorized_voter_keypair
        switch_proof_hash,
    )
}

pub fn new_test_tx_with_number_of_ixs<T>(number_of_ixs: usize) -> T
where
    T: FromTestTx,
{
    let program_id = Pubkey::new_unique();
    let account = Pubkey::new_unique();

    let mut instructions = Vec::with_capacity(number_of_ixs);
    for i in 0..number_of_ixs {
        instructions.push(Instruction {
            program_id,
            accounts: vec![AccountMeta::new(account, false)],
            data: vec![i as u8],
        });
    }

    let payer = Keypair::new();
    let blockhash = Hash::new_unique();

    T::from_test_tx(&payer, blockhash, instructions)
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
