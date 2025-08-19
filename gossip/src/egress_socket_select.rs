#[cfg(feature = "agave-unstable-api")]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    OnceLock,
};

#[cfg(feature = "agave-unstable-api")]
/// Offset in the retransmit_sockets: Vec<UdpSocket>
static TVU_RETRANSMIT_ACTIVE_OFFSET: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "agave-unstable-api")]
/// Number of sockets per interface — initialized at startup
/// todo, could maybe make an arc and pass around... not sure
static NUM_TVU_RETRANSMIT_SOCKETS: OnceLock<usize> = OnceLock::new();

#[cfg(feature = "agave-unstable-api")]
pub fn init(num_sockets: usize) {
    NUM_TVU_RETRANSMIT_SOCKETS
        .set(num_sockets)
        .expect("should only call init() once");
    // start on primary interface (index 0)
    TVU_RETRANSMIT_ACTIVE_OFFSET.store(0, Ordering::Release);
}

#[cfg(feature = "agave-unstable-api")]
/// Hot swap egress sockets to `interface_index` (0 = primary, 1 = first backup, etc.).
pub fn select_interface(interface_index: usize) {
    let count = *NUM_TVU_RETRANSMIT_SOCKETS
        .get()
        .expect("should call init() first");
    TVU_RETRANSMIT_ACTIVE_OFFSET.store(interface_index * count, Ordering::Release);
}

#[cfg(feature = "agave-unstable-api")]
#[inline(always)]
pub fn active_offset() -> usize {
    TVU_RETRANSMIT_ACTIVE_OFFSET.load(Ordering::Acquire)
}

#[cfg(feature = "agave-unstable-api")]
/// Must call init() first
pub fn num_retransmit_sockets_per_interface() -> usize {
    *NUM_TVU_RETRANSMIT_SOCKETS
        .get()
        .expect("egress_socket_select::init must be called before use")
}
