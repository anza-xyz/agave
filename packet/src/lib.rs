//! The definition of a Solana network packet.
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(feature = "bincode")]
use bincode::{Options, Result};
// `BufMut` provides convenient methods for writing bytes into `BytesMut`.
// Export them.
pub use bytes::BufMut;
#[cfg(feature = "serde")]
use serde_derive::{Deserialize, Serialize};
#[cfg(feature = "frozen-abi")]
use solana_frozen_abi_macro::AbiExample;
use {
    bitflags::bitflags,
    bytes::{Bytes, BytesMut},
    std::{
        fmt,
        io::Write,
        mem::{self, MaybeUninit},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        ops::{Deref, DerefMut},
        slice::SliceIndex,
    },
};

#[cfg(test)]
static_assertions::const_assert_eq!(PACKET_DATA_SIZE, 1232);
/// Maximum over-the-wire size of a Transaction
///   1280 is IPv6 minimum MTU
///   40 bytes is the size of the IPv6 header
///   8 bytes is the size of the fragment header
pub const PACKET_DATA_SIZE: usize = 1280 - 40 - 8;

bitflags! {
    #[repr(C)]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct PacketFlags: u8 {
        const DISCARD        = 0b0000_0001;
        const FORWARDED      = 0b0000_0010;
        const REPAIR         = 0b0000_0100;
        const SIMPLE_VOTE_TX = 0b0000_1000;
        // Previously used - this can now be re-used for something else.
        const UNUSED_0  = 0b0001_0000;
        // Previously used - this can now be re-used for something else.
        const UNUSED_1 = 0b0010_0000;
        /// For tracking performance
        const PERF_TRACK_PACKET  = 0b0100_0000;
        /// For marking packets from staked nodes
        const FROM_STAKED_NODE = 0b1000_0000;
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Meta {
    pub addr: IpAddr,
    pub port: u16,
    pub flags: PacketFlags,
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

/// Returns an immutable slice of the provided `buffer`.
///
/// Returns `None` if the index is invalid or if the provided `meta` is marked
/// as discard.
#[inline]
fn data<'a, I>(
    buffer: &'a [u8],
    meta: &Meta,
    index: I,
) -> Option<&'a <I as SliceIndex<[u8]>>::Output>
where
    I: SliceIndex<[u8]>,
{
    // If the packet is marked as discard, it is either invalid or
    // otherwise should be ignored, and so the payload should not be read
    // from.
    if meta.discard() {
        None
    } else {
        buffer.get(index)
    }
}

/// Creates a [`BytesMut`] buffer and [`Meta`] from the given serializable
/// `data`.
#[cfg(feature = "bincode")]
fn from_data<T>(dest: Option<&SocketAddr>, data: T) -> Result<(BytesMut, Meta)>
where
    T: serde::Serialize,
{
    let buffer = BytesMut::with_capacity(PACKET_DATA_SIZE);
    let mut writer = buffer.writer();
    bincode::serialize_into(&mut writer, &data)?;
    let buffer = writer.into_inner();
    let mut meta = Meta::default();
    if let Some(dest) = dest {
        meta.set_socket_addr(dest);
    }
    Ok((buffer, meta))
}

/// Read data and metadata from the packet.
pub trait PacketRead {
    fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>;
    /// Returns an immutable reference to the metadata.
    fn meta(&self) -> &Meta;
    fn size(&self) -> usize;

    #[cfg(feature = "bincode")]
    fn deserialize_slice<T, I>(&self, index: I) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        I: SliceIndex<[u8], Output = [u8]>,
    {
        let bytes = self.data(index).ok_or(bincode::ErrorKind::SizeLimit)?;
        bincode::options()
            .with_limit(PACKET_DATA_SIZE as u64)
            .with_fixint_encoding()
            .reject_trailing_bytes()
            .deserialize(bytes)
    }
}

/// Representation of a network packet, consisting of the `buffer` containing
/// the payload and `meta` with information about socket address, size and
/// flags.
///
/// `Packet` is cheaply clonable. Multiple `Packet` instances can point to
/// the same underlying memory. Cloning a `Packet` copies only metadata.
///
/// `Packet`'s `buffer` is immutable. If you are looking for a structure
/// meant for receiving socket messages and mutation, use [`PacketMut`].
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Clone, Default, Eq)]
pub struct Packet {
    buffer: Bytes,
    meta: Meta,
}

