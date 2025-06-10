/// Wrapper around the gossip rebinder channel.
/// The channel is used to send commands to GossipService to rebind the gossip socket to a new address.
/// Used by the Admin RPC service.
use {
    crossbeam_channel::Sender,
    std::{
        io::{Error, ErrorKind, Result},
        net::SocketAddr,
    },
};

#[derive(Clone)]
pub struct GossipRebinder {
    tx: Sender<SocketAddr>,
}
impl GossipRebinder {
    pub fn new(tx: Sender<SocketAddr>) -> Self {
        Self { tx }
    }
    pub fn rebind(&self, addr: SocketAddr) -> Result<()> {
        self.tx
            .send(addr)
            .map_err(|_| Error::new(ErrorKind::Other, "gossip rebind channel closed"))
    }
}
