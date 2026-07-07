//! The `packet` module defines data structures and methods to pull data from the network.
#[cfg(feature = "dev-context-only-utils")]
use wincode::{ReadError, config::DefaultConfig};
use {
    crate::{recycled_vec::RecycledVec, recycler::Recycler},
    bytes::Bytes,
    rayon::{
        iter::{IndexedParallelIterator, ParallelIterator},
        prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator},
    },
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{
        io::Cursor,
        mem::MaybeUninit,
        net::{IpAddr, SocketAddr},
        ops::{Deref, DerefMut, Index, IndexMut},
        slice::{Iter, SliceIndex},
    },
    wincode::{
        ReadResult, SchemaRead, SchemaWrite, WriteResult,
        config::{Config, Configuration},
        io::{Reader, Writer},
        len::SeqLen,
    },
};
pub use {
    bytes,
    solana_packet::{self, Meta, PACKET_DATA_SIZE, Packet, PacketFlags},
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

/// Representation of a packet used in TPU.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BytesPacket {
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
    pub fn as_ref(&self) -> PacketRef<'_> {
        PacketRef::Bytes(self)
    }

    #[inline]
    pub fn as_mut(&mut self) -> PacketRefMut<'_> {
        PacketRefMut::Bytes(self)
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PacketBatch {
    Pinned(RecycledPacketBatch),
    Bytes(BytesPacketBatch),
    Single(BytesPacket),
}

impl PacketBatch {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn first(&self) -> Option<PacketRef<'_>> {
        match self {
            Self::Pinned(batch) => batch.first().map(PacketRef::from),
            Self::Bytes(batch) => batch.first().map(PacketRef::from),
            Self::Single(packet) => Some(PacketRef::from(packet)),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn first_mut(&mut self) -> Option<PacketRefMut<'_>> {
        match self {
            Self::Pinned(batch) => batch.first_mut().map(PacketRefMut::from),
            Self::Bytes(batch) => batch.first_mut().map(PacketRefMut::from),
            Self::Single(packet) => Some(PacketRefMut::from(packet)),
        }
    }

    /// Returns `true` if the batch contains no elements.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Pinned(batch) => batch.is_empty(),
            Self::Bytes(batch) => batch.is_empty(),
            Self::Single(_) => false,
        }
    }

    /// Returns a reference to an element.
    pub fn get(&self, index: usize) -> Option<PacketRef<'_>> {
        match self {
            Self::Pinned(batch) => batch.get(index).map(PacketRef::from),
            Self::Bytes(batch) => batch.get(index).map(PacketRef::from),
            Self::Single(packet) => (index == 0).then_some(PacketRef::from(packet)),
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<PacketRefMut<'_>> {
        match self {
            Self::Pinned(batch) => batch.get_mut(index).map(PacketRefMut::from),
            Self::Bytes(batch) => batch.get_mut(index).map(PacketRefMut::from),
            Self::Single(packet) => (index == 0).then_some(PacketRefMut::from(packet)),
        }
    }

    pub fn iter(&self) -> PacketBatchIter<'_> {
        match self {
            Self::Pinned(batch) => PacketBatchIter::Pinned(batch.iter()),
            Self::Bytes(batch) => PacketBatchIter::Bytes(batch.iter()),
            Self::Single(packet) => PacketBatchIter::Bytes(core::array::from_ref(packet).iter()),
        }
    }

    pub fn iter_mut(&mut self) -> PacketBatchIterMut<'_> {
        match self {
            Self::Pinned(batch) => PacketBatchIterMut::Pinned(batch.iter_mut()),
            Self::Bytes(batch) => PacketBatchIterMut::Bytes(batch.iter_mut()),
            Self::Single(packet) => {
                PacketBatchIterMut::Bytes(core::array::from_mut(packet).iter_mut())
            }
        }
    }

    pub fn par_iter(&self) -> PacketBatchParIter<'_> {
        match self {
            Self::Pinned(batch) => {
                PacketBatchParIter::Pinned(batch.par_iter().map(PacketRef::from))
            }
            Self::Bytes(batch) => PacketBatchParIter::Bytes(batch.par_iter().map(PacketRef::from)),
            Self::Single(packet) => PacketBatchParIter::Bytes(
                core::array::from_ref(packet)
                    .par_iter()
                    .map(PacketRef::from),
            ),
        }
    }

    pub fn par_iter_mut(&mut self) -> PacketBatchParIterMut<'_> {
        match self {
            Self::Pinned(batch) => {
                PacketBatchParIterMut::Pinned(batch.par_iter_mut().map(PacketRefMut::from))
            }
            Self::Bytes(batch) => {
                PacketBatchParIterMut::Bytes(batch.par_iter_mut().map(PacketRefMut::from))
            }
            Self::Single(packet) => PacketBatchParIterMut::Bytes(
                core::array::from_mut(packet)
                    .par_iter_mut()
                    .map(PacketRefMut::from),
            ),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Pinned(batch) => batch.len(),
            Self::Bytes(batch) => batch.len(),
            Self::Single(_) => 1,
        }
    }
}

