use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_message::{compiled_instruction::CompiledInstruction, Message},
    solana_pubkey::Pubkey,
    solana_transaction_status::extract_memos::{spl_memo_id_v1, spl_memo_id_v3, ExtractMemos},
};

fn bench_extract_memos(c: &mut Criterion) {
    let mut account_keys: Vec<Pubkey> = (0..64).map(|_| Pubkey::new_unique()).collect();
    account_keys[62] = spl_memo_id_v1();
    account_keys[63] = spl_memo_id_v3();
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

    c.bench_function("extract_memos", |b| {
        b.iter(|| message.extract_memos());
    });
}

criterion_group!(benches, bench_extract_memos);
criterion_main!(benches);
