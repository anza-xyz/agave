use geyser_shaq_bindings::*;
use rts_alloc::Allocator;
use shaq::{error::Error as ShaqError, Consumer, Producer};
use std::{ptr::NonNull, str, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Drop events if queue is full (default for live validator)
    #[default]
    NonBlocking,
    /// Block until space available (for offline replay / data integrity)
    Blocking { 
        /// How long to wait between retries when queue is full
        retry_interval: Duration,
        /// Optional maximum wait time before giving up
        timeout: Option<Duration>,
    },
}

pub struct GeyserQueues {
    pub accounts: Producer<AccountUpdateMessage>,
    pub transactions: Producer<TransactionMessage>,
    pub slot_status: Producer<SlotStatusMessage>,
    pub block_metadata: Producer<BlockMetadataMessage>,
    pub entries: Producer<EntryMessage>,
    allocator: Arc<Allocator>,
    mode: QueueMode
}

// Debug implementation
impl std::fmt::Debug for GeyserQueues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeyserQueues")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

unsafe impl Send for GeyserQueues {}
unsafe impl Sync for GeyserQueues {}

#[derive(Debug, Error)]
pub enum GeyserQueueError {
    #[error("Shaq error: {0:?}")]
    ShaqError(ShaqError),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("RTS Alloc error: {0:?}")]
    RtsAllocError(rts_alloc::error::Error),

    #[error("Insufficient space for queue")]
    InsufficientSpace,

    #[error("{0} queue is full")]
    QueueFull(String),

    #[error("Blocking operation timed out")]
    BlockingTimeout,
}


impl From<ShaqError> for GeyserQueueError {
    fn from(err: ShaqError) -> Self {
        GeyserQueueError::ShaqError(err)
    }
}

impl From<rts_alloc::error::Error> for GeyserQueueError {
    fn from(err: rts_alloc::error::Error) -> Self {
        GeyserQueueError::RtsAllocError(err)
    }
}

impl GeyserQueues {
    pub fn create(base_path: &str) -> Result<Self, GeyserQueueError> {
        Self::create_with_mode(base_path, QueueMode::default())
    }
    /// Called by the validator on startup to create queues
    pub fn create_with_mode(base_path: &str, mode: QueueMode) -> Result<Self, GeyserQueueError> {
        let accounts = Producer::create(
            &format!("{}/accounts.shaq", base_path),
            1024 * 1024 * 100, // 100MB Queue
        )?;

        let transactions = Producer::create(
            &format!("{}/transactions.shaq", base_path),
            1024 * 1024 * 50, // 50MB Queue
        )?;

        let slot_status = Producer::create(
            &format!("{}/slot_status.shaq", base_path),
            1024 * 1024, // 1MB Queue
        )?;

        let block_metadata = Producer::create(
            &format!("{}/block_metadata.shaq", base_path),
            1024 * 1024, // 1MB Queue
        )?;

        let entries = Producer::create(
            &format!("{}/entries.shaq", base_path),
            1024 * 1024, // 1MB Queue
        )?;

        let allocator = Arc::new(Allocator::create(
            format!("{}/allocator", base_path),
            1024 * 1024 * 1024, // 1GB file size
            2,                  // num_workers (producer=0 + 1 consumer)
            64 * 1024,          // 64KB slab size
            0,                  // worker_index = 0 (producer)
        )?);

        Ok(Self {
            accounts,
            transactions,
            slot_status,
            block_metadata,
            entries,
            allocator,
            mode
        })
    }

