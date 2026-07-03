mod interceptor;
mod invariants;
mod mutations;
mod schedule;
use {
    crate::{
        byzzfuzz::{
            interceptor::{AlpenglowInterceptAction, AlpenglowInterceptor, AlpenglowRng},
            invariants::validate_invariants,
            mutations::maybe_mutate_alpenglow_message,
            schedule::{FaultSchedule, message_slot},
        },
        integration_tests::{
            AG_DEBUG_LOG_FILTER, DEFAULT_NODE_STAKE, create_custom_leader_schedule_with_random_keys,
        },
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::safe_clone_config,
    },
    agave_votor_messages::consensus_message::BLS_KEYPAIR_DERIVE_SEED,
    log::*,
    rand::{Rng, SeedableRng},
    solana_bls_signatures::keypair::Keypair as BLSKeypair,
    solana_clock::Slot,
    solana_core::validator::ValidatorConfig,
    solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
    solana_leader_schedule::FixedSchedule,
    solana_net_utils::SocketAddrSpace,
    solana_signer::Signer,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    },
};

#[test]
fn test_alpenglow_byzfuzz() {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    const NUM_NODES: usize = 4;
    const BYZANTINE_INDICES: &[usize] = &[0];
    const ROOT_SLOT_TO_WAIT_FOR: Slot = 128;
    // Fixed seed for the honest-stake partition so failures are reproducible.
    let random_seed: Option<u64> = match std::env::var("ALPENGLOW_TEST_SEED") {
        Ok(val) => val.parse::<u64>().ok(),
        Err(_) => None,
    };
    let random_seed = random_seed.unwrap_or(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64,
    );
    info!("using seed {random_seed:?}");
    // create leader schedules & keyts
    let (leader_schedule, validator_keys) =
        create_custom_leader_schedule_with_random_keys(&[4; NUM_NODES]);
    let node_pubkeys = validator_keys
        .iter()
        .map(|keys| keys.node_keypair.pubkey())
        .collect::<Vec<_>>();
    let byzantine_sources = BYZANTINE_INDICES
        .iter()
        .map(|&i| node_pubkeys[i])
        .collect::<HashSet<_>>();
    let byzantine_bls_keypairs = Arc::new(
        BYZANTINE_INDICES
            .iter()
            .map(|&i| {
                let keys = &validator_keys[i];
                let bls_keypair = BLSKeypair::derive_from_signer(
                    keys.vote_keypair.as_ref(),
                    BLS_KEYPAIR_DERIVE_SEED,
                )
                .expect("byzfuzz: derive byzantine BLS keypair");
                (node_pubkeys[i], Arc::new(bls_keypair))
            })
            .collect::<HashMap<_, _>>(),
    );
    let byzantine_sources_for_policy = byzantine_sources.clone();
    let byzantine_bls_keypairs_for_policy = byzantine_bls_keypairs.clone();
    let byzantine_message_count = Arc::new(AtomicU64::new(0));
    let byzantine_message_count_for_policy = byzantine_message_count.clone();

    // set stakes: the byzantine node(s) always hold a combined 19% of cluster
    // stake (kept just under the fault threshold), while the honest nodes split
    // the remaining 81% in random, seed-deterministic proportions.
    const BYZANTINE_STAKE_PERCENT: u64 = 19;
    let total_cluster_stake: u64 = DEFAULT_NODE_STAKE.saturating_mul(NUM_NODES as u64);
    let stake_percents = random_stake_percents(
        random_seed,
        NUM_NODES,
        BYZANTINE_INDICES,
        BYZANTINE_STAKE_PERCENT,
    );
    let node_stakes = stake_percents
        .iter()
        .map(|&pct| percent_of(total_cluster_stake, pct))
        .collect::<Vec<_>>();
    assert_eq!(node_stakes.len(), NUM_NODES);
    let source_stakes = node_pubkeys
        .iter()
        .copied()
        .zip(node_stakes.iter().copied())
        .collect::<HashMap<_, _>>();

    // Sample the entire fault schedule from the seed before the cluster starts,
    // so the faults a run applies depend only on the seed.  Logged here so a
    // failing run prints its own reproduction recipe.
    let schedule = Arc::new(FaultSchedule::sample(
        random_seed,
        &node_pubkeys,
        &source_stakes,
        ROOT_SLOT_TO_WAIT_FOR,
    ));
    info!("byzfuzz fault schedule (seed {random_seed}): {schedule:?}");
    let schedule_for_policy = schedule.clone();

    // Centralize all Alpenglow messages here before adding fuzz actions.
    let proxy = AlpenglowInterceptor::new(&validator_keys, move |intercepted| {
        // Network faults are temporal and apply to every node: a node is
        // isolated while consensus is *progressing through* the window, keyed
        // on logical time, not the message's subject slot.  Keying on subject
        // slot would drop catch-up traffic for those slots forever and stall
        // the isolated node permanently.
        if schedule_for_policy.drops_link(
            intercepted.current_slot,
            &intercepted.source,
            &intercepted.destination,
        ) {
            return AlpenglowInterceptAction::Drop;
        }
        // Corruptions target a slot's agreement, so they key on the message's
        // subject slot, and only apply to byzantine sources: re-signing a
        // mutated message needs that source's BLS key.
        let slot = message_slot(&intercepted.message);
        if byzantine_sources_for_policy.contains(&intercepted.source)
            && schedule_for_policy.in_corruption_window(slot)
        {
            byzantine_message_count_for_policy.fetch_add(1, Ordering::Relaxed);
            let source_bls_keypair = byzantine_bls_keypairs_for_policy
                .get(&intercepted.source)
                .map(|keypair| keypair.as_ref());
            // Content-derived RNG: deterministic per (message, destination), so
            // the same vote to different peers can be mutated differently.
            let mut rng =
                schedule_for_policy.corruption_rng(&intercepted.message, &intercepted.destination);
            if let Some(action) = maybe_mutate_alpenglow_message(
                &intercepted.message,
                intercepted.shred_version,
                &mut rng,
                source_bls_keypair,
            ) {
                return action;
            }
        }
        AlpenglowInterceptAction::Forward
    });

    info!(
        "Alpenglow byzantine test stake distribution (byzantine indices = {BYZANTINE_INDICES:?}, \
         percents = {stake_percents:?}, stakes = {node_stakes:?} (sum = {total_cluster_stake})",
    );
    // initialize validator config
    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.fixed_leader_schedule = Some(FixedSchedule {
        leader_schedule: Arc::new(leader_schedule),
    });
    validator_config.wait_for_supermajority = Some(0);
    //validator_config.alpenglow_invariant_event_sender = Some(invariant_sender);
    let validator_configs = validator_keys
        .iter()
        .map(|_| {
            let mut config = safe_clone_config(&validator_config);
            config.voting_service_test_override = Some(proxy.voting_service_override());
            config
        })
        .collect::<Vec<_>>();
    // initialize cluster
    let mut cluster_config = ClusterConfig {
        validator_configs,
        validator_keys: Some(
            validator_keys
                .iter()
                .cloned()
                .zip(std::iter::repeat(true))
                .collect(),
        ),
        node_stakes: node_stakes.clone(),
        ticks_per_slot: 8,
        slots_per_epoch: MINIMUM_SLOTS_PER_EPOCH,
        stakers_slot_offset: MINIMUM_SLOTS_PER_EPOCH,
        skip_warmup_slots: true,
        ..ClusterConfig::default()
    };
    // Start the cluster only after overrides have been attached.  Then publish
    // the actual destination sockets back into the proxy so workers can forward
    // traffic that arrived during startup.
    let cluster = LocalCluster::new_alpenglow(&mut cluster_config, SocketAddrSpace::Unspecified);
    proxy.set_destinations_from_cluster(&cluster);

    cluster.check_min_slot_is_rooted(
        ROOT_SLOT_TO_WAIT_FOR,
        "test_alpenglow_byzfuzz",
        SocketAddrSpace::Unspecified,
    );
    let byzantine_messages = byzantine_message_count.load(Ordering::Relaxed);
    assert!(
        byzantine_messages > 0,
        "byzfuzz interceptor saw no byzantine messages"
    );
    info!("byzfuzz intercepted {byzantine_messages} byzantine messages");
    drop(cluster);
    validate_invariants(
        &proxy.state.lock().unwrap(),
        &byzantine_sources,
        &source_stakes,
        ROOT_SLOT_TO_WAIT_FOR,
    );
}

