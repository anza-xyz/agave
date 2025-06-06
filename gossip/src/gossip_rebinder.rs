// gossip_rebinder.rs
use {crossbeam_channel::Sender, log::info, std::net::SocketAddr};

#[derive(Clone)]
pub struct GossipRebinder {
    tx: Sender<SocketAddr>,
}
impl GossipRebinder {
    pub fn new(tx: Sender<SocketAddr>) -> Self {
        Self { tx }
    }
    pub fn rebind(&self, addr: SocketAddr) -> std::io::Result<()> {
        info!("greg: GossipRebinder::rebind gossip socket to {addr}");
        self.tx.send(addr).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::Other, "gossip rebind channel closed")
        })
    }
}