/// Serialized size of one logical packet: `Meta`'s relevant fields + the payload bytes
/// up to `Meta::size`. Callers should filter out discarded packets, they have
/// no readable payload, so there's nothing meaningful to trace for it.
fn size_of_traced_packet<C: Config>(packet: PacketRef<'_>) -> WriteResult<usize> {
    let meta = packet.meta();
    let payload = packet
        .data(..)
        .expect("discarded packets must be filtered out before tracing");
    // A single packet's serialized size can never approach `usize::MAX`.
    #[allow(clippy::arithmetic_side_effects)]
    Ok(<IpAddr as SchemaWrite<C>>::size_of(&meta.addr)?
        + <u8 as SchemaWrite<C>>::size_of(&meta.flags.bits())?
        + <[u8; 32] as SchemaWrite<C>>::size_of(
            &meta.remote_pubkey().unwrap_or_default().to_bytes(),
        )?
        + <[u8] as SchemaWrite<C>>::size_of(payload)?)
}

/// Writes one packet for banking trace.
///
/// This is dev/debug tooling, only needed for ledger tool and banking
/// simulation, so only we only write the fields that are actually useful for them.
fn write_packet_for_banking_trace<C: Config>(
    mut writer: impl Writer,
    packet: PacketRef<'_>,
) -> WriteResult<()> {
    let meta = packet.meta();
    let payload = packet
        .data(..)
        .expect("discarded packets must be filtered out before tracing");
    <IpAddr as SchemaWrite<C>>::write(writer.by_ref(), &meta.addr)?;
    <u8 as SchemaWrite<C>>::write(writer.by_ref(), &meta.flags.bits())?;
    <[u8; 32] as SchemaWrite<C>>::write(
        writer.by_ref(),
        &meta.remote_pubkey().unwrap_or_default().to_bytes(),
    )?;
    <[u8] as SchemaWrite<C>>::write(writer, payload)
}

/// Read one logical packet written by `write_packet_for_banking_trace` back into
/// an owned `BytesPacket`.
///
/// This is dev/debug tooling, only needed for ledger tool and banking
/// simulation.
fn read_packet_from_banking_trace<'de, C: Config>(
    mut reader: impl Reader<'de>,
) -> ReadResult<BytesPacket> {
    let addr = <IpAddr as SchemaRead<'de, C>>::get(reader.by_ref())?;
    let flags = <u8 as SchemaRead<'de, C>>::get(reader.by_ref())?;
    let remote_pubkey = <[u8; 32] as SchemaRead<'de, C>>::get(reader.by_ref())?;
    let payload = <Vec<u8> as SchemaRead<'de, C>>::get(reader)?;
    let meta = Meta::new(
        payload.len(),
        addr,
        0,
        PacketFlags::from_bits_retain(flags),
        (remote_pubkey != [0; 32]).then(|| Pubkey::new_from_array(remote_pubkey)),
    );
    Ok(BytesPacket::new(Bytes::from(payload), meta))
}

// SAFETY: `write`/`size_of` operate on exactly the same field sequence.
unsafe impl<C: Config> SchemaWrite<C> for PacketBatch {
    type Src = Self;

