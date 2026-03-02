//! This module defines [`QuicSocket`] enum which wraps `std::net::UdpSocket` along with [`QuicXdpSocketBundle`].

use {
    agave_xdp::xdp_retransmitter::XdpSender,
    std::{
        fmt::{self, Debug},
        io::{self},
        net::SocketAddr,
    },
};

/// [`QuicSocket`] is a thin wrapper around `std::net::UdpSocket` that is introduced to simplify the switch
/// between kernel UDP socket and XDP socket.
#[derive(Debug)]
pub enum QuicSocket {
    /// A QUIC socket that uses XDP for sending and kernel UDP socket for receiving.
    Xdp(QuicXdpSocketBundle),
    /// A QUIC socket that uses kernel UDP socket for both sending and receiving. This is used when
    /// XDP is not available or disabled.
    Kernel(std::net::UdpSocket),
}

impl From<std::net::UdpSocket> for QuicSocket {
    fn from(socket: std::net::UdpSocket) -> Self {
        QuicSocket::Kernel(socket)
    }
}

impl QuicSocket {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            QuicSocket::Xdp(cfg) => cfg.socket.local_addr(),
            QuicSocket::Kernel(socket) => socket.local_addr(),
        }
    }
}

/// [`QuicXdpSocketBundle`] is a configuration struct for creating a QUIC socket.
///
/// We must create and bundle together both an `XdpSender` and a `std::net::UdpSocket` instead of
/// directly creating an `AsyncUdpSocket` instance because the underlying sockets can only be
/// constructed when a Tokio runtime is present. In the case of Streamer and other components, the
/// runtime is created deep inside the call stack. Therefore, we propagate this bundle up to the
/// Endpoint creation.
pub struct QuicXdpSocketBundle {
    pub socket: std::net::UdpSocket,
    pub xdp_sender: XdpSender,
}

impl Debug for QuicXdpSocketBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicXdpSocketBundle")
            .field("socket", &self.socket)
            .finish()
    }
}
