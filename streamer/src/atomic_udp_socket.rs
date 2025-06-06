// src/atomic_udp_socket.rs
use {
    arc_swap::ArcSwap,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

#[derive(Clone)]
pub struct AtomicUdpSocket {
    inner: Arc<ArcSwap<UdpSocket>>,
}
impl AtomicUdpSocket {
    pub fn new(sock: UdpSocket) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(sock)),
        }
    }
    #[inline]
    pub fn load(&self) -> Arc<UdpSocket> {
        self.inner.load_full()
    }
    #[inline]
    pub fn swap(&self, new_sock: UdpSocket) {
        info!("greg: AtomicUdpSocket::swap new_sock: {:?}", new_sock);
        self.inner.store(Arc::new(new_sock));
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.load().local_addr()
    }
}
