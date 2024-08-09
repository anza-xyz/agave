use std::cell::RefCell;
use std::rc::Rc;
use criterion::{criterion_group, criterion_main, Criterion};
use solana_log_collector::{ic_logger_msg, LogCollector};

const PROGRAM_ID: &str = "11111111111111111111111111111111";

fn criterion_benchmark(c: &mut Criterion) {


    c.bench_function("LogCollector using to_string", |b| {
        let mut lc = LogCollector::default();
        b.iter(|| {
            for _i in 0..10000 {
                lc.log("This is a test.");
            }
        })
    });

    c.bench_function("ic_logger_msg using format!", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        b.iter(|| {
                ic_logger_msg!(&lc, "Program {} success", PROGRAM_ID);
        })
    });

    c.bench_function("ic_logger_msg using string push", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        b.iter(|| {
            let mut result = String::with_capacity(45);
            result.push_str("Program ");
            result.push_str(PROGRAM_ID.to_string().as_str());
            result.push_str(" success");

            ic_logger_msg!(&lc, result.as_str());
        })
    });

    c.bench_function("ic_logger_msg using array join", |b| {
        let lc: Option<Rc<RefCell<LogCollector>>> = Some(LogCollector::new_ref());
        b.iter(|| {
            ic_logger_msg!(&lc, &["Program ", PROGRAM_ID, " success"].join(""));
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);