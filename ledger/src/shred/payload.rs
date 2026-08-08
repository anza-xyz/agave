#[cfg(any(test, feature = "dev-context-only-utils"))]
use {
    crate::shred::Nonce,
    solana_perf::packet::{BytesPacket, Meta, Packet, bytes::BufMut},
    std::mem,
};
use {
    bytes::{Bytes, BytesMut},
    std::ops::{Deref, DerefMut},
    wincode::{SchemaRead, SchemaWrite},
};

#[derive(Clone, Debug, Eq, SchemaRead, SchemaWrite)]
pub struct Payload {
    pub bytes: Bytes,
}

impl Payload {
    /// Shortens the buffer, keeping the first `len` bytes and dropping the rest.
    ///
    /// See [`Bytes::truncate`].
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }
}

impl PartialEq for Payload {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl From<Vec<u8>> for Payload {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Bytes::from(bytes),
        }
    }
}

impl From<Bytes> for Payload {
    #[inline]
    fn from(bytes: Bytes) -> Self {
        Self { bytes }
    }
}

impl From<BytesMut> for Payload {
    #[inline]
    fn from(bytes: BytesMut) -> Self {
        Self {
            bytes: bytes.freeze(),
        }
    }
}

impl AsRef<[u8]> for Payload {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl Deref for Payload {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.bytes.deref()
    }
}

/// A shred payload under construction.
///
/// Deliberately not [`Clone`]: a payload being built has exactly one owner,
/// which guarantee mutations to be in-place both copy-free. Once it is fully
/// populated, [`Self::build`] turns it into an (immutable) [`Payload`] that is
/// cheap to share.
#[derive(Debug)]
pub struct PayloadBuilder {
    bytes: BytesMut,
}

impl PayloadBuilder {
    /// Allocates a zero-filled payload of `len` bytes.
    #[inline]
    pub fn zeroed(len: usize) -> Self {
        Self {
            bytes: BytesMut::zeroed(len),
        }
    }

    /// Builds the payload, making it immutable and cheaply shareable. Never copies.
    #[inline]
    pub fn build(self) -> Payload {
        Payload {
            bytes: self.bytes.freeze(),
        }
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl From<Payload> for PayloadBuilder {
    /// Reopens a frozen payload for mutation.
    ///
    /// This does not copy if `payload` is the only handle to the *entire* underlying buffer, which
    /// is the case for a payload that was just frozen and never cloned. Otherwise the payload is
    /// copied, so that the mutation stays invisible to the other holders.
    #[inline]
    fn from(payload: Payload) -> Self {
        Self {
            bytes: payload.bytes.into(),
        }
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl From<&Payload> for PayloadBuilder {
    /// Copies `payload` into a new buffer for mutation, leaving the original untouched.
    #[inline]
    fn from(payload: &Payload) -> Self {
        Self {
            bytes: BytesMut::from(payload.as_ref()),
        }
    }
}

impl Deref for PayloadBuilder {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.bytes.deref()
    }
}

impl DerefMut for PayloadBuilder {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.bytes.deref_mut()
    }
}

impl AsRef<[u8]> for PayloadBuilder {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl AsMut<[u8]> for PayloadBuilder {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.bytes.as_mut()
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl Payload {
    pub fn copy_to_packet(&self, packet: &mut Packet) {
        let size = self.len();
        packet.buffer_mut()[..size].copy_from_slice(&self[..]);
        packet.meta_mut().size = size;
    }

    pub fn to_packet(&self, nonce: Option<Nonce>) -> Packet {
        let mut packet = Packet::default();
        let size = self.len();
        packet.buffer_mut()[..size].copy_from_slice(self);
        let size = if let Some(nonce) = nonce {
            let full_size = size + mem::size_of::<Nonce>();
            packet.buffer_mut()[size..full_size].copy_from_slice(&nonce.to_le_bytes());
            full_size
        } else {
            size
        };
        packet.meta_mut().size = size;
        packet
    }

    pub fn to_bytes_packet(&self, nonce: Option<Nonce>) -> BytesPacket {
        let cap = self.len() + nonce.map(|_| mem::size_of::<Nonce>()).unwrap_or(0);
        let mut buffer = BytesMut::with_capacity(cap);
        buffer.put_slice(&self[..]);
        if let Some(nonce) = nonce {
            buffer.put_u32_le(nonce);
        }
        BytesPacket::new(buffer.freeze(), Meta::default())
    }
}

#[cfg(test)]
mod test {
    use crate::shred::wire;

    #[test]
    fn test_to_bytes_packet_nonce_endianness() {
        use {
            crate::shredder::{ReedSolomonCache, Shredder},
            solana_entry::entry::Entry,
            solana_hash::Hash,
            solana_keypair::Keypair,
            solana_perf::packet::PacketFlags,
        };

        // Build a valid shred payload using the shredder helper.
        let keypair = Keypair::new();
        let shredder = Shredder::new(1, 0, 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let mut stats = crate::shred::ProcessShredsStats::default();
        let shreds: Vec<_> = shredder
            .make_merkle_shreds_from_entries(
                &keypair,
                &entries,
                /*is_last_in_slot:*/ false,
                Hash::default(),
                0,
                0,
                &ReedSolomonCache::default(),
                &mut stats,
            )
            .collect();
        let shred = &shreds[0];

        // Create a BytesPacket with a trailing nonce and mark it as REPAIR.
        let nonce: super::Nonce = 0x0A0B_0C0D;
        let mut bytes_packet = shred.payload().to_bytes_packet(Some(nonce));
        bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;

        // Ensure wire::get_shred_and_repair_nonce reads the same nonce (LE).
        let (bytes, got) = wire::get_shred_and_repair_nonce(bytes_packet.as_ref())
            .expect("valid packet and nonce");
        assert_eq!(bytes, shred.payload().as_ref());
        assert_eq!(got, Some(nonce));
    }
}
