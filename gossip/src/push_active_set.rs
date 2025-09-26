use {
    crate::{cluster_info::REFRESH_PUSH_ACTIVE_SET_INTERVAL_MS, weighted_shuffle::WeightedShuffle},
    agave_low_pass_filter::api as lpf,
    indexmap::IndexMap,
    rand::Rng,
    solana_bloom::bloom::{Bloom, ConcurrentBloom},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

const NUM_PUSH_ACTIVE_SET_ENTRIES: usize = 25;

const ALPHA_MIN: u64 = lpf::SCALE.get();
const ALPHA_MAX: u64 = 2 * lpf::SCALE.get();
const DEFAULT_ALPHA: u64 = ALPHA_MAX;
// Low pass filter convergence time (ms)
const DEFAULT_TC_MS: u64 = 30_000;

pub struct IIRFilter {
    alpha: u64, // alpha in [1.0, 2.0], smoothed over time, scaled up by 1,000,000 to avoid floating-point math
    k: u64,     // default: 611,015
}

#[inline]
fn get_weight(bucket: u64, alpha: u64) -> u64 {
    debug_assert!((ALPHA_MIN..=ALPHA_MIN + lpf::SCALE.get()).contains(&alpha));
    let b = bucket + 1;
    let b_squared = b.saturating_mul(b);
    gossip_interpolate_weight(b, b_squared, alpha)
}

/// Approximates `base^alpha` rounded to nearest integer using
/// integer-only linear interpolation between `base^1` and `base^2`.
///
/// Note: This function is most accurate when `base` is small e.g. < ~25.
#[inline]
#[allow(clippy::arithmetic_side_effects)]
fn gossip_interpolate_weight(base: u64, base_squared: u64, alpha: u64) -> u64 {
    let scale = lpf::SCALE.get();
    let t = alpha.saturating_sub(ALPHA_MIN);
    debug_assert!(t <= scale, "interpolation t={t} > SCALE={scale}");
    // ((base * (scale - t) + base_squared * t) + scale / 2) / scale
    ((base.saturating_mul(scale.saturating_sub(t))).saturating_add(base_squared.saturating_mul(t)))
        .saturating_add(scale / 2)
        / scale
}

// Each entry corresponds to a stake bucket for
//     min stake of { this node, crds value owner }
// The entry represents set of gossip nodes to actively
// push to for crds values belonging to the bucket.
pub(crate) struct PushActiveSet {
    entries: [PushActiveSetEntry; NUM_PUSH_ACTIVE_SET_ENTRIES],
    filter: IIRFilter,
}

impl PushActiveSet {
    pub(crate) fn new() -> Self {
        Self {
            entries: Default::default(),
            filter: IIRFilter {
                alpha: DEFAULT_ALPHA,
                k: lpf::compute_k(REFRESH_PUSH_ACTIVE_SET_INTERVAL_MS, DEFAULT_TC_MS),
            },
        }
    }
}

// Keys are gossip nodes to push messages to.
// Values are which origins the node has pruned.
#[derive(Default)]
struct PushActiveSetEntry(IndexMap</*node:*/ Pubkey, /*origins:*/ ConcurrentBloom<Pubkey>>);

impl PushActiveSet {
    #[cfg(debug_assertions)]
    const MIN_NUM_BLOOM_ITEMS: usize = 512;
    #[cfg(not(debug_assertions))]
    const MIN_NUM_BLOOM_ITEMS: usize = crate::cluster_info::CRDS_UNIQUE_PUBKEY_CAPACITY;

    pub(crate) fn get_nodes<'a>(
        &'a self,
        pubkey: &'a Pubkey, // This node.
        origin: &'a Pubkey, // CRDS value owner.
        // If true forces gossip push even if the node has pruned the origin.
        should_force_push: impl FnMut(&Pubkey) -> bool + 'a,
        stakes: &HashMap<Pubkey, u64>,
    ) -> impl Iterator<Item = &'a Pubkey> + 'a {
        let stake = stakes.get(pubkey).min(stakes.get(origin));
        self.get_entry(stake)
            .get_nodes(pubkey, origin, should_force_push)
    }

    // Prunes origins for the given gossip node.
    // We will stop pushing messages from the specified origins to the node.
    pub(crate) fn prune(
        &self,
        pubkey: &Pubkey,    // This node.
        node: &Pubkey,      // Gossip node.
        origins: &[Pubkey], // CRDS value owners.
        stakes: &HashMap<Pubkey, u64>,
    ) {
        let stake = stakes.get(pubkey);
        for origin in origins {
            if origin == pubkey {
                continue;
            }
            let stake = stake.min(stakes.get(origin));
            self.get_entry(stake).prune(node, origin)
        }
    }

    pub(crate) fn rotate<R: Rng>(
        &mut self,
        rng: &mut R,
        size: usize, // Number of nodes to retain in each active-set entry.
        cluster_size: usize,
        // Gossip nodes to be sampled for each push active set.
        nodes: &[Pubkey],
        stakes: &HashMap<Pubkey, u64>,
        self_pubkey: &Pubkey,
    ) {
        if nodes.is_empty() {
            return;
        }
        let num_bloom_filter_items = cluster_size.max(Self::MIN_NUM_BLOOM_ITEMS);
        // Active set of nodes to push to are sampled from these gossip nodes,
        // using sampling probabilities obtained from the stake bucket of each
        // node.
        let buckets: Vec<_> = nodes
            .iter()
            .map(|node| get_stake_bucket(stakes.get(node)))
            .collect();

        let self_bucket = get_stake_bucket(stakes.get(self_pubkey));
        let num_unstaked = buckets
            .iter()
            .filter(|&&b| b == 0)
            .count()
            .saturating_add(if self_bucket == 0 { 1 } else { 0 });
        let total_nodes = nodes.len().saturating_add(1);

        let f_scaled = ((num_unstaked.saturating_mul(lpf::SCALE.get() as usize))
            .saturating_add(total_nodes / 2))
            / total_nodes;
        let alpha_target = ALPHA_MIN.saturating_add(f_scaled as u64);
        self.filter.alpha = lpf::filter_alpha(
            self.filter.alpha,
            alpha_target,
            lpf::FilterConfig {
                output_range: ALPHA_MIN..ALPHA_MAX,
                k: self.filter.k,
            },
        );

        // alpha = [1.0, 2.0] → weight = (bucket + 1)^alpha
        // (k, entry) represents push active set where the stake bucket of
        //     min stake of {this node, crds value owner}
        // is equal to `k`. The `entry` maintains set of gossip nodes to
        // actively push to for crds values belonging to this bucket.
        for (k, entry) in self.entries.iter_mut().enumerate() {
            let weights: Vec<u64> = buckets
                .iter()
                .map(|&bucket| {
                    // bucket <- get_stake_bucket(min stake of {
                    //  this node, crds value owner and gossip peer
                    // })
                    // weight <- (bucket + 1)^alpha
                    // min stake of {...} is a proxy for how much we care about
                    // the link, and tries to mirror similar logic on the
                    // receiving end when pruning incoming links:
                    // https://github.com/solana-labs/solana/blob/81394cf92/gossip/src/received_cache.rs#L100-L105
                    let bucket = bucket.min(k) as u64;
                    get_weight(bucket, self.filter.alpha)
                })
                .collect();
            entry.rotate(rng, size, num_bloom_filter_items, nodes, &weights);
        }
    }

    fn get_entry(&self, stake: Option<&u64>) -> &PushActiveSetEntry {
        &self.entries[get_stake_bucket(stake)]
    }
}

