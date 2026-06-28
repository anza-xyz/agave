use {
    quinn::{ConnectError, ConnectionError, SendDatagramError},
    solana_pubkey::Pubkey,
    std::{io, net::SocketAddr},
    thiserror::Error,
};

/// Application close codes and matching reason strings.
///
/// Numeric codes are part of the wire format - adding a new one is fine,
/// changing the value of an existing one is a breaking protocol change.
pub(crate) mod close_codes {
    use quinn::{Connection, VarInt};

    pub(crate) struct Spec {
        pub code: VarInt,
        pub reason: &'static [u8],
    }

    impl Spec {
        /// Close `conn` with this spec's code/reason pair.
        pub fn close(&self, conn: &Connection) {
            conn.close(self.code, self.reason);
        }
    }

    pub(crate) const INVALID_IDENTITY: Spec = Spec {
        code: VarInt::from_u32(2),
        reason: b"INVALID_IDENTITY",
    };

    pub(crate) const NOT_ADMITTED: Spec = Spec {
        code: VarInt::from_u32(3),
        reason: b"NOT_ADMITTED",
    };

    pub(crate) const BANNED: Spec = Spec {
        code: VarInt::from_u32(4),
        reason: b"BANNED",
    };

    pub(crate) const TABLE_FULL: Spec = Spec {
        code: VarInt::from_u32(5),
        reason: b"TABLE_FULL",
    };

    /// Peer exhausted its flood-control budget.
    pub(crate) const FLOODING: Spec = Spec {
        code: VarInt::from_u32(6),
        reason: b"FLOODING",
    };

    pub(crate) const IDENTITY_ROTATED: Spec = Spec {
        code: VarInt::from_u32(11),
        reason: b"IDENTITY_ROTATED",
    };

    pub(crate) const PEER_MOVED: Spec = Spec {
        code: VarInt::from_u32(12),
        reason: b"PEER_MOVED",
    };
}

/// All errors observed by the endpoint.
/// Fed into the stats via `record_client_error` and `record_server_error`.
#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Connect(#[from] ConnectError),

    #[error(transparent)]
    Connection(#[from] ConnectionError),

    /// TLS handshake succeeded but the peer cert did not yield a recoverable
    /// solana ed25519 pubkey. The connection is closed by the caller.
    #[error("invalid identity from {0:?}")]
    InvalidIdentity(SocketAddr),

    /// Peer pubkey is not in the peer_list.
    #[error("peer {0} is not admitted (unstaked)")]
    NotAdmitted(Pubkey),

    /// Peer pubkey is currently banned.
    #[error("peer {0} is banned")]
    Banned(Pubkey),

    /// Inbound refused at a capacity limit: this peer already holds
    /// [`crate::MAX_INBOUND_CONNECTIONS_PER_PEER`] connections.
    #[error("connection table full")]
    TableFull,

    #[error(transparent)]
    SendDatagram(#[from] SendDatagramError),

    /// Identity rotated between handshake start and the post-handshake
    /// install. The completed connection is authenticated under our previous
    /// cert and was closed.
    #[error("identity rotated mid-handshake; connection to {0} aborted")]
    IdentityRotated(Pubkey),

    /// `quinn::Endpoint::new` failed. Construction-time only.
    #[error(transparent)]
    Endpoint(#[from] io::Error),
}
