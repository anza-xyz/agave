use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_streamer::{
        nonblocking::{
            stream_throttle::{Refiller, StreamRateLimiter},
            testing_utilities::fill_connection_table,
        },
        quic::StreamerStats,
    },
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::Arc,
    },
    tokio::sync::Mutex,
};

const NUM_CLIENTS: usize = 10000;
fn bench_refiller(c: &mut Criterion) {
    let stats = Arc::new(StreamerStats::default());
    let sockets: Vec<_> = (0..NUM_CLIENTS as u32)
        .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::from_bits(i)), 0))
        .collect();

    let rate_limiters: Vec<_> = sockets
        .iter()
        .map(|_| Arc::new(StreamRateLimiter::new_unstaked()))
        .collect();
    let connection_table1 = fill_connection_table(&sockets, &rate_limiters, stats.clone());
    let connection_table2 = fill_connection_table(&sockets, &rate_limiters, stats);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    // Using mutexes here is obviously not ideal,
    // but we are looking to prove that refilling is not expensive,
    // rather than to get super accurate data
    let refiller =
        Arc::new(Mutex::new(rt.block_on(async {
            Refiller::new(connection_table1, connection_table2).await
        })));

    c.bench_function(&format!("do_refill_{NUM_CLIENTS}"), |b| {
        b.to_async(&rt).iter(|| async {
            refiller.lock().await.do_refill(100000).await;
        });
    });
}

criterion_group!(benches, bench_refiller);

criterion_main!(benches);