impl PushActiveSetEntry {
    const BLOOM_FALSE_RATE: f64 = 0.1;
    const BLOOM_MAX_BITS: usize = 1024 * 8 * 4;

    fn get_nodes<'a>(
        &'a self,
        pubkey: &'a Pubkey, // This node.
        origin: &'a Pubkey, // CRDS value owner.
        // If true forces gossip push even if the node has pruned the origin.
        mut should_force_push: impl FnMut(&Pubkey) -> bool + 'a,
    ) -> impl Iterator<Item = &'a Pubkey> + 'a {
        let pubkey_eq_origin = pubkey == origin;
        self.0
            .iter()
            .filter(move |(node, bloom_filter)| {
                // Bloom filter can return false positive for origin == pubkey
                // but a node should always be able to push its own values.
                !bloom_filter.contains(origin)
                    || (pubkey_eq_origin && &pubkey != node)
                    || should_force_push(node)
            })
            .map(|(node, _bloom_filter)| node)
    }

    fn prune(
        &self,
        node: &Pubkey,   // Gossip node.
        origin: &Pubkey, // CRDS value owner
    ) {
        if let Some(bloom_filter) = self.0.get(node) {
            bloom_filter.add(origin);
        }
    }

    fn rotate<R: Rng>(
        &mut self,
        rng: &mut R,
        size: usize, // Number of nodes to retain.
        num_bloom_filter_items: usize,
        nodes: &[Pubkey],
        weights: &[u64],
    ) {
        debug_assert_eq!(nodes.len(), weights.len());
        debug_assert!(weights.iter().all(|&weight| weight != 0u64));
        let mut weighted_shuffle = WeightedShuffle::<u64>::new("rotate-active-set", weights);
        for node in weighted_shuffle.shuffle(rng).map(|k| &nodes[k]) {
            // We intend to discard the oldest/first entry in the index-map.
            if self.0.len() > size {
                break;
            }
            if self.0.contains_key(node) {
                continue;
            }
            let bloom = ConcurrentBloom::from(Bloom::random(
                num_bloom_filter_items,
                Self::BLOOM_FALSE_RATE,
                Self::BLOOM_MAX_BITS,
            ));
            bloom.add(node);
            self.0.insert(*node, bloom);
        }
        // Drop the oldest entry while preserving the ordering of others.
        while self.0.len() > size {
            self.0.shift_remove_index(0);
        }
    }
}

