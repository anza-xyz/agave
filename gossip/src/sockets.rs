//! This module defines structures [`Sockets`] and [`TpuSockets`] used by a
//! validator to communicate with the cluster over different protocols.

use std::{
    net::{TcpListener, UdpSocket},
    sync::Arc,
};

/// [`Sockets`] structure groups together all node-level sockets.
#[derive(Debug)]
pub struct Sockets {
    pub alpenglow: Option<UdpSocket>,
    pub tpu: TpuSockets,

    pub gossip: Arc<[UdpSocket]>,
    pub ip_echo: Option<TcpListener>,
    pub tvu: Vec<UdpSocket>,
    pub tvu_quic: UdpSocket,
    // Socket sending out local repair requests,
    // and receiving repair responses from the cluster.
    pub repair: UdpSocket,
    pub repair_quic: UdpSocket,
    pub retransmit_sockets: Vec<UdpSocket>,
    // Socket receiving remote repair requests from the cluster,
    // and sending back repair responses.
    pub serve_repair: UdpSocket,
    pub serve_repair_quic: UdpSocket,
    // Socket sending out local RepairProtocol::AncestorHashes,
    // and receiving AncestorHashesResponse from the cluster.
    pub ancestor_hashes_requests: UdpSocket,
    pub ancestor_hashes_requests_quic: UdpSocket,

    /// Connection cache endpoint for QUIC-based Vote
    pub quic_vote_client: UdpSocket,
    /// Client-side socket for RPC/SendTransactionService.
    pub rpc_sts_client: UdpSocket,
}

/// [`TpuSockets`] structure groups together all tpu related sockets.
#[derive(Debug)]
pub struct TpuSockets {
    pub transactions: Vec<UdpSocket>,
    pub transaction_forwards: Vec<UdpSocket>,
    pub vote: Vec<UdpSocket>,
    pub broadcast: Vec<UdpSocket>,
    pub transactions_quic: Vec<UdpSocket>,
    pub transactions_forwards_quic: Vec<UdpSocket>,
    pub vote_quic: Vec<UdpSocket>,
    /// Client-side socket for the forwarding votes.
    pub vote_forwarding_client: UdpSocket,
    /// Client-side socket for ForwardingStage non-vote transactions
    pub transaction_forwarding_client: UdpSocket,
    pub vortexor_receivers: Option<Vec<UdpSocket>>,
}