fn percent_of(total: u64, percent: u64) -> u64 {
    (total / 100) * percent
}

/// Assign integer stake percentages (summing to exactly 100) across `num_nodes`
/// nodes: the byzantine nodes hold a combined `byzantine_total_percent`, split
/// as evenly as possible, and the honest nodes split the remainder in random,
/// seed-deterministic proportions with every honest node getting at least 1%
/// and no single node exceeding `MAX_NODE_STAKE_PERCENT` (so no node can form a
/// stand-alone supermajority).
fn random_stake_percents(
    seed: u64,
    num_nodes: usize,
    byzantine_indices: &[usize],
    byzantine_total_percent: u64,
) -> Vec<u64> {
    // No single node may hold more than this share of stake.
    const MAX_NODE_STAKE_PERCENT: u64 = 50;
    // Salt so stake randomization is independent of the fault-schedule RNG that
    // also seeds off `seed`.
    let mut rng = AlpenglowRng::seed_from_u64(seed ^ 0x5741_4b45_5f53_544b);
    let honest_indices = (0..num_nodes)
        .filter(|i| !byzantine_indices.contains(i))
        .collect::<Vec<_>>();
    assert!(!byzantine_indices.is_empty(), "need at least one byzantine node");
    assert!(!honest_indices.is_empty(), "need at least one honest node");
    let honest_total_percent = 100 - byzantine_total_percent;
    assert!(
        honest_total_percent as usize >= honest_indices.len(),
        "not enough honest stake to give each honest node at least 1%",
    );
    assert!(
        honest_total_percent <= honest_indices.len() as u64 * MAX_NODE_STAKE_PERCENT,
        "honest stake cannot be spread across nodes under the per-node cap",
    );
    assert!(
        byzantine_total_percent / byzantine_indices.len() as u64 <= MAX_NODE_STAKE_PERCENT,
        "a byzantine node would exceed the per-node cap",
    );

    // Random positive weight per honest node; distribute the honest stake above
    // the per-node floor of 1% in proportion to those weights.
    let weights = honest_indices
        .iter()
        .map(|_| rng.random_range(1..=1000u64))
        .collect::<Vec<_>>();
    let weight_sum: u64 = weights.iter().sum();
    let distributable = honest_total_percent - honest_indices.len() as u64;
    let mut honest_percents = vec![1u64; honest_indices.len()];
    let mut allocated = 0u64;
    for (k, w) in weights.iter().enumerate() {
        let extra = distributable * w / weight_sum;
        honest_percents[k] += extra;
        allocated += extra;
    }
    // Assign the rounding remainder to the first honest node.
    honest_percents[0] += distributable - allocated;

    // Enforce the per-node cap: clamp any honest node above the cap and
    // water-fill the excess one point at a time into nodes that still have
    // headroom, so the total stays exact and no node keeps a supermajority.
    let mut excess = 0u64;
    for p in honest_percents.iter_mut() {
        if *p > MAX_NODE_STAKE_PERCENT {
            excess += *p - MAX_NODE_STAKE_PERCENT;
            *p = MAX_NODE_STAKE_PERCENT;
        }
    }
    let mut k = 0usize;
    while excess > 0 {
        let idx = k % honest_percents.len();
        if honest_percents[idx] < MAX_NODE_STAKE_PERCENT {
            honest_percents[idx] += 1;
            excess -= 1;
        }
        k += 1;
    }

    let mut percents = vec![0u64; num_nodes];
    let per_byz = byzantine_total_percent / byzantine_indices.len() as u64;
    for &i in byzantine_indices {
        percents[i] = per_byz;
    }
    percents[byzantine_indices[0]] +=
        byzantine_total_percent - per_byz * byzantine_indices.len() as u64;
    for (k, &i) in honest_indices.iter().enumerate() {
        percents[i] = honest_percents[k];
    }
    debug_assert_eq!(percents.iter().sum::<u64>(), 100);
    debug_assert!(percents.iter().all(|&p| p <= MAX_NODE_STAKE_PERCENT));
    percents
}
