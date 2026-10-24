//! Canonical reward accounting when one voter's direct path to the collector is absent.

use {
    super::*,
    agave_votor_messages::{
        migration::AG_MIGRATION_EPOCH_CREDIT, reward_certificate::NUM_SLOTS_FOR_REWARD,
    },
    solana_account::ReadableAccount,
    solana_entry::block_component::{
        BlockComponent, BlockFooterV1, VersionedBlockFooter, VersionedBlockMarker,
    },
    solana_runtime::{bank::Bank, validated_reward_certificate::ValidatedRewardCert},
    solana_signer_store::{Decoded, decode},
    solana_vote_program::vote_state::VoteStateV4,
};

const SLOTS_PER_EPOCH: Slot = 32;
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

fn footer(blockstore: &Blockstore, slot: Slot) -> Option<BlockFooterV1> {
    let (components, _, full) = blockstore
        .get_slot_components_with_shred_info(slot, 0, false)
        .unwrap();
    if !full {
        return None;
    }
    components.into_iter().find_map(|component| {
        let BlockComponent::BlockMarker(VersionedBlockMarker::V1(marker)) = component else {
            return None;
        };
        marker
            .as_block_footer()
            .map(|VersionedBlockFooter::V1(footer)| footer.clone())
    })
}

fn bitmap_has_vote_account(bitmap: &[u8], bank: &Bank, source_slot: Slot, vote: Pubkey) -> bool {
    let rank_map = bank
        .epoch_stakes_from_slot(source_slot)
        .unwrap()
        .bls_pubkey_to_rank_map();
    let rank = usize::from(*rank_map.get_rank_for_vote_pubkey(&vote).unwrap());
    let Decoded::Base2(signers) = decode(bitmap, rank_map.len()).unwrap() else {
        panic!("canonical reward and finalization aggregates must use base2");
    };
    signers[rank]
}

fn vote_credits(bank: &Bank, vote: &Pubkey) -> u64 {
    let account = bank.get_account(vote).unwrap();
    VoteStateV4::deserialize(account.data(), vote)
        .unwrap()
        .epoch_credits
        .iter()
        .rev()
        .find(|entry| **entry != AG_MIGRATION_EPOCH_CREDIT)
        .map_or(0, |(_, credits, _)| *credits)
}

fn cluster_tips(cluster: &LocalCluster) -> HashMap<Pubkey, (Slot, Slot)> {
    cluster
        .validators
        .iter()
        .map(|(identity, info)| {
            let banks = info.validator.as_ref().unwrap().bank_forks.read().unwrap();
            (*identity, (banks.root(), banks.highest_slot()))
        })
        .collect()
}

/// Confirms that the source vote was present in an actual B-produced canonical footer before
/// C's reward block. With A -> C blackholed, B remains the path for this consensus evidence.
fn source_evidence_reached_collector(
    blockstore: &Blockstore,
    bank: &Bank,
    source_slot: Slot,
    a_vote: Pubkey,
    b: Pubkey,
) -> bool {
    ((source_slot + 1)..bank.slot()).any(|carrier| {
        if !blockstore.is_root(carrier) {
            return false;
        }
        // During these source slots B is the fixed leader. Check the shred signature as well
        // as the schedule so this witness is an actual B-produced block on the rooted chain.
        if !blockstore
            .get_data_shreds_for_slot(carrier, 0)
            .unwrap()
            .into_iter()
            .all(|shred| shred.verify(&b))
        {
            return false;
        }
        let Some(final_cert) = footer(blockstore, carrier).and_then(|f| f.block_final_cert) else {
            return false;
        };
        if final_cert.slot != source_slot {
            return false;
        }
        let notar = final_cert
            .notar_aggregate
            .as_ref()
            .unwrap_or(&final_cert.final_aggregate);
        bitmap_has_vote_account(notar.bitmap(), bank, source_slot, a_vote)
    })
}

