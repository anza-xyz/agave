pub use solana_ledger::blockstore_processor::CacheBlockMetaSender;
use {
    crossbeam_channel::{Receiver, TryRecvError},
    solana_ledger::blockstore::Blockstore,
    solana_measure::measure::Measure,
    solana_runtime::bank::Bank,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::Duration,
    },
};

pub type CacheBlockMetaReceiver = Receiver<Arc<Bank>>;

pub struct CacheBlockMetaService {
    thread_hdl: JoinHandle<()>,
}

// ReplayStage sends a new item to this service for every frozen Bank. Banks
// are frozen every 400ms at steady state so checking every 100ms is sufficient
const TRY_RECV_INTERVAL: Duration = Duration::from_millis(100);

const CACHE_BLOCK_TIME_WARNING_MS: u64 = 150;

impl CacheBlockMetaService {
    pub fn new(
        cache_block_meta_receiver: CacheBlockMetaReceiver,
        blockstore: Arc<Blockstore>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solCacheBlkTime".to_string())
            .spawn(move || loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }

                let bank = match cache_block_meta_receiver.try_recv() {
                    Ok(bank) => bank,
                    Err(TryRecvError::Empty) => {
                        sleep(TRY_RECV_INTERVAL);
                        continue;
                    }
                    Err(err @ TryRecvError::Disconnected) => {
                        info!("CacheBlockMetaService is stopping because {err}");
                        break;
                    }
                };

                let mut cache_block_meta_timer = Measure::start("cache_block_meta_timer");
                Self::cache_block_meta(&bank, &blockstore);
                cache_block_meta_timer.stop();
                if cache_block_meta_timer.as_ms() > CACHE_BLOCK_TIME_WARNING_MS {
                    warn!(
                        "cache_block_meta operation took: {}ms",
                        cache_block_meta_timer.as_ms()
                    );
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn cache_block_meta(bank: &Bank, blockstore: &Blockstore) {
        if let Err(e) = blockstore.cache_block_time(bank.slot(), bank.clock().unix_timestamp) {
            error!("cache_block_time failed: slot {:?} {:?}", bank.slot(), e);
        }
        if let Err(e) = blockstore.cache_block_height(bank.slot(), bank.block_height()) {
            error!("cache_block_height failed: slot {:?} {:?}", bank.slot(), e);
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
