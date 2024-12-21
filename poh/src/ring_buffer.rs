use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Simple cell holding a value that has a thread-safe
/// flag to indicate if the value is set or not.
struct AtomicCell<T> {
    /// A flag to indicate if the value is set or not.
    is_set: AtomicBool,
    /// Value stored in the cell.
    value: UnsafeCell<Option<T>>,
}

impl<T> Default for AtomicCell<T> {
    fn default() -> Self {
        Self {
            is_set: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }
}

impl<T> AtomicCell<T> {
    /// Sets the value in the cell.
    fn set(&self, value: T) {
        debug_assert!(!self.is_set.load(Ordering::Acquire));

        unsafe { *self.value.get() = Some(value) };
        self.is_set.store(true, Ordering::Release);
    }

    fn try_take(&self) -> Option<T> {
        if self.is_set.load(Ordering::Acquire) {
            self.is_set.store(false, Ordering::Release);
            let value = unsafe { &mut *self.value.get() }
                .take()
                .expect("value is set");
            Some(value)
        } else {
            None
        }
    }
}

enum ProducerCheckResult<'a, T> {
    /// The buffer is full or shut down.
    Full,
    /// The buffer has space for the producer to write.
    CanWrite(&'a AtomicCell<T>),
}

enum ConsumerCheckResult<T> {
    /// The buffer is empty or shut down.
    NoItems,
    /// The buffer positions indicate there is an item to consume
    /// at the given index, but the item has not been written yet.
    NotReady,
    /// The item was found at the given index.
    Found(T),
}

pub struct RingBuffer<T> {
    buffer: Box<[AtomicCell<T>]>,

    /// The position for a producer to write at.
    ///
    /// On push, this is optimistically updated by the producer.
    /// If the buffer is found to be full or shut off, the producer will reset
    /// this position. This decision was made since being full or shut off are
    /// typically more rare events than the buffer having space for the
    /// use-case in mind.
    producer_position: AtomicU64,

    /// The position for the consumer to read from.
    /// It is updated only by the consumer, but is read by the `producer` to
    /// determine if the buffer is either full or shut down.
    consumer_position: AtomicU64,

    /// Indicates if producers have been shut down.
    /// This is used by consumer so it can still consume even though producers
    /// have been shut down.
    /// This is used by the producer as a quick check before doing the
    /// optimistic update of position. This is to avoid the producers
    /// continuously updating the position when they are shut off which can
    /// cause contention with the consumer.
    producers_shut_down: AtomicBool,
}

unsafe impl<T> Sync for RingBuffer<T> {}

impl<T> RingBuffer<T> {
    pub fn with_capacity(capacity: u32) -> Self {
        let capacity = capacity.next_power_of_two();
        let buffer = (0..capacity)
            .map(|_| AtomicCell::default())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            buffer,
            producer_position: AtomicU64::new(0),
            consumer_position: AtomicU64::new(0),
            producers_shut_down: AtomicBool::new(false),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.buffer.len() as u64
    }

    pub fn try_push(&self, item: T) -> Result<(), T> {
        let ProducerCheckResult::CanWrite(cell) = self.producer_check() else {
            return Err(item);
        };

        cell.set(item);
        Ok(())
    }

    fn producer_check(&self) -> ProducerCheckResult<T> {
        if self.is_shut_down() {
            return ProducerCheckResult::Full;
        }

        // Optimistically update the producer position.
        let producer_position = self.producer_position.fetch_add(1, Ordering::AcqRel);

        // Check if the buffer is full.
        if producer_position.wrapping_sub(self.consumer_position.load(Ordering::Acquire))
            >= self.capacity()
        {
            // Reset the producer position.
            self.producer_position.fetch_sub(1, Ordering::AcqRel);
            ProducerCheckResult::Full
        } else {
            let index = self.position_to_index(producer_position);
            ProducerCheckResult::CanWrite(&self.buffer[index])
        }
    }

    pub fn pop(&self) -> Option<T> {
        loop {
            match self.check_consumer() {
                ConsumerCheckResult::NoItems => return None,
                ConsumerCheckResult::NotReady => {
                    core::hint::spin_loop();
                    continue;
                }
                ConsumerCheckResult::Found(value) => return Some(value),
            }
        }
    }

