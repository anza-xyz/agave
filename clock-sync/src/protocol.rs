//! Wire format for clock-sync datagrams: fixed 18-byte layout,
//! `[version: u8][msg_type: u8][round: u64 LE][lateness_ns: i64 LE]`.
//! Sender identity comes from the authenticated QUIC connection, not the
//! payload.

use bytes::Bytes;

/// Current wire version. Bump on any layout change.
pub const WIRE_VERSION: u8 = 1;

/// Total encoded size of every message.
pub const MESSAGE_SIZE: usize = 18;

const MSG_TYPE_PULSE: u8 = 0;
/// Reserved for the Phase 2 coarse loop.
const MSG_TYPE_PANIC: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// Broadcast once per period at the sender's scheduled pulse time.
    Pulse {
        round: u64,
        /// How long after its scheduled pulse time the sender actually
        /// enqueued the broadcast; receivers subtract it from the offset
        /// estimate.
        lateness_ns: i64,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("bad message length {0}, expected {MESSAGE_SIZE}")]
    BadLength(usize),
    #[error("unsupported wire version {0}")]
    BadVersion(u8),
    #[error("unknown message type {0}")]
    BadType(u8),
    #[error("message type {0} is reserved for a later protocol phase")]
    Reserved(u8),
}

pub fn encode_pulse(round: u64, lateness_ns: i64) -> Bytes {
    let mut buf = [0u8; MESSAGE_SIZE];
    buf[0] = WIRE_VERSION;
    buf[1] = MSG_TYPE_PULSE;
    buf[2..10].copy_from_slice(&round.to_le_bytes());
    buf[10..18].copy_from_slice(&lateness_ns.to_le_bytes());
    Bytes::copy_from_slice(&buf)
}

pub fn decode(bytes: &[u8]) -> Result<Message, ProtocolError> {
    let buf: &[u8; MESSAGE_SIZE] = bytes
        .try_into()
        .map_err(|_| ProtocolError::BadLength(bytes.len()))?;
    if buf[0] != WIRE_VERSION {
        return Err(ProtocolError::BadVersion(buf[0]));
    }
    match buf[1] {
        MSG_TYPE_PULSE => Ok(Message::Pulse {
            round: u64::from_le_bytes(buf[2..10].try_into().expect("fixed slice")),
            lateness_ns: i64::from_le_bytes(buf[10..18].try_into().expect("fixed slice")),
        }),
        MSG_TYPE_PANIC => Err(ProtocolError::Reserved(MSG_TYPE_PANIC)),
        other => Err(ProtocolError::BadType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_round_trip() {
        for (round, lateness) in [(0u64, 0i64), (42, 137_000), (u64::MAX, i64::MIN)] {
            let bytes = encode_pulse(round, lateness);
            assert_eq!(bytes.len(), MESSAGE_SIZE);
            assert_eq!(
                decode(&bytes).unwrap(),
                Message::Pulse {
                    round,
                    lateness_ns: lateness
                }
            );
        }
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode(&[]), Err(ProtocolError::BadLength(0)));
        assert_eq!(
            decode(&[WIRE_VERSION; MESSAGE_SIZE + 1]),
            Err(ProtocolError::BadLength(MESSAGE_SIZE + 1))
        );

        let mut short = encode_pulse(1, 1).to_vec();
        short.pop();
        assert_eq!(decode(&short), Err(ProtocolError::BadLength(17)));

        let mut bad_version = encode_pulse(1, 1).to_vec();
        bad_version[0] = 99;
        assert_eq!(decode(&bad_version), Err(ProtocolError::BadVersion(99)));

        let mut panic_msg = encode_pulse(1, 1).to_vec();
        panic_msg[1] = MSG_TYPE_PANIC;
        assert_eq!(decode(&panic_msg), Err(ProtocolError::Reserved(1)));

        let mut bad_type = encode_pulse(1, 1).to_vec();
        bad_type[1] = 7;
        assert_eq!(decode(&bad_type), Err(ProtocolError::BadType(7)));
    }
}
