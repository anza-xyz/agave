use {
    arc_swap::ArcSwap,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

/// Wrapper around UdpSocket that allows for atomic swapping of the socket.
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
        self.inner.store(Arc::new(new_sock));
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.load().local_addr()
    }
}

/// Trait for providing a socket.
pub trait SocketProvider {
    fn current_socket(&mut self) -> (&UdpSocket, bool);
}

/// Fixed UDP Socket -> default
pub struct FixedSocketProvider {
    socket: Arc<UdpSocket>,
}
impl FixedSocketProvider {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self { socket }
    }
}
impl SocketProvider for FixedSocketProvider {
    #[inline]
    fn current_socket(&mut self) -> (&UdpSocket, bool) {
        (&self.socket, false)
    }
}

/// Hot-swappable `AtomicUdpSocket`
pub struct AtomicSocketProvider {
    atomic: Arc<AtomicUdpSocket>,
    last_id: usize,
    current: Arc<UdpSocket>,
}
impl AtomicSocketProvider {
    pub fn new(atomic: Arc<AtomicUdpSocket>) -> Self {
        let s = atomic.load();
        Self {
            atomic,
            current: s,
            last_id: 0,
        }
    }
}
impl SocketProvider for AtomicSocketProvider {
    // Check if the socket has changed since the last call
    #[inline]
    fn current_socket(&mut self) -> (&UdpSocket, bool) {
        let s = self.atomic.load();
        let id = Arc::as_ptr(&s) as usize;
        let changed = id != self.last_id;
        if changed {
            self.last_id = id;
            self.current = s;
        }
        (&*self.current, changed)
    }
}