    fn check_consumer(&self) -> ConsumerCheckResult<T> {
        // Check if the buffer is empty.
        let consumer_position = self.consumer_position.load(Ordering::Acquire);
        let producer_position = self.producer_position.load(Ordering::Acquire);
        if consumer_position == producer_position
            || (self.is_shut_down()
                && producer_position.wrapping_sub(consumer_position) == self.capacity())
        {
            return ConsumerCheckResult::NoItems;
        }

        // Try to take the item from the buffer.
        // It may not be possible due to a few reasons:
        // 1. The producer has not written the item yet.
        // 2. The producer position had an optimistic update.
        let index = self.position_to_index(consumer_position);
        if let Some(value) = self.buffer[index].try_take() {
            self.consumer_position.fetch_add(1, Ordering::AcqRel);
            ConsumerCheckResult::Found(value)
        } else {
            ConsumerCheckResult::NotReady
        }
    }

    /// Shut producers off by making the buffer appear full.
    pub fn shut_off_producers(&self) {
        if self.is_shut_down() {
            return;
        }

        self.producer_position
            .fetch_add(self.capacity(), Ordering::AcqRel);
        self.producers_shut_down.store(true, Ordering::Release);
    }

    /// Re-enable producers by making the buffer appear normally.
    pub fn enable_producers(&self) {
        if !self.is_shut_down() {
            return;
        }

        self.producer_position
            .fetch_sub(self.capacity(), Ordering::AcqRel);
        self.producers_shut_down.store(false, Ordering::Release);
    }

    fn position_to_index(&self, position: u64) -> usize {
        let mask = self.capacity() - 1;
        (position & mask) as usize
    }

