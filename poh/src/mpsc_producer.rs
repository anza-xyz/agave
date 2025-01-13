use {crate::ring_buffer::RingBuffer, std::sync::Arc};

pub struct Producer<T> {
    pub(crate) ring_buffer: Arc<RingBuffer<T>>,
}

impl<T> Producer<T> {
    pub fn try_push(&self, item: T) -> Result<(), T> {
        self.ring_buffer.try_push(item)
    }
    pub fn bank_slot(&self) -> u64 { self.ring_buffer.working_bank() }
    pub fn set_bank(&self, bank_id: u64) { self.ring_buffer.set_bank(bank_id); }
    pub fn empty(&self) -> bool { self.ring_buffer.empty() }
}

// Explicitly implement Clone to account for types that don't implement Clone.
impl<T> Clone for Producer<T> {
    fn clone(&self) -> Self {
        Producer {
            ring_buffer: self.ring_buffer.clone(),
        }
    }
}
