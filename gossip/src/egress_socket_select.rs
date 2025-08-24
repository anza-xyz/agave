#[cfg(feature = "agave-unstable-api")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "agave-unstable-api")]
#[derive(Default)]
pub struct EgressSocketSelect {
    tvu_retransmit_active_offset: AtomicUsize,
    num_tvu_retransmit_sockets: usize,
}

#[cfg(feature = "agave-unstable-api")]
impl EgressSocketSelect {
    pub fn new(num_sockets: usize) -> Self {
        Self {
            tvu_retransmit_active_offset: AtomicUsize::new(0),
            num_tvu_retransmit_sockets: num_sockets,
        }
    }

    pub fn select_interface(&self, interface_index: usize) {
        self.tvu_retransmit_active_offset.store(
            interface_index * self.num_tvu_retransmit_sockets,
            Ordering::Release,
        );
    }

    pub fn active_offset(&self) -> usize {
        self.tvu_retransmit_active_offset.load(Ordering::Acquire)
    }

    pub fn num_retransmit_sockets_per_interface(&self) -> usize {
        self.num_tvu_retransmit_sockets
    }
}