impl Packet {
    pub fn new(buffer: Bytes, meta: Meta) -> Self {
        Self { buffer, meta }
    }

    /// Returns a mutable reference to the metadata.
    #[inline]
    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    #[cfg(feature = "bincode")]
    pub fn from_data<T>(dest: Option<&SocketAddr>, data: T) -> Result<Self>
    where
        T: serde::Serialize,
    {
        let (buffer, meta) = from_data(dest, data)?;
        let buffer = buffer.freeze();
        Ok(Packet { buffer, meta })
    }

    /// Converts the packet into [`PacketMut`]. Makes a copy.
    pub fn to_packet_mut(&self) -> PacketMut {
        let buffer = match self.data(..) {
            Some(data) => BytesMut::from(data),
            None => BytesMut::new(),
        };
        let meta = self.meta.clone();
        PacketMut { buffer, meta }
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Packet {{ addr: {:?} }}", self.meta.socket_addr())
    }
}

impl Deref for Packet {
    type Target = Bytes;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl From<PacketArray> for Packet {
    fn from(packet_array: PacketArray) -> Self {
        let buffer = match packet_array.data(..) {
            Some(data) => Bytes::from(data.to_vec()),
            None => Bytes::new(),
        };
        let PacketArray { meta, .. } = packet_array;
        Self { buffer, meta }
    }
}

impl PacketRead for Packet {
    #[inline]
    fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        data(&self.buffer, &self.meta, index)
    }

    #[inline]
    fn meta(&self) -> &Meta {
        &self.meta
    }

    #[inline]
    fn size(&self) -> usize {
        self.buffer.len()
    }
}

impl PartialEq for Packet {
    fn eq(&self, other: &Self) -> bool {
        self.meta() == other.meta() && self.data(..) == other.data(..)
    }
}

/// Mutable representation of a network packet, consisting of the `buffer` for
/// storing the payload and `meta` for keeping information about socket
/// address, size and flags.
///
/// The main use case is using `PacketMut` as a buffer for receving messages
/// with syscalls like `recvmmsg`. It's also convenient for writing tests,
/// where we want to construct a packet manually before passing it to a tested
/// function.
///
/// `PacketMut` is cheaply clonable. Multiple `PacketMut` instances can point
/// to the same underlying memory. Cloning a `PacketMut` copies only metadata.
///
/// `PacketMut` can be converted into an immutable packet using
/// [`freeze`](Self::freeze) method.
#[derive(Clone, Eq, PartialEq)]
pub struct PacketMut {
    buffer: BytesMut,
    meta: Meta,
}

impl PacketMut {
    /// Returns a mutable reference to the underlying buffer. The returned
    /// buffer can be extended with new data.
    ///
    /// Intended to use for receiving messages or other low-level network
    /// operations.
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }

    /// Returns a mutable reference to underlying data. The returned slice is
    /// mutable, but, like any mutable slice, cannot be resized.
    ///
    /// Intended to use for payload modifications, which don't write new data,
    /// e.g. resigning shreds.
    #[inline]
    pub fn data_mut<I>(&mut self, index: I) -> Option<&mut <I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        // If the packet is marked as discard, it is either invalid or
        // otherwise should be ignored, and so the payload should not be read
        // from.
        if self.meta.discard() {
            None
        } else {
            self.buffer.get_mut(index)
        }
    }

    /// Returns a mutable reference to the metadata.
    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    #[cfg(feature = "bincode")]
    pub fn from_data<T>(dest: Option<&SocketAddr>, data: T) -> Result<Self>
    where
        T: serde::Serialize,
    {
        let (buffer, meta) = from_data(dest, data)?;
        Ok(PacketMut { buffer, meta })
    }

    /// Converts `self` into an immutable [`Packet`].
    ///
    /// The conversion is zero cost and is used to indicate that the packet
    /// buffer referenced by the handle will no longer be mutated. The
    /// resulting [`Packet`] can be cloned and shared across threads.
    pub fn freeze(self) -> Packet {
        let Self { buffer, meta } = self;
        let buffer = buffer.freeze();
        Packet { buffer, meta }
    }
}

impl fmt::Debug for PacketMut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PacketMut {{ addr: {:?} }}", self.meta.socket_addr())
    }
}

