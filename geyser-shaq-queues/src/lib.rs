use geyser_shaq_bindings::*;
use rts_alloc::Allocator;
use shaq::{error::Error as ShaqError, Consumer, Producer};
use std::{fs::OpenOptions, ptr::NonNull, str, sync::Arc, time::Duration};
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
    mode: QueueMode,
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
    /// Open existing queues (for additional producer threads)
    pub fn open(base_path: &str) -> Result<Self, GeyserQueueError> {
        Self::open_with_mode(base_path, QueueMode::default())
    }

    pub fn create(base_path: &str) -> Result<Self, GeyserQueueError> {
        Self::create_with_mode(base_path, QueueMode::default())
    }

    pub fn open_with_mode(base_path: &str, mode: QueueMode) -> Result<Self, GeyserQueueError> {
        use std::fs::OpenOptions;

        // Open file handles for all queues
        let accounts_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/accounts.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open accounts.shaq: {}", e))
            })?;

        let transactions_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/transactions.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open transactions.shaq: {}", e))
            })?;

        let slot_status_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/slot_status.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open slot_status.shaq: {}", e))
            })?;

        let block_metadata_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/block_metadata.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open block_metadata.shaq: {}", e))
            })?;

        let entries_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/entries.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open entries.shaq: {}", e))
            })?;

        let allocator_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/allocator", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open allocator: {}", e))
            })?;

        // Join using file references (note: unsafe blocks removed as they're not needed in v0.2.0)
        let accounts = Producer::join(&accounts_file)?;
        let transactions = Producer::join(&transactions_file)?;
        let slot_status = Producer::join(&slot_status_file)?;
        let block_metadata = Producer::join(&block_metadata_file)?;
        let entries = Producer::join(&entries_file)?;

        let allocator = Arc::new(unsafe {
            Allocator::join(&allocator_file, 0)? // worker_index = 0 for producer
        });

        Ok(Self {
            accounts,
            transactions,
            slot_status,
            block_metadata,
            entries,
            allocator,
            mode,
        })
    }

    /// Called by the validator on startup to create queues
    /// Called by the validator on startup to create queues
    pub fn create_with_mode(base_path: &str, mode: QueueMode) -> Result<Self, GeyserQueueError> {
        // Ensure the directory exists before creating files
        std::fs::create_dir_all(base_path).map_err(|e| {
            GeyserQueueError::InvalidPath(format!(
                "Failed to create directory {}: {}",
                base_path, e
            ))
        })?;

        // Create file handles for all queues
        let accounts_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/accounts.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to create accounts.shaq: {}", e))
            })?;

        let transactions_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/transactions.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to create transactions.shaq: {}", e))
            })?;

        let slot_status_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/slot_status.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to create slot_status.shaq: {}", e))
            })?;

        let block_metadata_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/block_metadata.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!(
                    "Failed to create block_metadata.shaq: {}",
                    e
                ))
            })?;

        let entries_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/entries.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to create entries.shaq: {}", e))
            })?;

        let allocator_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(format!("{}/allocator", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to create allocator: {}", e))
            })?;

        // Create queues using file references
        let accounts = Producer::create(
            &accounts_file,
            1024 * 1024 * 500, // 500MB Queue
        )?;

        let transactions = Producer::create(
            &transactions_file,
            1024 * 1024 * 50, // 50MB Queue
        )?;

        let slot_status = Producer::create(
            &slot_status_file,
            1024 * 1024, // 1MB Queue
        )?;

        let block_metadata = Producer::create(
            &block_metadata_file,
            1024 * 1024, // 1MB Queue
        )?;

        let entries = Producer::create(
            &entries_file,
            1024 * 1024, // 1MB Queue
        )?;

        let allocator = Arc::new(Allocator::create(
            &allocator_file,
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
            mode,
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
            QueueMode::Blocking {
                retry_interval,
                timeout,
            } => {
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
    /// Push account update - now uses static push_internal
    pub fn push_account_update(
        &mut self,
        msg: AccountUpdateMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.accounts, self.mode, msg, "accounts")
    }

    pub fn push_transaction(&mut self, msg: TransactionMessage) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.transactions, self.mode, msg, "transactions")
    }

    pub fn push_slot_status(&mut self, msg: SlotStatusMessage) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.slot_status, self.mode, msg, "slot_status")
    }

    pub fn push_block_metadata(
        &mut self,
        msg: BlockMetadataMessage,
    ) -> Result<(), GeyserQueueError> {
        Self::push_internal(&mut self.block_metadata, self.mode, msg, "block_metadata")
    }

    pub fn push_entry(&mut self, msg: EntryMessage) -> Result<(), GeyserQueueError> {
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
            QueueMode::Blocking {
                retry_interval,
                timeout,
            } => {
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
        use std::fs::OpenOptions;

        // Open file handles for all queues
        let accounts_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/accounts.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open accounts.shaq: {}", e))
            })?;

        let transactions_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/transactions.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open transactions.shaq: {}", e))
            })?;

        let slot_status_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/slot_status.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open slot_status.shaq: {}", e))
            })?;

        let block_metadata_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/block_metadata.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open block_metadata.shaq: {}", e))
            })?;

        let entries_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/entries.shaq", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open entries.shaq: {}", e))
            })?;

        // Join using file references
        let accounts = unsafe { Consumer::join(&accounts_file)? };
        let transactions = unsafe { Consumer::join(&transactions_file)? };
        let slot_status = unsafe { Consumer::join(&slot_status_file)? };
        let block_metadata = unsafe { Consumer::join(&block_metadata_file)? };
        let entries = unsafe { Consumer::join(&entries_file)? };

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
        use std::fs::OpenOptions;

        let consumers = Self::join(base_path)?;

        // Open allocator file
        let allocator_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/allocator", base_path))
            .map_err(|e| {
                GeyserQueueError::InvalidPath(format!("Failed to open allocator: {}", e))
            })?;

        // SAFETY: Consumer joins as worker_id=1 (producer is 0)
        let allocator = Arc::new(unsafe { Allocator::join(&allocator_file, 1)? });

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

        self.accounts.sync();

        let len = self.accounts.len();
        let capacity = self.accounts.capacity();

        // Handle transient wraparound state
        if len > capacity {
            return messages;
        }

        for _ in 0..max {
            let Some(ptr) = self.accounts.try_read() else {
                break;
            };
            let msg = unsafe { ptr.as_ptr().read() };
            messages.push(msg);
        }

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

