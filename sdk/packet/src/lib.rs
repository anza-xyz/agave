//! The definition of a Solana network packet.
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(feature = "bincode")]
use bincode::{Options, Result};
#[cfg(feature = "frozen-abi")]
use solana_frozen_abi_macro::AbiExample;
use {
    bitflags::bitflags,
    std::{
        fmt,
        io::{self, Write},
        mem,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        ptr,
        slice::SliceIndex,
    },
};
#[cfg(feature = "serde")]
use {
    serde_derive::{Deserialize, Serialize},
    serde_with::{serde_as, Bytes},
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
        const TRACER_PACKET  = 0b0001_0000;
        // Previously used - this can now be re-used for something else.
        const UNUSED = 0b0010_0000;
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
    pub size: usize,
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

// serde_as is used as a work around because array isn't supported by serde
// (and serde_bytes).
//
// the root cause is of a historical special handling for [T; 0] in rust's
// `Default` and supposedly mirrored serde's `Serialize` (macro) impls,
// pre-dating stabilized const generics, meaning it'll take long time...:
//   https://github.com/rust-lang/rust/issues/61415
//   https://github.com/rust-lang/rust/issues/88744#issuecomment-1138678928
//
// Due to the nature of the root cause, the current situation is complicated.
// All in all, the serde_as solution is chosen for good perf and low maintenance
// need at the cost of another crate dependency..
//
// For details, please refer to the below various links...
//
// relevant merged/published pr for this serde_as functionality used here:
//   https://github.com/jonasbb/serde_with/pull/277
// open pr at serde_bytes:
//   https://github.com/serde-rs/bytes/pull/28
// open issue at serde:
//   https://github.com/serde-rs/serde/issues/1937
// closed pr at serde (due to the above mentioned [N; 0] issue):
//   https://github.com/serde-rs/serde/pull/1860
// ryoqun's dirty experiments:
//   https://github.com/ryoqun/serde-array-comparisons
//
// We use the cfg_eval crate as advised by the serde_with guide:
// https://docs.rs/serde_with/latest/serde_with/guide/serde_as/index.html#gating-serde_as-on-features
#[cfg_attr(feature = "serde", cfg_eval::cfg_eval, serde_as)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[derive(Clone, Eq)]
#[repr(C)]
pub struct Packet {
    // Bytes past Packet.meta.size are not valid to read from.
    // Use Packet.data(index) to read from the buffer.
    #[cfg_attr(feature = "serde", serde_as(as = "Bytes"))]
    buffer: [u8; PACKET_DATA_SIZE],
    meta: Meta,
}

impl Packet {
    pub fn new(buffer: [u8; PACKET_DATA_SIZE], meta: Meta) -> Self {
        Self { buffer, meta }
    }

    /// Returns an immutable reference to the underlying buffer up to
    /// packet.meta.size. The rest of the buffer is not valid to read from.
    /// packet.data(..) returns packet.buffer.get(..packet.meta.size).
    /// Returns None if the index is invalid or if the packet is already marked
    /// as discard.
    #[inline]
    pub fn data<I>(&self, index: I) -> Option<&<I as SliceIndex<[u8]>>::Output>
    where
        I: SliceIndex<[u8]>,
    {
        // If the packet is marked as discard, it is either invalid or
        // otherwise should be ignored, and so the payload should not be read
        // from.
        if self.meta.discard() {
            None
        } else {
            self.buffer.get(..self.meta.size)?.get(index)
        }
    }

    /// Returns a mutable reference to the entirety of the underlying buffer to
    /// write into. The caller is responsible for updating Packet.meta.size
    /// after writing to the buffer.
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        debug_assert!(!self.meta.discard());
        &mut self.buffer[..]
    }

    #[inline]
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    #[inline]
    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    #[cfg(feature = "bincode")]
    /// Initializes a std::mem::MaybeUninit<Packet> such that the Packet can
    /// be safely extracted via methods such as MaybeUninit::assume_init()
    pub fn init_packet_from_data<T: serde::Serialize>(
        packet: &mut mem::MaybeUninit<Packet>,
        data: &T,
        addr: Option<&SocketAddr>,
    ) -> Result<()> {
        let mut writer = PacketWriter::new_from_uninit_packet(packet);
        bincode::serialize_into(&mut writer, data)?;

        let serialized_size = writer.position();
        let (ip, port) = if let Some(addr) = addr {
            (addr.ip(), addr.port())
        } else {
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        };
        Self::init_packet_meta(
            packet,
            Meta {
                size: serialized_size,
                addr: ip,
                port,
                flags: PacketFlags::empty(),
            },
        );

        Ok(())
    }

    pub fn init_packet_from_bytes(
        packet: &mut mem::MaybeUninit<Packet>,
        bytes: &[u8],
        addr: Option<&SocketAddr>,
    ) -> io::Result<()> {
        let mut writer = PacketWriter::new_from_uninit_packet(packet);
        let num_bytes_written = writer.write(bytes)?;
        debug_assert_eq!(bytes.len(), num_bytes_written);

        let size = writer.position();
        let (ip, port) = if let Some(addr) = addr {
            (addr.ip(), addr.port())
        } else {
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        };
        Self::init_packet_meta(
            packet,
            Meta {
                size,
                addr: ip,
                port,
                flags: PacketFlags::empty(),
            },
        );

        Ok(())
    }

    fn init_packet_meta(packet: &mut mem::MaybeUninit<Packet>, meta: Meta) {
        // SAFETY: Access the field by pointer as creating a reference to
        // and/or within the uninitialized Packet is undefined behavior
        unsafe { ptr::addr_of_mut!((*packet.as_mut_ptr()).meta).write(meta) };
    }

    #[cfg(feature = "bincode")]
    pub fn from_data<T: serde::Serialize>(dest: Option<&SocketAddr>, data: T) -> Result<Self> {
        let mut packet = mem::MaybeUninit::uninit();
        Self::init_packet_from_data(&mut packet, &data, dest)?;
        // SAFETY: init_packet_from_data() just initialized the packet
        unsafe { Ok(packet.assume_init()) }
    }

    #[cfg(feature = "bincode")]
    pub fn populate_packet<T: serde::Serialize>(
        &mut self,
        dest: Option<&SocketAddr>,
        data: &T,
    ) -> Result<()> {
        debug_assert!(!self.meta.discard());
        let mut wr = std::io::Cursor::new(self.buffer_mut());
        bincode::serialize_into(&mut wr, data)?;
        self.meta.size = wr.position() as usize;
        if let Some(dest) = dest {
            self.meta.set_socket_addr(dest);
        }
        Ok(())
    }

    #[cfg(feature = "bincode")]
    pub fn deserialize_slice<T, I>(&self, index: I) -> Result<T>
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

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Packet {{ size: {:?}, addr: {:?} }}",
            self.meta.size,
            self.meta.socket_addr()
        )
    }
}

