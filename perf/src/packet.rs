//! The `packet` module defines data structures and methods to pull data from the network.
#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample};
#[cfg(feature = "dev-context-only-utils")]
use wincode::{ReadError, ReadResult, SchemaRead, config::DefaultConfig};
use {
    crate::{recycled_vec::RecycledVec, recycler::Recycler},
    bitflags::bitflags,
    bytes::Bytes,
    rayon::{
        iter::{IndexedParallelIterator, ParallelIterator},
        prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator},
    },
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{
        borrow::Borrow,
        io::Cursor,
        net::{IpAddr, SocketAddr},
        ops::{Deref, DerefMut, Index, IndexMut},
        slice::{Iter, SliceIndex},
    },
    wincode::{
        SchemaWrite, WriteResult,
        config::{Config, Configuration},
    },
};
pub use {
    bytes,
    solana_packet::{self, Meta, PACKET_DATA_SIZE, Packet, PacketFlags as LegacyPacketFlags},
};

bitflags! {
    #[repr(C)]
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PacketFlags: u8 {
        const DISCARD          = 0b0000_0001;
        const FORWARDED        = 0b0000_0010;
        const REPAIR           = 0b0000_0100;
        const SIMPLE_VOTE_TX   = 0b0000_1000;
        const UNUSED_0         = 0b0001_0000;
        const UNUSED_1         = 0b0010_0000;
        const PERF_TRACK_PACKET = 0b0100_0000;
        const FROM_STAKED_NODE = 0b1000_0000;
    }
}

#[cfg(feature = "frozen-abi")]
impl ::solana_frozen_abi::abi_example::AbiExample for PacketFlags {
    fn example() -> Self {
        Self::empty()
    }
}

#[cfg(feature = "frozen-abi")]
impl ::solana_frozen_abi::abi_example::TransparentAsHelper for PacketFlags {}

#[cfg(feature = "frozen-abi")]
impl ::solana_frozen_abi::abi_example::EvenAsOpaque for PacketFlags {
    const TYPE_NAME_MATCHER: &'static str = "::_::InternalBitFlags";
}

#[cfg(feature = "frozen-abi")]
impl ::solana_frozen_abi::stable_abi::StableAbi for PacketFlags {
    fn random_with_context(
        rng: &mut (impl ::solana_frozen_abi::rand::RngCore + ?Sized),
        _ctx: (),
    ) -> Self {
        Self::from_bits_truncate(::solana_frozen_abi::rand::Rng::random(rng))
    }
}

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
    let buffer = Bytes::copy_from_slice(&buffer[..size]);
    Ok(match dest {
        Some(dest) => BytesPacket::new_with_addr_and_port(buffer, dest.ip(), dest.port()),
        None => BytesPacket::new(buffer),
    })
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
    size: usize,
    addr: Option<IpAddr>,
    port: Option<u16>,
    flags: PacketFlags,
    remote_pubkey: Option<Pubkey>,
}

impl BytesPacket {
    pub fn new(buffer: Bytes) -> Self {
        let size = buffer.len();
        Self {
            buffer,
            size,
            addr: None,
            port: None,
            flags: PacketFlags::empty(),
            remote_pubkey: None,
        }
    }

    pub fn new_with_addr_and_port(buffer: Bytes, addr: IpAddr, port: u16) -> Self {
        Self {
            addr: Some(addr),
            port: Some(port),
            ..Self::new(buffer)
        }
    }