    // A batch's total serialized size and packet count can never approach `usize::MAX`.
    #[allow(clippy::arithmetic_side_effects)]
    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        let mut count = 0usize;
        let mut size = 0usize;
        for packet in src.iter().filter(|p| !p.meta().discard()) {
            count += 1;
            size += size_of_traced_packet::<C>(packet)?;
        }
        Ok(<C::LengthEncoding as SeqLen<C>>::write_bytes_needed(count)? + size)
    }

    fn write(mut writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
        let len = src.iter().filter(|p| !p.meta().discard()).count();
        <C::LengthEncoding as SeqLen<C>>::write(writer.by_ref(), len)?;
        for packet in src.iter().filter(|p| !p.meta().discard()) {
            write_packet_for_banking_trace::<C>(writer.by_ref(), packet)?;
        }
        Ok(())
    }
}

// SAFETY: `read` only initializes `dst` after every packet has been read successfully.
unsafe impl<'de, C: Config> SchemaRead<'de, C> for PacketBatch {
    type Dst = Self;

    fn read(mut reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
        let len =
            <C::LengthEncoding as SeqLen<C>>::read_prealloc_check::<BytesPacket>(reader.by_ref())?;
        let mut packets = Vec::with_capacity(len);
        for _ in 0..len {
            packets.push(read_packet_from_banking_trace::<C>(reader.by_ref())?);
        }
        dst.write(PacketBatch::from(packets));
        Ok(())
    }
}

impl From<RecycledPacketBatch> for PacketBatch {
    fn from(batch: RecycledPacketBatch) -> Self {
        Self::Pinned(batch)
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
    type Item = PacketRef<'a>;
    type IntoIter = PacketBatchIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut PacketBatch {
    type Item = PacketRefMut<'a>;
    type IntoIter = PacketBatchIterMut<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a> IntoParallelIterator for &'a PacketBatch {
    type Iter = PacketBatchParIter<'a>;
    type Item = PacketRef<'a>;
    fn into_par_iter(self) -> Self::Iter {
        self.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut PacketBatch {
    type Iter = PacketBatchParIterMut<'a>;
    type Item = PacketRefMut<'a>;
    fn into_par_iter(self) -> Self::Iter {
        self.par_iter_mut()
    }
}

#[derive(Clone, Copy, Debug, Eq)]
pub enum PacketRef<'a> {
    Packet(&'a Packet),
    Bytes(&'a BytesPacket),
}

impl PartialEq for PacketRef<'_> {
    fn eq(&self, other: &PacketRef<'_>) -> bool {
        self.meta().eq(other.meta()) && self.data(..).eq(&other.data(..))
    }
}

impl<'a> From<&'a Packet> for PacketRef<'a> {
    fn from(packet: &'a Packet) -> Self {
        Self::Packet(packet)
    }
}

impl<'a> From<&'a mut Packet> for PacketRef<'a> {
    fn from(packet: &'a mut Packet) -> Self {
        Self::Packet(packet)
    }
}

impl<'a> From<&'a BytesPacket> for PacketRef<'a> {
    fn from(packet: &'a BytesPacket) -> Self {
        Self::Bytes(packet)
    }
}

impl<'a> From<&'a mut BytesPacket> for PacketRef<'a> {
    fn from(packet: &'a mut BytesPacket) -> Self {
        Self::Bytes(packet)
    }
}

impl<'a> PacketRef<'a> {
    pub fn data<I>(&self, index: I) -> Option<&'a <I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        match self {
            Self::Packet(packet) => packet.data(index),
            Self::Bytes(packet) => packet.data(index),
        }
    }

    #[inline]
    pub fn meta(&self) -> &Meta {
        match self {
            Self::Packet(packet) => packet.meta(),
            Self::Bytes(packet) => packet.meta(),
        }
    }

    pub fn to_bytes_packet(&self) -> BytesPacket {
        match self {
            // In case of the legacy `Packet` variant, we unfortunately need to
            // make a copy.
            Self::Packet(packet) => {
                let buffer = packet
                    .data(..)
                    .map(|data| Bytes::from(data.to_vec()))
                    .unwrap_or_else(Bytes::new);
                BytesPacket::new(buffer, self.meta().clone())
            }
            // Cheap clone of `Bytes`.
            // We call `to_owned()` twice, because `packet` is `&&BytesPacket`
            // at this point. This will become less annoying once we switch to
            // `BytesPacket` entirely and deal just with `Vec<BytesPacket>`
            // everywhere.
            Self::Bytes(packet) => packet.to_owned().to_owned(),
        }
    }
}