    fn is_shut_down(&self) -> bool {
        self.producers_shut_down.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer() {
        let ring_buffer = RingBuffer::with_capacity(4);

        assert_eq!(ring_buffer.capacity(), 4);
        assert_eq!(ring_buffer.pop(), None);

        assert!(ring_buffer.try_push(1).is_ok());
        assert!(ring_buffer.try_push(2).is_ok());
        assert!(ring_buffer.try_push(3).is_ok());
        assert!(ring_buffer.try_push(4).is_ok());
        assert_eq!(ring_buffer.try_push(5), Err(5));

        assert_eq!(ring_buffer.pop(), Some(1));
        assert_eq!(ring_buffer.pop(), Some(2));
        assert_eq!(ring_buffer.pop(), Some(3));
        assert_eq!(ring_buffer.pop(), Some(4));
        assert_eq!(ring_buffer.pop(), None);

        eprintln!("shut off producers");
        ring_buffer.shut_off_producers();
        assert_eq!(ring_buffer.try_push(6), Err(6));
        assert_eq!(ring_buffer.pop(), None);
    }

    #[test]
    fn test_buffer_wrap_around() {
        let ring_buffer = RingBuffer::with_capacity(4);
        // Move both positions to the end of the buffer.
        ring_buffer
            .producer_position
            .store(u64::MAX, Ordering::Release);
        ring_buffer
            .consumer_position
            .store(u64::MAX, Ordering::Release);

        // Push items into the buffer and make sure our positional checks on
        // the buffer wrap around correctly.
        assert!(ring_buffer.try_push(1).is_ok());
        assert!(ring_buffer.try_push(2).is_ok());
        assert!(ring_buffer.try_push(3).is_ok());
        assert!(ring_buffer.try_push(4).is_ok());
        assert_eq!(ring_buffer.try_push(5), Err(5));

        assert_eq!(ring_buffer.pop(), Some(1));
        assert_eq!(ring_buffer.pop(), Some(2));
        assert_eq!(ring_buffer.pop(), Some(3));
        assert_eq!(ring_buffer.pop(), Some(4));
        assert_eq!(ring_buffer.pop(), None);
    }

    #[test]
    fn test_ring_buffer_optimistic_update() {
        let ring_buffer = RingBuffer::with_capacity(4);

        // The producer should be able to find a cell to write to in the buffer.
        let ProducerCheckResult::CanWrite(cell) = ring_buffer.producer_check() else {
            panic!("expected CanWrite");
        };

        // In this test, we delay writing to the cell to simulate 2 threads running.
        // If the consumer tries to read at this point, it should see that the
        // buffer is **not** empty, since the producer has updated the position.
        // However, it should not be able to find the item yet, since it has
        // not yet been written.
        let consumer_check_result = ring_buffer.check_consumer();
        assert!(matches!(
            consumer_check_result,
            ConsumerCheckResult::NotReady
        ));

        // The producer can write to the cell.
        cell.set(1);

        // The consumer should now be able to read the item.
        let consumer_check_result = ring_buffer.check_consumer();
        assert!(matches!(
            consumer_check_result,
            ConsumerCheckResult::Found(1)
        ));

        // The buffer should not indicate empty.
        assert!(matches!(
            ring_buffer.check_consumer(),
            ConsumerCheckResult::NoItems
        ));
    }

    #[test]
    fn test_ring_buffer_optimistic_rollback() {
        let ring_buffer = RingBuffer::<i32>::with_capacity(4);

        // This test spoofs the rollback of an optimistic update.
        // The producer should find a cell to write to in the buffer.
        let ProducerCheckResult::CanWrite(_cell) = ring_buffer.producer_check() else {
            panic!("expected CanWrite");
        };

        // The consumer finds the buffer not empty, but the item is not ready.
        let consumer_check_result = ring_buffer.check_consumer();
        assert!(matches!(
            consumer_check_result,
            ConsumerCheckResult::NotReady
        ));

        // Rollback the optimistic update by the producer.
        // Under normal circumstances this should only happen when the buffer
        // is full or the producers have just been shut down.
        ring_buffer.producer_position.fetch_sub(1, Ordering::AcqRel);

        // The buffer should indicate empty.
        assert!(matches!(
            ring_buffer.check_consumer(),
            ConsumerCheckResult::NoItems
        ));
    }

    #[test]
    fn test_ring_buffer_many_producers_write() {
        let ring_buffer = RingBuffer::with_capacity(4);

        // Simulate 5 concurrent producers writing to the buffer.
        let producer_check_results: [_; 5] = core::array::from_fn(|_| ring_buffer.producer_check());

        // The first 4 producers should be able to write to the buffer.
        for (index, result) in producer_check_results[..4].iter().enumerate() {
            assert!(
                matches!(result, ProducerCheckResult::CanWrite(_)),
                "index: {index}",
            );
        }
        // The 5th producer should not be able to write to the buffer.
        assert!(matches!(
            producer_check_results[4],
            ProducerCheckResult::Full
        ));

        // Before the producers actually place the items in the buffer, the
        // consumer should not be able to find any items.
        assert!(matches!(
            ring_buffer.check_consumer(),
            ConsumerCheckResult::NotReady
        ));

        // The first 4 producers write to the buffer.
        for (index, producer_check_result) in producer_check_results[..4].iter().enumerate() {
            let ProducerCheckResult::CanWrite(cell) = producer_check_result else {
                panic!("expected CanWrite")
            };

            cell.set(index as i32);
        }

        // The consumer should be able to read the items from the buffer.
        for index in 0..4 {
            assert_eq!(ring_buffer.pop(), Some(index));
        }
    }

    #[test]
    fn test_ring_buffer_shut_off() {
        let ring_buffer = RingBuffer::with_capacity(4);

        // Push a few items into the buffer.
        assert!(ring_buffer.try_push(1).is_ok());
        assert!(ring_buffer.try_push(2).is_ok());
        assert!(ring_buffer.try_push(3).is_ok());

        // Shut off the producers.
        ring_buffer.shut_off_producers();

        // The buffer should appear full to the producers.
        assert_eq!(ring_buffer.try_push(4), Err(4));

        // The consumer should still be able to read the items.
        assert_eq!(ring_buffer.pop(), Some(1));
        assert_eq!(ring_buffer.pop(), Some(2));
        assert_eq!(ring_buffer.pop(), Some(3));
        assert_eq!(ring_buffer.pop(), None);

        // The producers should still be shut off.
        assert_eq!(ring_buffer.try_push(5), Err(5));
    }
}