    pub fn new_with_socket_addr(buffer: Bytes, socket_addr: &SocketAddr) -> Self {
        Self::new_with_addr_and_port(buffer, socket_addr.ip(), socket_addr.port())
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn empty() -> Self {
        Self::new(Bytes::new())
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn from_bytes(dest: Option<&SocketAddr>, buffer: impl Into<Bytes>) -> Self {
        let buffer = buffer.into();
        match dest {
            Some(dest) => Self::new_with_socket_addr(buffer, dest),
            None => Self::new(buffer),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn from_data<T>(data: T) -> WriteResult<Self>
    where
        T: SchemaWrite<DefaultConfig, Src = T>,
    {
        let buffer = Bytes::from(wincode::serialize(&data)?);
        Ok(Self::new(buffer))
    }

    #[inline]
    pub fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        if self.discard() {
            None
        } else {
            self.buffer.get(..self.size)?.get(index)
        }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_size(&mut self, size: usize) {
        self.size = size;
    }

    #[inline]
    pub fn addr(&self) -> Option<IpAddr> {
        self.addr
    }

    #[inline]
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    #[inline]
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        Some(SocketAddr::new(self.addr?, self.port?))
    }

    #[inline]
    pub fn set_socket_addr(&mut self, socket_addr: &SocketAddr) {
        self.addr = Some(socket_addr.ip());
        self.port = Some(socket_addr.port());
    }

    #[inline]
    pub fn flags(&self) -> PacketFlags {
        self.flags
    }

    #[inline]
    pub fn set_flags(&mut self, flags: PacketFlags) {
        self.flags = flags;
    }

    #[inline]
    pub fn insert_flags(&mut self, flags: PacketFlags) {
        self.flags.insert(flags);
    }

    #[inline]
    pub fn remove_flags(&mut self, flags: PacketFlags) {
        self.flags.remove(flags);
    }

    #[inline]
    pub fn discard(&self) -> bool {
        self.flags.contains(PacketFlags::DISCARD)
    }

    #[inline]
    pub fn set_discard(&mut self, discard: bool) {
        self.flags.set(PacketFlags::DISCARD, discard);
    }

    #[inline]
    pub fn forwarded(&self) -> bool {
        self.flags.contains(PacketFlags::FORWARDED)
    }

    #[inline]
    pub fn repair(&self) -> bool {
        self.flags.contains(PacketFlags::REPAIR)
    }

    #[inline]
    pub fn is_simple_vote_tx(&self) -> bool {
        self.flags.contains(PacketFlags::SIMPLE_VOTE_TX)
    }

    #[inline]
    pub fn is_from_staked_node(&self) -> bool {
        self.flags.contains(PacketFlags::FROM_STAKED_NODE)
    }

    #[inline]
    pub fn set_from_staked_node(&mut self, from_staked_node: bool) {
        self.flags
            .set(PacketFlags::FROM_STAKED_NODE, from_staked_node);
    }

    #[inline]
    pub fn remote_pubkey(&self) -> Option<Pubkey> {
        self.remote_pubkey
    }

    #[inline]
    pub fn set_remote_pubkey(&mut self, remote_pubkey: Option<Pubkey>) {
        self.remote_pubkey = remote_pubkey;
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn copy_from_slice(&mut self, slice: &[u8]) {
        self.buffer = Bytes::from(slice.to_vec());
        self.size = slice.len();
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
        self.size = buffer.len();
        self.buffer = buffer;
    }
}

#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
        self.size() == other.size()
            && self.addr() == other.addr()
            && self.port() == other.port()
            && self.flags() == other.flags()
            && self.remote_pubkey() == other.remote_pubkey()
            && self.data(..).eq(&other.data(..))
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

    pub fn size(&self) -> usize {
        match self {
            Self::Packet(packet) => packet.meta().size,
            Self::Bytes(packet) => packet.size(),
        }
    }

    pub fn addr(&self) -> Option<IpAddr> {
        match self {
            Self::Packet(packet) => {
                let addr = packet.meta().addr;
                (!addr.is_unspecified()).then_some(addr)
            }
            Self::Bytes(packet) => packet.addr(),
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Packet(packet) => {
                let port = packet.meta().port;
                (port != 0).then_some(port)
            }
            Self::Bytes(packet) => packet.port(),
        }
    }

    pub fn socket_addr(&self) -> Option<SocketAddr> {
        Some(SocketAddr::new(self.addr()?, self.port()?))
    }

    pub fn flags(&self) -> PacketFlags {
        match self {
            Self::Packet(packet) => PacketFlags::from_bits_retain(packet.meta().flags.bits()),
            Self::Bytes(packet) => packet.flags(),
        }
    }

    pub fn discard(&self) -> bool {
        self.flags().contains(PacketFlags::DISCARD)
    }

    pub fn forwarded(&self) -> bool {
        self.flags().contains(PacketFlags::FORWARDED)
    }

    pub fn repair(&self) -> bool {
        self.flags().contains(PacketFlags::REPAIR)
    }

    pub fn is_simple_vote_tx(&self) -> bool {
        self.flags().contains(PacketFlags::SIMPLE_VOTE_TX)
    }

    pub fn is_from_staked_node(&self) -> bool {
        self.flags().contains(PacketFlags::FROM_STAKED_NODE)
    }

    pub fn remote_pubkey(&self) -> Option<Pubkey> {
        match self {
            Self::Packet(packet) => packet.meta().remote_pubkey(),
            Self::Bytes(packet) => packet.remote_pubkey(),
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
                let mut bytes_packet = match self.socket_addr() {
                    Some(socket_addr) => BytesPacket::new_with_socket_addr(buffer, &socket_addr),
                    None => BytesPacket::new(buffer),
                };
                bytes_packet.set_flags(self.flags());
                bytes_packet.set_remote_pubkey(self.remote_pubkey());
                bytes_packet
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
        self.as_ref().eq(&other.as_ref())
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

    pub fn size(&self) -> usize {
        self.as_ref().size()
    }

    pub fn addr(&self) -> Option<IpAddr> {
        self.as_ref().addr()
    }

    pub fn port(&self) -> Option<u16> {
        self.as_ref().port()
    }

    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.as_ref().socket_addr()
    }

    pub fn flags(&self) -> PacketFlags {
        self.as_ref().flags()
    }

    pub fn discard(&self) -> bool {
        self.as_ref().discard()
    }

    pub fn forwarded(&self) -> bool {
        self.as_ref().forwarded()
    }

    pub fn repair(&self) -> bool {
        self.as_ref().repair()
    }

    pub fn is_simple_vote_tx(&self) -> bool {
        self.as_ref().is_simple_vote_tx()
    }

    pub fn is_from_staked_node(&self) -> bool {
        self.as_ref().is_from_staked_node()
    }

    pub fn remote_pubkey(&self) -> Option<Pubkey> {
        self.as_ref().remote_pubkey()
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_size(&mut self, size: usize) {
        match self {
            Self::Packet(packet) => packet.meta_mut().size = size,
            Self::Bytes(packet) => packet.set_size(size),
        }
    }

    pub fn set_socket_addr(&mut self, socket_addr: &SocketAddr) {
        match self {
            Self::Packet(packet) => packet.meta_mut().set_socket_addr(socket_addr),
            Self::Bytes(packet) => packet.set_socket_addr(socket_addr),
        }
    }

    pub fn set_flags(&mut self, flags: PacketFlags) {
        match self {
            Self::Packet(packet) => {
                packet.meta_mut().flags = solana_packet::PacketFlags::from_bits_retain(flags.bits())
            }
            Self::Bytes(packet) => packet.set_flags(flags),
        }
    }

    pub fn insert_flags(&mut self, flags: PacketFlags) {
        self.set_flags(self.flags() | flags);
    }

    pub fn remove_flags(&mut self, flags: PacketFlags) {
        let mut packet_flags = self.flags();
        packet_flags.remove(flags);
        self.set_flags(packet_flags);
    }

    pub fn set_discard(&mut self, discard: bool) {
        let mut flags = self.flags();
        flags.set(PacketFlags::DISCARD, discard);
        self.set_flags(flags);
    }

    pub fn set_from_staked_node(&mut self, from_staked_node: bool) {
        let mut flags = self.flags();
        flags.set(PacketFlags::FROM_STAKED_NODE, from_staked_node);
        self.set_flags(flags);
    }

    pub fn set_remote_pubkey(&mut self, remote_pubkey: Option<Pubkey>) {
        match self {
            Self::Packet(packet) => packet
                .meta_mut()
                .set_remote_pubkey(remote_pubkey.unwrap_or_default()),
            Self::Bytes(packet) => packet.set_remote_pubkey(remote_pubkey),
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

#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
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
        packets: impl IntoIterator<Item = Packet, IntoIter: ExactSizeIterator>,
    ) -> Self {
        let packets = packets.into_iter();
        let mut batch = Self::new_with_recycler(recycler, packets.len(), name);
        batch.packets.extend(packets);
        batch
    }

    pub fn new_with_recycler_data_and_dests<S, T>(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        dests_and_data: impl IntoIterator<Item = (S, T), IntoIter: ExactSizeIterator>,
    ) -> Self
    where
        S: Borrow<SocketAddr>,
        T: solana_packet::Encode,
    {
        let dests_and_data = dests_and_data.into_iter();
        let mut batch = Self::new_with_recycler(recycler, dests_and_data.len(), name);
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
                    error!("Couldn't write to packet {e:?}. Data skipped.");
                    packet.meta_mut().set_discard(true);
                }
            } else {
                trace!("Dropping packet, as destination is unknown");
                packet.meta_mut().set_discard(true);
            }
        }
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
                    BytesPacket::new(buffer)
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
    fn test_bytes_packet_constructors() {
        let buffer = Bytes::from_static(b"packet");
        let packet = BytesPacket::new(buffer.clone());
        assert_eq!(packet.buffer(), &buffer);
        assert_eq!(packet.size(), buffer.len());
        assert_eq!(packet.data(..), Some(buffer.as_ref()));
        assert_eq!(packet.addr(), None);
        assert_eq!(packet.port(), None);
        assert_eq!(packet.socket_addr(), None);
        assert_eq!(packet.flags(), PacketFlags::empty());
        assert_eq!(packet.remote_pubkey(), None);

        let socket_addr = "127.0.0.1:1234".parse::<SocketAddr>().unwrap();
        let packet = BytesPacket::new_with_socket_addr(buffer.clone(), &socket_addr);
        assert_eq!(packet.size(), buffer.len());
        assert_eq!(packet.addr(), Some(socket_addr.ip()));
        assert_eq!(packet.port(), Some(socket_addr.port()));
        assert_eq!(packet.socket_addr(), Some(socket_addr));
    }

    #[test]
    fn test_bytes_packet_metadata() {
        let socket_addr = "[::1]:4321".parse::<SocketAddr>().unwrap();
        let remote_pubkey = Pubkey::new_unique();
        let mut packet = BytesPacket::new(Bytes::from_static(b"packet"));
        packet.set_socket_addr(&socket_addr);
        packet.insert_flags(PacketFlags::REPAIR | PacketFlags::FROM_STAKED_NODE);
        packet.set_remote_pubkey(Some(remote_pubkey));

        assert_eq!(packet.socket_addr(), Some(socket_addr));
        assert!(packet.repair());
        assert!(packet.is_from_staked_node());
        assert_eq!(packet.remote_pubkey(), Some(remote_pubkey));

        packet.set_buffer(Bytes::from_static(b"new packet"));
        assert_eq!(packet.size(), 10);
        assert_eq!(packet.data(..), Some(&b"new packet"[..]));
    }

    #[test]
    fn test_packet_ref_metadata_bridge() {
        let socket_addr = "127.0.0.1:1234".parse::<SocketAddr>().unwrap();
        let mut packet = Packet::default();
        packet.meta_mut().set_socket_addr(&socket_addr);
        packet.meta_mut().flags = LegacyPacketFlags::REPAIR;

        let mut packet = PacketRefMut::from(&mut packet);
        assert_eq!(packet.socket_addr(), Some(socket_addr));
        assert_eq!(packet.flags(), PacketFlags::REPAIR);
        packet.insert_flags(PacketFlags::DISCARD);
        assert!(packet.discard());

        let PacketRefMut::Packet(packet) = packet else {
            unreachable!();
        };
        assert_eq!(
            packet.meta().flags,
            LegacyPacketFlags::REPAIR | LegacyPacketFlags::DISCARD
        );
    }

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
}
