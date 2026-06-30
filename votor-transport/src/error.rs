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
    pub(crate) const PEER_MOVED: Spec = Spec {
        code: VarInt::from_u32(1),
        reason: b"PEER_MOVED",
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

    pub(crate) const TOO_MANY_CONNECTIONS: Spec = Spec {
        code: VarInt::from_u32(5),
        reason: b"TOO_MANY_CONNECTIONS",
    };

    /// Peer exhausted its flood-control budget.
    pub(crate) const FLOODING: Spec = Spec {
        code: VarInt::from_u32(6),
        reason: b"FLOODING",
    };

    pub(crate) const IDENTITY_CHANGED: Spec = Spec {
        code: VarInt::from_u32(11),
        reason: b"IDENTITY_ROTATED",
    };
    // When adding new close code, make sure to also capture it
    // in test_close_codes_are_frozen.
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
    #[error("too many connections for peer")]
    TooManyConnections,

    #[error(transparent)]
    SendDatagram(#[from] SendDatagramError),

    /// `quinn::Endpoint::new` failed. Construction-time only.
    #[error(transparent)]
    Endpoint(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use {super::close_codes, quinn::VarInt};

    /// Close codes are wire format: changing an existing value is a breaking
    /// change. Pin every code/reason pair so an accidental edit fails here
    /// instead of silently breaking interop.
    #[test]
    fn test_close_codes_are_frozen() {
        let pinned: [(&close_codes::Spec, u32, &[u8]); 7] = [
            (&close_codes::PEER_MOVED, 1, b"PEER_MOVED"),
            (&close_codes::INVALID_IDENTITY, 2, b"INVALID_IDENTITY"),
            (&close_codes::NOT_ADMITTED, 3, b"NOT_ADMITTED"),
            (&close_codes::BANNED, 4, b"BANNED"),
            (
                &close_codes::TOO_MANY_CONNECTIONS,
                5,
                b"TOO_MANY_CONNECTIONS",
            ),
            (&close_codes::FLOODING, 6, b"FLOODING"),
            (&close_codes::IDENTITY_CHANGED, 11, b"IDENTITY_ROTATED"),
        ];
        for (spec, code, reason) in pinned {
            assert_eq!(
                spec.code,
                VarInt::from_u32(code),
                "close code value changed: this is a breaking protocol change"
            );
            assert_eq!(
                spec.reason, reason,
                "close reason changed: this is a breaking protocol change"
            );
        }
    }
}
