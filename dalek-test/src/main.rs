use {
    solana_sdk::{
        instruction::Instruction,
        signature::{Keypair, Signer},
        transaction::Transaction,
    },
    solana_test_validator::{TestValidator, TestValidatorGenesis},
    solana_zk_token_sdk::{
        encryption::pedersen::Pedersen, instruction::BatchedRangeProofU128Data,
        zk_token_proof_instruction::verify_batched_verify_range_proof_u128,
    },
    std::sync::Arc,
};

#[tokio::main]
async fn main() {
    let (test_validator, payer) = new_validator_for_test().await;

    let payer: Arc<dyn Signer> = Arc::new(payer);
    let rpc_client = Arc::new(test_validator.get_async_rpc_client());

    let instructions = vec![batched_range_proof_u128()];
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer],
        rpc_client.get_latest_blockhash().await.unwrap(),
    );

    let signature = rpc_client
        .send_and_confirm_transaction(&transaction)
        .await
        .unwrap();
    println!("signature: {:?}", signature);
}

fn batched_range_proof_u128() -> Instruction {
    let amount_1 = 23_u64;
    let amount_2 = 24_u64;

    let (commitment_1, opening_1) = Pedersen::new(amount_1);
    let (commitment_2, opening_2) = Pedersen::new(amount_2);

    let proof_data = BatchedRangeProofU128Data::new(
        vec![&commitment_1, &commitment_2],
        vec![amount_1, amount_2],
        vec![64, 64],
        vec![&opening_1, &opening_2],
    )
    .unwrap();

    verify_batched_verify_range_proof_u128(None, &proof_data)
}

async fn new_validator_for_test() -> (TestValidator, Keypair) {
    solana_logger::setup();
    let test_validator_genesis = TestValidatorGenesis::default();
    test_validator_genesis.start_async().await
}
