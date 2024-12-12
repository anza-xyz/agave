//! The `CacheBlockMetaService` is responsible for persisting block metadata
//! from banks into the `Blockstore`

pub use solana_ledger::blockstore_processor::CacheBlockMetaSender;
use {
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_ledger::blockstore::Blockstore,
    solana_runtime::bank::Bank,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub type CacheBlockMetaReceiver = Receiver<Arc<Bank>>;

pub struct CacheBlockMetaService {
    thread_hdl: JoinHandle<()>,
}

impl CacheBlockMetaService {
    pub fn new(
        cache_block_meta_receiver: CacheBlockMetaReceiver,
        blockstore: Arc<Blockstore>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solCacheBlkTime".to_string())
            .spawn(move || {
                info!("CacheBlockMetaService has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let bank = match cache_block_meta_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(bank) => bank,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(err @ RecvTimeoutError::Disconnected) => {
                            info!("CacheBlockMetaService is stopping because: {err}");
                            break;
                        },
                    };

                    Self::cache_block_meta(&bank, &blockstore);

                }
                info!("CacheBlockMetaService has stopped");
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
