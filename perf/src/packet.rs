//! The `packet` module defines data structures and methods to pull data from the network.
#[cfg(feature = "dev-context-only-utils")]
use bytes::{BufMut, BytesMut};
use {
    crate::{cuda_runtime::PinnedVec, recycler::Recycler},
    bincode::config::Options,
    bytes::Bytes,
    rayon::{
        iter::{IndexedParallelIterator, ParallelIterator},
        prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator},
    },
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    std::{
        borrow::Borrow,
        io::Read,
        net::SocketAddr,
        ops::{Deref, DerefMut, Index, IndexMut},
        slice::{Iter, SliceIndex},
    },
};
pub use {
    bytes,
    solana_packet::{self, Meta, Packet, PacketFlags, PACKET_DATA_SIZE},
};

pub const NUM_PACKETS: usize = 1024 * 8;

pub const PACKETS_PER_BATCH: usize = 64;
pub const NUM_RCVMMSGS: usize = 64;

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
    pub fn from_bytes(dest: Option<&SocketAddr>, buffer: Bytes) -> Self {
        let mut meta = Meta {
            size: buffer.len(),
            ..Default::default()
        };
        if let Some(dest) = dest {
            meta.set_socket_addr(dest);
        }

        Self { buffer, meta }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn from_data<T>(dest: Option<&SocketAddr>, data: T) -> bincode::Result<Self>
    where
        T: solana_packet::Encode,
    {
        let buffer = BytesMut::with_capacity(PACKET_DATA_SIZE);
        let mut writer = buffer.writer();
        data.encode(&mut writer)?;
        let buffer = writer.into_inner();
        let buffer = buffer.freeze();

        let mut meta = Meta {
            size: buffer.len(),
            ..Default::default()
        };
        if let Some(dest) = dest {
            meta.set_socket_addr(dest);
        }

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

    pub fn deserialize_slice<T, I>(&self, index: I) -> bincode::Result<T>
    where
        T: serde::de::DeserializeOwned,
        I: SliceIndex<[u8], Output = [u8]>,
    {
        let bytes = self.data(index).ok_or(bincode::ErrorKind::SizeLimit)?;
        bincode::options()
            .with_limit(self.meta().size as u64)
            .with_fixint_encoding()
            .reject_trailing_bytes()
            .deserialize(bytes)
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
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PacketBatch {
    Pinned(PinnedPacketBatch),
    Bytes(BytesPacketBatch),
}

impl PacketBatch {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn first(&self) -> Option<PacketRef<'_>> {
        match self {
            Self::Pinned(batch) => batch.first().map(PacketRef::from),
            Self::Bytes(batch) => batch.first().map(PacketRef::from),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn first_mut(&mut self) -> Option<PacketRefMut<'_>> {
        match self {
            Self::Pinned(batch) => batch.first_mut().map(PacketRefMut::from),
            Self::Bytes(batch) => batch.first_mut().map(PacketRefMut::from),
        }
    }

    /// Returns `true` if the batch contains no elements.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Pinned(batch) => batch.is_empty(),
            Self::Bytes(batch) => batch.is_empty(),
        }
    }

    /// Returns a reference to an element.
    pub fn get(&self, index: usize) -> Option<PacketRef<'_>> {
        match self {
            Self::Pinned(batch) => batch.get(index).map(PacketRef::from),
            Self::Bytes(batch) => batch.get(index).map(PacketRef::from),
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<PacketRefMut<'_>> {
        match self {
            Self::Pinned(batch) => batch.get_mut(index).map(PacketRefMut::from),
            Self::Bytes(batch) => batch.get_mut(index).map(PacketRefMut::from),
        }
    }

    pub fn iter(&self) -> PacketBatchIter<'_> {
        match self {
            Self::Pinned(batch) => PacketBatchIter::Pinned(batch.iter()),
            Self::Bytes(batch) => PacketBatchIter::Bytes(batch.iter()),
        }
    }

    pub fn iter_mut(&mut self) -> PacketBatchIterMut<'_> {
        match self {
            Self::Pinned(batch) => PacketBatchIterMut::Pinned(batch.iter_mut()),
            Self::Bytes(batch) => PacketBatchIterMut::Bytes(batch.iter_mut()),
        }
    }

    pub fn par_iter(&self) -> PacketBatchParIter {
        match self {
            Self::Pinned(batch) => {
                PacketBatchParIter::Pinned(batch.par_iter().map(PacketRef::from))
            }
            Self::Bytes(batch) => PacketBatchParIter::Bytes(batch.par_iter().map(PacketRef::from)),
        }
    }

    pub fn par_iter_mut(&mut self) -> PacketBatchParIterMut {
        match self {
            Self::Pinned(batch) => {
                PacketBatchParIterMut::Pinned(batch.par_iter_mut().map(PacketRefMut::from))
            }
            Self::Bytes(batch) => {
                PacketBatchParIterMut::Bytes(batch.par_iter_mut().map(PacketRefMut::from))
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Pinned(batch) => batch.len(),
            Self::Bytes(batch) => batch.len(),
        }
    }
}

