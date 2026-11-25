use {
    lru::LruCache,
    solana_clock::Epoch,
    solana_rpc_client_api::response::RpcVatActiveSetRankMap,
    std::{
        num::NonZeroUsize,
        sync::{Arc, Mutex},
    },
    tokio::sync::OnceCell,
};

// The outer Arc lets requests initialize or await the cell without holding the cache lock.
// The inner Arc lets an owned response share the map without cloning its validator entries.
type RankMapCell = Arc<OnceCell<Arc<RpcVatActiveSetRankMap>>>;

pub struct RankMapCache {
    // Only maps from finalized banks may be inserted. The bank's retained epoch stakes
    // determine availability; a miss here is rebuilt from those stakes, including after restart.
    entries: Mutex<LruCache<Epoch, RankMapCell>>,
}

impl RankMapCache {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(capacity)),
        }
    }

    // Return an owned cell so the cache lock is released before initialization or waiting.
    pub(crate) fn get_or_insert(&self, epoch: Epoch) -> RankMapCell {
        Arc::clone(
            self.entries
                .lock()
                .unwrap()
                .get_or_insert(epoch, || Arc::new(OnceCell::new())),
        )
    }
}

#[cfg(test)]
pub mod test {
    use {super::*, std::time::Duration};

    #[test]
    fn test_rank_map_cache_epochs_and_eviction() {
        let cache = RankMapCache::new(NonZeroUsize::new(5).unwrap());
        let first = cache.get_or_insert(0);
        assert!(Arc::ptr_eq(&first, &cache.get_or_insert(0)));
        let second = cache.get_or_insert(1);
        assert!(!Arc::ptr_eq(&first, &second));
        for epoch in 2..5 {
            cache.get_or_insert(epoch);
        }
        // A hit keeps the first map resident while the least recently used epoch is evicted.
        assert!(Arc::ptr_eq(&first, &cache.get_or_insert(0)));
        cache.get_or_insert(5);
        assert_eq!(cache.entries.lock().unwrap().len(), 5);
        assert!(!cache.entries.lock().unwrap().contains(&1));
        assert!(Arc::ptr_eq(&first, &cache.get_or_insert(0)));
    }

    #[tokio::test]
    async fn test_rank_map_cache_initializes_once() {
        let cache = RankMapCache::new(NonZeroUsize::new(5).unwrap());
        let cell = cache.get_or_insert(0);
        let response = Arc::new(RpcVatActiveSetRankMap {
            epoch: 0,
            total_stake: 1.try_into().unwrap(),
            validators: Vec::new(),
        });
        let (first, second) = tokio::join!(
            cell.get_or_init(|| async {
                tokio::task::yield_now().await;
                Arc::clone(&response)
            }),
            cell.get_or_init(|| async { panic!("map should already be initialized") }),
        );
        assert!(Arc::ptr_eq(first, &response));
        assert!(Arc::ptr_eq(second, &response));
        assert!(Arc::ptr_eq(
            cell.get_or_init(|| async { panic!("map should be cached") })
                .await,
            &response,
        ));
    }

    #[tokio::test]
    async fn test_rank_map_cache_initializes_epochs_independently() {
        let cache = RankMapCache::new(NonZeroUsize::new(2).unwrap());
        let first = cache.get_or_insert(0);
        let (started, wait_for_start) = tokio::sync::oneshot::channel();
        let (release, wait_for_release) = tokio::sync::oneshot::channel();

        let initialize_first = first.get_or_init(|| async {
            started.send(()).unwrap();
            wait_for_release.await.unwrap();
            Arc::new(RpcVatActiveSetRankMap {
                epoch: 0,
                total_stake: 1.try_into().unwrap(),
                validators: Vec::new(),
            })
        });
        let initialize_second = async {
            wait_for_start.await.unwrap();
            assert!(cache.entries.try_lock().is_ok());
            let second = cache.get_or_insert(1);
            let response = second
                .get_or_init(|| async {
                    Arc::new(RpcVatActiveSetRankMap {
                        epoch: 1,
                        total_stake: 1.try_into().unwrap(),
                        validators: Vec::new(),
                    })
                })
                .await;
            assert_eq!(response.epoch, 1);
            assert!(first.get().is_none());
            release.send(()).unwrap();
        };
        let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(initialize_first, initialize_second)
        })
        .await
        .unwrap();
        assert_eq!(response.epoch, 0);
    }

    #[tokio::test]
    async fn test_rank_map_cache_retries_failed_initialization() {
        let cache = RankMapCache::new(NonZeroUsize::new(1).unwrap());
        let cell = cache.get_or_insert(0);
        assert_eq!(cell.get_or_try_init(|| async { Err(()) }).await, Err(()));
        assert!(cell.get().is_none());

        let retry = cache.get_or_insert(0);
        assert!(Arc::ptr_eq(&cell, &retry));
        let response = retry
            .get_or_try_init(|| async {
                Ok::<_, ()>(Arc::new(RpcVatActiveSetRankMap {
                    epoch: 0,
                    total_stake: 1.try_into().unwrap(),
                    validators: Vec::new(),
                }))
            })
            .await
            .unwrap();
        assert_eq!(response.epoch, 0);
    }
}