#[cfg(test)]
mod queue_mode_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn setup_test_queues(mode: QueueMode) -> (TempDir, GeyserQueues) {
        let temp_dir = TempDir::new().unwrap();
        let queues =
            GeyserQueues::create_with_mode(temp_dir.path().to_str().unwrap(), mode).unwrap();
        (temp_dir, queues)
    }

    fn create_dummy_account_msg() -> AccountUpdateMessage {
        AccountUpdateMessage {
            slot: 1,
            is_startup: false,
            pubkey: [0u8; 32],
            lamports: 1000,
            owner: [0u8; 32],
            executable: false,
            rent_epoch: 0,
            data: SharableDataRegion {
                offset: 0,
                length: 0,
            },
            write_version: 1,
            txn_signature: [0; 64],
            has_txn: false,
        }
    }

    #[test]
    fn test_nonblocking_queue_full_immediate_error() {
        let (_temp, mut queues) = setup_test_queues(QueueMode::NonBlocking);

        // Fill queue to capacity
        let mut count = 0;
        loop {
            match queues.push_account_update(create_dummy_account_msg()) {
                Ok(_) => count += 1,
                Err(GeyserQueueError::QueueFull(name)) => {
                    assert_eq!(name, "accounts");
                    break;
                }
                Err(e) => panic!("Unexpected error: {:?}", e),
            }
        }

        println!("Queue filled with {} messages", count);
        assert!(count > 0, "Should have pushed at least some messages");

        // Verify next push fails immediately
        let start = Instant::now();
        let result = queues.push_account_update(create_dummy_account_msg());
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(GeyserQueueError::QueueFull(_))));
        assert!(
            elapsed < Duration::from_millis(10),
            "Should fail immediately"
        );
    }

    #[test]
    fn test_blocking_with_timeout_expires() {
        let mode = QueueMode::Blocking {
            retry_interval: Duration::from_millis(50),
            timeout: Some(Duration::from_millis(200)),
        };
        let (_temp, mut queues) = setup_test_queues(mode);

        // Fill queue
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        // Try to push with timeout - should retry and eventually timeout
        let start = Instant::now();
        let result = queues.push_account_update(create_dummy_account_msg());
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(GeyserQueueError::BlockingTimeout)));
        assert!(
            elapsed >= Duration::from_millis(200),
            "Should wait for timeout"
        );
        assert!(
            elapsed < Duration::from_millis(300),
            "Should not wait too long"
        );
    }

    #[test]
    fn test_blocking_succeeds_when_space_freed() {
        let mode = QueueMode::Blocking {
            retry_interval: Duration::from_millis(50),
            timeout: Some(Duration::from_secs(5)),
        };
        let (temp_dir, mut queues) = setup_test_queues(mode);

        // Fill queue
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        // Start consumer in background that drains queue
        let base_path = temp_dir.path().to_str().unwrap().to_string();
        let consumer_handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100)); // Let producer start blocking
            let mut consumers = GeyserConsumers::join(&base_path).unwrap();

            // Drain some messages
            for _ in 0..10 {
                consumers.pop_account_update();
                thread::sleep(Duration::from_millis(10));
            }
        });

        // This should block, then succeed once consumer frees space
        let start = Instant::now();
        let result = queues.push_account_update(create_dummy_account_msg());
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed after space freed");
        assert!(elapsed >= Duration::from_millis(100), "Should have blocked");

        consumer_handle.join().unwrap();
    }

    #[test]
    fn test_batch_nonblocking_partial_push() {
        let (_temp, mut queues) = setup_test_queues(QueueMode::NonBlocking);

        // Fill queue almost to capacity
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        // Try to push batch of 100 - should push partial
        let msgs: Vec<_> = (0..100).map(|_| create_dummy_account_msg()).collect();
        let result = queues.push_account_updates_batch(&msgs);

        match result {
            Ok(pushed) => {
                assert!(pushed < 100, "Should push partial batch");
                assert!(pushed == 0, "Queue is full, should push 0");
            }
            Err(e) => panic!("Batch should not error in non-blocking: {:?}", e),
        }
    }

    #[test]
    fn test_batch_blocking_with_timeout() {
        let mode = QueueMode::Blocking {
            retry_interval: Duration::from_millis(50),
            timeout: Some(Duration::from_millis(200)),
        };
        let (_temp, mut queues) = setup_test_queues(mode);

        // Fill queue
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        // Try batch push with no consumer - should timeout
        let msgs: Vec<_> = (0..10).map(|_| create_dummy_account_msg()).collect();
        let start = Instant::now();
        let result = queues.push_account_updates_batch(&msgs);
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(GeyserQueueError::BlockingTimeout)));
        assert!(elapsed >= Duration::from_millis(200));
    }

    #[test]
    fn test_mode_switching() {
        let (_temp, mut queues) = setup_test_queues(QueueMode::NonBlocking);

        // Fill queue in non-blocking mode
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        // Verify fails immediately
        let result = queues.push_account_update(create_dummy_account_msg());
        assert!(matches!(result, Err(GeyserQueueError::QueueFull(_))));

        // Switch to blocking mode
        queues.set_mode(QueueMode::Blocking {
            retry_interval: Duration::from_millis(50),
            timeout: Some(Duration::from_millis(100)),
        });

        // Now should timeout instead of immediate failure
        let start = Instant::now();
        let result = queues.push_account_update(create_dummy_account_msg());
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(GeyserQueueError::BlockingTimeout)));
        assert!(elapsed >= Duration::from_millis(100));
    }

    #[test]
    fn test_sync_behavior_under_load() {
        let mode = QueueMode::Blocking {
            retry_interval: Duration::from_millis(10),
            timeout: Some(Duration::from_secs(2)),
        };
        let (temp_dir, mut queues) = setup_test_queues(mode);

        // Fill queue
        loop {
            if queues
                .push_account_update(create_dummy_account_msg())
                .is_err()
            {
                break;
            }
        }

        let base_path = temp_dir.path().to_str().unwrap().to_string();
        let sync_count = Arc::new(Mutex::new(0));
        let sync_count_clone = sync_count.clone();

        // Consumer that counts sync calls
        let consumer_handle = thread::spawn(move || {
            let mut consumers = GeyserConsumers::join(&base_path).unwrap();
            for _ in 0..5 {
                thread::sleep(Duration::from_millis(50));
                consumers.pop_account_update();
                *sync_count_clone.lock().unwrap() += 1;
            }
        });

        // Producer should retry multiple times
        let result = queues.push_account_update(create_dummy_account_msg());
        assert!(result.is_ok());

        consumer_handle.join().unwrap();
        let syncs = *sync_count.lock().unwrap();
        assert!(syncs > 0, "Consumer should have freed space");
    }
}