// Maps stake to bucket index.
fn get_stake_bucket(stake: Option<&u64>) -> usize {
    let stake = stake.copied().unwrap_or_default() / LAMPORTS_PER_SOL;
    let bucket = u64::BITS - stake.leading_zeros();
    (bucket as usize).min(NUM_PUSH_ACTIVE_SET_ENTRIES - 1)
}

#[cfg(test)]
mod tests {
    use {
        super::*, itertools::iproduct, rand::SeedableRng, rand_chacha::ChaChaRng,
        std::iter::repeat_with,
    };

    const MAX_STAKE: u64 = (1 << 20) * LAMPORTS_PER_SOL;

    // Helper to generate a stake map given unstaked count
    fn make_stakes(
        nodes: &[Pubkey],
        num_unstaked: usize,
        rng: &mut ChaChaRng,
    ) -> HashMap<Pubkey, u64> {
        nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let stake = if i < num_unstaked {
                    0
                } else {
                    rng.gen_range(1..=MAX_STAKE)
                };
                (*node, stake)
            })
            .collect()
    }

    #[test]
    fn test_get_stake_bucket() {
        assert_eq!(get_stake_bucket(None), 0);
        let buckets = [0, 1, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5];
        for (k, bucket) in buckets.into_iter().enumerate() {
            let stake = (k as u64) * LAMPORTS_PER_SOL;
            assert_eq!(get_stake_bucket(Some(&stake)), bucket);
        }
        for (stake, bucket) in [
            (4_194_303, 22),
            (4_194_304, 23),
            (8_388_607, 23),
            (8_388_608, 24),
        ] {
            let stake = stake * LAMPORTS_PER_SOL;
            assert_eq!(get_stake_bucket(Some(&stake)), bucket);
        }
        assert_eq!(
            get_stake_bucket(Some(&u64::MAX)),
            NUM_PUSH_ACTIVE_SET_ENTRIES - 1
        );
    }

    #[test]
    fn test_push_active_set_dynamic_weighting() {
        const CLUSTER_SIZE: usize = 117;
        let mut rng = ChaChaRng::from_seed([14u8; 32]);
        let pubkey = Pubkey::new_unique();
        let nodes: Vec<_> = repeat_with(Pubkey::new_unique).take(20).collect();
        let stakes = repeat_with(|| rng.gen_range(1..MAX_STAKE));
        let mut stakes: HashMap<_, _> = nodes.iter().copied().zip(stakes).collect();
        stakes.insert(pubkey, rng.gen_range(1..MAX_STAKE));
        let mut active_set = PushActiveSet::new();
        assert!(active_set.entries.iter().all(|entry| entry.0.is_empty()));
        active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &pubkey);
        assert!(active_set.entries.iter().all(|entry| entry.0.len() == 5));
        // Assert that for all entries, each filter already prunes the key.
        for entry in &active_set.entries {
            for (node, filter) in entry.0.iter() {
                assert!(filter.contains(node));
            }
        }
        let other = &nodes[6];
        let origin = &nodes[17];
        assert!(active_set
            .get_nodes(&pubkey, origin, |_| false, &stakes)
            .eq([7, 6, 2, 4, 12].into_iter().map(|k| &nodes[k])));
        assert!(active_set
            .get_nodes(&pubkey, other, |_| false, &stakes)
            .eq([7, 2, 4, 12].into_iter().map(|k| &nodes[k])));

        active_set.prune(&pubkey, &nodes[6], &[*origin], &stakes);
        active_set.prune(&pubkey, &nodes[11], &[*origin], &stakes);
        active_set.prune(&pubkey, &nodes[4], &[*origin], &stakes);
        assert!(active_set
            .get_nodes(&pubkey, origin, |_| false, &stakes)
            .eq([7, 2, 12].into_iter().map(|k| &nodes[k])));
        assert!(active_set
            .get_nodes(&pubkey, other, |_| false, &stakes)
            .eq([7, 2, 4, 12].into_iter().map(|k| &nodes[k])));
        active_set.rotate(&mut rng, 7, CLUSTER_SIZE, &nodes, &stakes, &pubkey);
        assert!(active_set.entries.iter().all(|entry| entry.0.len() == 7));
        assert!(active_set
            .get_nodes(&pubkey, origin, |_| false, &stakes)
            .eq([2, 12, 15, 14, 16].into_iter().map(|k| &nodes[k])));
        assert!(active_set
            .get_nodes(&pubkey, other, |_| false, &stakes)
            .eq([2, 4, 12, 15, 14, 16].into_iter().map(|k| &nodes[k])));
        let origins = [*origin, *other];
        active_set.prune(&pubkey, &nodes[2], &origins, &stakes);
        active_set.prune(&pubkey, &nodes[12], &origins, &stakes);
        active_set.prune(&pubkey, &nodes[14], &origins, &stakes);
        assert!(active_set
            .get_nodes(&pubkey, origin, |_| false, &stakes)
            .eq([15, 16].into_iter().map(|k| &nodes[k])));
        assert!(active_set
            .get_nodes(&pubkey, other, |_| false, &stakes)
            .eq([4, 15, 16].into_iter().map(|k| &nodes[k])));
    }

    #[test]
    fn test_push_active_set_entry() {
        const NUM_BLOOM_FILTER_ITEMS: usize = 100;
        let mut rng = ChaChaRng::from_seed([147u8; 32]);
        let nodes: Vec<_> = repeat_with(Pubkey::new_unique).take(20).collect();
        let weights: Vec<_> = repeat_with(|| rng.gen_range(1..1000)).take(20).collect();
        let mut entry = PushActiveSetEntry::default();
        entry.rotate(
            &mut rng,
            5, // size
            NUM_BLOOM_FILTER_ITEMS,
            &nodes,
            &weights,
        );
        assert_eq!(entry.0.len(), 5);
        let keys = [&nodes[16], &nodes[11], &nodes[17], &nodes[14], &nodes[5]];
        assert!(entry.0.keys().eq(keys));
        for (pubkey, origin) in iproduct!(&nodes, &nodes) {
            if !keys.contains(&origin) {
                assert!(entry.get_nodes(pubkey, origin, |_| false).eq(keys));
            } else {
                assert!(entry.get_nodes(pubkey, origin, |_| true).eq(keys));
                assert!(entry
                    .get_nodes(pubkey, origin, |_| false)
                    .eq(keys.into_iter().filter(|&key| key != origin)));
            }
        }
        // Assert that each filter already prunes the key.
        for (node, filter) in entry.0.iter() {
            assert!(filter.contains(node));
        }
        for (pubkey, origin) in iproduct!(&nodes, keys) {
            assert!(entry.get_nodes(pubkey, origin, |_| true).eq(keys));
            assert!(entry
                .get_nodes(pubkey, origin, |_| false)
                .eq(keys.into_iter().filter(|&node| node != origin)));
        }
        // Assert that prune excludes node from get.
        let origin = &nodes[3];
        entry.prune(&nodes[11], origin);
        entry.prune(&nodes[14], origin);
        entry.prune(&nodes[19], origin);
        for pubkey in &nodes {
            assert!(entry.get_nodes(pubkey, origin, |_| true).eq(keys));
            assert!(entry.get_nodes(pubkey, origin, |_| false).eq(keys
                .into_iter()
                .filter(|&&node| pubkey == origin || (node != nodes[11] && node != nodes[14]))));
        }
        // Assert that rotate adds new nodes.
        entry.rotate(&mut rng, 5, NUM_BLOOM_FILTER_ITEMS, &nodes, &weights);
        let keys = [&nodes[11], &nodes[17], &nodes[14], &nodes[5], &nodes[7]];
        assert!(entry.0.keys().eq(keys));
        entry.rotate(&mut rng, 6, NUM_BLOOM_FILTER_ITEMS, &nodes, &weights);
        let keys = [
            &nodes[17], &nodes[14], &nodes[5], &nodes[7], &nodes[1], &nodes[13],
        ];
        assert!(entry.0.keys().eq(keys));
        entry.rotate(&mut rng, 4, NUM_BLOOM_FILTER_ITEMS, &nodes, &weights);
        let keys = [&nodes[5], &nodes[7], &nodes[1], &nodes[13]];
        assert!(entry.0.keys().eq(keys));
    }

    fn alpha_of(pas: &PushActiveSet) -> u64 {
        pas.filter.alpha
    }

    #[test]
    fn test_alpha_converges_to_expected_target() {
        const CLUSTER_SIZE: usize = 415;
        const TOLERANCE_MILLI: u64 = lpf::SCALE.get() / 100; // ±1% of alpha

        let mut rng = ChaChaRng::from_seed([77u8; 32]);
        let mut nodes: Vec<Pubkey> = repeat_with(Pubkey::new_unique).take(CLUSTER_SIZE).collect();

        // 39% unstaked → alpha_target = 1,000,000 + 39 * 10000 = 1,390,000
        let percent_unstaked = 39;
        let num_unstaked = (CLUSTER_SIZE * percent_unstaked + 50) / 100;
        let expected_alpha_milli = 1_000_000 + (percent_unstaked as u64 * 10_000);

        let stakes = make_stakes(&nodes, num_unstaked, &mut rng);
        let my_pubkey = nodes.pop().unwrap();

        let mut active_set = PushActiveSet::new();

        // Simulate repeated calls to `rotate()` (as would happen every 7.5s)
        // 8 calls (60s) should be enough to converge to the expected target alpha.
        // We converge in about 4 calls (30s).
        for _ in 0..8 {
            active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &my_pubkey);
        }

        let actual_alpha = alpha_of(&active_set);
        assert!(
            (actual_alpha as i32 - expected_alpha_milli as i32).abs() <= TOLERANCE_MILLI as i32,
            "alpha={actual_alpha} did not converge to expected alpha={expected_alpha_milli}"
        );

        // 93% unstaked → alpha_target = 1,000,000 + 93 * 10000 = 1,930,000
        let percent_unstaked = 93;
        let num_unstaked = (CLUSTER_SIZE * percent_unstaked + 50) / 100;
        let expected_alpha_milli = 1_000_000 + (percent_unstaked as u64 * 10_000);

        let stakes = make_stakes(&nodes, num_unstaked, &mut rng);
        for _ in 0..8 {
            active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &my_pubkey);
        }

        let actual_alpha = alpha_of(&active_set);
        assert!(
            (actual_alpha as i32 - expected_alpha_milli as i32).abs() <= TOLERANCE_MILLI as i32,
            "alpha={actual_alpha} did not reconverge to expected alpha={expected_alpha_milli}"
        );
    }

    #[test]
    fn test_alpha_converges_up_and_down() {
        const CLUSTER_SIZE: usize = 415;
        const TOLERANCE_MILLI: u64 = lpf::SCALE.get() / 100; // ±1% of alpha
        const ROTATE_CALLS: usize = 8;

        let mut rng = ChaChaRng::from_seed([99u8; 32]);
        let mut nodes: Vec<Pubkey> = repeat_with(Pubkey::new_unique).take(CLUSTER_SIZE).collect();

        let mut active_set = PushActiveSet::new();

        // 0% unstaked → alpha_target = 1,000,000
        let num_unstaked = 0;
        let expected_alpha_0 = 1_000_000;
        let stakes = make_stakes(&nodes, num_unstaked, &mut rng);
        let my_pubkey = nodes.pop().unwrap();

        for _ in 0..ROTATE_CALLS {
            active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &my_pubkey);
        }
        let alpha = alpha_of(&active_set);
        assert!(
            (alpha as i32 - expected_alpha_0).abs() <= TOLERANCE_MILLI as i32,
            "alpha={alpha} did not converge to alpha_0={expected_alpha_0}"
        );

        // 100% unstaked → alpha_target = 2,000,000
        let num_unstaked = CLUSTER_SIZE;
        let expected_alpha_100 = 2_000_000;
        let stakes = make_stakes(&nodes, num_unstaked, &mut rng);
        for _ in 0..ROTATE_CALLS {
            active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &my_pubkey);
        }
        let alpha = alpha_of(&active_set);
        assert!(
            (alpha as i32 - expected_alpha_100).abs() <= TOLERANCE_MILLI as i32,
            "alpha={alpha} did not converge to alpha_100={expected_alpha_100}"
        );

        // back to 0% unstaked → alpha_target = 1,000,000
        let num_unstaked = 0;
        let stakes = make_stakes(&nodes, num_unstaked, &mut rng);
        for _ in 0..ROTATE_CALLS {
            active_set.rotate(&mut rng, 5, CLUSTER_SIZE, &nodes, &stakes, &my_pubkey);
        }
        let alpha = alpha_of(&active_set);
        assert!(
            (alpha as i32 - expected_alpha_0).abs() <= TOLERANCE_MILLI as i32,
            "alpha={alpha} did not reconverge to alpha_0={expected_alpha_0}"
        );
    }

    #[test]
    fn test_alpha_progression_matches_expected() {
        let mut alpha = ALPHA_MAX;
        let target_down = ALPHA_MIN;
        let target_up = ALPHA_MAX;
        let filter_k = lpf::compute_k(REFRESH_PUSH_ACTIVE_SET_INTERVAL_MS, DEFAULT_TC_MS);

        // Expected values from rotating to 1,000,000 from 2,000,000
        let expected_down = [
            1_388_985, 1_151_309, 1_058_856, 1_022_894, 1_008_905, 1_003_463, 1_001_347, 1_000_523,
        ];

        for (i, expected) in expected_down.iter().enumerate() {
            alpha = lpf::filter_alpha(
                alpha,
                target_down,
                lpf::FilterConfig {
                    output_range: ALPHA_MIN..ALPHA_MAX,
                    k: filter_k,
                },
            );
            assert_eq!(
                alpha, *expected as u64,
                "step {i}: alpha did not match expected during convergence down"
            );
        }

        // Rotate upward from current alpha (1,000,000) to 2,000,000
        let expected_up = [
            1_611_218, 1_848_769, 1_941_173, 1_977_117, 1_991_098, 1_996_537, 1_998_652, 1_999_475,
        ];
        for (i, expected) in expected_up.iter().enumerate() {
            alpha = lpf::filter_alpha(
                alpha,
                target_up,
                lpf::FilterConfig {
                    output_range: ALPHA_MIN..ALPHA_MAX,
                    k: filter_k,
                },
            );
            assert_eq!(
                alpha, *expected as u64,
                "step {i}: alpha did not match expected during convergence up"
            );
        }

        // Rotate downward again from current alpha (1,999,000) to 1,000,000
        let expected_down2 = [
            1_388_780, 1_151_229, 1_058_825, 1_022_882, 1_008_900, 1_003_461, 1_001_346, 1_000_523,
        ];
        for (i, expected) in expected_down2.iter().enumerate() {
            alpha = lpf::filter_alpha(
                alpha,
                target_down,
                lpf::FilterConfig {
                    output_range: ALPHA_MIN..ALPHA_MAX,
                    k: filter_k,
                },
            );
            assert_eq!(
                alpha, *expected as u64,
                "step {i}: alpha did not match expected during final convergence down"
            );
        }
    }

    #[test]
    fn test_interpolate_t_zero() {
        // When alpha = ALPHA_MIN (t = 0), should return base
        assert_eq!(gossip_interpolate_weight(100, 100 * 100, ALPHA_MIN), 100);
        assert_eq!(gossip_interpolate_weight(0, 0, ALPHA_MIN), 0);
        assert_eq!(
            gossip_interpolate_weight(1_000_000, 1_000_000 * 1_000_000, ALPHA_MIN),
            1_000_000
        );
    }

    #[test]
    fn test_interpolate_t_max() {
        // When alpha = ALPHA_MAX (t = SCALE), should return base^2
        let base = 100;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MAX);
        assert_eq!(result, base * base);

        let base2 = 1000;
        let result = gossip_interpolate_weight(base2, base2 * base2, ALPHA_MAX);
        assert_eq!(result, base2 * base2);
    }

    #[test]
    fn test_interpolate_values() {
        let t_10 = lpf::SCALE.get() / 10; // 10%
        let t_50 = lpf::SCALE.get() / 2; // 50%
        let t_75 = lpf::SCALE.get() * 3 / 4; // 75%

        let base = 3;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_10);
        assert_eq!(result, 4);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_50);
        assert_eq!(result, 6);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_75);
        assert_eq!(result, 8);

        let base = 15;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_10);
        assert_eq!(result, 36);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_50);
        assert_eq!(result, 120);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_75);
        assert_eq!(result, 173);

        let base = 24;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_10);
        assert_eq!(result, 79);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_50);
        assert_eq!(result, 300);

        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t_75);
        assert_eq!(result, 438);
    }

    #[test]
    fn test_interpolate_large_base() {
        let base = 1_000_000_000u64;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + lpf::SCALE.get() / 2);
        assert!(result >= base);
        assert!(result < base * base);
    }

    #[test]
    fn test_interpolate_edge_cases() {
        // Test with base = 1
        assert_eq!(gossip_interpolate_weight(1, 1, ALPHA_MIN), 1);
        assert_eq!(gossip_interpolate_weight(1, 1, ALPHA_MAX), 1);
        assert_eq!(
            gossip_interpolate_weight(1, 1, ALPHA_MIN + (lpf::SCALE.get() / 2)),
            1
        );

        // Test with base = 0
        assert_eq!(gossip_interpolate_weight(0, 0, ALPHA_MIN), 0);
        assert_eq!(gossip_interpolate_weight(0, 0, ALPHA_MAX), 0);
        assert_eq!(
            gossip_interpolate_weight(0, 0, ALPHA_MIN + (lpf::SCALE.get() / 2)),
            0
        );
    }

    #[test]
    fn test_interpolate_rounding() {
        let base = 3;
        let t = lpf::SCALE.get() / 3;
        let result = gossip_interpolate_weight(base, base * base, ALPHA_MIN + t);

        assert!(result >= 3);
        assert!(result <= 9);
    }

    #[test]
    fn test_integration_filter_and_interpolate() {
        // Test using filtered alpha with interpolate
        // Alpha range is [SCALE, 2*SCALE] as used in push_active_set
        let alpha_min = lpf::SCALE.get();
        let alpha_max = 2 * lpf::SCALE.get();

        let config = lpf::FilterConfig {
            output_range: alpha_min..alpha_max,
            k: lpf::SCALE.get() / 10, // 10%
        };

        let prev_alpha = alpha_min + lpf::SCALE.get() / 4; // 1.25 * SCALE
        let target_alpha = alpha_min + lpf::SCALE.get() / 2; // 1.5 * SCALE
        let filtered_alpha = lpf::filter_alpha(prev_alpha, target_alpha, config);

        let base = 2;
        let result = gossip_interpolate_weight(base, base * base, filtered_alpha);

        assert!(result >= base);
        assert!(result <= base * base);

        assert!(filtered_alpha >= alpha_min);
        assert!(filtered_alpha <= alpha_max);
    }

    #[test]
    fn test_get_weight_specific_values() {
        // Test get_weight with specific bucket=15 and alpha=1118676
        let bucket = 15;
        let alpha = 1118676;

        // Verify alpha is in the valid range
        assert!(alpha >= ALPHA_MIN);
        assert!(alpha <= ALPHA_MAX);

        let result = get_weight(bucket, alpha);

        // Expected calculation:
        // b = bucket + 1 = 16
        // t = alpha - ALPHA_MIN = 1118676 - 1000000 = 118676
        // interpolate(16, 118676) should return 44
        assert_eq!(result, 44);
    }
}
