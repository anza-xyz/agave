//! This module defines [`PinnedXdpSender`] which is a convenience wrapper around
//! [`agave_xdp::transmitter::XdpSender`] for the case when source address is fixed.
use {
    agave_xdp::transmitter as tx,
    bytes::Bytes,
    std::{net::SocketAddrV4, time::Duration},
};

/// [`PinnedXdpSender`] simplifies sending packets over XDP
/// when source address is fixed for all items.
#[derive(Clone)]
pub struct PinnedXdpSender {
    sender: tx::XdpSender,
    src_addr: SocketAddrV4,
}

impl PinnedXdpSender {
    pub fn new(sender: tx::XdpSender, src_addr: SocketAddrV4) -> Self {
        Self { sender, src_addr }
    }

    #[inline]
    pub fn send_timeout(
        &self,
        sender_index: usize,
        addr: impl Into<tx::XdpAddrs>,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<(), tx::SendTimeoutError<tx::BytesTxPacket>> {
        self.sender.send_timeout(
            sender_index,
            tx::BytesTxPacket::new(self.src_addr, addr, None, payload),
            timeout,
        )
    }
}
