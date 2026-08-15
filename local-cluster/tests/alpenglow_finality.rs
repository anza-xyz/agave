#![allow(clippy::arithmetic_side_effects)]
//! Finality-path coverage for Alpenglow local clusters.
//!
//! The existing Alpenglow local-cluster tests assert liveness (roots keep
//! advancing) but do not observe *how* finalization tracks the tip. These
//! tests measure two per-slot signals while the cluster runs:
//!
//! * wall-clock latency from a slot first being seen at `processed`
//!   commitment to that slot reaching `finalized` commitment, and
//! * finalized depth: how many slots `finalized` trails `processed`.
//!
//! Absolute wall-clock latency on a colocated single-machine cluster is not
//! representative of a real network, so it is logged for inspection rather
//! than asserted. Finalized depth is protocol-defined (Alpenglow finalizes
//! within a slot or two of the tip; TowerBFT roots a fixed 32 slots behind),
//! which makes it a stable regression signal, and the liveness boundary at
//! 60% online stake (SIMD-0326 notarization/finalization threshold) is
//! asserted directly on both sides.

use {
    log::*,
    serial_test::serial,
    solana_commitment_config::CommitmentConfig,
    solana_core::validator::ValidatorConfig,
    solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
    solana_keypair::{Keypair, keypair_from_seed},
    solana_local_cluster::{
        cluster::Cluster,
        integration_tests::{AG_DEBUG_LOG_FILTER, DEFAULT_NODE_STAKE, ValidatorKeys},
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::make_identical_validator_configs,
    },
    solana_net_utils::SocketAddrSpace,
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_signer::Signer,
    std::{
        collections::HashMap,
        sync::Arc,
        thread::sleep,
        time::{Duration, Instant},
    },
};

/// Per-slot finality observations collected from one node over a fixed window.
struct FinalityObservations {
    /// Wall-clock ms from a slot first seen at `processed` to first seen at
    /// `finalized`. Slots already processed before the window opened are
    /// excluded, so every sample covers the slot's full journey.
    latency_ms: Vec<f64>,
    /// `processed - finalized` slot gap, sampled once per poll iteration.
    finalized_depth: Vec<u64>,
}

impl FinalityObservations {
    fn latency_p50_p90(&self) -> (f64, f64) {
        (pctl(&self.latency_ms, 50.0), pctl(&self.latency_ms, 90.0))
    }

    fn depth_p50(&self) -> u64 {
        let mut sorted = self.finalized_depth.clone();
        sorted.sort_unstable();
        if sorted.is_empty() {
            return 0;
        }
        sorted[sorted.len() / 2]
    }
}

fn pctl(samples: &[f64], p: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("latency samples are finite"));
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * sorted.len() as f64).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Minimum finalized slot across the given nodes, or `None` if none respond.
fn min_finalized(cluster: &LocalCluster, pubkeys: &[Pubkey]) -> Option<u64> {
    let mut min = u64::MAX;
    let mut any = false;
    for pk in pubkeys {
        if let Some(ci) = cluster.get_contact_info(pk)
            && let Some(addr) = ci.rpc()
            && let Ok(slot) = RpcClient::new(format!("http://{addr}"))
                .get_slot_with_commitment(CommitmentConfig::finalized())
        {
            any = true;
            min = min.min(slot);
        }
    }
    any.then_some(min)
}

/// Tight-polls one node, timestamping each slot when first seen at `processed`
/// and again when first seen at `finalized`.
fn observe_finality(rpc: &RpcClient, window: Duration) -> FinalityObservations {
    let start = Instant::now();
    let mut processed_at: HashMap<u64, Instant> = HashMap::new();
    let mut observations = FinalityObservations {
        latency_ms: Vec::new(),
        finalized_depth: Vec::new(),
    };
    let mut hi_processed: Option<u64> = None;
    let mut hi_finalized: Option<u64> = None;

    while start.elapsed() < window {
        let processed = rpc.get_slot_with_commitment(CommitmentConfig::processed());
        let finalized = rpc.get_slot_with_commitment(CommitmentConfig::finalized());
        let now = Instant::now();
        if let (Ok(processed), Ok(finalized)) = (processed, finalized) {
            match hi_processed {
                None => {
                    processed_at.insert(processed, now);
                }
                Some(hi) => {
                    for slot in (hi + 1)..=processed {
                        processed_at.insert(slot, now);
                    }
                }
            }
            hi_processed = Some(hi_processed.map_or(processed, |hi| hi.max(processed)));
            if let Some(hi) = hi_finalized {
                for slot in (hi + 1)..=finalized {
                    if let Some(t0) = processed_at.remove(&slot) {
                        observations
                            .latency_ms
                            .push(now.duration_since(t0).as_secs_f64() * 1000.0);
                    }
                }
            }
            hi_finalized = Some(hi_finalized.map_or(finalized, |hi| hi.max(finalized)));
            observations
                .finalized_depth
                .push(processed.saturating_sub(finalized));
        }
        sleep(Duration::from_millis(5));
    }
    observations
}

