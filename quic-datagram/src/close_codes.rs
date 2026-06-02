//! Application close codes and reason strings, paired.
//!
//! Each `Spec` bundles a wire close code (`VarInt`) with its reason string
//! so the two cannot drift apart at call sites. Use as
//! `close::HANDOVER.close(&conn)`.
//!
//! Numeric codes are part of the wire format — adding a new one is fine,
//! changing the value of an existing one is a breaking protocol change.

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

/// Local endpoint is shutting down. Issued by [`crate::QuicDatagramEndpoint::close`]
/// to every live connection.
pub(crate) const SHUTDOWN: Spec = Spec {
    code: VarInt::from_u32(0),
    reason: b"SHUTDOWN",
};

/// Peer's TLS cert chain did not yield a recoverable Solana ed25519 pubkey
/// (wrong shape, multi-cert chain, parse failure). Issued from the server
/// accept path and from the client after dial when the attested pubkey does
/// not match the one the caller targeted.
pub(crate) const INVALID_IDENTITY: Spec = Spec {
    code: VarInt::from_u32(2),
    reason: b"INVALID_IDENTITY",
};

/// Peer's pubkey is not in the local [`crate::Admission`] allow-list
/// (typically: not in the current staked-nodes snapshot).
pub(crate) const NOT_ADMITTED: Spec = Spec {
    code: VarInt::from_u32(3),
    reason: b"NOT_ADMITTED",
};

/// Peer is in the local [`crate::Banlist<Pubkey>`] — either via an external
/// trigger (e.g. BLS sigverify failure) or via the only internal trigger,
/// HANDOVER reception in the per-connection read loop.
pub(crate) const BANNED: Spec = Spec {
    code: VarInt::from_u32(4),
    reason: b"BANNED",
};

/// Connection table is at [`crate::MAX_PEERS`] and this is a fresh pubkey
/// — no slot available.
pub(crate) const TABLE_FULL: Spec = Spec {
    code: VarInt::from_u32(5),
    reason: b"TABLE_FULL",
};

/// Lex-pubkey tiebreaker violation: peer dialed us but its pubkey is greater
/// than or equal to ours. Per the rule the lower pubkey is the canonical
/// dialer; the higher pubkey listens.
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

/// Local endpoint's identity (TLS cert / pubkey) was rotated. All existing
/// connections are closed so peers re-handshake against the new cert. The
/// receiving side reaps the connection without banning — we are still the
/// same logical node, just with a fresh certificate.
pub(crate) const IDENTITY_ROTATED: Spec = Spec {
    code: VarInt::from_u32(11),
    reason: b"IDENTITY_ROTATED",
};

/// The caller (e.g. votor via a gossip refresh) supplied a different socket
/// address for this peer than the one our cached outbound connection was
/// opened to. We close the stale connection and re-connect at the new addr.
/// Only emitted by the lex-lower (dialer) side; lex-higher cached entries
/// are server-accepted and trusted regardless of caller's claimed addr
/// (NAT-mapped source addrs can legitimately differ from gossip ones).
pub(crate) const PEER_MOVED: Spec = Spec {
    code: VarInt::from_u32(12),
    reason: b"PEER_MOVED",
};