impl Default for PacketMut {
    fn default() -> Self {
        let buffer = BytesMut::with_capacity(PACKET_DATA_SIZE);
        let meta = Meta::default();
        Self { buffer, meta }
    }
}

impl Deref for PacketMut {
    type Target = BytesMut;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for PacketMut {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl PacketRead for PacketMut {
    #[inline]
    fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        data(&self.buffer, &self.meta, index)
    }

    fn meta(&self) -> &Meta {
        &self.meta
    }

    #[inline]
    fn size(&self) -> usize {
        self.buffer.len()
    }
}

// TODO: Re-evaluate the necessity of `PacketArray`.
//
// `Bytes` contain contiguous memory. We know the size of each bytes. What's
// not contiguous is the `Packet` struct itself. Perhaps if we pass `Bytes` of
// a packet to CUDA separately (just `Bytes`, not the whole `Packet`), it's
// going to work just fine. However, doing so is not going to be trivial.

/// Representation of a network packet, where the `buffer` is an array.
///
/// `PacketArray` is expensive to clone and loses all the zero-copy benefits of
/// [`Packet`] and [`PacketMut`]. However, `PacketArray` is a contiguous struct
/// with no pointers, and therefore is very convenient for CUDA.
#[derive(Clone)]
#[repr(C)]
pub struct PacketArray {
    buffer: [MaybeUninit<u8>; PACKET_DATA_SIZE],
    meta: Meta,
    size: usize,
}

impl PacketArray {
    pub fn from_packet<P>(packet: &P) -> Self
    where
        P: PacketRead,
    {
        let meta = packet.meta().to_owned();
        let mut new_buffer = [MaybeUninit::uninit(); PACKET_DATA_SIZE];
        if let Some(data) = packet.data(..) {
            let mut writer = new_buffer.writer();
            // PANICS: We are writing to a buffer. The only chance of any error
            // happening here is if the data is larger than a buffer, but we
            // already prevent that by ensuring the constraints in `Packet`.
            writer.write_all(data).unwrap();
        }

        Self {
            buffer: new_buffer,
            meta,
            size: packet.size(),
        }
    }

    #[cfg(feature = "bincode")]
    pub fn from_data<T>(dest: Option<&SocketAddr>, data: T) -> Result<Self>
    where
        T: serde::Serialize,
    {
        let mut packet = PacketArray {
            size: bincode::serialized_size(&data)? as usize,
            ..Default::default()
        };
        let mut writer = packet.buffer.writer();
        bincode::serialize_into(&mut writer, &data)?;
        if let Some(dest) = dest {
            packet.meta.set_socket_addr(dest);
        }
        Ok(packet)
    }
}

impl fmt::Debug for PacketArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Packet {{ addr: {:?}, size: {:?} }}",
            self.meta.socket_addr(),
            self.size
        )
    }
}

impl Default for PacketArray {
    fn default() -> Self {
        Self {
            buffer: [MaybeUninit::uninit(); PACKET_DATA_SIZE],
            meta: Meta::default(),
            size: usize::default(),
        }
    }
}

impl Eq for PacketArray {}

impl<P> From<P> for PacketArray
where
    P: PacketRead,
{
    fn from(packet: P) -> Self {
        Self::from_packet(&packet)
    }
}

impl PacketArray {
    fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        if self.meta.discard() {
            None
        } else {
            // SAFETY: We are sure that the elements up to `self.size` are
            // initialized.
            let data =
                unsafe { mem::transmute::<&[MaybeUninit<u8>], &[u8]>(&self.buffer[..self.size]) };
            let data = data.get(index)?;
            Some(data)
        }
    }

    #[inline]
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }
}

impl PartialEq for PacketArray {
    fn eq(&self, other: &Self) -> bool {
        self.meta() == other.meta() && self.data(..) == other.data(..) && self.size == other.size
    }
}

