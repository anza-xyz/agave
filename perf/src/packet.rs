//! The `packet` module defines data structures and methods to pull data from the network.
#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample};
#[cfg(feature = "dev-context-only-utils")]
use wincode::{ReadError, ReadResult, SchemaRead, config::DefaultConfig};
pub use {
    bytes,
    solana_packet::{self, Meta, PACKET_DATA_SIZE, Packet, PacketFlags},
};
use {
    bytes::Bytes,
    rayon::prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator},
    serde::{Deserialize, Serialize},
    std::{
        io::Cursor,
        net::SocketAddr,
        ops::{Deref, DerefMut},
        slice::{Iter, SliceIndex},
    },
    wincode::{
        SchemaWrite, WriteResult,
        config::{Config, Configuration},
    },
};

pub const NUM_PACKETS: usize = 1024 * 8;

pub const PACKETS_PER_BATCH: usize = 64;
pub const NUM_RCVMMSGS: usize = 64;

pub type PacketConfig = Configuration<true, PACKET_DATA_SIZE>;

/// wincode configuration setup to use for deserializing a Packet.
/// - Zero-copy alignment check is enabled.
/// - Preallocation size limit is PACKET_DATA_SIZE.
#[inline]
const fn packet_config_inner() -> PacketConfig {
    Configuration::default().with_preallocation_size_limit::<{ solana_packet::PACKET_DATA_SIZE }>()
}

#[inline]
pub const fn packet_config() -> impl Config {
    packet_config_inner()
}

#[cfg(feature = "dev-context-only-utils")]
pub fn deserialize_slice_from_packet<'de, T, I>(packet: &'de Packet, index: I) -> ReadResult<T>
where
    T: SchemaRead<'de, PacketConfig, Dst = T>,
    I: SliceIndex<[u8], Output = [u8]>,
{
    let data = packet
        .data(index)
        .ok_or(ReadError::Custom("packet discarded"))?;
    wincode::config::deserialize(data, packet_config_inner())
}

pub fn packet_from_data<T>(dest: Option<&SocketAddr>, data: T) -> WriteResult<Packet>
where
    T: SchemaWrite<PacketConfig, Src = T>,
{
    let mut packet = Packet::default();
    let mut wr = Cursor::new(packet.buffer_mut());
    wincode::config::serialize_into(&mut wr, &data, packet_config_inner())?;
    packet.meta_mut().size = wr.position() as usize;
    if let Some(dest) = dest {
        packet.meta_mut().set_socket_addr(dest);
    }
    Ok(packet)
}

/// Serialize `data` into a freshly allocated [`BytesPacket`].
///
/// Like [`packet_from_data`], serialization is bounded to [`PACKET_DATA_SIZE`], so oversized
/// payloads fail instead of producing a packet that cannot be sent.
pub fn bytes_packet_from_data<T>(dest: Option<&SocketAddr>, data: T) -> WriteResult<BytesPacket>
where
    T: SchemaWrite<PacketConfig, Src = T>,
{
    bytes_packet_from_data_with_config(dest, data, packet_config_inner())
}

/// Like [`bytes_packet_from_data`], but serializes with a caller-provided wincode `config`.
///
/// Wincode's preallocation limit bounds in-memory collection size (`len * size_of::<T>()`),
/// not serialized bytes, so the default [`PacketConfig`] rejects payloads whose sequences
/// are small on the wire but large in memory. Output is still bounded to [`PACKET_DATA_SIZE`]
/// by the destination buffer.
pub fn bytes_packet_from_data_with_config<T, C>(
    dest: Option<&SocketAddr>,
    data: T,
    config: C,
) -> WriteResult<BytesPacket>
where
    C: Config,
    T: SchemaWrite<C, Src = T>,
{
    let mut buffer = [0u8; PACKET_DATA_SIZE];
    let mut wr = Cursor::new(buffer.as_mut_slice());
    wincode::config::serialize_into(&mut wr, &data, config)?;
    let size = wr.position() as usize;
    let mut meta = Meta::default();
    meta.size = size;
    if let Some(dest) = dest {
        meta.set_socket_addr(dest);
    }
    Ok(BytesPacket::new(
        Bytes::copy_from_slice(&buffer[..size]),
        meta,
    ))
}

/// Representation of a packet used in TPU.
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BytesPacket {
    #[cfg_attr(
        feature = "stable-abi",
        stable_abi_sample(with = "solana_frozen_abi::stable_abi::sample_collection(rng)")
    )]
    buffer: Bytes,
    meta: Meta,
}

impl BytesPacket {
    pub fn new(buffer: Bytes, meta: Meta) -> Self {
        Self { buffer, meta }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn empty() -> Self {
        Self {
            buffer: Bytes::new(),
            meta: Meta::default(),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn from_bytes(dest: Option<&SocketAddr>, buffer: impl Into<Bytes>) -> Self {
        let buffer = buffer.into();
        let mut meta = Meta::default();
        meta.size = buffer.len();
        if let Some(dest) = dest {
            meta.set_socket_addr(dest);
        }

        Self { buffer, meta }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn from_data<T>(data: T) -> WriteResult<Self>
    where
        T: SchemaWrite<DefaultConfig, Src = T>,
    {
        let buffer = Bytes::from(wincode::serialize(&data)?);
        let mut meta = Meta::default();
        meta.size = buffer.len();
        Ok(Self { buffer, meta })
    }

    #[inline]
    pub fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        if self.meta.discard() {
            None
        } else {
            self.buffer.get(index)
        }
    }

    #[inline]
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    #[inline]
    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn copy_from_slice(&mut self, slice: &[u8]) {
        self.buffer = Bytes::from(slice.to_vec());
    }

    #[inline]
    pub fn buffer(&self) -> &Bytes {
        &self.buffer
    }

    #[inline]
    pub fn set_buffer(&mut self, buffer: impl Into<Bytes>) {
        let buffer = buffer.into();
        self.meta.size = buffer.len();
        self.buffer = buffer;
    }
}

#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PacketBatch {
    Bytes(BytesPacketBatch),
    Single(BytesPacket),
}

#[cfg(feature = "dev-context-only-utils")]
impl From<&Packet> for BytesPacket {
    fn from(packet: &Packet) -> Self {
        let buffer = packet.data(..).map(Bytes::copy_from_slice);
        Self::new(buffer.unwrap_or_default(), packet.meta().clone())
    }
}

impl Deref for PacketBatch {
    type Target = [BytesPacket];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Bytes(batch) => batch,
            Self::Single(packet) => core::array::from_ref(packet),
        }
    }
}

