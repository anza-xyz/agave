use {
    chacha20::{
        ChaCha20,
        cipher::{KeyIvInit, StreamCipher},
    },
    chacha20poly1305::aead::KeyInit,
    poly1305::{Poly1305, universal_hash::UniversalHash},
    std::{net::SocketAddr, ops::Deref},
    wincode::{SchemaRead, SchemaWrite},
};

/// marker trait for valid signature sizes
pub trait _ValidSigSize {} //TODO: turn this into a const generic assert once it stabilizes
// can be anything from 1 to 16 bytes in practice, feel free to expand
impl _ValidSigSize for _ConstUsize<2> {}
impl _ValidSigSize for _ConstUsize<4> {}
impl _ValidSigSize for _ConstUsize<8> {}
impl _ValidSigSize for _ConstUsize<16> {}

pub struct _ConstUsize<const N: usize>;
impl<const N: usize> _ConstUsize<N> {}

/// Poly 1305 signature for a packet.
/// Can be truncated from default of 16 bytes down as needed.
#[derive(SchemaWrite, SchemaRead, Debug, PartialEq, Eq, Clone, Copy)]
#[repr(transparent)]
pub struct Signature<const N: usize = 16> {
    signature: [u8; N],
}

impl<const N: usize> Signature<N>
where
    _ConstUsize<N>: _ValidSigSize,
{
    /// Fill UDP pseudoheader based on packet contents.
    /// Panics if given incompatible address family addresses.
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
                &pseudoheader[0..12]
            }
            (SocketAddr::V6(src), SocketAddr::V6(dst)) => {
                pseudoheader[0..16].copy_from_slice(&src.ip().octets());
                pseudoheader[16..18].copy_from_slice(&src.port().to_be_bytes());
                pseudoheader[18..34].copy_from_slice(&dst.ip().octets());
                pseudoheader[34..36].copy_from_slice(&dst.port().to_be_bytes());
                pseudoheader.as_slice()
            }
            _ => {
                debug_assert!(false, "Can not mix v4 and v6 addresses in one packet");
                &pseudoheader[0..0]
            }
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
        let mut pseudoheader = [0u8; 36]; // fit both v6 and v4
        let pseudoheader = Self::fill_udp_pseudoheader(src, dst, &mut pseudoheader);
        Self {
            signature: *chacha20_poly1305_mac(key, nonce, &[pseudoheader, msg])
                .first_chunk()
                .expect("This operation is infallible for any N<=16 as enforced by type system"),
        }
    }
}

impl<const N: usize> Deref for Signature<N>
where
    _ConstUsize<N>: _ValidSigSize,
{
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
    // 1) Derive one-time Poly1305 key = ChaCha20(key, nonce, counter=0)
    let mut cipher: ChaCha20 = KeyIvInit::new(key.into(), nonce.into());
    let mut otk = [0u8; 32];
    cipher.apply_keystream(&mut otk); // keystream over zeros to get OTK

    // 2) Compute Poly1305 tag over the message using the one-time key
    let mut poly = Poly1305::new((&otk).into());
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
    #[test]
    fn mac_udp_roundtrip() {
        let key = &[0x11u8; 32];
        let nonce = &[0x22u8; 12];
        let msg = &mut [1u8; 42];
        let src1 = "1.2.3.4:8833".parse().unwrap();
        let src2 = "1.2.3.4:8822".parse().unwrap();
        let dst1 = "1.2.3.4:8833".parse().unwrap();
        let signature1: Signature<8> = Signature::new_poly1305_for_udp(src1, dst1, key, nonce, msg);
        let signature2 = Signature::new_poly1305_for_udp(src1, dst1, key, nonce, msg);
        assert_eq!(signature1, signature2);

        // negative tests
        let signature2 = Signature::new_poly1305_for_udp(src2, dst1, key, nonce, msg);
        assert_ne!(signature1, signature2);
        msg[2] ^= 1;
        let signature2 = Signature::new_poly1305_for_udp(src1, dst1, key, nonce, msg);
        assert_ne!(signature1, signature2);
    }
}
