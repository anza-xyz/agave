use {
    agave_xdp::xdp_retransmitter::{XdpAddrs, XdpSender},
    bytes::Bytes,
    crossbeam_channel::TrySendError,
    quinn::{
        AsyncUdpSocket, UdpPoller,
        udp::{RecvMeta, Transmit, UdpSocketState},
    },
    std::{
        fmt::{self, Debug},
        io::{self, IoSliceMut},
        net::{SocketAddr, SocketAddrV4},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, ready},
    },
    tokio::io::Interest,
};

#[derive(Debug)]
pub enum QuicSocket {
    /// A QUIC socket that uses XDP for sending and kernel UDP socket for receiving.
    Xdp(QuicXdpSocketConfig),
    /// A QUIC socket that uses kernel UDP socket for both sending and receiving. This is used when
    /// XDP is not available or disabled.
    Kernel(std::net::UdpSocket),
}

impl QuicSocket {
    pub fn new(socket: std::net::UdpSocket, xdp_sender: Option<XdpSender>) -> Self {
        if let Some(xdp_sender) = xdp_sender {
            Self::Xdp(QuicXdpSocketConfig { socket, xdp_sender })
        } else {
            Self::Kernel(socket)
        }
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

/// Config is required because we may construct underlying sockets only when tokio runtime is
/// present but in case of Streamer and other components runtimes are created deeply inside the call
/// stack. Hence, we propagte this Config up to the Endpoint creation.
pub struct QuicXdpSocketConfig {
    pub socket: std::net::UdpSocket,
    pub xdp_sender: XdpSender,
}

impl Debug for QuicXdpSocketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicXdpSocketConfig")
            .field("socket", &self.socket)
            .finish()
    }
}

struct IndexedXdpSender {
    xdp_sender: XdpSender,
    src_addr: SocketAddrV4,
    next_sender: AtomicUsize,
}

impl IndexedXdpSender {
    fn try_send(
        &self,
        destination: SocketAddr,
        payload: Bytes,
    ) -> Result<(), TrySendError<(XdpAddrs, Bytes, Option<SocketAddrV4>)>> {
        let sender_idx = self.next_sender.fetch_add(1, Ordering::Relaxed);
        self.xdp_sender
            .try_send(sender_idx, destination, payload, Some(self.src_addr))
    }
}

pub struct QuicXdpSocket {
    ingress_kernel_udp: UdpSocket,
    egress_xdp: IndexedXdpSender,
}

impl QuicXdpSocket {
    pub fn new(
        QuicXdpSocketConfig { socket, xdp_sender }: QuicXdpSocketConfig,
    ) -> io::Result<Self> {
        let src_addr = socket.local_addr()?;
        let SocketAddr::V4(src_addr) = src_addr else {
            panic!("IPv6 not supported");
        };

        Ok(Self {
            ingress_kernel_udp: UdpSocket::new(socket)?,
            egress_xdp: IndexedXdpSender {
                xdp_sender,
                src_addr,
                next_sender: AtomicUsize::new(0),
            },
        })
    }
}

impl fmt::Debug for QuicXdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicXdpSocket")
            .field("local_addr", &self.ingress_kernel_udp.local_addr())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct ReadyPoller;

impl UdpPoller for ReadyPoller {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for QuicXdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(ReadyPoller)
    }

    fn try_send(&self, t: &Transmit<'_>) -> io::Result<()> {
        let payload = Bytes::from(t.contents.to_vec());
        match self.egress_xdp.try_send(t.destination, payload) {
            Ok(()) => Ok(()),

            Err(TrySendError::Full(_)) => Err(io::ErrorKind::WouldBlock.into()),

            Err(TrySendError::Disconnected(_)) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.ingress_kernel_udp.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.ingress_kernel_udp.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        // no GSO batches, so each transmit describes exactly one datagram
        1
    }

    fn max_receive_segments(&self) -> usize {
        self.ingress_kernel_udp.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.ingress_kernel_udp.may_fragment()
    }
}

/// Adapted from quinn's `UdpSocket` which is private.
#[derive(Debug)]
struct UdpSocket {
    io: tokio::net::UdpSocket,
    inner: UdpSocketState,
}

impl UdpSocket {
    fn new(sock: std::net::UdpSocket) -> io::Result<Self> {
        Ok(Self {
            inner: UdpSocketState::new((&sock).into())?,
            io: tokio::net::UdpSocket::from_std(sock)?,
        })
    }
}

impl AsyncUdpSocket for UdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        unimplemented!("quic_xdp_socket does not support async IO on the kernel UDP socket")
    }

    fn try_send(&self, _transmit: &Transmit) -> io::Result<()> {
        unimplemented!("quic_xdp_socket does not support sending on the kernel UDP socket")
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            if let Ok(res) = self.io.try_io(Interest::READABLE, || {
                self.inner.recv((&self.io).into(), bufs, meta)
            }) {
                return Poll::Ready(Ok(res));
            }
        }
    }

    fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_gso_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.gro_segments()
    }
}
