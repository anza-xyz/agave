#![allow(dead_code, unused_imports, unused_variables)]
pub mod interceptor;
pub mod invariants;
mod mutations;
mod schedule;
use {
    crate::{
        byzzfuzz::{
            interceptor::{
                AlpenglowInterceptAction, AlpenglowInterceptor, AlpenglowRng,
                describe_consensus_message, describe_intercept_action,
            },
            invariants::{report_coverage, validate_invariants},
            mutations::{Mutations, maybe_mutate_alpenglow_message},
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
    solana_core::{
        repair::{
            malicious_repair_handler::MaliciousRepairConfig, repair_handler::RepairHandlerType,
        },
        validator::ValidatorConfig,
    },
    solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
    solana_leader_schedule::FixedSchedule,
    solana_ledger::shred::filter::{TurbineMode, TurbineModeKind},
    solana_net_utils::SocketAddrSpace,
    solana_signer::Signer,
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    },
};

const ROOT_SLOT_TO_WAIT_FOR: Slot = 32;
const BYZANTINE_STAKE_PERCENT: u64 = 19;

#[test]
fn test_alpenglow_byzfuzz() {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    run_alpenglow_byzfuzz("test_alpenglow_byzfuzz", 7, &[0, 5]);
}

fn run_alpenglow_byzfuzz(
    test_name: &'static str,
    num_nodes: usize,
    byzantine_indices: &'static [usize],
) {
    // Fixed seed for the stake partition and fault schedule so failures are reproducible.
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
    info!("{test_name}: using seed {random_seed:?}");

    let leader_slots = vec![4; num_nodes];
    let (leader_schedule, validator_keys) =
        create_custom_leader_schedule_with_random_keys(&leader_slots);
    let node_pubkeys = validator_keys
        .iter()
        .map(|keys| keys.node_keypair.pubkey())
        .collect::<Vec<_>>();
    let byzantine_sources = byzantine_indices
        .iter()
        .map(|&i| node_pubkeys[i])
        .collect::<HashSet<_>>();
    let byzantine_bls_keypairs = Arc::new(
        byzantine_indices
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
    // Coverage: histogram of which concrete mutation variants actually took
    // effect (returned an action), keyed by the mutation's Debug name. Reveals
    // which fuzzing surface a run exercised and which stays unreached.
    let mutation_coverage = Arc::new(Mutex::new(BTreeMap::<String, u64>::new()));
    let mutation_coverage_for_policy = mutation_coverage.clone();
    // Optional targeting knob: force every byzantine corruption to apply one
    // specific mutation, so hand-built schedules can deterministically drive a
    // chosen adversarial behavior into rarely-covered votor paths.
    let forced_mutation = std::env::var("ALPENGLOW_FORCE_MUTATION")
        .ok()
        .and_then(|name| {
            let parsed = Mutations::from_name(&name);
            if parsed.is_none() {
                warn!("{test_name}: unknown ALPENGLOW_FORCE_MUTATION={name:?}, ignoring");
            }
            parsed
        });
    info!("{test_name}: forced_mutation = {forced_mutation:?}");

    // The byzantine identity/identities hold a combined 19% of cluster stake
    // (kept just under the fault threshold), while the honest identities split
    // the remaining 81% in random, seed-deterministic proportions.
    let total_cluster_stake: u64 = DEFAULT_NODE_STAKE.saturating_mul(num_nodes as u64);
    let stake_percents = random_stake_percents(
        random_seed,
        num_nodes,
        byzantine_indices,
        BYZANTINE_STAKE_PERCENT,
    );
    let node_stakes = stake_percents
        .iter()
        .map(|&pct| percent_of(total_cluster_stake, pct))
        .collect::<Vec<_>>();
    assert_eq!(node_stakes.len(), num_nodes);
    let source_stakes = node_pubkeys
        .iter()
        .copied()
        .zip(node_stakes.iter().copied())
        .collect::<HashMap<_, _>>();

    // Sample the entire fault schedule from the seed before the cluster starts,
    // so the faults a run applies depend only on the seed. Logged here so a
    // failing run prints its own reproduction recipe.
    //
    // ALPENGLOW_MANUAL_SCHEDULE overrides the sampled schedule with a hand-built
    // one aimed at a specific rarely-covered votor path (the guardrailed sampler
    // never lines these up). Currently: "intrawindow_repair".
    let schedule = match std::env::var("ALPENGLOW_MANUAL_SCHEDULE").ok().as_deref() {
        Some("intrawindow_repair") => {
            // Isolate the lowest-stake honest node over the parent+child of several
            // intrawindow leader slots. The observer misses those blocks while the
            // supermajority notarizes them; on reconnect it ingests the notarize
            // votes and fires safe-to-notar for a block it never voted, pushing it
            // onto pending_safe_to_notar (consensus_pool_service::process_pending_
            // safe_to_notar -> request_repair, the 0%-covered repair/re-queue path).
            let byz: HashSet<usize> = byzantine_indices.iter().copied().collect();
            let observer = (0..num_nodes)
                .filter(|i| !byz.contains(i))
                .min_by_key(|&i| node_stakes[i])
                .expect("byzfuzz: need an honest observer node");
            let observer_pk = node_pubkeys[observer];
            info!(
                "{test_name}: MANUAL intrawindow_repair observer=idx{observer} ({observer_pk}) \
                 stake={}",
                node_stakes[observer]
            );
            // child slots c with c%4 in {2,3} (intrawindow); isolate [c-1 ..= c] so
            // both the child and its parent are missed by the observer.
            let mut faults = Vec::new();
            let mut c = 6;
            while c <= ROOT_SLOT_TO_WAIT_FOR.saturating_sub(2) {
                faults.push(((c - 1)..=c, HashSet::from([observer_pk])));
                c += 4;
            }
            // Corruption across the whole run so byzantine nodes equivocate their
            // notarize block_ids (pair with ALPENGLOW_FORCE_MUTATION=NotarizeEquivocation).
            // Votes for a phantom block_id the observer never has in blockstore are
            // what can drive safe-to-notar -> repair (block-not-received) branch.
            let corruption = vec![1..=ROOT_SLOT_TO_WAIT_FOR];
            Arc::new(FaultSchedule::manual(faults, corruption, random_seed))
        }
        Some(other) => {
            warn!("{test_name}: unknown ALPENGLOW_MANUAL_SCHEDULE={other:?}, using sampler");
            Arc::new(FaultSchedule::sample(
                random_seed,
                &node_pubkeys,
                &source_stakes,
                ROOT_SLOT_TO_WAIT_FOR,
            ))
        }
        None => Arc::new(FaultSchedule::sample(
            random_seed,
            &node_pubkeys,
            &source_stakes,
            ROOT_SLOT_TO_WAIT_FOR,
        )),
    };
    info!("{test_name}: byzfuzz fault schedule (seed {random_seed}): {schedule:#?}");
    let schedule_for_policy = schedule.clone();

    // Centralize all Alpenglow messages here before adding fuzz actions.
    let proxy = AlpenglowInterceptor::new(&validator_keys, move |intercepted| {
        // Network faults are temporal and apply to every node: a node is
        // isolated while consensus is *progressing through* the window, keyed
        // on logical time, not the message's subject slot. Keying on subject
        // slot would drop catch-up traffic for those slots forever and stall
        // the isolated node permanently.
        if schedule_for_policy.drops_link(
            intercepted.current_slot,
            &intercepted.source,
            &intercepted.destination,
        ) {
            debug!(
                "byzfuzz policy current_slot={} source={} destination={} msg=\"{}\" \
                 action=\"drop\" reason=network_isolation",
                intercepted.current_slot,
                intercepted.source,
                intercepted.destination,
                describe_consensus_message(&intercepted.message),
            );
            return AlpenglowInterceptAction::Drop;
        }

        // Corruptions target a slot's agreement, so they key on the message's
        // subject slot, and only apply to byzantine sources: re-signing a
        // mutated message needs that source's BLS key.
        let slot = message_slot(&intercepted.message);
        if byzantine_sources_for_policy.contains(&intercepted.source)
            && schedule_for_policy.in_corruption_window(slot)
        {
            let source_bls_keypair = byzantine_bls_keypairs_for_policy
                .get(&intercepted.source)
                .map(|keypair| keypair.as_ref());
            // Content-derived RNG: deterministic per (message, destination), so
            // the same vote to different peers can be mutated differently.
            let mut rng =
                schedule_for_policy.corruption_rng(&intercepted.message, &intercepted.destination);

            byzantine_message_count_for_policy.fetch_add(1, Ordering::Relaxed);
            if let Some(result) = maybe_mutate_alpenglow_message(
                &intercepted.message,
                intercepted.shred_version,
                &mut rng,
                source_bls_keypair,
                forced_mutation,
            ) {
                debug!(
                    "byzfuzz policy current_slot={} source={} destination={} msg=\"{}\" \
                     action=\"{}\" reason=byzantine_corruption mutation={:?}",
                    intercepted.current_slot,
                    intercepted.source,
                    intercepted.destination,
                    describe_consensus_message(&intercepted.message),
                    describe_intercept_action(&result.action),
                    result.mutation,
                );
                *mutation_coverage_for_policy
                    .lock()
                    .unwrap()
                    .entry(format!("{:?}", result.mutation))
                    .or_default() += 1;
                return result.action;
            }
        }
        AlpenglowInterceptAction::Forward
    });

    info!(
        "{test_name}: Alpenglow byzantine test stake distribution (byzantine indices = {:?}, \
         percents = {:?}, stakes = {:?} (sum = {total_cluster_stake})",
        byzantine_indices, stake_percents, node_stakes,
    );

    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.fixed_leader_schedule = Some(FixedSchedule {
        leader_schedule: Arc::new(leader_schedule),
    });
    validator_config.wait_for_supermajority = Some(0);
    let mut validator_configs = validator_keys
        .iter()
        .map(|_| {
            let mut config = safe_clone_config(&validator_config);
            config.voting_service_test_override = Some(proxy.voting_service_override());
            config
        })
        .collect::<Vec<_>>();
    validator_configs[0].repair_handler_type =
        RepairHandlerType::Malicious(MaliciousRepairConfig {
            bad_shred_slot_frequency: Some(3),
            bad_shred_index_frequency: Some(2),
            slot_range: Some((0, ROOT_SLOT_TO_WAIT_FOR - 1)),
        });
    validator_configs[1].turbine_mode = TurbineMode::new(TurbineModeKind::TurbineDisabled);
    validator_configs[2].turbine_mode = TurbineMode::new(TurbineModeKind::TurbineDisabled);

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

    // Start the cluster only after overrides have been attached. Then publish
    // the actual destination sockets back into the proxy so workers can forward
    // traffic that arrived during startup.
    let cluster = LocalCluster::new_alpenglow(&mut cluster_config, SocketAddrSpace::Unspecified);
    proxy.set_destinations_from_cluster(&cluster);

    cluster.check_min_slot_is_rooted(
        ROOT_SLOT_TO_WAIT_FOR,
        test_name,
        SocketAddrSpace::Unspecified,
    );
    let byzantine_messages = byzantine_message_count.load(Ordering::Relaxed);
    assert!(
        byzantine_messages > 0,
        "{test_name}: interceptor saw no byzantine messages"
    );
    info!("{test_name}: intercepted {byzantine_messages} byzantine messages");
    info!(
        "{test_name}: byzfuzz mutation coverage (applied): {:?}",
        mutation_coverage.lock().unwrap(),
    );
    drop(cluster);
    report_coverage(test_name, &proxy.state.lock().unwrap());
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
/// randomly across the configured byzantine indices, and the honest nodes split
/// the remainder in random, seed-deterministic proportions. Every node gets at
/// least 1%, and no single node exceeds `MAX_NODE_STAKE_PERCENT` (so no node can
/// form a stand-alone supermajority).
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
    assert!(
        !byzantine_indices.is_empty(),
        "need at least one byzantine node"
    );
    assert!(!honest_indices.is_empty(), "need at least one honest node");
    let honest_total_percent = 100 - byzantine_total_percent;
    assert!(
        byzantine_total_percent as usize >= byzantine_indices.len(),
        "not enough byzantine stake to give each byzantine node at least 1%",
    );
    assert!(
        honest_total_percent as usize >= honest_indices.len(),
        "not enough honest stake to give each honest node at least 1%",
    );
    assert!(
        byzantine_total_percent <= byzantine_indices.len() as u64 * MAX_NODE_STAKE_PERCENT,
        "byzantine stake cannot be spread across nodes under the per-node cap",
    );
    assert!(
        honest_total_percent <= honest_indices.len() as u64 * MAX_NODE_STAKE_PERCENT,
        "honest stake cannot be spread across nodes under the per-node cap",
    );
    let byzantine_percents = random_positive_percents(
        byzantine_total_percent,
        byzantine_indices.len(),
        MAX_NODE_STAKE_PERCENT,
        &mut rng,
    );
    let honest_percents = random_positive_percents(
        honest_total_percent,
        honest_indices.len(),
        MAX_NODE_STAKE_PERCENT,
        &mut rng,
    );

    let mut percents = vec![0u64; num_nodes];
    for (k, &i) in byzantine_indices.iter().enumerate() {
        percents[i] = byzantine_percents[k];
    }
    for (k, &i) in honest_indices.iter().enumerate() {
        percents[i] = honest_percents[k];
    }
    debug_assert_eq!(percents.iter().sum::<u64>(), 100);
    debug_assert!(percents.iter().all(|&p| p <= MAX_NODE_STAKE_PERCENT));
    percents
}

fn random_positive_percents(
    total_percent: u64,
    count: usize,
    max_percent: u64,
    rng: &mut AlpenglowRng,
) -> Vec<u64> {
    // Random positive weights followed by integer apportionment.
    let weights = (0..count)
        .map(|_| rng.random_range(1..=1000u64))
        .collect::<Vec<_>>();
    let weight_sum: u64 = weights.iter().sum();
    let distributable = total_percent - count as u64;
    let mut percents = vec![1u64; count];
    let mut allocated = 0u64;
    for (k, w) in weights.iter().enumerate() {
        let extra = distributable * w / weight_sum;
        percents[k] += extra;
        allocated += extra;
    }
    percents[0] += distributable - allocated;

    // Move any stake over the cap to entries that still have headroom.
    let mut excess = 0u64;
    for p in percents.iter_mut() {
        if *p > max_percent {
            excess += *p - max_percent;
            *p = max_percent;
        }
    }
    let mut k = 0usize;
    while excess > 0 {
        let idx = k % percents.len();
        if percents[idx] < max_percent {
            percents[idx] += 1;
            excess -= 1;
        }
        k += 1;
    }

    debug_assert_eq!(percents.iter().sum::<u64>(), total_percent);
    debug_assert!(percents.iter().all(|&p| p >= 1));
    debug_assert!(percents.iter().all(|&p| p <= max_percent));
    percents
}
