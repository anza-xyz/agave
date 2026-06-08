//! This module defines [`PinnedXdpSender`] which is a convenience wrapper around
//! [`agave_xdp::transmitter::XdpSender`] for the case when source address is fixed.
use {
    agave_xdp::transmitter as tx, bytes::Bytes, crossbeam_channel::TrySendError,
    std::net::SocketAddrV4,
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
    pub fn try_send(
        &self,
        sender_index: usize,
        addr: impl Into<tx::XdpAddrs>,
        payload: Bytes,
    ) -> Result<(), TrySendError<tx::BytesTxPacket>> {
        self.sender.try_send(
            sender_index,
            tx::BytesTxPacket::new(self.src_addr, addr, None, payload),
        )
    }
}