impl Meta {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.port)
    }

    pub fn set_socket_addr(&mut self, socket_addr: &SocketAddr) {
        self.addr = socket_addr.ip();
        self.port = socket_addr.port();
    }

    pub fn set_from_staked_node(&mut self, from_staked_node: bool) {
        self.flags
            .set(PacketFlags::FROM_STAKED_NODE, from_staked_node);
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
    pub fn set_track_performance(&mut self, is_performance_track: bool) {
        self.flags
            .set(PacketFlags::PERF_TRACK_PACKET, is_performance_track);
    }

    #[inline]
    pub fn set_simple_vote(&mut self, is_simple_vote: bool) {
        self.flags.set(PacketFlags::SIMPLE_VOTE_TX, is_simple_vote);
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
    pub fn is_perf_track_packet(&self) -> bool {
        self.flags.contains(PacketFlags::PERF_TRACK_PACKET)
    }

    #[inline]
    pub fn is_from_staked_node(&self) -> bool {
        self.flags.contains(PacketFlags::FROM_STAKED_NODE)
    }
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            flags: PacketFlags::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, bytes::BufMut, std::net::SocketAddrV4};

    #[test]
    fn test_packet_partial_eq() {
        let mut p1 = PacketMut::default();
        let mut p2 = PacketMut::default();

        p1.put_u8(0);
        p2.put_u8(0);

        assert!(p1 == p2);

        let fp1 = p1.clone().freeze();
        let fp2 = p2.clone().freeze();

        assert!(fp1 == fp2);

        p2.buffer_mut()[0] = 4;
        assert!(p1 != p2);
    }

    #[test]
    fn test_freeze() {
        let p = PacketMut::from_data(None, u32::MAX).unwrap();
        let p = p.freeze();
        assert_eq!(
            p.meta(),
            &Meta {
                addr: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                port: 0,
                flags: PacketFlags::empty(),
            }
        );

        let p = PacketMut::from_data(
            Some(&SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(10, 0, 0, 1),
                9001,
            ))),
            u32::MAX,
        )
        .unwrap();
        assert_eq!(
            p.meta(),
            &Meta {
                addr: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                port: 9001,
                flags: PacketFlags::empty(),
            }
        );
    }

    #[test]
    fn test_packet_deserialize_slice() {
        let p = Packet::from_data(None, u32::MAX).unwrap();
        assert_eq!(p.deserialize_slice(..).ok(), Some(u32::MAX));
        assert_eq!(p.deserialize_slice(0..4).ok(), Some(u32::MAX));
        assert_eq!(
            p.deserialize_slice::<u16, _>(0..4)
                .map_err(|e| e.to_string()),
            Err("Slice had bytes remaining after deserialization".to_string()),
        );
        assert_eq!(
            p.deserialize_slice::<u32, _>(0..0)
                .map_err(|e| e.to_string()),
            Err("io error: unexpected end of file".to_string()),
        );
        assert_eq!(
            p.deserialize_slice::<u32, _>(0..1)
                .map_err(|e| e.to_string()),
            Err("io error: unexpected end of file".to_string()),
        );
        assert_eq!(
            p.deserialize_slice::<u32, _>(0..5)
                .map_err(|e| e.to_string()),
            Err("the size limit has been reached".to_string()),
        );
        #[allow(clippy::reversed_empty_ranges)]
        let reversed_empty_range = 4..0;
        assert_eq!(
            p.deserialize_slice::<u32, _>(reversed_empty_range)
                .map_err(|e| e.to_string()),
            Err("the size limit has been reached".to_string()),
        );
        assert_eq!(
            p.deserialize_slice::<u32, _>(4..5)
                .map_err(|e| e.to_string()),
            Err("the size limit has been reached".to_string()),
        );
    }

    #[test]
    fn test_packet_array_from_owned() {
        let p = Packet::from_data(None, u32::MAX).unwrap();
        assert_eq!(p.data(..).unwrap(), u32::MAX.to_ne_bytes());

        let pa: PacketArray = p.into();
        assert_eq!(pa.data(..).unwrap(), u32::MAX.to_ne_bytes());

        let p: Packet = pa.into();
        assert_eq!(p.data(..).unwrap(), u32::MAX.to_ne_bytes());
    }

    #[test]
    fn test_packet_array_from_ref() {
        let p = &Packet::from_data(None, u32::MAX).unwrap();
        assert_eq!(p.data(..).unwrap(), u32::MAX.to_ne_bytes());

        let pa = PacketArray::from_packet(p);
        assert_eq!(pa.data(..).unwrap(), u32::MAX.to_ne_bytes());

        let p: Packet = pa.into();
        assert_eq!(p.data(..).unwrap(), u32::MAX.to_ne_bytes());
    }
}
