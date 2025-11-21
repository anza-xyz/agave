#[cfg(any(test, feature = "dev-context-only-utils"))]
use {
    crate::shred::Nonce,
    solana_perf::packet::{bytes::BufMut, BytesPacket, Meta, Packet},
};
use {
    bytes::{Bytes, BytesMut},
    std::{
        mem,
        ops::{Bound, Deref, DerefMut, RangeBounds, RangeFull},
        slice::SliceIndex,
    },
};

#[derive(Clone, Debug, Eq)]
pub struct Payload {
    pub bytes: Bytes,
}

impl Payload {
    /// Convert the payload's inner [`Bytes`] into a [`BytesMut`], consuming the [`Payload`].
    ///
    /// If the payload is unique (single reference), this will return a [`BytesMut`] with the
    /// contents of the payload without copying. If the payload is not unique, this will make a copy
    /// of the payload in a new [`BytesMut`]. As such, take care to avoid performing this conversion
    /// if the payload is not unique.
    #[inline]
    pub fn into_bytes_mut(self) -> BytesMut {
        self.bytes.into()
    }

    /// Get a mutable reference via [`PayloadMutGuard`] to the payload's _full_ inner bytes.
    /// See [`Payload::get_mut`] for selecting a subset of the payload's inner bytes.
    ///
    /// If the payload is unique (single reference), this will not perform any copying. Otherwise it
    /// will. As such, take care to avoid performing this conversion if the payload is not unique.
    #[inline]
    pub fn as_mut(&mut self) -> PayloadMutGuard<'_, RangeFull> {
        PayloadMutGuard::new(self, ..)
    }

    #[inline]
    /// Get a mutable reference via [`PayloadMutGuard`] to a subset of the payload's inner bytes.
    ///
    /// If the payload is unique (single reference), this will not perform any copying. Otherwise it
    /// will. As such, take care to avoid performing this conversion if the payload is not unique.
    pub fn get_mut<I>(&mut self, index: I) -> Option<PayloadMutGuard<'_, I>>
    where
        I: RangeBounds<usize>,
    {
        match index.end_bound() {
            Bound::Included(&end) if end >= self.bytes.len() => None,
            Bound::Excluded(&end) if end > self.bytes.len() => None,
            _ => Some(PayloadMutGuard::new(self, index)),
        }
    }

    /// Shortens the buffer, keeping the first `len` bytes and dropping the rest.
    ///
    /// See [`Bytes::truncate`].
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
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

pub(crate) mod serde_bytes_payload {
    use {
        super::Payload,
        serde::{Deserialize, Deserializer, Serializer},
        serde_bytes::ByteBuf,
    };

    pub(crate) fn serialize<S: Serializer>(
        payload: &Payload,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(payload)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Payload, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer)
            .map(ByteBuf::into_vec)
            .map(Payload::from)
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

/// Convenience wrapper around [`Payload`] and a [`BytesMut`] into that payload's bytes.
///
/// [`Bytes`] is immutable, yet it's desirable to be able to "simulate" mutability for quick
/// inline updates when buildilng shreds, especially to minimize code changes at the time of this
/// refactor. Given that references to shreds are not propagated until a shred is fully constructed,
/// we should not incur any copying overhead when using this guard to facilitate mutability during
/// shred construction.
///
/// # How it works
///
/// Upon construction, the guard converts the payload's [`Bytes`] into a [`BytesMut`], temporarily
/// replacing the payload's internal bytes reference with an empty [`Bytes`] (which does not
/// allocate). This will not perform any copying if the payload is unique (single reference).
///
/// The guard will then provide a mutable reference to the bytes via [`DerefMut`] and [`AsMut`]
/// implementations, which forward indexing to the underlying [`BytesMut`].
///
/// The guard has a specialized [`Drop`] implementation that will write back the mutated bytes to the
/// payload, effectively "simulating" typical mutability semantics.
pub struct PayloadMutGuard<'a, I> {
    payload: &'a mut Payload,
    bytes_mut: BytesMut,
    slice_index: I,
}

impl<'a, I> PayloadMutGuard<'a, I> {
    #[inline]
    pub fn new(payload: &'a mut Payload, slice_index: I) -> Self {
        let bytes_mut: BytesMut = mem::take(&mut payload.bytes).into();
        Self {
            payload,
            bytes_mut,
            slice_index,
        }
    }
}

impl<I> Drop for PayloadMutGuard<'_, I> {
    #[inline]
    fn drop(&mut self) {
        self.payload.bytes = mem::take(&mut self.bytes_mut).freeze();
    }
}

impl<I> Deref for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8]> + Clone,
{
    type Target = <I as SliceIndex<[u8]>>::Output;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> DerefMut for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8]> + Clone,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> AsMut<[u8]> for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8], Output = [u8]> + Clone,
{
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> AsRef<[u8]> for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8], Output = [u8]> + Clone,
{
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes_mut[self.slice_index.clone()]
    }
}

#[cfg(test)]
mod test {
    use {super::Payload, crate::shred::wire};

    #[test]
    fn test_guard_write_back() {
        let mut payload = Payload::from(vec![1, 2, 3, 4, 5]);
        {
            let mut guard = payload.get_mut(..).unwrap();
            assert_eq!(guard[0], 1);
            assert_eq!(guard[1], 2);
            guard[0] = 10;
            guard[1] = 20;
            assert_eq!(guard[0], 10);
            assert_eq!(guard[1], 20);
        }

        assert_eq!(payload.bytes[..], vec![10, 20, 3, 4, 5]);
    }

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
