use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    solana_message::{Message, compiled_instruction::CompiledInstruction},
    solana_pubkey::Pubkey,
    solana_transaction_status::extract_memos::ExtractMemos,
};

#[allow(clippy::arithmetic_side_effects)]
fn bench_extract_memos(b: &mut Bencher) {
    let mut account_keys: Vec<Pubkey> = (0..64).map(|_| Pubkey::new_unique()).collect();
    account_keys[62] = spl_memo_interface::v1::id();
    account_keys[63] = spl_memo_interface::v3::id();
    let memo = "Test memo";

    let instructions: Vec<_> = (0..20)
        .map(|i| CompiledInstruction {
            program_id_index: 62 + (i % 2),
            accounts: vec![],
            data: memo.as_bytes().to_vec(),
        })
        .collect();

    let message = Message {
        account_keys,
        instructions,
        ..Message::default()
    };

    b.iter(|| message.extract_memos());
}

benchmark_group!(benches, bench_extract_memos);
benchmark_main!(benches);