#[derive(Debug, Eq)]
pub enum PacketRefMut<'a> {
    Packet(&'a mut Packet),
    Bytes(&'a mut BytesPacket),
}

impl<'a> PartialEq for PacketRefMut<'a> {
    fn eq(&self, other: &PacketRefMut<'a>) -> bool {
        self.data(..).eq(&other.data(..)) && self.meta().eq(other.meta())
    }
}

impl<'a> From<&'a mut Packet> for PacketRefMut<'a> {
    fn from(packet: &'a mut Packet) -> Self {
        Self::Packet(packet)
    }
}

impl<'a> From<&'a mut BytesPacket> for PacketRefMut<'a> {
    fn from(packet: &'a mut BytesPacket) -> Self {
        Self::Bytes(packet)
    }
}

impl PacketRefMut<'_> {
    pub fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        match self {
            Self::Packet(packet) => packet.data(index),
            Self::Bytes(packet) => packet.data(index),
        }
    }

    #[inline]
    pub fn meta(&self) -> &Meta {
        match self {
            Self::Packet(packet) => packet.meta(),
            Self::Bytes(packet) => packet.meta(),
        }
    }

    #[inline]
    pub fn meta_mut(&mut self) -> &mut Meta {
        match self {
            Self::Packet(packet) => packet.meta_mut(),
            Self::Bytes(packet) => packet.meta_mut(),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    #[inline]
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        match self {
            Self::Packet(packet) => {
                let size = src.len();
                packet.buffer_mut()[..size].copy_from_slice(src);
            }
            Self::Bytes(packet) => packet.copy_from_slice(src),
        }
    }

    #[inline]
    pub fn as_ref(&self) -> PacketRef<'_> {
        match self {
            Self::Packet(packet) => PacketRef::Packet(packet),
            Self::Bytes(packet) => PacketRef::Bytes(packet),
        }
    }
}

pub enum PacketBatchIter<'a> {
    Pinned(std::slice::Iter<'a, Packet>),
    Bytes(std::slice::Iter<'a, BytesPacket>),
}

impl DoubleEndedIterator for PacketBatchIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Pinned(iter) => iter.next_back().map(PacketRef::Packet),
            Self::Bytes(iter) => iter.next_back().map(PacketRef::Bytes),
        }
    }
}

impl<'a> Iterator for PacketBatchIter<'a> {
    type Item = PacketRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Pinned(iter) => iter.next().map(PacketRef::Packet),
            Self::Bytes(iter) => iter.next().map(PacketRef::Bytes),
        }
    }
}

pub enum PacketBatchIterMut<'a> {
    Pinned(std::slice::IterMut<'a, Packet>),
    Bytes(std::slice::IterMut<'a, BytesPacket>),
}

impl DoubleEndedIterator for PacketBatchIterMut<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Pinned(iter) => iter.next_back().map(PacketRefMut::Packet),
            Self::Bytes(iter) => iter.next_back().map(PacketRefMut::Bytes),
        }
    }
}

impl<'a> Iterator for PacketBatchIterMut<'a> {
    type Item = PacketRefMut<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Pinned(iter) => iter.next().map(PacketRefMut::Packet),
            Self::Bytes(iter) => iter.next().map(PacketRefMut::Bytes),
        }
    }
}

type PacketParIter<'a> = rayon::slice::Iter<'a, Packet>;
type BytesPacketParIter<'a> = rayon::slice::Iter<'a, BytesPacket>;

