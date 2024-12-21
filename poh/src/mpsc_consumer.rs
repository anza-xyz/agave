use {crate::ring_buffer::RingBuffer, std::sync::Arc};

pub struct Consumer<T> {
    pub(crate) ring_buffer: Arc<RingBuffer<T>>,
}

impl<T> Consumer<T> {
    pub fn pop(&self) -> Option<T> {
        self.ring_buffer.pop()
    }

    pub fn shut_off_producers(&self) {
        self.ring_buffer.shut_off_producers();
    }

    pub fn enable_producers(&self) {
        self.ring_buffer.enable_producers();
    }
}