/// Boots an `num_nodes` equal-stake cluster, waits until it is provably
/// finalizing, optionally takes `num_offline_nodes` offline, then observes the
/// finality path from one surviving node for `window`.
fn run_finality_scenario(
    num_nodes: usize,
    num_offline_nodes: usize,
    is_alpenglow: bool,
    window: Duration,
) -> FinalityObservations {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    let validator_keys = (0..num_nodes)
        .map(|i| {
            (
                ValidatorKeys {
                    node_keypair: Arc::new(keypair_from_seed(&[i as u8; 32]).unwrap()),
                    vote_keypair: Arc::new(Keypair::new()),
                },
                true,
            )
        })
        .collect::<Vec<_>>();
    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.wait_for_supermajority = Some(0);

    let mut config = ClusterConfig {
        validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
        validator_keys: Some(validator_keys.clone()),
        node_stakes: vec![DEFAULT_NODE_STAKE; num_nodes],
        // Tower needs its normal cadence here: with 8-tick slots several
        // validators can fork before gossip converges and wedge the healthy
        // baseline. Latency is never compared across the two cadences.
        ticks_per_slot: if is_alpenglow { 8 } else { 64 },
        slots_per_epoch: MINIMUM_SLOTS_PER_EPOCH * 2,
        stakers_slot_offset: MINIMUM_SLOTS_PER_EPOCH * 2,
        poh_config: PohConfig {
            target_tick_duration: PohConfig::default().target_tick_duration,
            hashes_per_tick: None,
            target_tick_count: None,
        },
        ..ClusterConfig::default()
    };
    let mut cluster = if is_alpenglow {
        LocalCluster::new_alpenglow(&mut config, SocketAddrSpace::Unspecified)
    } else {
        LocalCluster::new(&mut config, SocketAddrSpace::Unspecified)
    };
    assert_eq!(cluster.validators.len(), num_nodes);

    // Warmup: require finalized slot 40 so both consensus modes are past
    // startup and Tower's 32-deep rooting pipeline is full before the fault.
    let all_pubkeys = cluster.get_node_pubkeys();
    let warmup_cap = Duration::from_secs(if is_alpenglow { 90 } else { 180 });
    let warmup = Instant::now();
    loop {
        if let Some(slot) = min_finalized(&cluster, &all_pubkeys)
            && slot >= 40
        {
            break;
        }
        assert!(
            warmup.elapsed() < warmup_cap,
            "healthy cluster failed to reach finalized slot 40 within {warmup_cap:?}"
        );
        sleep(Duration::from_millis(400));
    }

    if num_offline_nodes > 0 {
        info!("Shutting down {num_offline_nodes} nodes");
        for (key, _) in validator_keys.iter().take(num_offline_nodes) {
            cluster.exit_node(&key.node_keypair.pubkey());
        }
        // Let votes cast before the fault drain so the observation window
        // reflects steady state under the fault.
        sleep(Duration::from_secs(5));
    }

    let alive = cluster.get_node_pubkeys();
    let rpc_addr = alive
        .iter()
        .find_map(|pk| cluster.get_contact_info(pk).and_then(|ci| ci.rpc()))
        .expect("at least one surviving node exposes RPC");
    let observations = observe_finality(&RpcClient::new(format!("http://{rpc_addr}")), window);

    let (p50, p90) = observations.latency_p50_p90();
    info!(
        "finality observations: {} slots finalized in window, latency p50={p50:.1}ms \
         p90={p90:.1}ms (colocated; informational only), finalized depth p50={} slots",
        observations.latency_ms.len(),
        observations.depth_p50(),
    );
    observations
}

/// All-online Alpenglow baseline: finalization keeps pace with the tip and
/// per-slot latency is logged for inspection. This does not assert absolute
/// latency, which is machine-dependent on a colocated cluster.
#[test]
#[serial]
fn test_alpenglow_finality_latency_4() {
    const NUM_NODES: usize = 4;
    let observations = run_finality_scenario(
        NUM_NODES,
        0,
        /* is_alpenglow */ true,
        Duration::from_secs(30),
    );
    assert!(
        observations.latency_ms.len() >= 8,
        "expected at least 8 slots to finalize within the window, got {}",
        observations.latency_ms.len()
    );
}

/// Exactly 60% of stake online — the SIMD-0326 notarization/finalization
/// threshold. Finalization must continue at the boundary.
#[test]
#[serial]
fn test_alpenglow_finality_at_liveness_threshold() {
    const NUM_NODES: usize = 5;
    const NUM_OFFLINE: usize = 2;
    let observations = run_finality_scenario(
        NUM_NODES,
        NUM_OFFLINE,
        /* is_alpenglow */ true,
        Duration::from_secs(45),
    );
    assert!(
        observations.latency_ms.len() >= 8,
        "cluster at exactly 60% online stake should keep finalizing, got {} slots",
        observations.latency_ms.len()
    );
}

/// Below the 60%-online threshold (40% online) no finalization certificate can
/// form, so finalized slots must stop advancing.
#[test]
#[serial]
fn test_alpenglow_finality_stalls_below_liveness_threshold() {
    const NUM_NODES: usize = 5;
    const NUM_OFFLINE: usize = 3;
    let observations = run_finality_scenario(
        NUM_NODES,
        NUM_OFFLINE,
        /* is_alpenglow */ true,
        Duration::from_secs(45),
    );
    // Certificates already in flight when the fault lands may finalize one or
    // two more slots; sustained progress is impossible below the threshold.
    assert!(
        observations.latency_ms.len() <= 2,
        "cluster at 40% online stake should stall, but finalized {} slots",
        observations.latency_ms.len()
    );
}

/// TowerBFT control on identical hardware: rooting trails the tip by the
/// protocol-defined 32 slots, which validates the depth instrumentation and
/// pins the behavior Alpenglow replaces. This does not compare wall-clock
/// latency across the two consensus modes, which run at different test
/// cadences.
#[test]
#[serial]
fn test_tower_finality_depth_control() {
    const NUM_NODES: usize = 5;
    let observations = run_finality_scenario(
        NUM_NODES,
        0,
        /* is_alpenglow */ false,
        Duration::from_secs(60),
    );
    let depth = observations.depth_p50();
    assert!(
        (30..=34).contains(&depth),
        "TowerBFT finalized depth should be ~32 slots behind processed, got {depth}"
    );
}