pub enum PacketBatchParIter<'a> {
    Pinned(
        rayon::iter::Map<
            PacketParIter<'a>,
            fn(<PacketParIter<'a> as ParallelIterator>::Item) -> PacketRef<'a>,
        >,
    ),
    Bytes(
        rayon::iter::Map<
            BytesPacketParIter<'a>,
            fn(<BytesPacketParIter<'a> as ParallelIterator>::Item) -> PacketRef<'a>,
        >,
    ),
}

impl<'a> ParallelIterator for PacketBatchParIter<'a> {
    type Item = PacketRef<'a>;
    fn drive_unindexed<C>(self, consumer: C) -> C::Result
    where
        C: rayon::iter::plumbing::UnindexedConsumer<Self::Item>,
    {
        match self {
            Self::Pinned(iter) => iter.drive_unindexed(consumer),
            Self::Bytes(iter) => iter.drive_unindexed(consumer),
        }
    }
}

impl IndexedParallelIterator for PacketBatchParIter<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Pinned(iter) => iter.len(),
            Self::Bytes(iter) => iter.len(),
        }
    }

    fn drive<C: rayon::iter::plumbing::Consumer<Self::Item>>(self, consumer: C) -> C::Result {
        match self {
            Self::Pinned(iter) => iter.drive(consumer),
            Self::Bytes(iter) => iter.drive(consumer),
        }
    }

    fn with_producer<CB: rayon::iter::plumbing::ProducerCallback<Self::Item>>(
        self,
        callback: CB,
    ) -> CB::Output {
        match self {
            Self::Pinned(iter) => iter.with_producer(callback),
            Self::Bytes(iter) => iter.with_producer(callback),
        }
    }
}

type PacketParIterMut<'a> = rayon::slice::IterMut<'a, Packet>;
type BytesPacketParIterMut<'a> = rayon::slice::IterMut<'a, BytesPacket>;

pub enum PacketBatchParIterMut<'a> {
    Pinned(
        rayon::iter::Map<
            PacketParIterMut<'a>,
            fn(<PacketParIterMut<'a> as ParallelIterator>::Item) -> PacketRefMut<'a>,
        >,
    ),
    Bytes(
        rayon::iter::Map<
            BytesPacketParIterMut<'a>,
            fn(<BytesPacketParIterMut<'a> as ParallelIterator>::Item) -> PacketRefMut<'a>,
        >,
    ),
}

impl<'a> ParallelIterator for PacketBatchParIterMut<'a> {
    type Item = PacketRefMut<'a>;
    fn drive_unindexed<C>(self, consumer: C) -> C::Result
    where
        C: rayon::iter::plumbing::UnindexedConsumer<Self::Item>,
    {
        match self {
            Self::Pinned(iter) => iter.drive_unindexed(consumer),
            Self::Bytes(iter) => iter.drive_unindexed(consumer),
        }
    }
}

impl IndexedParallelIterator for PacketBatchParIterMut<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Pinned(iter) => iter.len(),
            Self::Bytes(iter) => iter.len(),
        }
    }

    fn drive<C: rayon::iter::plumbing::Consumer<Self::Item>>(self, consumer: C) -> C::Result {
        match self {
            Self::Pinned(iter) => iter.drive(consumer),
            Self::Bytes(iter) => iter.drive(consumer),
        }
    }

    fn with_producer<CB: rayon::iter::plumbing::ProducerCallback<Self::Item>>(
        self,
        callback: CB,
    ) -> CB::Output {
        match self {
            Self::Pinned(iter) => iter.with_producer(callback),
            Self::Bytes(iter) => iter.with_producer(callback),
        }
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct RecycledPacketBatch {
    packets: RecycledVec<Packet>,
}

pub type PacketBatchRecycler = Recycler<RecycledVec<Packet>>;

impl RecycledPacketBatch {
    pub fn new(packets: Vec<Packet>) -> Self {
        Self {
            packets: RecycledVec::from_vec(packets),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let packets = RecycledVec::with_capacity(capacity);
        Self { packets }
    }

    pub fn new_with_recycler(
        recycler: &PacketBatchRecycler,
        capacity: usize,
        name: &'static str,
    ) -> Self {
        let mut packets = recycler.allocate(name);
        packets.preallocate(capacity);
        Self { packets }
    }

    pub fn new_with_recycler_data(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        mut packets: Vec<Packet>,
    ) -> Self {
        let mut batch = Self::new_with_recycler(recycler, packets.len(), name);
        batch.packets.append(&mut packets);
        batch
    }

    pub fn set_addr(&mut self, addr: &SocketAddr) {
        for p in self.iter_mut() {
            p.meta_mut().set_socket_addr(addr);
        }
    }

    pub fn push(&mut self, packet: Packet) {
        self.packets.push(packet)
    }

    pub fn truncate(&mut self, len: usize) {
        self.packets.truncate(len)
    }

    pub fn resize(&mut self, packets_per_batch: usize, value: Packet) {
        self.packets.resize(packets_per_batch, value)
    }

    pub fn capacity(&self) -> usize {
        self.packets.capacity()
    }
}

impl Deref for RecycledPacketBatch {
    type Target = [Packet];

    fn deref(&self) -> &Self::Target {
        &self.packets
    }
}

impl DerefMut for RecycledPacketBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packets
    }
}

