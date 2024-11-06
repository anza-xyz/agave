use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use solana_log_collector::LogCollector;
use solana_program_runtime::{stable_log, stable_log_old};
use solana_sdk::pubkey::Pubkey;

fn bench_program_logging(c: &mut Criterion) {
    let mut group = c.benchmark_group("program_return");

    // Benchmark small data
    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let small_data = vec![1, 2, 3, 4];

        group.bench_function("small_data", |b| {
            b.iter(|| {
                stable_log::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&small_data),
                )
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let small_data = vec![1, 2, 3, 4];

        group.bench_function("small_data2", |b| {
            b.iter(|| {
                stable_log::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&small_data),
                )
            })
        });
    }

    // Benchmark large data
    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let large_data = vec![42; 1000];

        group.bench_function("large_data", |b| {
            b.iter(|| {
                stable_log::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&large_data),
                )
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let large_data = vec![42; 1000];

        group.bench_function("large_data2", |b| {
            b.iter(|| {
                stable_log_old::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&large_data),
                )
            })
        });
    }

    // Benchmark with no collector
    {
        let program_id = Pubkey::new_unique();
        let data = vec![1, 2, 3, 4];

        group.bench_function("no_collector", |b| {
            b.iter(|| {
                stable_log::program_return(
                    black_box(&None),
                    black_box(&program_id),
                    black_box(&data),
                )
            })
        });
    }

    {
        let program_id = Pubkey::new_unique();
        let data = vec![1, 2, 3, 4];

        group.bench_function("no_collector2", |b| {
            b.iter(|| {
                stable_log_old::program_return(
                    black_box(&None),
                    black_box(&program_id),
                    black_box(&data),
                )
            })
        });
    }

    group.finish();

    // Benchmark program_data
    let mut group = c.benchmark_group("program_data");

    // Single item
    {
        let log_collector = Some(LogCollector::new_ref());
        let data = [b"Hello World" as &[u8]];

        group.bench_function("single_item", |b| {
            b.iter(|| {
                stable_log::program_data(black_box(&log_collector), black_box(&data))
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let data = [b"Hello World" as &[u8]];

        group.bench_function("single_item2", |b| {
            b.iter(|| {
                stable_log_old::program_data(black_box(&log_collector), black_box(&data))
            })
        });
    }

    // Multiple items
    {
        let log_collector = Some(LogCollector::new_ref());
        let data = [b"Hello" as &[u8], b"World" as &[u8], b"Test" as &[u8]];

        group.bench_function("multiple_items", |b| {
            b.iter(|| {
                stable_log::program_data(black_box(&log_collector), black_box(&data))
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let data = [b"Hello" as &[u8], b"World" as &[u8], b"Test" as &[u8]];

        group.bench_function("multiple_items2", |b| {
            b.iter(|| {
                stable_log_old::program_data(black_box(&log_collector), black_box(&data))
            })
        });
    }

    group.finish();

    // Benchmark program_invoke
    let mut group = c.benchmark_group("program_invoke");

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let depth = 5;

        group.bench_function("typical", |b| {
            b.iter(|| {
                stable_log::program_invoke(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(depth),
                )
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let depth = 5;

        group.bench_function("typical2", |b| {
            b.iter(|| {
                stable_log_old::program_invoke(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(depth),
                )
            })
        });
    }

    group.finish();

    // Benchmark program_log
    let mut group = c.benchmark_group("program_log");

    {
        let log_collector = Some(LogCollector::new_ref());
        let message = "Test log message";

        group.bench_function("typical", |b| {
            b.iter(|| {
                stable_log::program_log(black_box(&log_collector), black_box(message))
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let message = "Test log message";

        group.bench_function("typical2", |b| {
            b.iter(|| {
                stable_log_old::program_log(black_box(&log_collector), black_box(message))
            })
        });
    }

    group.finish();

    // Benchmark program_success and failure
    let mut group = c.benchmark_group("program_status");

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let error = "Test error message";

        group.bench_function("success", |b| {
            b.iter(|| {
                stable_log::program_success(black_box(&log_collector), black_box(&program_id))
            })
        });

        group.bench_function("failure", |b| {
            b.iter(|| {
                stable_log::program_failure(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&error),
                )
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let error = "Test error message";

        group.bench_function("success2", |b| {
            b.iter(|| {
                stable_log_old::program_success(black_box(&log_collector), black_box(&program_id))
            })
        });

        group.bench_function("failure2", |b| {
            b.iter(|| {
                stable_log_old::program_failure(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&error),
                )
            })
        });
    }

    group.finish();

    // Benchmark with size limits
    let mut group = c.benchmark_group("size_limited");

    {
        let log_collector = Some(LogCollector::new_ref_with_limit(Some(100)));
        let program_id = Pubkey::new_unique();
        let data = vec![42; 200]; // Data that will exceed the limit

        group.bench_function("exceeding_limit", |b| {
            b.iter(|| {
                stable_log::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&data),
                )
            })
        });
    }

    {
        let log_collector = Some(LogCollector::new_ref_with_limit(Some(100)));
        let program_id = Pubkey::new_unique();
        let data = vec![42; 200]; // Data that will exceed the limit

        group.bench_function("exceeding_limit2", |b| {
            b.iter(|| {
                stable_log_old::program_return(
                    black_box(&log_collector),
                    black_box(&program_id),
                    black_box(&data),
                )
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_program_logging);
criterion_main!(benches);