use {
    chacha20::{
        cipher::{KeyIvInit, StreamCipher},
        ChaCha20,
    },
    chacha20poly1305::aead::KeyInit,
    poly1305::{universal_hash::UniversalHash, Poly1305},
    serde::{Deserialize, Serialize},
    serde_with::serde_as,
    std::{net::SocketAddr, ops::Deref},
};

#[serde_as]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
pub struct Signature<const N: usize> {
    #[serde_as(as = "[_; N]")]
    signature: [u8; N],
}

impl<const N: usize> Signature<N> {
    fn fill_udp_pseudoheader(
        src: SocketAddr,
        dst: SocketAddr,
        pseudoheader: &mut [u8; 36],
    ) -> &[u8] {
        match (src, dst) {
            (SocketAddr::V4(src), SocketAddr::V4(dst)) => {
                pseudoheader[0..4].copy_from_slice(&src.ip().octets());
                pseudoheader[4..6].copy_from_slice(&src.port().to_be_bytes());
                pseudoheader[6..10].copy_from_slice(&dst.ip().octets());
                pseudoheader[10..12].copy_from_slice(&dst.port().to_be_bytes());
                &pseudoheader[0..14]
            }
            (SocketAddr::V6(src), SocketAddr::V6(dst)) => {
                pseudoheader[0..16].copy_from_slice(&src.ip().octets());
                pseudoheader[16..18].copy_from_slice(&src.port().to_be_bytes());
                pseudoheader[18..34].copy_from_slice(&dst.ip().octets());
                pseudoheader[34..36].copy_from_slice(&dst.port().to_be_bytes());
                pseudoheader.as_slice()
            }
            _ => panic!("Can not mix v4 and v6 addresses in one packet"),
        }
    }

    /// Compute a Poly1305 MAC with a one-time key derived from ChaCha20.
    /// This will truncate the 16-byte MAC to the required length
    /// - src: source address
    /// - dst: destination address
    /// - key: 32-byte pre-shared key
    /// - nonce: 12-byte IETF nonce (unique per message under a given key)
    /// - msg: message bytes
    pub fn new_poly1305_for_udp(
        src: SocketAddr,
        dst: SocketAddr,
        key: &[u8; 32],
        nonce: &[u8; 12],
        msg: &[u8],
    ) -> Self {
        debug_assert!(N <= 16, "Can not use >16 bytes signature");
        let mut pseudoheader = [0u8; 36]; // fit both v6 and v4
        let pseudoheader = Self::fill_udp_pseudoheader(src, dst, &mut pseudoheader);
        Self {
            signature: chacha20_poly1305_mac(key, nonce, &[pseudoheader, msg])[0..N]
                .try_into()
                .unwrap(),
        }
    }
}

impl<const N: usize> Deref for Signature<N> {
    type Target = [u8; N];

    fn deref(&self) -> &Self::Target {
        &self.signature
    }
}

/// Compute a Poly1305 MAC with a one-time key derived from ChaCha20.
/// - key: 32-byte pre-shared key
/// - nonce: 12-byte IETF nonce (unique per message under a given key)
/// - msg: message to authenticate
fn chacha20_poly1305_mac(key: &[u8; 32], nonce: &[u8; 12], blocks: &[&[u8]]) -> [u8; 16] {
    // 1) Derive one-time Poly1305 key = ChaCha20(key, nonce, counter=0) ^ 32*0x00
    let mut cipher = ChaCha20::new(key.into(), nonce.into()); // counter=0 by default
    let mut otk = [0u8; 32];
    cipher.apply_keystream(&mut otk); // keystream over zeros -> otk

    // 2) Compute Poly1305 tag over the message using the one-time key
    let mut poly = Poly1305::new((&otk).into()); // library handles clamping of r
    for &block in blocks.iter() {
        poly.update_padded(block);
    }
    let tag = poly.finalize();

    let mut out = [0u8; 16];
    out.copy_from_slice(tag.as_slice());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_roundtrip() {
        let mut key = [0x11u8; 32];
        let nonce = [0x22u8; 12];
        let mut msg = [1u8; 42];
        let tag = chacha20_poly1305_mac(&key, &nonce, &[&msg]);
        let tag2 = chacha20_poly1305_mac(&key, &nonce, &[&msg[..32], &msg[32..]]);
        assert_eq!(tag, tag2);
        // negative tests
        msg[2] ^= 1;
        let bad = chacha20_poly1305_mac(&key, &nonce, &[&msg]);
        assert_ne!(bad, tag);
        msg[2] ^= 1;
        key[2] ^= 1;
        let bad = chacha20_poly1305_mac(&key, &nonce, &[&msg]);
        assert_ne!(bad, tag);
    }
}
