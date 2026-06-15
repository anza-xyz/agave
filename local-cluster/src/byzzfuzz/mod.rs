mod interceptor;
mod invariants;
mod mutations;
mod schedule;
use agave_votor_messages::consensus_message::BLS_KEYPAIR_DERIVE_SEED;
use log::*;
use solana_bls_signatures::keypair::Keypair as BLSKeypair;
use solana_clock::Slot;
use solana_core::validator::ValidatorConfig;
use solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH;
use solana_leader_schedule::FixedSchedule;
use solana_net_utils::SocketAddrSpace;
use solana_signer::Signer;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    byzzfuzz::interceptor::{AlpenglowInterceptAction, AlpenglowInterceptor},
    byzzfuzz::invariants::validate_invariants,
    byzzfuzz::mutations::maybe_mutate_alpenglow_message,
    byzzfuzz::schedule::{FaultSchedule, message_slot},
    integration_tests::{
        AG_DEBUG_LOG_FILTER, DEFAULT_NODE_STAKE, create_custom_leader_schedule_with_random_keys,
    },
    local_cluster::{ClusterConfig, LocalCluster},
    validator_configs::safe_clone_config,
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

    // set stakes
    let total_cluster_stake: u64 = DEFAULT_NODE_STAKE.saturating_mul(NUM_NODES as u64);
    let node_stakes = vec![
        percent_of(total_cluster_stake, 19),
        percent_of(total_cluster_stake, 21),
        percent_of(total_cluster_stake, 30),
        percent_of(total_cluster_stake, 30),
    ];
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
            if let Some(action) =
                maybe_mutate_alpenglow_message(&intercepted.message, &mut rng, source_bls_keypair)
            {
                return action;
            }
        }
        AlpenglowInterceptAction::Forward
    });

    info!(
        "Alpenglow byzantine test stake distribution \
         (byzantine indices = {BYZANTINE_INDICES:?} \
         {node_stakes:?} (sum = {total_cluster_stake})",
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
