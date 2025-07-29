use {
    arc_swap::ArcSwap,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    },
};

/// Wrapper around UdpSocket that allows for atomic swapping of the socket.
#[derive(Debug)]
pub struct AtomicUdpSocketInner {
    socket: ArcSwap<UdpSocket>,
    did_change: AtomicBool,
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
                did_change: AtomicBool::new(false),
            }),
        }
    }

    #[inline]
    pub fn load(&self) -> Arc<UdpSocket> {
        self.inner.socket.load_full()
    }

    /// Returns true if the socket has changed since the last call to this method.
    ///
    /// Will swap the `did_change` flag to `false`.
    #[inline]
    pub fn did_change(&self) -> bool {
        self.inner.did_change.swap(false, Ordering::Acquire)
    }

    #[inline]
    pub fn swap(&self, new_sock: UdpSocket) {
        self.inner.socket.store(Arc::new(new_sock));
        self.inner.did_change.store(true, Ordering::Release);
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

    fn did_change(&self) -> bool;

    #[inline]
    fn current_socket_ref(&self) -> Arc<UdpSocket> {
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
    fn did_change(&self) -> bool {
        false
    }

    #[inline]
    fn current_socket(&self) -> CurrentSocket {
        CurrentSocket::Same(self.socket.clone())
    }
}

/// Hot-swappable `AtomicUdpSocket`
pub struct AtomicSocketProvider {
    atomic: Arc<AtomicUdpSocket>,
}

impl AtomicSocketProvider {
    pub fn new(atomic: Arc<AtomicUdpSocket>) -> Self {
        Self { atomic }
    }
}

impl SocketProvider for AtomicSocketProvider {
    #[inline]
    fn did_change(&self) -> bool {
        self.atomic.did_change()
    }

    // Check if the socket has changed since the last call
    #[inline]
    fn current_socket(&self) -> CurrentSocket {
        let sock = self.atomic.load();
        if self.did_change() {
            CurrentSocket::Changed(sock)
        } else {
            CurrentSocket::Same(sock)
        }
    }
}
