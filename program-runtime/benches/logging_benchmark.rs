use criterion::{criterion_group, criterion_main, Criterion};
use solana_log_collector::LogCollector;
use solana_program_runtime::{stable_log, stable_log_old,};
use solana_sdk::pubkey::Pubkey;
use std::cell::RefCell;
use std::rc::Rc;

const PROGRAM_ID: &str = "11111111111111111111111111111111";
const TEST_DATA: &[u8] = "This is some test data".as_bytes();

const NESTED_TEST_DATA: &[&[u8]] = &["This is some test data".as_bytes(), "This is more data".as_bytes()];

fn criterion_benchmark(c: &mut Criterion) {

    c.bench_function("program_invoke new", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log::program_invoke(&lc, &program_id, 1);
        })
    });

    c.bench_function("program_invoke old", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log_old::program_invoke(&lc, &program_id, 1);
        })
    });

    c.bench_function("program_success new", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log::program_success(&lc, &program_id);
        })
    });

    c.bench_function("program_success old", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log_old::program_success(&lc, &program_id);
        })
    });

    c.bench_function("program_return new", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log::program_return(&lc, &program_id, TEST_DATA);
        })
    });

    c.bench_function("program_return old", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log_old::program_return(&lc, &program_id, TEST_DATA);
        })
    });

    c.bench_function("program_data new", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        b.iter(|| {
            stable_log::program_data(&lc, NESTED_TEST_DATA);
        })
    });

    c.bench_function("program_data old", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        b.iter(|| {
            stable_log_old::program_data(&lc, NESTED_TEST_DATA);
        })
    });

    c.bench_function("complete program logs new", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log::program_invoke(&lc, &program_id, 1);
            stable_log::program_data(&lc, NESTED_TEST_DATA);
            stable_log::program_data(&lc, NESTED_TEST_DATA);
            stable_log::program_data(&lc, NESTED_TEST_DATA);
            stable_log::program_success(&lc, &program_id);
            stable_log::program_return(&lc, &program_id, TEST_DATA);
        })
    });

    c.bench_function("complete program logs old", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        let program_id: Pubkey = Pubkey::new_unique();
        b.iter(|| {
            stable_log_old::program_invoke(&lc, &program_id, 1);
            stable_log_old::program_data(&lc, NESTED_TEST_DATA);
            stable_log_old::program_data(&lc, NESTED_TEST_DATA);
            stable_log_old::program_data(&lc, NESTED_TEST_DATA);
            stable_log_old::program_success(&lc, &program_id);
            stable_log_old::program_return(&lc, &program_id, TEST_DATA);
        })
    });

}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);