impl<I: SliceIndex<[Packet]>> Index<I> for RecycledPacketBatch {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.packets[index]
    }
}

impl<I: SliceIndex<[Packet]>> IndexMut<I> for RecycledPacketBatch {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.packets[index]
    }
}

impl<'a> IntoIterator for &'a RecycledPacketBatch {
    type Item = &'a Packet;
    type IntoIter = Iter<'a, Packet>;

    fn into_iter(self) -> Self::IntoIter {
        self.packets.iter()
    }
}

impl<'a> IntoParallelIterator for &'a RecycledPacketBatch {
    type Iter = rayon::slice::Iter<'a, Packet>;
    type Item = &'a Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut RecycledPacketBatch {
    type Iter = rayon::slice::IterMut<'a, Packet>;
    type Item = &'a mut Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter_mut()
    }
}

impl From<RecycledPacketBatch> for Vec<Packet> {
    fn from(batch: RecycledPacketBatch) -> Self {
        batch.packets.into()
    }
}

pub fn to_packet_batches<T: Serialize>(items: &[T], chunk_size: usize) -> Vec<PacketBatch> {
    items
        .chunks(chunk_size)
        .map(|batch_items| {
            let mut batch = RecycledPacketBatch::with_capacity(batch_items.len());
            batch.packets.resize(batch_items.len(), Packet::default());
            for (item, packet) in batch_items.iter().zip(batch.packets.iter_mut()) {
                Packet::populate_packet(packet, None, item).expect("serialize request");
            }
            batch.into()
        })
        .collect()
}

#[cfg(test)]
fn to_packet_batches_for_tests<T: Serialize>(items: &[T]) -> Vec<PacketBatch> {
    to_packet_batches(items, NUM_PACKETS)
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Clone, Eq, PartialEq)]
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

    #[test]
    fn test_to_packets_pinning() {
        let recycler = PacketBatchRecycler::default();
        for i in 0..2 {
            let _first_packets =
                RecycledPacketBatch::new_with_recycler(&recycler, i + 1, "first one");
        }
    }

    #[test]
    fn test_packet_batch_wincode_roundtrip() {
        use std::net::Ipv4Addr;

        let meta = Meta::new(
            11,
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
            1234,
            PacketFlags::SIMPLE_VOTE_TX | PacketFlags::FROM_STAKED_NODE,
            Some(Pubkey::new_unique()),
        );
        let mut buffer = [0u8; PACKET_DATA_SIZE];
        buffer[..11].copy_from_slice(b"hello world");
        let packet = Packet::new(buffer, meta.clone());

        let mut discarded = packet.clone();
        discarded.meta_mut().set_discard(true);

        let batch = PacketBatch::Pinned(RecycledPacketBatch::new(vec![
            packet.clone(),
            packet.clone(),
            discarded,
            packet,
        ]));

        let bytes = wincode::serialize(&batch).unwrap();
        let restored: PacketBatch = wincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.len(), 3); // discarded packet must NOT be serialized

        for restored in restored.iter() {
            assert_eq!(restored.meta().addr, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
            // `port` is intentionally not traced: it has no downstream consumer.
            assert_eq!(restored.meta().port, 0);
            assert!(restored.meta().is_simple_vote_tx());
            assert!(restored.meta().is_from_staked_node());
            assert_eq!(restored.meta().remote_pubkey(), meta.remote_pubkey());
            assert_eq!(restored.data(..).unwrap(), b"hello world");
        }
    }

    #[test]
    fn test_packet_batch_wincode_roundtrip_empty() {
        let batch = PacketBatch::from(Vec::<BytesPacket>::new());
        let bytes = wincode::serialize(&batch).unwrap();
        let restored: PacketBatch = wincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.len(), 0);
    }
}
