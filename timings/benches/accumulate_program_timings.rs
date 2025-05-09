use {
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    rand::{thread_rng, Rng},
    solana_pubkey::Pubkey,
    solana_timings::ExecuteDetailsTimings,
};

// bench how much time spend on accumulate per-program-timing for a block
fn bench_accumulate_program_timings(c: &mut Criterion) {
    let num_txs = 3000; // number txs per block
    let num_ixs = 4; // number of ixs per tx

    let mut rng = thread_rng();

    // Construct the instructino process results for all txs.
    let test_data: Vec<Vec<(Pubkey, u64, u64, bool)>> = (0..num_txs)
        .map(|_| {
            (0..num_ixs)
                .map(|_| {
                    let key = Pubkey::new_unique(); // probably not always unique in real world
                    let execute_time = rng.gen_range(0..100);
                    let execute_units = rng.gen_range(0..9000);
                    let is_err = rng.gen_bool(0.9); // 90% instruction process are success
                    (key, execute_time, execute_units, is_err)
                })
                .collect()
        })
        .collect();

    let mut execute_details_timings = ExecuteDetailsTimings::default();
    c.bench_function("accumulate program timing", |b| {
        b.iter(|| {
            for tx in black_box(&test_data) {
                for (pubkey, execute_time, execute_units, is_err) in tx {
                    execute_details_timings.accumulate_program(
                        black_box(pubkey),
                        black_box(*execute_time),
                        black_box(*execute_units),
                        black_box(*is_err),
                    );
                }
            }
        });
    });
}

criterion_group!(benches, bench_accumulate_program_timings,);
criterion_main!(benches);