    /// Generic push that handles blocking/non-blocking logic
    fn push_internal<T>(
        producer: &mut Producer<T>,
        mode: QueueMode,
        msg: T,
        queue_name: &str,
    ) -> Result<(), GeyserQueueError> {
        match mode {
            QueueMode::NonBlocking => {
                let Some(ptr) = producer.reserve() else {
                    return Err(GeyserQueueError::QueueFull(queue_name.to_string()));
                };
                unsafe { ptr.as_ptr().write(msg) };
                producer.commit();
                Ok(())
            }
            QueueMode::Blocking { retry_interval, timeout } => {
                let start = std::time::Instant::now();
                loop {
                    if let Some(ptr) = producer.reserve() {
                        unsafe { ptr.as_ptr().write(msg) };
                        producer.commit();
                        return Ok(());
                    }

                    if let Some(max_wait) = timeout {
                        if start.elapsed() >= max_wait {
                            return Err(GeyserQueueError::BlockingTimeout);
                        }
                    }

                    producer.sync();
                    std::thread::sleep(retry_interval);
                }
            }
        }
    }

    /// Push account update using reserve/commit pattern
    /// Note: Requires &mut self because Producer::reserve() modifies internal cache
    /// /// Push account update - now uses static push_internal
    pub fn push_account_update(
        &mut self,
        msg: AccountUpdateMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.accounts, self.mode, msg, "accounts")
    }

    pub fn push_transaction(
        &mut self,
        msg: TransactionMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.transactions, self.mode, msg, "transactions")
    }

    pub fn push_slot_status(
        &mut self,
        msg: SlotStatusMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.slot_status, self.mode, msg, "slot_status")
    }

    pub fn push_block_metadata(
        &mut self,
        msg: BlockMetadataMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.block_metadata, self.mode, msg, "block_metadata")
    }

    pub fn push_entry(
        &mut self,
        msg: EntryMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.entries, self.mode, msg, "entries")
    }


    /// Batch push - reserves multiple slots then commits once
    /// This is more efficient than individual pushes
    pub fn push_account_updates_batch(
        &mut self,
        msgs: &[AccountUpdateMessage],
    ) -> Result<usize, GeyserQueueError> {
        match self.mode {
            QueueMode::NonBlocking => {
                let mut pushed = 0;
                for msg in msgs {
                    let Some(ptr) = self.accounts.reserve() else {
                        break;
                    };
                    unsafe { ptr.as_ptr().write(msg.clone()) };
                    pushed += 1;
                }

                // Single commit for entire batch - this publishes all reserved writes atomically
                if pushed > 0 {
                    self.accounts.commit();
                }
                Ok(pushed)
            }
            QueueMode::Blocking { retry_interval, timeout } => {
                let start = std::time::Instant::now();
                let mut pushed = 0;

                while pushed < msgs.len() {
                    let mut batch_pushed = 0;
                    for msg in &msgs[pushed..] {
                        let Some(ptr) = self.accounts.reserve() else {
                            break;
                        };
                        unsafe { ptr.as_ptr().write(msg.clone()) };
                        batch_pushed += 1;
                    }

                    if batch_pushed > 0 {
                        self.accounts.commit();
                        pushed += batch_pushed;
                    }

                    if pushed == msgs.len() {
                        return Ok(pushed);
                    }

                    if let Some(max_wait) = timeout {
                        if start.elapsed() >= max_wait {
                            return Err(GeyserQueueError::BlockingTimeout);
                        }
                    }

                    self.accounts.sync();
                    std::thread::sleep(retry_interval);
                }
                Ok(pushed)
            }
        }
    }

    /// Sync producer with consumer's read position
    /// Call periodically to see how much space is available
    pub fn sync(&mut self) {
        self.accounts.sync();
        self.transactions.sync();
        self.slot_status.sync();
        self.block_metadata.sync();
        self.entries.sync();
    }

    /// Allocate data into shared memory and return region descriptor
    pub fn allocate_data(&self, data: &[u8]) -> Result<SharableDataRegion, GeyserQueueError> {
        if data.is_empty() {
            return Ok(SharableDataRegion {
                offset: 0,
                length: 0,
            });
        }

        let data_len: u32 = data.len().try_into().map_err(|_| {
            GeyserQueueError::InvalidPath("Data too large for allocation".to_string())
        })?;

        let ptr = self
            .allocator
            .allocate(data_len)
            .ok_or(GeyserQueueError::InsufficientSpace)?;

        let offset: usize = unsafe { self.allocator.offset(ptr) };

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.as_ptr(), data.len());
        }

        Ok(SharableDataRegion {
            offset,
            length: data_len,
        })
    }

    /// Get allocator reference (useful for metrics)
    pub fn allocator(&self) -> &Arc<Allocator> {
        &self.allocator
    }

    pub fn mode(&self) -> QueueMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }
}

