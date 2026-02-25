//! This module defines [`XdpSender`] which is a convenience wrapper around
//! [`agave_xdp::xdp_retransmitter::XdpSender`] for the case when source address is fixed for all
//! items like it is in turbine.
use {
    agave_xdp::xdp_retransmitter as tx, bytes::Bytes, crossbeam_channel::TrySendError,
    std::net::SocketAddrV4,
};

/// [`XdpSender`] is a structure that simplifies sending packets over XDP with `XdpSender`
/// when source address is fixed for all items.
#[derive(Clone)]
pub struct XdpSender {
    sender: tx::XdpSender,
    src_addr: SocketAddrV4,
}

impl XdpSender {
    pub fn new(sender: tx::XdpSender, src_addr: SocketAddrV4) -> Self {
        Self { sender, src_addr }
    }

    #[inline]
    pub fn try_send(
        &self,
        sender_index: usize,
        addr: impl Into<tx::XdpAddrs>,
        payload: Bytes,
    ) -> Result<(), TrySendError<tx::XdpTransmitItem>> {
        self.sender.try_send(
            sender_index,
            tx::XdpTransmitItem::new(self.src_addr, addr, payload),
        )
    }
}
