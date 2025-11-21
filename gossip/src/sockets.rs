//! This module defines structures [`Sockets`] and [`TpuSockets`] used by a
//! validator to communicate with the cluster over different protocols.

use std::{
    io::Result,
    net::{TcpListener, UdpSocket},
    sync::Arc,
};

pub trait TryClone: Sized {
    fn try_clone(&self) -> Result<Self>;
}

impl TryClone for Box<[UdpSocket]> {
    fn try_clone(&self) -> Result<Self> {
        self.iter()
            .map(|s| s.try_clone())
            .collect::<Result<Vec<_>>>()
            .map(|v| v.into_boxed_slice())
    }
}

/// [`Sockets`] structure groups together all node-level sockets.
#[derive(Debug)]
pub struct Sockets {
    pub gossip: Arc<[UdpSocket]>,
    pub ip_echo: Option<TcpListener>,
    pub tvu: Vec<UdpSocket>,
    pub tvu_quic: UdpSocket,
    pub tpu: TpuSockets,
    // Socket sending out local repair requests and receiving repair responses from the cluster.
    pub repair: UdpSocket,
    pub repair_quic: UdpSocket,
    pub retransmit_sockets: Vec<UdpSocket>,
    // Socket receiving remote repair requests from the cluster and sending back repair responses.
    pub serve_repair: UdpSocket,
    pub serve_repair_quic: UdpSocket,
    // Socket sending out local RepairProtocol::AncestorHashes and receiving AncestorHashesResponse from the cluster.
    pub ancestor_hashes_requests: UdpSocket,
    pub ancestor_hashes_requests_quic: UdpSocket,
    // Socket for alpenglow consensus logic
    pub alpenglow: Option<UdpSocket>,
    // Connection cache endpoint for QUIC-based Alpenglow messages.
    pub alpenglow_client_quic: UdpSocket,
    // Client-side socket for RPC/SendTransactionService.
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
    // Client-side socket for the forwarding votes.
    pub vote_forwarding_client: UdpSocket,
    pub vortexor_receivers: Option<Vec<UdpSocket>>,
    // Connection cache endpoint for QUIC-based Vote.
    pub vote_client_quic: UdpSocket,
    // Client-side socket for ForwardingStage non-vote transactions.
    pub transaction_forwarding_clients: Box<[UdpSocket]>,
}