impl DerefMut for PacketBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Bytes(batch) => batch,
            Self::Single(packet) => core::array::from_mut(packet),
        }
    }
}

impl PacketBatch {
    pub fn par_iter(&self) -> rayon::slice::Iter<'_, BytesPacket> {
        (**self).par_iter()
    }

    pub fn par_iter_mut(&mut self) -> rayon::slice::IterMut<'_, BytesPacket> {
        (**self).par_iter_mut()
    }
}

impl From<BytesPacketBatch> for PacketBatch {
    fn from(batch: BytesPacketBatch) -> Self {
        Self::Bytes(batch)
    }
}

impl From<Vec<BytesPacket>> for PacketBatch {
    fn from(batch: Vec<BytesPacket>) -> Self {
        Self::Bytes(BytesPacketBatch::from(batch))
    }
}

impl<'a> IntoIterator for &'a PacketBatch {
    type Item = &'a BytesPacket;
    type IntoIter = Iter<'a, BytesPacket>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut PacketBatch {
    type Item = &'a mut BytesPacket;
    type IntoIter = std::slice::IterMut<'a, BytesPacket>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a> IntoParallelIterator for &'a PacketBatch {
    type Iter = rayon::slice::Iter<'a, BytesPacket>;
    type Item = &'a BytesPacket;
    fn into_par_iter(self) -> Self::Iter {
        self.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut PacketBatch {
    type Iter = rayon::slice::IterMut<'a, BytesPacket>;
    type Item = &'a mut BytesPacket;
    fn into_par_iter(self) -> Self::Iter {
        self.par_iter_mut()
    }
}

pub fn to_packet_batches<T: wincode::Serialize<Src = T>>(
    items: &[T],
    chunk_size: usize,
) -> Vec<PacketBatch> {
    items
        .chunks(chunk_size)
        .map(|batch_items| {
            batch_items
                .iter()
                .map(|item| {
                    let buffer = Bytes::from(wincode::serialize(item).expect("serialize request"));
                    let mut meta = Meta::default();
                    meta.size = buffer.len();
                    BytesPacket::new(buffer, meta)
                })
                .collect::<BytesPacketBatch>()
                .into()
        })
        .collect()
}

#[cfg(test)]
fn to_packet_batches_for_tests<T: wincode::Serialize<Src = T>>(items: &[T]) -> Vec<PacketBatch> {
    to_packet_batches(items, NUM_PACKETS)
}

#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct BytesPacketBatch {
    packets: Vec<BytesPacket>,
}

impl BytesPacketBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let packets = Vec::with_capacity(capacity);
        Self { packets }
    }
}

impl Deref for BytesPacketBatch {
    type Target = Vec<BytesPacket>;

    fn deref(&self) -> &Self::Target {
        &self.packets
    }
}

impl DerefMut for BytesPacketBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packets
    }
}

impl From<Vec<BytesPacket>> for BytesPacketBatch {
    fn from(packets: Vec<BytesPacket>) -> Self {
        Self { packets }
    }
}

impl FromIterator<BytesPacket> for BytesPacketBatch {
    fn from_iter<T: IntoIterator<Item = BytesPacket>>(iter: T) -> Self {
        let packets = Vec::from_iter(iter);
        Self { packets }
    }
}

impl<'a> IntoIterator for &'a BytesPacketBatch {
    type Item = &'a BytesPacket;
    type IntoIter = Iter<'a, BytesPacket>;

    fn into_iter(self) -> Self::IntoIter {
        self.packets.iter()
    }
}

impl<'a> IntoParallelIterator for &'a BytesPacketBatch {
    type Iter = rayon::slice::Iter<'a, BytesPacket>;
    type Item = &'a BytesPacket;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut BytesPacketBatch {
    type Iter = rayon::slice::IterMut<'a, BytesPacket>;
    type Item = &'a mut BytesPacket;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_hash::Hash, solana_keypair::Keypair, solana_signer::Signer,
        solana_system_transaction::transfer,
    };

    #[test]
    fn test_to_packet_batches() {
        let keypair = Keypair::new();
        let hash = Hash::new_from_array([1; 32]);
        let tx = transfer(&keypair, &keypair.pubkey(), 1, hash);
        let rv = to_packet_batches_for_tests(&[tx.clone(); 1]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].len(), 1);

        #[allow(clippy::useless_vec)]
        let rv = to_packet_batches_for_tests(&vec![tx.clone(); NUM_PACKETS]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].len(), NUM_PACKETS);

        #[allow(clippy::useless_vec)]
        let rv = to_packet_batches_for_tests(&vec![tx; NUM_PACKETS + 1]);
        assert_eq!(rv.len(), 2);
        assert_eq!(rv[0].len(), NUM_PACKETS);
        assert_eq!(rv[1].len(), 1);
    }
}
