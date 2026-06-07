use {
    quinn::{ConnectError, ConnectionError, SendDatagramError},
    solana_pubkey::Pubkey,
    std::{io, net::SocketAddr},
    thiserror::Error,
};

/// Application close codes and reason strings, paired.
///
/// Each `Spec` bundles a wire close code (`VarInt`) with its reason string
/// so the two cannot drift apart at call sites. Use as
/// `close_codes::HANDOVER.close(&conn)`.
///
/// Numeric codes are part of the wire format — adding a new one is fine,
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

    pub(crate) const SHUTDOWN: Spec = Spec {
        code: VarInt::from_u32(0),
        reason: b"SHUTDOWN",
    };

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

    /// Sent to the non-canonical side when both peers dial simultaneously.
    /// The canonical direction is determined by the lex-pubkey tiebreaker:
    /// lower-pubkey side holds the Outbound connection, higher-pubkey side
    /// holds the Inbound. The losing connection is closed with this code; the
    /// peer routes its traffic through the canonical connection instead.
    pub(crate) const WRONG_DIRECTION: Spec = Spec {
        code: VarInt::from_u32(9),
        reason: b"WRONG_DIRECTION",
    };

    /// A new handshake from a pubkey already in our table replaced the prior
    /// connection. The receiving side observes this close in its read loop,
    /// soft-bans the evicting peer, and forwards a handover-event uplevel so
    /// consensus can decide whether to shut the node down.
    pub(crate) const HANDOVER: Spec = Spec {
        code: VarInt::from_u32(10),
        reason: b"HANDOVER",
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

    /// A connection lost the lex-pubkey tiebreaker. When both peers dial
    /// simultaneously the canonical connection (outbound for lower pubkey,
    /// inbound for higher pubkey) is kept and the other is closed with
    /// `WRONG_DIRECTION`. This error represents the losing side.
    #[error("connection to/from {0} rejected: lost lex tiebreaker")]
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
