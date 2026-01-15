#![allow(clippy::arithmetic_side_effects)]
use {
    solana_net_utils::token_bucket::*,
    std::{
        net::{IpAddr, Ipv4Addr},
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
    },
};

fn bench_token_bucket() {
    println!("Running bench_token_bucket...");
    let run_duration = Duration::from_secs(5);
    let fill_rate = 10000.0;
    let request_size = 3;
    let target_rate = fill_rate / request_size as f64;
    let tb = TokenBucket::new(1, 600, fill_rate);

    let accepted = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);

    let start = Instant::now();
    let workers = 8;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if start.elapsed() > run_duration {
                        break;
                    }
                    match tb.consume_tokens(request_size) {
                        Ok(_) => accepted.fetch_add(1, Ordering::Relaxed),
                        Err(_) => rejected.fetch_add(1, Ordering::Relaxed),
                    };
                }
            });
        }
        // periodically check for races
        let jh = scope.spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let elapsed = start.elapsed();
                if elapsed > run_duration {
                    break;
                }
                let acc = accepted.load(Ordering::Relaxed);
                let rate = acc as f64 / elapsed.as_secs_f64();
                assert!(
                    tb.current_tokens() < request_size * 2,
                    "bucket should have no spare tokens"
                );
                assert!(
                    // allow 1% error
                    (rate - target_rate).abs() < target_rate / 100.0,
                    "Accepted rate should be about {target_rate}, actual {rate}"
                );
            }
        });
        jh.join().expect("Rate checks should pass");
    });

    let acc = accepted.load(Ordering::Relaxed);
    let rej = rejected.load(Ordering::Relaxed);
    println!("Run complete over {:?} seconds", run_duration.as_secs());
    println!("Accepted {acc}, Rejected: {rej}");
    println!(
        "processed {} requests, {} per second",
        acc + rej,
        (acc + rej) as f32 / run_duration.as_secs_f32()
    );
}

fn bench_token_bucket_eviction() {
    println!("Running bench_token_bucket_eviction...");
    let run_duration = Duration::from_secs(5);
    let target_size = 256;
    let tb = TokenBucket::new(1, 60, 100.0);
    let mut limiter = KeyedRateLimiter::new(target_size, tb, 8);
    // make shrinking more aggressive than default
    // since only one worker is shrinking the
    // datastructure at any given moment so we do not flake this test
    // too hard
    limiter.set_shrink_interval(32);

    let accepted = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);

    let start = Instant::now();
    let ip_pool = 1024;
    let workers = 8;

    let max_size = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                for i in 1.. {
                    if Instant::now() > start + run_duration {
                        break;
                    }
                    let ip = IpAddr::V4(Ipv4Addr::from_bits(i % ip_pool as u32));
                    if limiter.consume_tokens(ip, 1).is_ok() {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    let len_approx = limiter.len_approx();
                    max_size.fetch_max(len_approx, Ordering::Relaxed);
                }
            });
        }
    });

    let acc = accepted.load(Ordering::Relaxed);
    let rej = rejected.load(Ordering::Relaxed);
    println!("Run complete over {:?} seconds", run_duration.as_secs());
    eprintln!("Max observed size was {}", max_size.load(Ordering::Relaxed));
    assert!(
        max_size.load(Ordering::Relaxed) <= target_size * 2,
        "Max target size should never be exceeded"
    );
    println!(
        "processed {} requests, {} per second",
        acc + rej,
        (acc + rej) as f32 / run_duration.as_secs_f32()
    );
    println!("Rejected: {rej}");
}

fn bench_keyed_rate_limiter() {
    println!("Running bench_keyed_rate_limiter...");
    let run_duration = Duration::from_secs(5);
    let tb = TokenBucket::new(1, 60, 100.0);
    let limiter = KeyedRateLimiter::new(2048, tb, 8);

    let accepted = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);

    let start = Instant::now();
    let ip_pool = 2048;
    let expected_total_accepts = (run_duration.as_secs() * 100 * ip_pool) as i64;
    let workers = 32;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                for i in 1.. {
                    if Instant::now() > start + run_duration {
                        break;
                    }
                    let ip = IpAddr::V4(Ipv4Addr::from_bits(i % ip_pool as u32));
                    if limiter.consume_tokens(ip, 1).is_ok() {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let acc = accepted.load(Ordering::Relaxed);
    let rej = rejected.load(Ordering::Relaxed);
    println!("Run complete over {:?} seconds", run_duration.as_secs());
    println!("Accepted: {acc} (target {expected_total_accepts})");
    println!("Rejected: {rej}");
    println!(
        "processed {} requests, {} per second",
        acc + rej,
        (acc + rej) as f32 / run_duration.as_secs_f32()
    );
    assert!(((acc as i64) - expected_total_accepts).abs() < expected_total_accepts / 10);
}

fn main() {
    bench_token_bucket();
    println!("==========");
    bench_token_bucket_eviction();
    println!("==========");
    bench_keyed_rate_limiter();
}
