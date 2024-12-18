use {
    solana_faucet::faucet::{request_airdrop_transaction, run_local_faucet},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_signer::Signer,
    solana_system_interface::instruction::transfer,
    solana_transaction::Transaction,
};

#[test]
fn test_local_faucet() {
    let keypair = Keypair::new();
    let to = solana_pubkey::new_rand();
    let lamports = 50;
    let blockhash = Hash::new_from_array(to.to_bytes());
    let create_instruction = transfer(&keypair.pubkey(), &to, lamports);
    let message = Message::new(&[create_instruction], Some(&keypair.pubkey()));
    let expected_tx = Transaction::new(&[&keypair], message, blockhash);

    let faucet_addr = run_local_faucet(keypair, None);

    let result = request_airdrop_transaction(&faucet_addr, &to, lamports, blockhash);
    assert_eq!(expected_tx, result.unwrap());
}
