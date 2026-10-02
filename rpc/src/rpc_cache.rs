use {
    lru::LruCache,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_rpc_client_api::{
        config::RpcLargestAccountsFilter,
        response::{RpcAccountBalance, RpcRankMap, RpcRankMapEntry},
    },
    solana_runtime::{
        bank::MAX_LEADER_SCHEDULE_STAKES,
        epoch_stakes::{BLSPubkeyStakeEntry, BLSPubkeyToRankMap, VersionedEpochStakes},
    },
    solana_vote::vote_account::{VoteAccounts, VoteAccountsHashMap},
    std::{
        collections::HashMap,
        num::NonZeroUsize,
        sync::Arc,
        time::{Duration, SystemTime},
    },
    tokio::sync::OnceCell,
};

type RankMapCell = Arc<OnceCell<Option<CachedRankMap>>>;

pub struct RankMapCache {
    // Retain the accounts so the pointer used as a key cannot be reused while cached.
    entries: LruCache<(Epoch, usize), (Arc<VoteAccountsHashMap>, RankMapCell)>,
}

impl Default for RankMapCache {
    fn default() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(MAX_LEADER_SCHEDULE_STAKES as usize)
                    .expect("at least one epoch's stakes are retained"),
            ),
        }
    }
}

impl RankMapCache {
    pub(crate) fn get_or_insert(&mut self, epoch: Epoch, accounts: &VoteAccounts) -> RankMapCell {
        let accounts = Arc::from(accounts);
        // Epoch alone is insufficient: processed and confirmed banks can be on different forks.
        let key = (epoch, Arc::as_ptr(&accounts) as usize);
        let (_, cell) = self
            .entries
            .get_or_insert(key, || (accounts, Arc::new(OnceCell::new())));
        Arc::clone(cell)
    }
}

pub(crate) struct CachedRankMap {
    rank_map: Arc<BLSPubkeyToRankMap>,
    response: Arc<RpcRankMap>,
}

impl CachedRankMap {
    pub(crate) fn new(epoch: Epoch, stakes: &VersionedEpochStakes) -> Option<Self> {
        let rank_map = Arc::clone(stakes.try_bls_pubkey_to_rank_map()?);
        let validators = rank_map
            .iter()
            .map(|(rank, entry)| {
                // Keep the wire format explicit, and require a decision when the runtime adds fields.
                let BLSPubkeyStakeEntry {
                    vote_account_pubkey,
                    node_pubkey,
                    bls_pubkey,
                    stake,
                } = entry;
                RpcRankMapEntry {
                    rank,
                    vote_pubkey: vote_account_pubkey.to_string(),
                    node_pubkey: node_pubkey.to_string(),
                    bls_pubkey_compressed: bs58::encode(bls_pubkey.to_bytes_compressed())
                        .into_string(),
                    stake: *stake,
                }
            })
            .collect();
        let response = Arc::new(RpcRankMap {
            epoch,
            total_stake: rank_map.total_stake(),
            validators,
        });
        Some(Self { rank_map, response })
    }

    pub(crate) fn response(&self, identity: Option<&Pubkey>) -> Arc<RpcRankMap> {
        match identity {
            None => Arc::clone(&self.response),
            Some(identity) => {
                let validators = self
                    .rank_map
                    .get_ranked_entry_for_node(identity)
                    .map(|(rank, _)| self.response.validators[usize::from(rank)].clone())
                    .into_iter()
                    .collect();
                Arc::new(RpcRankMap {
                    epoch: self.response.epoch,
                    total_stake: self.response.total_stake,
                    validators,
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LargestAccountsCache {
    duration: u64,
    cache: HashMap<Option<RpcLargestAccountsFilter>, LargestAccountsCacheValue>,
}

#[derive(Debug, Clone)]
struct LargestAccountsCacheValue {
    accounts: Vec<RpcAccountBalance>,
    slot: u64,
    cached_time: SystemTime,
}

impl LargestAccountsCache {
    pub(crate) fn new(duration: u64) -> Self {
        Self {
            duration,
            cache: HashMap::new(),
        }
    }

    pub(crate) fn get_largest_accounts(
        &self,
        filter: &Option<RpcLargestAccountsFilter>,
    ) -> Option<(u64, Vec<RpcAccountBalance>)> {
        self.cache.get(filter).and_then(|value| {
            if let Ok(elapsed) = value.cached_time.elapsed()
                && elapsed < Duration::from_secs(self.duration)
            {
                return Some((value.slot, value.accounts.clone()));
            }
            None
        })
    }

    pub(crate) fn set_largest_accounts(
        &mut self,
        filter: &Option<RpcLargestAccountsFilter>,
        slot: u64,
        accounts: &[RpcAccountBalance],
    ) {
        self.cache.insert(
            filter.clone(),
            LargestAccountsCacheValue {
                accounts: accounts.to_owned(),
                slot,
                cached_time: SystemTime::now(),
            },
        );
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    fn test_rank_map_cache_forks_and_eviction() {
        let mut cache = RankMapCache::default();
        let accounts = VoteAccounts::default();
        let first = cache.get_or_insert(0, &accounts);
        assert!(Arc::ptr_eq(
            &first,
            &cache.get_or_insert(0, &accounts.clone())
        ));
        let other_fork = cache.get_or_insert(0, &VoteAccounts::default());
        assert!(!Arc::ptr_eq(&first, &other_fork));
        let next_epoch = cache.get_or_insert(1, &accounts);
        assert!(!Arc::ptr_eq(&first, &next_epoch));
        for epoch in 2..MAX_LEADER_SCHEDULE_STAKES - 1 {
            cache.get_or_insert(epoch, &accounts);
        }
        // A hit keeps the first map resident while the least recently used fork is evicted.
        assert!(Arc::ptr_eq(&first, &cache.get_or_insert(0, &accounts)));
        cache.get_or_insert(MAX_LEADER_SCHEDULE_STAKES, &accounts);
        assert_eq!(cache.entries.len(), MAX_LEADER_SCHEDULE_STAKES as usize);
        assert!(
            !cache
                .entries
                .iter()
                .any(|(_, (_, cell))| Arc::ptr_eq(cell, &other_fork))
        );
        assert!(Arc::ptr_eq(&first, &cache.get_or_insert(0, &accounts)));
    }

    #[tokio::test]
    async fn test_rank_map_cache_initializes_once() {
        let mut cache = RankMapCache::default();
        let cell = cache.get_or_insert(0, &VoteAccounts::default());
        let (first, second) = tokio::join!(
            cell.get_or_init(|| async {
                tokio::task::yield_now().await;
                None
            }),
            cell.get_or_init(|| async { panic!("map should already be initialized") }),
        );
        assert!(first.is_none());
        assert!(second.is_none());
        assert!(
            cell.get_or_init(|| async { panic!("null should be cached") })
                .await
                .is_none()
        );
    }

    #[test]
    fn test_old_entries_expire() {
        let mut cache = LargestAccountsCache::new(1);

        let filter = Some(RpcLargestAccountsFilter::Circulating);

        let accounts: Vec<RpcAccountBalance> = Vec::new();

        cache.set_largest_accounts(&filter, 1000, &accounts);
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(cache.get_largest_accounts(&filter), None);
    }
}