#[allow(clippy::uninit_assumed_init)]
impl Default for Packet {
    fn default() -> Self {
        let buffer = std::mem::MaybeUninit::<[u8; PACKET_DATA_SIZE]>::uninit();
        Self {
            buffer: unsafe { buffer.assume_init() },
            meta: Meta::default(),
        }
    }
}

impl PartialEq for Packet {
    fn eq(&self, other: &Self) -> bool {
        self.meta() == other.meta() && self.data(..) == other.data(..)
    }
}

/// A custom implementation of io::Write to facilitate safe (non-UB)
/// initialization of a MaybeUninit<Packet>
struct PacketWriter {
    // A pointer to the current write position
    position: *mut u8,
    // The number of remaining bytes that can be written to
    spare_capacity: usize,
}

impl PacketWriter {
    fn new_from_uninit_packet(packet: &mut mem::MaybeUninit<Packet>) -> Self {
        // SAFETY: Access the field by pointer as creating a reference to
        // and/or within the uninitialized Packet is undefined behavior
        let position = unsafe { ptr::addr_of_mut!((*packet.as_mut_ptr()).buffer) as *mut u8 };
        let spare_capacity = PACKET_DATA_SIZE;

        Self {
            position,
            spare_capacity,
        }
    }

    /// The offset of the write pointer within the buffer, which is also the
    /// number of bytes that have been written
    fn position(&self) -> usize {
        PACKET_DATA_SIZE.saturating_sub(self.spare_capacity)
    }
}

impl io::Write for PacketWriter {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.spare_capacity {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }

        // SAFETY: We previously verifed that buf.len() <= self.spare_capacity
        // so this write will not push us past the end of the buffer. Likewise,
        // we can update self.spare_capacity without fear of overflow
        unsafe {
            ptr::copy_nonoverlapping(buf.as_ptr(), self.position, buf.len());
            // Update position and spare_capacity for the next call to write()
            self.position = self.position.add(buf.len());
            self.spare_capacity = self.spare_capacity.saturating_sub(buf.len());
        }

        Ok(buf.len())
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
    pub fn set_flags(&mut self, flags: PacketFlags) {
        self.flags = flags;
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
    pub fn set_tracer(&mut self, is_tracer: bool) {
        self.flags.set(PacketFlags::TRACER_PACKET, is_tracer);
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
    pub fn is_tracer_packet(&self) -> bool {
        self.flags.contains(PacketFlags::TRACER_PACKET)
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
            size: 0,
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            flags: PacketFlags::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Write};

    #[test]
    fn test_deserialize_slice() {
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
    fn test_packet_buffer_writer() {
        let mut packet = mem::MaybeUninit::<Packet>::uninit();
        let mut writer = PacketWriter::new_from_uninit_packet(&mut packet);
        let total_capacity = writer.spare_capacity;
        assert_eq!(total_capacity, PACKET_DATA_SIZE);
        let payload: [u8; PACKET_DATA_SIZE] = std::array::from_fn(|i| i as u8 % 255);

        // Write 1200 bytes (1200 total)
        let num_to_write = PACKET_DATA_SIZE - 32;
        assert_eq!(
            num_to_write,
            writer.write(&payload[..num_to_write]).unwrap()
        );
        assert_eq!(num_to_write, writer.position());
        // Write 28 bytes (1228 total)
        assert_eq!(28, writer.write(&payload[1200..1200 + 28]).unwrap());
        assert_eq!(1200 + 28, writer.position());
        // Attempt to write 5 bytes (1233 total) which exceeds buffer capacity
        assert!(writer
            .write(&payload[1200 + 28 - 1..PACKET_DATA_SIZE])
            .is_err());
        // writer.position() remains unchanged
        assert_eq!(1200 + 28, writer.position());
        // Write 4 bytes (1232 total) to fill buffer
        assert_eq!(4, writer.write(&payload[1200 + 28..]).unwrap());
        assert_eq!(PACKET_DATA_SIZE, writer.position());
        // Writing any amount of bytes will fail on the already full buffer
        assert!(writer.write(&[0]).is_err());
    }
}
