use {
    criterion::{Criterion, criterion_group, criterion_main},
    rand::{CryptoRng, Rng, SeedableRng},
    rand_chacha::ChaCha20Rng,
    solana_gossip::{
        contact_info::ContactInfo,
        crds::{Crds, GossipRoute},
        crds_data::CrdsData,
        crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
        crds_value::CrdsValue,
        ping_pong::{Ping, PingCache, Pong},
    },
    solana_keypair::{Keypair, Signer},
    std::{
        net::SocketAddr,
        time::{Duration, Instant},
    },
};

fn new_ping_cache(rng: &mut (impl Rng + CryptoRng)) -> PingCache<32> {
    // Keep ttl/rate_limit_delay consistent with production defaults but smaller
    // capacity for benchmarks.
    PingCache::new(
        rng,
        Instant::now(),
        Duration::from_secs(1280),
        Duration::from_secs(20),
        /*cap=*/ 4096,
    )
}

fn bench_deferred_contact_info_insert_on_pong(c: &mut Criterion) {
    let mut rng = ChaCha20Rng::from_seed([7u8; 32]);
    let this_node = Keypair::new();
    let remote_keypair = Keypair::new();
    let remote_socket: SocketAddr = "127.0.0.1:12345".parse().unwrap();
    let remote_node = (remote_keypair.pubkey(), remote_socket);
    let remote_ci = {
        let mut ci = ContactInfo::new_localhost(&remote_keypair.pubkey(), /*wallclock=*/ 0);
        ci.set_gossip(remote_socket).unwrap();
        ci
    };
    let remote_crds_value = CrdsValue::new(CrdsData::ContactInfo(remote_ci), &remote_keypair);

    c.bench_function("bench_deferred_contact_info_insert_on_pong", |b| {
        b.iter(|| {
            let mut ping_cache = new_ping_cache(&mut rng);
            let mut crds = Crds::default();
            let now = Instant::now();

            // Emulate ingress gating: create a ping and defer the ContactInfo.
            let (_check, ping) = ping_cache.check(&mut rng, &this_node, now, remote_node);
            let ping: Ping<32> = ping.expect("ping must be generated for new remote");

            ping_cache.defer_contact_info(remote_node, now, remote_crds_value.clone());

            // Remote responds; we add pong, then flush deferred value into crds.
            let pong = Pong::new(&ping, &remote_keypair);
            assert!(ping_cache.add(&pong, remote_socket, now));
            let value = ping_cache
                .take_deferred_contact_info(&remote_node, now)
                .expect("deferred value must be present");
            assert!(
                crds.insert(
                    value,
                    /*local_timestamp=*/ 0,
                    GossipRoute::PullResponse
                )
                .is_ok()
            );
            // Ensure observable effect.
            assert!(crds.get::<&ContactInfo>(remote_keypair.pubkey()).is_some());
        })
    });
}

fn bench_contact_info_insert_via_pull_after_refresh_delay(c: &mut Criterion) {
    let mut rng = ChaCha20Rng::from_seed([11u8; 32]);
    let this_node = Keypair::new();
    let remote_keypair = Keypair::new();
    let remote_socket: SocketAddr = "127.0.0.1:12345".parse().unwrap();
    let remote_node = (remote_keypair.pubkey(), remote_socket);
    let remote_ci = {
        let mut ci = ContactInfo::new_localhost(&remote_keypair.pubkey(), /*wallclock=*/ 0);
        ci.set_gossip(remote_socket).unwrap();
        ci
    };
    let remote_crds_value = CrdsValue::new(CrdsData::ContactInfo(remote_ci), &remote_keypair);
    let simulated_delay = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / 2);

    c.bench_function(
        "bench_contact_info_insert_via_pull_after_refresh_delay",
        |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let mut ping_cache = new_ping_cache(&mut rng);
                    let mut crds = Crds::default();

                    // Emulate old ingress behavior: ContactInfo is discarded and only a
                    // ping is potentially generated.
                    let t0 = Instant::now();
                    let now = Instant::now();
                    let (_check, _ping) = ping_cache.check(&mut rng, &this_node, now, remote_node);
                    let t1 = Instant::now();

                    // Old behavior relies on the next pull refresh cadence; model the
                    // "blind window" as a simulated delay without sleeping.
                    let t2 = Instant::now();
                    assert!(
                        crds.insert(
                            remote_crds_value.clone(),
                            /*local_timestamp=*/ 0,
                            GossipRoute::PullResponse,
                        )
                        .is_ok()
                    );
                    let t3 = Instant::now();

                    total = total
                        .saturating_add(t1.saturating_duration_since(t0))
                        .saturating_add(simulated_delay)
                        .saturating_add(t3.saturating_duration_since(t2));
                }
                total
            })
        },
    );
}

criterion_group!(
    benches,
    bench_deferred_contact_info_insert_on_pong,
    bench_contact_info_insert_via_pull_after_refresh_delay,
);
criterion_main!(benches);
