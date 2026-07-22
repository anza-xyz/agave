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

pub fn encode_pulse(round: u64, lateness_ns: i64) -> Bytes {
    let mut buf = [0u8; MESSAGE_SIZE];
    buf[0] = WIRE_VERSION;
    buf[1] = MSG_TYPE_PULSE;
    buf[2..10].copy_from_slice(&round.to_le_bytes());
    buf[10..18].copy_from_slice(&lateness_ns.to_le_bytes());
    Bytes::copy_from_slice(&buf)
}

/// `None` for anything malformed, unknown-typed, or reserved.
pub fn decode(bytes: &[u8]) -> Option<Message> {
    let buf: &[u8; MESSAGE_SIZE] = bytes.try_into().ok()?;
    if buf[0] != WIRE_VERSION {
        return None;
    }
    match buf[1] {
        MSG_TYPE_PULSE => Some(Message::Pulse {
            round: u64::from_le_bytes(buf[2..10].try_into().expect("fixed slice")),
            lateness_ns: i64::from_le_bytes(buf[10..18].try_into().expect("fixed slice")),
        }),
        MSG_TYPE_PANIC => None, // phase 2
        _ => None,
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
                decode(&bytes),
                Some(Message::Pulse {
                    round,
                    lateness_ns: lateness
                })
            );
        }
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[WIRE_VERSION; MESSAGE_SIZE + 1]), None);

        let good = encode_pulse(1, 1);
        assert_eq!(decode(&good[..MESSAGE_SIZE - 1]), None);

        let mut bad_version = good.to_vec();
        bad_version[0] = 99;
        assert_eq!(decode(&bad_version), None);

        let mut panic_msg = good.to_vec();
        panic_msg[1] = MSG_TYPE_PANIC;
        assert_eq!(decode(&panic_msg), None);

        let mut bad_type = good.to_vec();
        bad_type[1] = 7;
        assert_eq!(decode(&bad_type), None);
    }
}
