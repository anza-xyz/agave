use {
    arc_swap::ArcSwap,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

/// Wrapper around UdpSocket that allows for atomic swapping of the socket.
#[derive(Clone, Debug)]
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

pub enum CurrentSocket<'a> {
    Same(&'a UdpSocket),
    Changed(&'a UdpSocket),
}

/// Trait for providing a socket.
pub trait SocketProvider {
    fn current_socket(&mut self) -> CurrentSocket;

    #[inline]
    fn current_socket_ref(&mut self) -> &UdpSocket {
        match self.current_socket() {
            CurrentSocket::Same(sock) | CurrentSocket::Changed(sock) => sock,
        }
    }
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
    fn current_socket(&mut self) -> CurrentSocket {
        CurrentSocket::Same(&self.socket)
    }
}

/// Hot-swappable `AtomicUdpSocket`
pub struct AtomicSocketProvider {
    atomic: Arc<AtomicUdpSocket>,
    current: Arc<UdpSocket>,
}
impl AtomicSocketProvider {
    pub fn new(atomic: Arc<AtomicUdpSocket>) -> Self {
        let s = atomic.load();
        Self { atomic, current: s }
    }
}
impl SocketProvider for AtomicSocketProvider {
    // Check if the socket has changed since the last call
    #[inline]
    fn current_socket(&mut self) -> CurrentSocket {
        let sock = self.atomic.load();
        if !Arc::ptr_eq(&sock, &self.current) {
            self.current = sock;
            CurrentSocket::Changed(&self.current)
        } else {
            CurrentSocket::Same(&self.current)
        }
    }
}
