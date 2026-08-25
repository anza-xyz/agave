use std::{
    collections::{HashSet, VecDeque},
    hash::Hash,
};

/// A fixed size queue that does not allow duplicates and access to the front and
/// the back of the queue.
pub(crate) struct DedupQueue<T> {
    queue: VecDeque<T>,
    set: HashSet<T>,
    capacity: usize,
}

impl<T: Eq + Hash + Clone> DedupQueue<T> {
    /// Creates a new instance of `DedupQueue` with the given `capacity`.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Pushes `value` to the back of the queue.
    ///
    /// If the queue already has `value` then this function is a NOP.
    /// If the queue is full, then `Err(oldest_element)`.
    pub(crate) fn push_back_ejecting(&mut self, value: T) -> Result<(), T> {
        if self.set.contains(&value) {
            return Ok(());
        }
        let mut ret = Ok(());
        if self.queue.len() == self.capacity
            && let Some(old) = self.queue.pop_front()
        {
            self.set.remove(&old);
            ret = Err(old);
        }
        self.set.insert(value.clone());
        self.queue.push_back(value);
        ret
    }

    /// Pushes `value` to the front of the queue.
    ///
    /// If the queue already has `value` then this function is a NOP.
    /// If the queue is full, then an error and `Err(value)` is returned.
    pub(crate) fn try_push_front(&mut self, value: T) -> Result<(), T> {
        if self.set.contains(&value) {
            return Ok(());
        }
        if self.queue.len() == self.capacity {
            return Err(value);
        }
        self.set.insert(value.clone());
        self.queue.push_front(value);
        Ok(())
    }

    /// If the queue is not empty, then removes and returns the element in front of the queue.
    pub(crate) fn pop_front(&mut self) -> Option<T> {
        let value = self.queue.pop_front()?;
        self.set.remove(&value);
        Some(value)
    }

    #[must_use]
    /// Returns `true` is the queue is empty else `false`.
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_back_ejecting_deduplicates_and_ejects_oldest() {
        let mut queue = DedupQueue::new(2);

        assert_eq!(queue.push_back_ejecting(1), Ok(()));
        assert_eq!(queue.push_back_ejecting(2), Ok(()));
        // Inserting a duplicate is a no-op and does not change its position.
        assert_eq!(queue.push_back_ejecting(1), Ok(()));
        assert_eq!(queue.push_back_ejecting(3), Err(1));

        assert_eq!(queue.pop_front(), Some(2));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_try_push_front_restores_popped_value() {
        let mut queue = DedupQueue::new(2);
        queue.push_back_ejecting(1).unwrap();
        queue.push_back_ejecting(2).unwrap();

        let pending_value = queue.pop_front().unwrap();
        assert_eq!(queue.try_push_front(pending_value), Ok(()));

        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
        assert!(queue.is_empty());
    }

    #[test]
    fn test_try_push_front_rejects_new_value_when_full() {
        let mut queue = DedupQueue::new(2);
        queue.push_back_ejecting(1).unwrap();
        queue.push_back_ejecting(2).unwrap();

        assert_eq!(queue.try_push_front(3), Err(3));
        // A duplicate remains a no-op even when the queue is full.
        assert_eq!(queue.try_push_front(2), Ok(()));
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(2));
    }
}
