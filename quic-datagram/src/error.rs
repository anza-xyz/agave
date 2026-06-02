use {
    quinn::{ConnectError, ConnectionError, SendDatagramError},
    solana_pubkey::Pubkey,
    std::{io, net::SocketAddr},
    thiserror::Error,
};

/// All errors observed by the endpoint. Returned from public APIs and stamped
/// into [`crate::stats::QuicDatagramStats`] via `record_error`.
#[derive(Error, Debug)]
pub enum Error {
    #[error("egress channel closed")]
    EgressChannelClosed,

    #[error("ingress channel closed")]
    IngressChannelClosed,

    #[error(transparent)]
    Connect(#[from] ConnectError),

    #[error(transparent)]
    Connection(#[from] ConnectionError),

    /// TLS handshake succeeded but the peer cert did not yield a recoverable
    /// solana ed25519 pubkey. The connection is closed by the caller.
    #[error("invalid identity from {0:?}")]
    InvalidIdentity(SocketAddr),

    /// Peer pubkey is not in the allowlist.
    #[error("peer {0} is not admitted (unstaked)")]
    NotAdmitted(Pubkey),

    /// Peer pubkey is currently banned.
    #[error("peer {0} is banned")]
    Banned(Pubkey),

    /// Connection table already holds [`crate::MAX_PEERS`] distinct pubkeys
    /// and the incoming peer is not among them.
    #[error("connection table full")]
    TableFull,

    /// Inbound handshake from a peer whose pubkey is greater than ours.
    /// Per the lex-pubkey tiebreaker, the lower pubkey dials and the higher
    /// listens - so a peer with pubkey >= local must not be dialing us. We
    /// close the inbound; the surviving connection is the one our own client
    /// dials in the reverse direction.
    #[error("inbound from {0} rejected: wrong direction (lex tiebreaker)")]
    WrongDirection(Pubkey),

    #[error(transparent)]
    SendDatagram(#[from] SendDatagramError),

    /// Identity rotated between handshake start and the post-handshake
    /// `insert_connection`. The completed connection is authenticated
    /// under our previous cert and was closed with `IDENTITY_ROTATED`
    /// rather than inserted into the table.
    #[error("identity rotated mid-handshake; connection to {0} aborted")]
    IdentityRotated(Pubkey),

    /// `quinn::Endpoint::new` failed (e.g. socket already bound by another
    /// process). Construction-time only.
    #[error(transparent)]
    Endpoint(#[from] io::Error),
}
