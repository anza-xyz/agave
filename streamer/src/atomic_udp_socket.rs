use {
    arc_swap::ArcSwap,
    std::{
        cell::RefCell,
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

/// Wrapper around UdpSocket that allows for atomic swapping of the socket.
#[derive(Debug)]
pub struct AtomicUdpSocketInner {
    socket: ArcSwap<UdpSocket>,
}

#[derive(Debug, Clone)]
pub struct AtomicUdpSocket {
    inner: Arc<AtomicUdpSocketInner>,
}

impl AtomicUdpSocket {
    pub fn new(sock: UdpSocket) -> Self {
        Self {
            inner: Arc::new(AtomicUdpSocketInner {
                socket: ArcSwap::from_pointee(sock),
            }),
        }
    }

    #[inline]
    pub fn load(&self) -> Arc<UdpSocket> {
        self.inner.socket.load_full()
    }

    #[inline]
    pub fn swap(&self, new_sock: UdpSocket) {
        self.inner.socket.store(Arc::new(new_sock));
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.socket.load().local_addr()
    }
}

pub enum CurrentSocket {
    Same(Arc<UdpSocket>),
    Changed(Arc<UdpSocket>),
}

/// Trait for providing a socket.
pub trait SocketProvider {
    fn current_socket(&self) -> CurrentSocket;

    fn current_socket_ref(&self) -> Arc<UdpSocket>;
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
    fn current_socket(&self) -> CurrentSocket {
        CurrentSocket::Same(self.socket.clone())
    }

    #[inline]
    fn current_socket_ref(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }
}

/// Hot-swappable `AtomicUdpSocket`
pub struct AtomicSocketProvider {
    atomic: Arc<AtomicUdpSocket>,
    current: RefCell<Arc<UdpSocket>>, // per‑provider cached pointer
}

impl AtomicSocketProvider {
    pub fn new(atomic: Arc<AtomicUdpSocket>) -> Self {
        Self {
            current: RefCell::new(atomic.load()),
            atomic,
        }
    }
}

impl SocketProvider for AtomicSocketProvider {
    // Check if the socket has changed since the last call
    #[inline]
    fn current_socket(&self) -> CurrentSocket {
        let new_sock = self.atomic.load();
        let mut cached = self.current.borrow_mut();

        if !Arc::ptr_eq(&new_sock, &*cached) {
            *cached = new_sock.clone(); // update cache
            CurrentSocket::Changed(new_sock)
        } else {
            CurrentSocket::Same(new_sock)
        }
    }

    #[inline]
    fn current_socket_ref(&self) -> Arc<UdpSocket> {
        match self.current_socket() {
            CurrentSocket::Same(s) | CurrentSocket::Changed(s) => s,
        }
    }
}
