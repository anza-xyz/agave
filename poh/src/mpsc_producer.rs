use {crate::ring_buffer::RingBuffer, std::sync::Arc};

pub struct Producer<T> {
    pub(crate) ring_buffer: Arc<RingBuffer<T>>,
}

impl<T> Producer<T> {
    pub fn try_push(&self, item: T) -> Result<(), T> {
        self.ring_buffer.try_push(item)
    }
}

// Explicitly implement Clone to account for types that don't implement Clone.
impl<T> Clone for Producer<T> {
    fn clone(&self) -> Self {
        Producer {
            ring_buffer: self.ring_buffer.clone(),
        }
    }
}
