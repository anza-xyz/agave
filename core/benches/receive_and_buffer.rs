#[path = "receive_and_buffer_utils.rs"]
mod utils;
use {
    criterion::{criterion_group, criterion_main, Criterion, Throughput},
    solana_core::banking_stage::transaction_scheduler::{
        receive_and_buffer::{ReceiveAndBuffer, TransactionViewReceiveAndBuffer},
        transaction_state_container::StateContainer,
    },
    std::{
        hint::black_box,
        time::{Duration, Instant},
    },
};

fn bench_receive_and_buffer<T: ReceiveAndBuffer + utils::ReceiveAndBufferCreator>(
    c: &mut Criterion,
    bench_name: &str,
    num_instructions_per_tx: usize,
    probability_invalid_blockhash: f64,
    set_rand_cu_price: bool,
) {
    let num_txs = 16 * 1024;
    let utils::ReceiveAndBufferSetup {
        txs,
        sender,
        mut container,
        mut receive_and_buffer,
        decision,
    }: utils::ReceiveAndBufferSetup<T> = utils::setup_receive_and_buffer(
        num_txs,
        num_instructions_per_tx,
        probability_invalid_blockhash,
        set_rand_cu_price,
        true, // single fee payer for all transactions
    );

    let mut group = c.benchmark_group("receive_and_buffer");
    group.throughput(Throughput::Elements(num_txs as u64));
    group.bench_function(bench_name, |bencher| {
        bencher.iter_custom(|iters| {
            let mut total: Duration = std::time::Duration::ZERO;
            for _ in 0..iters {
                // Setup
                {
                    if sender.send(txs.clone()).is_err() {
                        panic!("Unexpectedly dropped receiver!");
                    }

                    // make sure container is empty.
                    container.clear();
                }

                let start = Instant::now();
                {
                    let res =
                        receive_and_buffer.receive_and_buffer_packets(&mut container, &decision);
                    assert!(res.unwrap().num_received == num_txs && !container.is_empty());
                    black_box(&container);
                }
                total = total.saturating_add(start.elapsed());
            }
            total
        })
    });
    group.finish();
}

fn bench_transaction_view_receive_and_buffer(c: &mut Criterion) {
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_max_instructions",
        utils::MAX_INSTRUCTIONS_PER_TRANSACTION,
        0.0,
        true,
    );
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_min_instructions",
        1,
        0.0,
        true,
    );
}

criterion_group!(benches, bench_transaction_view_receive_and_buffer);
criterion_main!(benches);