fn run_direct_vote_loss(enable_reward_recovery: bool) {
    agave_logger::setup_with_default(&format!("{AG_DEBUG_LOG_FILTER},agave_bls_sigverify=info"));
    let keys: Vec<_> = (0..3).map(|_| ValidatorKeys::new()).collect();
    let [a, b, c] = keys
        .iter()
        .map(|keys| keys.node_keypair.pubkey())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let a_vote = keys[0].vote_keypair.pubkey();

    // A+B has 79% stake, while B+C has only 56%. Any notarization proof that C learns must
    // therefore contain A even though C never receives A's individual vote on the direct path.
    // No pair reaches the 80% startup threshold: starting A+B without C lets them advance
    // through these short epochs before C has the epoch stakes needed to follow their proofs.
    let node_stakes = [44, 35, 21]
        .into_iter()
        .map(|weight| weight * DEFAULT_NODE_STAKE)
        .collect::<Vec<_>>();
    let leader_schedule = create_custom_leader_schedule(
        [
            (SlotLeader::from(&keys[1]), 16),
            (SlotLeader::from(&keys[2]), 16),
        ]
        .into_iter(),
    );
    let mut validator_configs = make_identical_validator_configs(
        &ValidatorConfig {
            fixed_leader_schedule: Some(FixedSchedule {
                leader_schedule: Arc::new(leader_schedule),
            }),
            wait_for_supermajority: Some(0),
            ..ValidatorConfig::default_for_test()
        },
        3,
    );

    // Keep the socket bound throughout the run: the address cannot be reused by a live peer.
    // Install this override before startup so no cached A -> C connection can precede the fault.
    // None would fall back to gossip discovery, so the blackhole must be a pinned Some address.
    let blackhole = bind_to_localhost_unique().unwrap();
    blackhole.set_nonblocking(true).unwrap();
    validator_configs[0].votor_peer_overrides = Arc::new(ArcSwap::from_pointee(HashMap::from([(
        c,
        Some(blackhole.local_addr().unwrap()),
    )])));
    validator_configs[2].enable_reward_recovery = enable_reward_recovery;
    assert!(!validator_configs[0].enable_reward_recovery);
    assert!(!validator_configs[1].enable_reward_recovery);
    assert!(
        !validator_configs[1]
            .votor_peer_overrides
            .load()
            .contains_key(&c)
    );

    let mut config = ClusterConfig {
        mint_lamports: DEFAULT_MINT_LAMPORTS + node_stakes.iter().sum::<u64>(),
        validator_configs,
        validator_keys: Some(keys.into_iter().map(|keys| (keys, true)).collect()),
        node_stakes,
        slots_per_epoch: SLOTS_PER_EPOCH,
        stakers_slot_offset: SLOTS_PER_EPOCH,
        skip_warmup_slots: true,
        poh_config: PohConfig {
            target_tick_duration: Duration::from_millis(2),
            hashes_per_tick: None,
            target_tick_count: None,
        },
        ..ClusterConfig::default()
    };
    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    let follower = cluster.validators[&b].validator.as_ref().unwrap();
    let blockstore = &follower.blockstore;
    // Startup can return with replay still behind production. Start after every live tip,
    // and observe successive C windows rather than assuming one window produces enough blocks.
    let startup_tips = cluster_tips(&cluster);
    let startup_tip = startup_tips.values().map(|(_, tip)| *tip).max().unwrap();
    let collector_first_slot = (startup_tip / SLOTS_PER_EPOCH + 1) * SLOTS_PER_EPOCH + 16;
    info!(
        "Canonical reward fixture: enabled={enable_reward_recovery}, A={a}, B={b}, C={c}, first C \
         slot={collector_first_slot}, startup (root, tip)={startup_tips:?}"
    );
    let started = Instant::now();
    let mut captured = HashMap::<Slot, (Arc<Bank>, Arc<Bank>)>::new();
    let mut checked = HashSet::new();
    let mut canonical_reward_blocks = 0;
    let mut recovered = false;
    let mut blackholed_packets = 0;
    let mut packet = [0; 2048];
    let mut last_reported_epoch = None;

    loop {
        while blackhole.recv_from(&mut packet).is_ok() {
            blackholed_packets += 1;
        }
        let (root, tip) = {
            let banks = follower.bank_forks.read().unwrap();
            let root = banks.root();
            // Hold at most the selected seven slots in each of three epochs. Release checked
            // banks immediately, and discard old unrooted forks even if the cluster keeps going.
            let capture_floor = root.saturating_sub(SLOTS_PER_EPOCH);
            let capture_ceiling = root.saturating_add(SLOTS_PER_EPOCH);
            captured.retain(|slot, _| *slot >= capture_floor && !checked.contains(slot));
            checked.retain(|slot| *slot >= capture_floor);
            // Retain the parent before freeze/root pruning can detach it. Assertions below still
            // require a frozen, independently replayed bank and its actual rooted footer.
            for (&slot, bank) in banks.banks() {
                if slot >= collector_first_slot
                    && (capture_floor..=capture_ceiling).contains(&slot)
                    && (16..=22).contains(&(slot % SLOTS_PER_EPOCH))
                    && !checked.contains(&slot)
                    && bank.leader().id == c
                    && let Some(parent) = bank.parent()
                {
                    captured
                        .entry(slot)
                        .or_insert_with(|| (bank.clone_without_scheduler(), parent));
                }
            }
            (root, banks.highest_slot())
        };
        for (slot, (bank, parent)) in &captured {
            if checked.contains(slot) || !bank.is_frozen() || !blockstore.is_root(*slot) {
                continue;
            }
            let Some(footer) = footer(blockstore, *slot) else {
                continue;
            };
            assert_eq!(footer.bank_hash, bank.hash());
            assert_eq!(bank.leader().id, c);
            assert!(bank.epoch() > 0, "genesis epoch has a zero reward budget");
            let source_slot = *slot - NUM_SLOTS_FOR_REWARD;
            assert!((8..=14).contains(&(source_slot % SLOTS_PER_EPOCH)));
            let Some(notar) = &footer.notar_reward_cert else {
                checked.insert(*slot);
                continue;
            };
            assert_eq!(notar.slot, source_slot);
            assert!(
                ValidatedRewardCert::try_new(
                    bank,
                    cluster.entry_point_info.shred_version(),
                    &footer.skip_reward_cert,
                    &footer.notar_reward_cert,
                )
                .unwrap()
                .is_some(),
                "rooted reward output must also pass replay cryptographic validation"
            );
            let includes_a = bitmap_has_vote_account(notar.bitmap(), bank, source_slot, a_vote)
                || footer.skip_reward_cert.as_ref().is_some_and(|skip| {
                    bitmap_has_vote_account(skip.to_bitmap(), bank, source_slot, a_vote)
                });
            let earned = vote_credits(bank, &a_vote) - vote_credits(parent, &a_vote);
            if !enable_reward_recovery {
                assert!(
                    !includes_a,
                    "disabled collector unexpectedly recovered A at {slot}"
                );
                assert_eq!(
                    earned, 0,
                    "A cannot earn credits through the missing direct path"
                );
            } else if includes_a
                && source_evidence_reached_collector(blockstore, bank, source_slot, a_vote, b)
            {
                assert!(
                    earned > 0,
                    "recovered signer must earn canonical vote-account credits"
                );
                recovered = true;
            }
            canonical_reward_blocks += 1;
            checked.insert(*slot);
        }
        captured.retain(|slot, _| !checked.contains(slot));

        if canonical_reward_blocks >= 2
            && (!enable_reward_recovery || recovered)
            && blackholed_packets > 0
        {
            break;
        }
        if last_reported_epoch != Some(root / SLOTS_PER_EPOCH) {
            info!(
                "Canonical reward observation: enabled={enable_reward_recovery}, B root={root}, \
                 tip={tip}, checked={checked:?}, pending={:?}, reward \
                 blocks={canonical_reward_blocks}, recovered={recovered}, cluster (root, tip)={:?}",
                captured.keys().collect::<Vec<_>>(),
                cluster_tips(&cluster),
            );
            last_reported_epoch = Some(root / SLOTS_PER_EPOCH);
        }
        assert!(
            started.elapsed() < TEST_TIMEOUT,
            "canonical reward observation timed out: enabled={enable_reward_recovery}, B \
             root={root}, tip={tip}, first C slot={collector_first_slot}, checked={checked:?}, \
             pending={:?}, reward blocks={canonical_reward_blocks}, recovered={recovered}, \
             blackholed packets={blackholed_packets}, startup (root, tip)={startup_tips:?}, \
             current (root, tip)={:?}",
            captured.keys().collect::<Vec<_>>(),
            cluster_tips(&cluster),
        );
        sleep(Duration::from_millis(10));
    }
    // A is never a leader in this fixture, so A's per-block credit increment cannot be a
    // leader reward. B's replayed rooted bank is the assertion target, rather than C's builder.
    assert_ne!(a, c);
}

#[test]
#[serial]
fn test_alpenglow_reward_recovery_disabled_loses_canonical_rewards_on_direct_vote_loss() {
    run_direct_vote_loss(false);
}

#[test]
#[serial]
fn test_alpenglow_reward_recovery_recovers_canonical_rewards_on_direct_vote_loss() {
    run_direct_vote_loss(true);
}