impl From<PinnedPacketBatch> for PacketBatch {
    fn from(batch: PinnedPacketBatch) -> Self {
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

    pub fn deserialize_slice<T, I>(&self, index: I) -> bincode::Result<T>
    where
        T: serde::de::DeserializeOwned,
        I: SliceIndex<[u8], Output = [u8]>,
    {
        match self {
            Self::Packet(packet) => packet.deserialize_slice(index),
            Self::Bytes(packet) => packet.deserialize_slice(index),
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

    pub fn deserialize_slice<T, I>(&self, index: I) -> bincode::Result<T>
    where
        T: serde::de::DeserializeOwned,
        I: SliceIndex<[u8], Output = [u8]>,
    {
        match self {
            Self::Packet(packet) => packet.deserialize_slice(index),
            Self::Bytes(packet) => packet.deserialize_slice(index),
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

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PinnedPacketBatch {
    packets: PinnedVec<Packet>,
}

pub type PacketBatchRecycler = Recycler<PinnedVec<Packet>>;

impl PinnedPacketBatch {
    pub fn new(packets: Vec<Packet>) -> Self {
        let packets = PinnedVec::from_vec(packets);
        Self { packets }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let packets = PinnedVec::with_capacity(capacity);
        Self { packets }
    }

    pub fn new_pinned_with_capacity(capacity: usize) -> Self {
        let mut batch = Self::with_capacity(capacity);
        batch.packets.reserve_and_pin(capacity);
        batch
    }

    pub fn new_unpinned_with_recycler(
        recycler: &PacketBatchRecycler,
        capacity: usize,
        name: &'static str,
    ) -> Self {
        let mut packets = recycler.allocate(name);
        packets.reserve(capacity);
        Self { packets }
    }

    pub fn new_with_recycler(
        recycler: &PacketBatchRecycler,
        capacity: usize,
        name: &'static str,
    ) -> Self {
        let mut packets = recycler.allocate(name);
        packets.reserve_and_pin(capacity);
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

    pub fn new_unpinned_with_recycler_data_and_dests<S, T>(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        dests_and_data: impl IntoIterator<Item = (S, T), IntoIter: ExactSizeIterator>,
    ) -> Self
    where
        S: Borrow<SocketAddr>,
        T: solana_packet::Encode,
    {
        let dests_and_data = dests_and_data.into_iter();
        let mut batch = Self::new_unpinned_with_recycler(recycler, dests_and_data.len(), name);
        batch
            .packets
            .resize(dests_and_data.len(), Packet::default());

        for ((addr, data), packet) in dests_and_data.zip(batch.packets.iter_mut()) {
            let addr = addr.borrow();
            if !addr.ip().is_unspecified() && addr.port() != 0 {
                if let Err(e) = Packet::populate_packet(packet, Some(addr), &data) {
                    // TODO: This should never happen. Instead the caller should
                    // break the payload into smaller messages, and here any errors
                    // should be propagated.
                    error!("Couldn't write to packet {:?}. Data skipped.", e);
                    packet.meta_mut().set_discard(true);
                }
            } else {
                trace!("Dropping packet, as destination is unknown");
                packet.meta_mut().set_discard(true);
            }
        }
        batch
    }

    pub fn new_unpinned_with_recycler_data(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        mut packets: Vec<Packet>,
    ) -> Self {
        let mut batch = Self::new_unpinned_with_recycler(recycler, packets.len(), name);
        batch.packets.append(&mut packets);
        batch
    }

    pub fn set_addr(&mut self, addr: &SocketAddr) {
        for p in self.iter_mut() {
            p.meta_mut().set_socket_addr(addr);
        }
    }
}

impl Deref for PinnedPacketBatch {
    type Target = PinnedVec<Packet>;

    fn deref(&self) -> &Self::Target {
        &self.packets
    }
}

impl DerefMut for PinnedPacketBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packets
    }
}

impl<I: SliceIndex<[Packet]>> Index<I> for PinnedPacketBatch {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.packets[index]
    }
}

impl<I: SliceIndex<[Packet]>> IndexMut<I> for PinnedPacketBatch {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.packets[index]
    }
}

impl<'a> IntoIterator for &'a PinnedPacketBatch {
    type Item = &'a Packet;
    type IntoIter = Iter<'a, Packet>;

    fn into_iter(self) -> Self::IntoIter {
        self.packets.iter()
    }
}

impl<'a> IntoParallelIterator for &'a PinnedPacketBatch {
    type Iter = rayon::slice::Iter<'a, Packet>;
    type Item = &'a Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut PinnedPacketBatch {
    type Iter = rayon::slice::IterMut<'a, Packet>;
    type Item = &'a mut Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter_mut()
    }
}

impl From<PinnedPacketBatch> for Vec<Packet> {
    fn from(batch: PinnedPacketBatch) -> Self {
        batch.packets.into()
    }
}

pub fn to_packet_batches<T: Serialize>(items: &[T], chunk_size: usize) -> Vec<PacketBatch> {
    items
        .chunks(chunk_size)
        .map(|batch_items| {
            let mut batch = PinnedPacketBatch::with_capacity(batch_items.len());
            batch.resize(batch_items.len(), Packet::default());
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

    pub fn to_pinned_packet_batch(&self) -> PinnedPacketBatch {
        let mut batch = PinnedPacketBatch::new_pinned_with_capacity(self.len());
        for bytes_packet in self.iter() {
            let mut packet = Packet::default();
            let size = bytes_packet.meta().size;
            *packet.meta_mut() = bytes_packet.meta().clone();
            packet.meta_mut().size = size;
            packet.buffer_mut()[..size].copy_from_slice(&bytes_packet.buffer);

            batch.push(packet);
        }

        batch
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

pub fn deserialize_from_with_limit<R, T>(reader: R) -> bincode::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    // with_limit causes pre-allocation size to be limited
    // to prevent against memory exhaustion attacks.
    bincode::options()
        .with_limit(PACKET_DATA_SIZE as u64)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from(reader)
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
                PinnedPacketBatch::new_with_recycler(&recycler, i + 1, "first one");
        }
    }
}
