#[cfg(test)]
use crate::{socketaddr, socketaddr_any};
use {
    crate::crds_data::reject_deserialize,
    serde::Serialize,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    std::net::SocketAddr,
};

/// Structure representing a node on the network
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyContactInfo {
    id: Pubkey,
    /// gossip address
    gossip: SocketAddr,
    /// address to connect to for replication
    tvu: SocketAddr,
    /// TVU over QUIC protocol.
    tvu_quic: SocketAddr,
    /// repair service over QUIC protocol.
    serve_repair_quic: SocketAddr,
    /// transactions address
    tpu: SocketAddr,
    /// address to forward unprocessed transactions to
    tpu_forwards: SocketAddr,
    /// address to which to send bank state requests
    tpu_vote: SocketAddr,
    /// address to which to send JSON-RPC requests
    rpc: SocketAddr,
    /// websocket for JSON-RPC push notifications
    rpc_pubsub: SocketAddr,
    /// address to send repair requests to
    serve_repair: SocketAddr,
    /// latest wallclock picked
    wallclock: u64,
    /// node shred version
    shred_version: u16,
}
reject_deserialize!(LegacyContactInfo, "LegacyContactInfo is deprecated");

impl Sanitize for LegacyContactInfo {
    fn sanitize(&self) -> std::result::Result<(), SanitizeError> {
        Err(SanitizeError::ValueOutOfBounds)
    }
}

#[cfg(test)]
impl Default for LegacyContactInfo {
    fn default() -> Self {
        LegacyContactInfo {
            id: Pubkey::default(),
            gossip: socketaddr_any!(),
            tvu: socketaddr_any!(),
            tvu_quic: socketaddr_any!(),
            serve_repair_quic: socketaddr_any!(),
            tpu: socketaddr_any!(),
            tpu_forwards: socketaddr_any!(),
            tpu_vote: socketaddr_any!(),
            rpc: socketaddr_any!(),
            rpc_pubsub: socketaddr_any!(),
            serve_repair: socketaddr_any!(),
            wallclock: 0,
            shred_version: 0,
        }
    }
}

impl LegacyContactInfo {
    #[inline]
    pub(crate) fn pubkey(&self) -> &Pubkey {
        &self.id
    }

    #[inline]
    pub(crate) fn wallclock(&self) -> u64 {
        self.wallclock
    }
}