pub struct GeyserConsumers {
    pub accounts: Consumer<AccountUpdateMessage>,
    pub transactions: Consumer<TransactionMessage>,
    pub slot_status: Consumer<SlotStatusMessage>,
    pub block_metadata: Consumer<BlockMetadataMessage>,
    pub entries: Consumer<EntryMessage>,
}

impl GeyserConsumers {
    /// Join existing queues (consumer side)
    pub fn join(base_path: &str) -> Result<Self, GeyserQueueError> {
        // SAFETY: Consumer uniquely accesses the queue
        let accounts = unsafe { Consumer::join(&format!("{}/accounts.shaq", base_path))? };
        let transactions = unsafe { Consumer::join(&format!("{}/transactions.shaq", base_path))? };
        let slot_status = unsafe { Consumer::join(&format!("{}/slot_status.shaq", base_path))? };
        let block_metadata =
            unsafe { Consumer::join(&format!("{}/block_metadata.shaq", base_path))? };
        let entries = unsafe { Consumer::join(&format!("{}/entries.shaq", base_path))? };

        Ok(Self {
            accounts,
            transactions,
            slot_status,
            block_metadata,
            entries,
        })
    }

    /// Join both queues and allocator (consumer needs allocator to free data)
    pub fn join_with_allocator(
        base_path: &str,
    ) -> Result<(Self, Arc<Allocator>), GeyserQueueError> {
        let consumers = Self::join(base_path)?;

        // SAFETY: Consumer joins as worker_id=1 (producer is 0)
        let allocator = Arc::new(unsafe {
            Allocator::join(format!("{}/allocator", base_path), 1)?
        });

        Ok((consumers, allocator))
    }

    /// Pop single account update
    pub fn pop_account_update(&mut self) -> Option<AccountUpdateMessage> {
        let ptr = self.accounts.try_read()?;
        let msg = unsafe { ptr.as_ptr().read() };
        self.accounts.finalize();
        Some(msg)
    }

    /// Batch pop - more efficient than individual pops
    pub fn pop_account_updates_batch(&mut self, max: usize) -> Vec<AccountUpdateMessage> {
        let mut messages = Vec::with_capacity(max);

        // Sync to see latest writes
        self.accounts.sync();

        // Read up to max items
        for _ in 0..max {
            let Some(ptr) = self.accounts.try_read() else {
                break;
            };
            let msg = unsafe { ptr.as_ptr().read() };
            messages.push(msg);
        }

        // Finalize marks all read items as consumed
        if !messages.is_empty() {
            self.accounts.finalize();
        }

        messages
    }

    /// Sync all queues to see latest writes
    pub fn sync(&mut self) {
        self.accounts.sync();
        self.transactions.sync();
        self.slot_status.sync();
        self.block_metadata.sync();
        self.entries.sync();
    }

    /// Free shared memory data after processing
    /// CRITICAL: Only call this AFTER you've finished processing the message
    /// 
    /// Memory safety contract:
    /// - Consumer MUST free allocations in the order they were created
    /// - Consumer MUST NOT free the same region twice
    /// - Consumer MUST NOT access data after freeing
    pub fn free_data(&self, allocator: &Allocator, region: &SharableDataRegion) {
        if region.length > 0 {
            // Convert offset back to pointer
            let ptr: NonNull<u8> = allocator.ptr_from_offset(region.offset);
            
            // Free the allocation
            // SAFETY: 
            // - ptr was allocated by this allocator
            // - ptr has not been freed before (caller's responsibility)
            unsafe {
                allocator.free(ptr);
            }
        }
    }
}