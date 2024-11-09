use {crate::ring_buffer::RingBuffer, std::sync::Arc};

#[derive(Clone)]
pub struct Producer<T> {
    pub(crate) ring_buffer: Arc<RingBuffer<T>>,
}

impl<T> Producer<T> {
    pub fn try_push(&self, item: T) -> Result<(), T> {
        self.ring_buffer.try_push(item)
    }
}
