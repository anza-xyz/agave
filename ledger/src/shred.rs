#![cfg_attr(not(feature = "agave-unstable-api"), allow(dead_code))]
//! The `shred` module defines data structures and methods to pull MTU sized data frames from the
//! network. There are two types of shreds: data and coding. Data shreds contain entry information
//! while coding shreds provide redundancy to protect against dropped network packets (erasures).
//!
//! +---------------------------------------------------------------------------------------------+
//! | Data Shred                                                                                  |
//! +---------------------------------------------------------------------------------------------+
//! | common       | data       | payload                                                         |
//! | header       | header     |                                                                 |
//! |+---+---+---  |+---+---+---|+----------------------------------------------------------+----+|
//! || s | s | .   || p | f | s || data (ie ledger entries)                                 | r  ||
//! || i | h | .   || a | l | i ||                                                          | e  ||
//! || g | r | .   || r | a | z || See notes immediately after shred diagrams for an        | s  ||
//! || n | e |     || e | g | e || explanation of the "restricted" section in this payload  | t  ||
//! || a | d |     || n | s |   ||                                                          | r  ||
//! || t |   |     || t |   |   ||                                                          | i  ||
//! || u | t |     ||   |   |   ||                                                          | c  ||
//! || r | y |     || o |   |   ||                                                          | t  ||
//! || e | p |     || f |   |   ||                                                          | e  ||
//! ||   | e |     || f |   |   ||                                                          | d  ||
//! |+---+---+---  |+---+---+---+|----------------------------------------------------------+----+|
//! +---------------------------------------------------------------------------------------------+
//!
//! +---------------------------------------------------------------------------------------------+
//! | Coding Shred                                                                                |
//! +---------------------------------------------------------------------------------------------+
//! | common       | coding     | payload                                                         |
//! | header       | header     |                                                                 |
//! |+---+---+---  |+---+---+---+----------------------------------------------------------------+|
//! || s | s | .   || n | n | p || data (encoded data shred data)                                ||
//! || i | h | .   || u | u | o ||                                                               ||
//! || g | r | .   || m | m | s ||                                                               ||
//! || n | e |     ||   |   | i ||                                                               ||
//! || a | d |     || d | c | t ||                                                               ||
//! || t |   |     ||   |   | i ||                                                               ||
//! || u | t |     || s | s | o ||                                                               ||
//! || r | y |     || h | h | n ||                                                               ||
//! || e | p |     || r | r |   ||                                                               ||
//! ||   | e |     || e | e |   ||                                                               ||
//! ||   |   |     || d | d |   ||                                                               ||
//! |+---+---+---  |+---+---+---+|+--------------------------------------------------------------+|
//! +---------------------------------------------------------------------------------------------+
//!
//! Notes:
//! a) Coding shreds encode entire data shreds: both of the headers AND the payload.
//! b) Coding shreds require their own headers for identification and etc.
//! c) The erasure algorithm requires data shred and coding shred bytestreams to be equal in length.
//!
//! So, given a) - c), we must restrict data shred's payload length such that the entire coding
//! payload can fit into one coding shred / packet.

pub(crate) use self::{merkle_tree::PROOF_ENTRIES_FOR_32_32_BATCH, payload::serde_bytes_payload};
use {
    self::traits::{Shred as _, ShredData as _},
    crate::blockstore::{self},
    assert_matches::debug_assert_matches,
    bitflags::bitflags,
    num_enum::{IntoPrimitive, TryFromPrimitive},
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_entry::entry::{create_ticks, Entry},
    solana_hash::Hash,
    solana_perf::packet::PacketRef,
    solana_pubkey::Pubkey,
    solana_sha256_hasher::hashv,
    solana_signature::{Signature, SIGNATURE_BYTES},
    static_assertions::const_assert_eq,
    std::{fmt::Debug, mem::MaybeUninit},
    thiserror::Error,
    wincode::{
        containers::Pod,
        io::{Reader, Writer},
        SchemaRead, SchemaWrite, TypeMeta,
    },
};
pub use {
    self::{
        merkle::{ShredCode, ShredData},
        payload::Payload,
        stats::{ProcessShredsStats, ShredFetchStats},
    },
    crate::shredder::{ReedSolomonCache, Shredder},
};
#[cfg(any(test, feature = "dev-context-only-utils"))]
use {solana_keypair::Keypair, solana_perf::packet::Packet, solana_signer::Signer};

mod common;
pub mod merkle;
mod merkle_tree;
mod payload;
mod shred_code;
pub(crate) mod shred_data;
mod stats;
mod traits;
pub mod wire;

// Alias for shred::wire::* for the old code.
// New code should use shred::wire::*.
pub mod layout {
    pub use super::wire::*;
}

pub type Nonce = u32;
const_assert_eq!(SIZE_OF_NONCE, 4);
pub const SIZE_OF_NONCE: usize = std::mem::size_of::<Nonce>();

/// The following constants are computed by hand, and hardcoded.
/// `test_shred_constants` ensures that the values are correct.
const SIZE_OF_COMMON_SHRED_HEADER: usize = 83;
pub const SIZE_OF_DATA_SHRED_HEADERS: usize = 88;
const SIZE_OF_CODING_SHRED_HEADERS: usize = 89;
const SIZE_OF_SIGNATURE: usize = SIGNATURE_BYTES;

// Shreds are uniformly split into erasure batches with a "target" number of
// data shreds per each batch as below. The actual number of data shreds in
// each erasure batch depends on the number of shreds obtained from serializing
// a &[Entry].
pub const DATA_SHREDS_PER_FEC_BLOCK: usize = 32;
pub const CODING_SHREDS_PER_FEC_BLOCK: usize = 32;
pub const SHREDS_PER_FEC_BLOCK: usize = DATA_SHREDS_PER_FEC_BLOCK + CODING_SHREDS_PER_FEC_BLOCK;

/// An upper bound on maximum number of data shreds we can handle in a slot
/// 32K shreds would allow ~320K peak TPS
/// (32K shreds per slot * 4 TX per shred * 2.5 slots per sec)
pub const MAX_DATA_SHREDS_PER_SLOT: usize = 32_768;
pub const MAX_CODE_SHREDS_PER_SLOT: usize = MAX_DATA_SHREDS_PER_SLOT;

// Statically compute the typical data batch size assuming:
// 1. 32:32 erasure coding batch
// 2. Merkles are chained
// 3. No retransmit signature (only included for last batch)
pub const fn get_data_shred_bytes_per_batch_typical() -> u64 {
    let capacity = match merkle::ShredData::const_capacity(PROOF_ENTRIES_FOR_32_32_BATCH, false) {
        Ok(v) => v,
        Err(_proof_size) => {
            panic!("this is unreachable");
        }
    };
    (DATA_SHREDS_PER_FEC_BLOCK * capacity) as u64
}

// LAST_SHRED_IN_SLOT also implies DATA_COMPLETE_SHRED.
// So it cannot be LAST_SHRED_IN_SLOT if not also DATA_COMPLETE_SHRED.
bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
    pub struct ShredFlags:u8 {
        const SHRED_TICK_REFERENCE_MASK = 0b0011_1111;
        const DATA_COMPLETE_SHRED       = 0b0100_0000;
        const LAST_SHRED_IN_SLOT        = 0b1100_0000;
    }
}

impl ShredFlags {
    /// Creates a new ShredFlags from the given reference_tick
    ///
    /// SHRED_TICK_REFERENCE_MASK is comprised of only six bits whereas the
    /// reference_tick has 8 bits (u8). The reference_tick bits will saturate
    /// in the event that reference_tick > SHRED_TICK_REFERENCE_MASK
    pub(crate) fn from_reference_tick(reference_tick: u8) -> Self {
        Self::from_bits_retain(Self::SHRED_TICK_REFERENCE_MASK.bits().min(reference_tick))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    WincodeRead(#[from] wincode::ReadError),
    #[error(transparent)]
    WincodeWrite(#[from] wincode::WriteError),
    #[error(transparent)]
    Erasure(#[from] reed_solomon_erasure::Error),
    #[error("Invalid data size: {size}, payload: {payload}")]
    InvalidDataSize { size: u16, payload: usize },
    #[error("Invalid deshred set")]
    InvalidDeshredSet,
    #[error("Invalid erasure config")]
    InvalidErasureConfig,
    #[error("Invalid erasure shard index: {0:?}")]
    InvalidErasureShardIndex(/*headers:*/ Box<dyn Debug + Send>),
    #[error("Invalid merkle proof")]
    InvalidMerkleProof,
    #[error("Invalid Merkle root")]
    InvalidMerkleRoot,
    #[error("Invalid num coding shreds: {0}")]
    InvalidNumCodingShreds(u16),
    #[error("Invalid parent_offset: {parent_offset}, slot: {slot}")]
    InvalidParentOffset { slot: Slot, parent_offset: u16 },
    #[error("Invalid parent slot: {parent_slot}, slot: {slot}")]
    InvalidParentSlot { slot: Slot, parent_slot: Slot },
    #[error("Invalid payload size: {0}")]
    InvalidPayloadSize(/*payload size:*/ usize),
    #[error("Invalid proof size: {0}")]
    InvalidProofSize(/*proof_size:*/ u8),
    #[error("Invalid recovered shred")]
    InvalidRecoveredShred,
    #[error("Invalid shard size: {0}")]
    InvalidShardSize(/*shard_size:*/ usize),
    #[error("Invalid shred flags: {0}")]
    InvalidShredFlags(u8),
    #[error("Invalid {0:?} shred index: {1}")]
    InvalidShredIndex(ShredType, /*shred index:*/ u32),
    #[error("Invalid shred type")]
    InvalidShredType,
    #[error("Invalid shred variant")]
    InvalidShredVariant,
    #[error("Invalid packet size, could not get the shred")]
    InvalidPacketSize,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Unknown proof size")]
    UnknownProofSize,
    #[error("Empty shreds list")]
    EmptyIterator,
}

#[repr(u8)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Deserialize,
    IntoPrimitive,
    Serialize,
    TryFromPrimitive,
    SchemaWrite,
    SchemaRead,
)]
#[serde(into = "u8", try_from = "u8")]
#[wincode(tag_encoding = "u8")]
pub enum ShredType {
    #[wincode(tag = 0b1010_0101)]
    Data = 0b1010_0101,
    #[wincode(tag = 0b0101_1010)]
    Code = 0b0101_1010,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(into = "u8", try_from = "u8")]
enum ShredVariant {
    // proof_size is the number of Merkle proof entries, and is encoded in the
    // lowest 4 bits of the binary representation. The first 4 bits identify
    // the shred variant:
    //   0b0110_????  MerkleCode chained
    //   0b0111_????  MerkleCode chained resigned
    //   0b1001_????  MerkleData chained
    //   0b1011_????  MerkleData chained resigned
    MerkleCode { proof_size: u8, resigned: bool }, // 0b01??_????
    MerkleData { proof_size: u8, resigned: bool }, // 0b10??_????
}

/// A common header that is present in data and code shred headers
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, SchemaRead, SchemaWrite)]
struct ShredCommonHeader {
    #[wincode(with = "Pod<_>")]
    signature: Signature,
    shred_variant: ShredVariant,
    slot: Slot,
    index: u32,
    version: u16,
    fec_set_index: u32,
}

/// The data shred header has parent offset and flags
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, SchemaRead, SchemaWrite)]
struct DataShredHeader {
    parent_offset: u16,
    #[wincode(with = "Pod<_>")]
    flags: ShredFlags,
    size: u16, // common shred header + data shred header + data
}

/// The coding shred header has FEC information
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, SchemaRead, SchemaWrite)]
struct CodingShredHeader {
    num_data_shreds: u16,
    num_coding_shreds: u16,
    position: u16, // [0..num_coding_shreds)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shred {
    ShredCode(ShredCode),
    ShredData(merkle::ShredData),
}

/// Tuple which uniquely identifies a shred should it exists.
#[derive(Clone, Copy, Eq, Debug, Hash, PartialEq)]
pub struct ShredId(Slot, /*shred index:*/ u32, ShredType);

impl ShredId {
    #[inline]
    pub fn new(slot: Slot, index: u32, shred_type: ShredType) -> ShredId {
        ShredId(slot, index, shred_type)
    }

    #[inline]
    pub fn slot(&self) -> Slot {
        self.0
    }

    #[inline]
    pub fn index(&self) -> u32 {
        self.1
    }

    #[inline]
    pub fn shred_type(&self) -> ShredType {
        self.2
    }

    #[inline]
    pub(crate) fn unpack(&self) -> (Slot, /*shred index:*/ u32, ShredType) {
        (self.0, self.1, self.2)
    }

    pub fn seed(&self, leader: &Pubkey) -> [u8; 32] {
        let ShredId(slot, index, shred_type) = self;
        hashv(&[
            &slot.to_le_bytes(),
            &u8::from(*shred_type).to_le_bytes(),
            &index.to_le_bytes(),
            AsRef::<[u8]>::as_ref(leader),
        ])
        .to_bytes()
    }
}

/// Tuple which identifies erasure coding set that the shred belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub(crate) struct ErasureSetId(Slot, /*fec_set_index:*/ u32);

impl ErasureSetId {
    pub(crate) fn new(slot: Slot, fec_set_index: u32) -> Self {
        Self(slot, fec_set_index)
    }

    pub(crate) fn slot(&self) -> Slot {
        self.0
    }

    // Storage key for ErasureMeta and MerkleRootMeta in blockstore db.
    // Note: ErasureMeta column uses u64 so this will need to be typecast
    pub(crate) fn store_key(&self) -> (Slot, /*fec_set_index:*/ u32) {
        (self.0, self.1)
    }
}

/// To be used with the [`Shred`] enum.
///
/// Writes a function implementation that forwards the invocation to an identically defined function
/// in one of the two enum branches.
///
/// Due to an inability of a macro to match on the `self` shorthand syntax, this macro has 3
/// branches.  But they are only different in the `self` argument matching.  Make sure to keep the
/// identical otherwise.
macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(&mut self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&mut self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    }
}

use dispatch;

impl Shred {
    dispatch!(fn common_header(&self) -> &ShredCommonHeader);
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    dispatch!(fn set_signature(&mut self, signature: Signature));
    dispatch!(fn signed_data(&self) -> Result<Hash, Error>);

    dispatch!(pub fn chained_merkle_root(&self) -> Result<Hash, Error>);
    dispatch!(pub(crate) fn retransmitter_signature(&self) -> Result<Signature, Error>);
    dispatch!(pub fn retransmitter_signature_offset(&self) -> Result<usize, Error>);

    dispatch!(pub fn into_payload(self) -> Payload);
    dispatch!(pub fn merkle_root(&self) -> Result<Hash, Error>);
    dispatch!(pub fn payload(&self) -> &Payload);
    dispatch!(pub fn sanitize(&self) -> Result<(), Error>);

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn copy_to_packet(&self, packet: &mut Packet) {
        let payload = self.payload();
        let size = payload.len();
        packet.buffer_mut()[..size].copy_from_slice(&payload[..]);
        packet.meta_mut().size = size;
    }

    pub fn new_from_serialized_shred<T>(shred: T) -> Result<Self, Error>
    where
        T: AsRef<[u8]> + Into<Payload>,
        Payload: From<T>,
    {
        Ok(match layout::get_shred_variant(shred.as_ref())? {
            ShredVariant::MerkleCode { .. } => {
                let shred = merkle::ShredCode::from_payload(shred)?;
                Self::ShredCode(shred)
            }
            ShredVariant::MerkleData { .. } => {
                let shred = merkle::ShredData::from_payload(shred)?;
                Self::ShredData(shred)
            }
        })
    }

    /// Unique identifier for each shred.
    pub fn id(&self) -> ShredId {
        ShredId(self.slot(), self.index(), self.shred_type())
    }

    pub fn slot(&self) -> Slot {
        self.common_header().slot
    }

    pub fn parent(&self) -> Result<Slot, Error> {
        match self {
            Self::ShredCode(_) => Err(Error::InvalidShredType),
            Self::ShredData(shred) => shred.parent(),
        }
    }

    pub fn index(&self) -> u32 {
        self.common_header().index
    }

    pub fn fec_set_index(&self) -> u32 {
        self.common_header().fec_set_index
    }

    pub(crate) fn first_coding_index(&self) -> Option<u32> {
        match self {
            Self::ShredCode(shred) => shred.first_coding_index(),
            Self::ShredData(_) => None,
        }
    }

    pub fn version(&self) -> u16 {
        self.common_header().version
    }

    // Identifier for the erasure coding set that the shred belongs to.
    pub(crate) fn erasure_set(&self) -> ErasureSetId {
        ErasureSetId(self.slot(), self.fec_set_index())
    }

    pub fn signature(&self) -> &Signature {
        &self.common_header().signature
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn sign(&mut self, keypair: &Keypair) {
        let data = self.signed_data().unwrap();
        let signature = keypair.sign_message(data.as_ref());
        self.set_signature(signature);
    }

    #[inline]
    pub fn shred_type(&self) -> ShredType {
        ShredType::from(self.common_header().shred_variant)
    }

    #[inline]
    pub fn is_data(&self) -> bool {
        self.shred_type() == ShredType::Data
    }

    #[inline]
    pub fn is_code(&self) -> bool {
        self.shred_type() == ShredType::Code
    }

    pub fn last_in_slot(&self) -> bool {
        match self {
            Self::ShredCode(_) => false,
            Self::ShredData(shred) => shred.last_in_slot(),
        }
    }

    pub fn data_complete(&self) -> bool {
        match self {
            Self::ShredCode(_) => false,
            Self::ShredData(shred) => shred.data_complete(),
        }
    }

    pub(crate) fn reference_tick(&self) -> u8 {
        match self {
            Self::ShredCode(_) => ShredFlags::SHRED_TICK_REFERENCE_MASK.bits(),
            Self::ShredData(shred) => shred.reference_tick(),
        }
    }

    #[must_use]
    pub fn verify(&self, pubkey: &Pubkey) -> bool {
        match self.signed_data() {
            Ok(data) => self.signature().verify(pubkey.as_ref(), data.as_ref()),
            Err(_) => false,
        }
    }

    // Returns true if the erasure coding of the two shreds mismatch.
    pub(crate) fn erasure_mismatch(&self, other: &Self) -> Result<bool, Error> {
        match (self, other) {
            (Self::ShredCode(shred), Self::ShredCode(other)) => Ok(shred.erasure_mismatch(other)),
            _ => Err(Error::InvalidShredType),
        }
    }

    pub(crate) fn num_data_shreds(&self) -> Result<u16, Error> {
        match self {
            Self::ShredCode(shred) => Ok(shred.num_data_shreds()),
            Self::ShredData(_) => Err(Error::InvalidShredType),
        }
    }

    pub(crate) fn num_coding_shreds(&self) -> Result<u16, Error> {
        match self {
            Self::ShredCode(shred) => Ok(shred.num_coding_shreds()),
            Self::ShredData(_) => Err(Error::InvalidShredType),
        }
    }

    /// Returns true if the other shred has the same ShredId, i.e. (slot, index,
    /// shred-type), but different payload.
    /// Retransmitter's signature is ignored when comparing payloads.
    pub fn is_shred_duplicate(&self, other: &Shred) -> bool {
        if self.id() != other.id() {
            return false;
        }
        fn get_payload(shred: &Shred) -> &[u8] {
            let Ok(offset) = shred.retransmitter_signature_offset() else {
                return shred.payload();
            };
            // Assert that the retransmitter's signature is at the very end of
            // the shred payload.
            debug_assert_eq!(offset + SIZE_OF_SIGNATURE, shred.payload().len());
            shred
                .payload()
                .get(..offset)
                .unwrap_or_else(|| shred.payload())
        }
        get_payload(self) != get_payload(other)
    }
}

impl From<merkle::Shred> for Shred {
    fn from(shred: merkle::Shred) -> Self {
        match shred {
            merkle::Shred::ShredCode(shred) => Self::ShredCode(shred),
            merkle::Shred::ShredData(shred) => Self::ShredData(shred),
        }
    }
}

impl TryFrom<Shred> for merkle::Shred {
    type Error = Error;

    fn try_from(shred: Shred) -> Result<Self, Self::Error> {
        match shred {
            Shred::ShredCode(shred) => Ok(Self::ShredCode(shred)),
            Shred::ShredData(shred) => Ok(Self::ShredData(shred)),
        }
    }
}

impl From<ShredVariant> for ShredType {
    #[inline]
    fn from(shred_variant: ShredVariant) -> Self {
        match shred_variant {
            ShredVariant::MerkleCode { .. } => ShredType::Code,
            ShredVariant::MerkleData { .. } => ShredType::Data,
        }
    }
}

impl From<ShredVariant> for u8 {
    #[inline]
    fn from(shred_variant: ShredVariant) -> u8 {
        match shred_variant {
            ShredVariant::MerkleCode {
                proof_size,
                resigned: false,
            } => proof_size | 0x60,
            ShredVariant::MerkleCode {
                proof_size,
                resigned: true,
            } => proof_size | 0x70,
            ShredVariant::MerkleData {
                proof_size,
                resigned: false,
            } => proof_size | 0x90,
            ShredVariant::MerkleData {
                proof_size,
                resigned: true,
            } => proof_size | 0xb0,
        }
    }
}

impl TryFrom<u8> for ShredVariant {
    type Error = Error;
    #[inline]
    fn try_from(shred_variant: u8) -> Result<Self, Self::Error> {
        if shred_variant == u8::from(ShredType::Code) || shred_variant == u8::from(ShredType::Data)
        {
            Err(Error::InvalidShredVariant)
        } else {
            let proof_size = shred_variant & 0x0F;
            match shred_variant & 0xF0 {
                0x60 => Ok(ShredVariant::MerkleCode {
                    proof_size,
                    resigned: false,
                }),
                0x70 => Ok(ShredVariant::MerkleCode {
                    proof_size,
                    resigned: true,
                }),
                0x90 => Ok(ShredVariant::MerkleData {
                    proof_size,
                    resigned: false,
                }),
                0xb0 => Ok(ShredVariant::MerkleData {
                    proof_size,
                    resigned: true,
                }),
                _ => Err(Error::InvalidShredVariant),
            }
        }
    }
}

impl SchemaWrite for ShredVariant {
    type Src = Self;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 1,
        zero_copy: false,
    };

    fn size_of(_src: &Self::Src) -> wincode::WriteResult<usize> {
        Ok(1)
    }

    fn write(writer: &mut impl Writer, src: &Self::Src) -> wincode::WriteResult<()> {
        let repr: u8 = (*src).into();
        u8::write(writer, &repr)
    }
}

impl<'a> SchemaRead<'a> for ShredVariant {
    type Dst = Self;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 1,
        zero_copy: false,
    };

    fn read(
        reader: &mut impl Reader<'a>,
        dst: &mut MaybeUninit<Self::Dst>,
    ) -> wincode::ReadResult<()> {
        let repr = u8::get(reader)?;
        let value = Self::try_from(repr)
            .map_err(|_| wincode::ReadError::InvalidTagEncoding(repr as usize))?;
        dst.write(value);
        Ok(())
    }
}

pub fn recover<T: IntoIterator<Item = Shred>>(
    shreds: T,
    reed_solomon_cache: &ReedSolomonCache,
) -> Result<impl Iterator<Item = Result<Shred, Error>> + use<T>, Error> {
    let shreds = shreds
        .into_iter()
        .map(|shred| {
            debug_assert_matches!(
                shred.common_header().shred_variant,
                ShredVariant::MerkleCode { .. } | ShredVariant::MerkleData { .. }
            );
            merkle::Shred::try_from(shred)
        })
        .collect::<Result<_, _>>()?;
    // With Merkle shreds, leader signs the Merkle root of the erasure batch
    // and all shreds within the same erasure batch have the same signature.
    // For recovered shreds, the (unique) signature is copied from shreds which
    // were received from turbine (or repair) and are already sig-verified.
    // The same signature also verifies for recovered shreds because when
    // reconstructing the Merkle tree for the erasure batch, we will obtain the
    // same Merkle root.
    let shreds = merkle::recover(shreds, reed_solomon_cache)?;
    Ok(shreds.map(|shred| shred.map(Shred::from)))
}

// Accepts shreds in the slot range [root + 1, max_slot].
#[must_use]
pub fn should_discard_shred<'a, P>(
    packet: P,
    root: Slot,
    max_slot: Slot,
    shred_version: u16,
    enforce_fixed_fec_set: impl Fn(Slot) -> bool,
    discard_unexpected_data_complete_shreds: impl Fn(Slot) -> bool,
    stats: &mut ShredFetchStats,
) -> bool
where
    P: Into<PacketRef<'a>>,
{
    debug_assert!(root < max_slot);
    let Some(shred) = layout::get_shred(packet) else {
        stats.index_overrun += 1;
        return true;
    };
    match layout::get_version(shred) {
        None => {
            stats.index_overrun += 1;
            return true;
        }
        Some(version) => {
            if version != shred_version {
                stats.shred_version_mismatch += 1;
                return true;
            }
        }
    }
    let Ok(shred_variant) = layout::get_shred_variant(shred) else {
        stats.bad_shred_type += 1;
        return true;
    };
    let slot = match layout::get_slot(shred) {
        Some(slot) => {
            if slot > max_slot {
                stats.slot_out_of_range += 1;
                return true;
            }
            slot
        }
        None => {
            stats.slot_bad_deserialize += 1;
            return true;
        }
    };
    let Some(index) = layout::get_index(shred) else {
        stats.index_bad_deserialize += 1;
        return true;
    };
    let Some(fec_set_index) = layout::get_fec_set_index(shred) else {
        stats.fec_set_index_bad_deserialize += 1;
        return true;
    };

    match ShredType::from(shred_variant) {
        ShredType::Code => {
            if index >= MAX_CODE_SHREDS_PER_SLOT as u32 {
                stats.index_out_of_bounds += 1;
                return true;
            }
            if slot <= root {
                stats.slot_out_of_range += 1;
                return true;
            }

            let Ok(erasure_config) = layout::get_erasure_config(shred) else {
                stats.erasure_config_bad_deserialize += 1;
                return true;
            };

            if !erasure_config.is_fixed() {
                stats.misaligned_erasure_config += 1;
                if enforce_fixed_fec_set(slot) {
                    return true;
                }
            }
        }
        ShredType::Data => {
            if index >= MAX_DATA_SHREDS_PER_SLOT as u32 {
                stats.index_out_of_bounds += 1;
                return true;
            }
            let Some(parent_offset) = layout::get_parent_offset(shred) else {
                stats.bad_parent_offset += 1;
                return true;
            };
            let Some(parent) = slot.checked_sub(Slot::from(parent_offset)) else {
                stats.bad_parent_offset += 1;
                return true;
            };
            if !blockstore::verify_shred_slots(slot, parent, root) {
                stats.slot_out_of_range += 1;
                return true;
            }

            let Ok(shred_flags) = layout::get_flags(shred) else {
                stats.shred_flags_bad_deserialize += 1;
                return true;
            };

            if shred_flags.contains(ShredFlags::DATA_COMPLETE_SHRED)
                && index != fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32 - 1
            {
                stats.unexpected_data_complete_shred += 1;

                if enforce_fixed_fec_set(slot) && discard_unexpected_data_complete_shreds(slot) {
                    return true;
                }
            }

            if shred_flags.contains(ShredFlags::LAST_SHRED_IN_SLOT)
                && !check_last_data_shred_index(index)
            {
                stats.misaligned_last_data_index += 1;
                if enforce_fixed_fec_set(slot) {
                    return true;
                }
            }
        }
    }

    if !check_fixed_fec_set(index, fec_set_index) {
        stats.misaligned_fec_set += 1;
        if enforce_fixed_fec_set(slot) {
            return true;
        }
    }

    match shred_variant {
        ShredVariant::MerkleCode { .. } => {
            stats.num_shreds_merkle_code_chained =
                stats.num_shreds_merkle_code_chained.saturating_add(1);
        }
        ShredVariant::MerkleData { .. } => {
            stats.num_shreds_merkle_data_chained =
                stats.num_shreds_merkle_data_chained.saturating_add(1);
        }
    }
    false
}

/// Returns true if `index` and `fec_set_index` are valid under the assumption that
/// all erasure sets contain exactly `DATA_SHREDS_PER_FEC_BLOCK` data and coding shreds:
/// - `index` is between `fec_set_index` and `fec_set_index + DATA_SHREDS_PER_FEC_BLOCK`
/// - `fec_set_index` is a multiple of `DATA_SHREDS_PER_FEC_BLOCK`
fn check_fixed_fec_set(index: u32, fec_set_index: u32) -> bool {
    index >= fec_set_index
        && index < fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32
        && fec_set_index.is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
}

/// Returns true if `index` of the last data shred is valid under the assumption that
/// all erasure sets contain exactly `DATA_SHREDS_PER_FEC_BLOCK` data and coding shreds:
/// - `index + 1` must be a multiple of `DATA_SHREDS_PER_FEC_BLOCK`
///
/// Note: this check is critical to verify that the last fec set is sufficiently sized.
/// This currently is checked post insert in `Blockstore::check_last_fec_set`, but in the
/// future it can be solely checked during ingest
fn check_last_data_shred_index(index: u32) -> bool {
    (index + 1).is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
}

pub fn max_ticks_per_n_shreds(num_shreds: u64, shred_data_size: Option<usize>) -> u64 {
    let ticks = create_ticks(1, 0, Hash::default());
    max_entries_per_n_shred(&ticks[0], num_shreds, shred_data_size)
}

// This is used in the integration tests for shredding.
#[cfg(feature = "dev-context-only-utils")]
pub fn max_entries_per_n_shred_last_or_not(
    entry: &Entry,
    num_shreds: u64,
    is_last_in_slot: bool,
) -> u64 {
    let vec_size = wincode::serialized_size(&vec![entry]).unwrap();
    let entry_size = wincode::serialized_size(entry).unwrap();
    let count_size = vec_size - entry_size;

    // Default 32:32 erasure batches yields 64 shreds; log2(64) = 6.
    if !is_last_in_slot {
        // all shreds are unsigned
        let shred_data_size =
            ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ false).unwrap() as u64;
        (shred_data_size * num_shreds - count_size) / entry_size
    } else {
        // last FEC SET is signed, all others are unsigned
        let shred_data_size_unsigned =
            ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ false).unwrap() as u64;
        let shred_data_size_signed =
            ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ true).unwrap() as u64;
        let shreds_per_fec_block = SHREDS_PER_FEC_BLOCK as u64;
        (shred_data_size_unsigned * (num_shreds - shreds_per_fec_block)
            + shred_data_size_signed * shreds_per_fec_block
            - count_size)
            / entry_size
    }
}

pub fn max_entries_per_n_shred(
    entry: &Entry,
    num_shreds: u64,
    shred_data_size: Option<usize>,
) -> u64 {
    // Default 32:32 erasure batches yields 64 shreds; log2(64) = 6.
    let data_buffer_size = ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ true).unwrap();
    let shred_data_size = shred_data_size.unwrap_or(data_buffer_size) as u64;
    let vec_size = wincode::serialized_size(&vec![entry]).unwrap();
    let entry_size = wincode::serialized_size(entry).unwrap();
    let count_size = vec_size - entry_size;

    (shred_data_size * num_shreds - count_size) / entry_size
}

#[cfg(feature = "dev-context-only-utils")]
pub fn verify_test_data_shred(
    shred: &Shred,
    index: u32,
    slot: Slot,
    parent: Slot,
    pk: &Pubkey,
    verify: bool,
    is_last_in_slot: bool,
    is_last_data: bool,
) {
    shred.sanitize().unwrap();
    assert!(shred.is_data());
    assert_eq!(shred.index(), index);
    assert_eq!(shred.slot(), slot);
    assert_eq!(shred.parent().unwrap(), parent);
    assert_eq!(verify, shred.verify(pk));
    if is_last_in_slot {
        assert!(shred.last_in_slot());
    } else {
        assert!(!shred.last_in_slot());
    }
    if is_last_data {
        assert!(shred.data_complete());
    } else {
        assert!(!shred.data_complete());
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::Rng,
        rand_chacha::{rand_core::SeedableRng, ChaChaRng},
        rayon::ThreadPoolBuilder,
        solana_keypair::keypair_from_seed,
        std::io::{Cursor, Seek, SeekFrom, Write},
        test_case::test_case,
    };

    const SIZE_OF_SHRED_INDEX: usize = 4;
    const SIZE_OF_SHRED_SLOT: usize = 8;
    const SIZE_OF_SHRED_VARIANT: usize = 1;
    const SIZE_OF_VERSION: usize = 2;
    const SIZE_OF_FEC_SET_INDEX: usize = 4;
    const SIZE_OF_PARENT_OFFSET: usize = 2;

    const OFFSET_OF_SHRED_VARIANT: usize = SIZE_OF_SIGNATURE;
    const OFFSET_OF_SHRED_SLOT: usize = SIZE_OF_SIGNATURE + SIZE_OF_SHRED_VARIANT;
    const OFFSET_OF_SHRED_INDEX: usize = OFFSET_OF_SHRED_SLOT + SIZE_OF_SHRED_SLOT;
    const OFFSET_OF_FEC_SET_INDEX: usize =
        OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX + SIZE_OF_VERSION;
    const OFFSET_OF_NUM_DATA: usize = OFFSET_OF_FEC_SET_INDEX + SIZE_OF_FEC_SET_INDEX;

    const OFFSET_OF_PARENT_OFFSET: usize = OFFSET_OF_FEC_SET_INDEX + SIZE_OF_FEC_SET_INDEX;
    const OFFSET_OF_SHRED_FLAGS: usize = OFFSET_OF_PARENT_OFFSET + SIZE_OF_PARENT_OFFSET;

    pub(super) fn make_merkle_shreds_for_tests<R: Rng>(
        rng: &mut R,
        slot: Slot,
        data_size: usize,
        is_last_in_slot: bool,
    ) -> Result<Vec<merkle::Shred>, Error> {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let chained_merkle_root = Hash::new_from_array(rng.random());
        let parent_offset = rng.random_range(1..=u16::try_from(slot).unwrap_or(u16::MAX));
        let parent_slot = slot.checked_sub(u64::from(parent_offset)).unwrap();
        let mut data = vec![0u8; data_size];
        let fec_set_index = rng.random_range(0..21) * DATA_SHREDS_PER_FEC_BLOCK as u32;
        rng.fill(&mut data[..]);
        merkle::make_shreds_from_data(
            &thread_pool,
            &Keypair::new(),
            chained_merkle_root,
            &data[..],
            slot,
            parent_slot,
            rng.random(),            // shred_version
            rng.random_range(1..64), // reference_tick
            is_last_in_slot,
            fec_set_index, // next_shred_index
            fec_set_index, // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
    }

    #[test]
    fn test_shred_constants() {
        use wincode::Serialize as _;

        let common_header = ShredCommonHeader {
            signature: Signature::default(),
            shred_variant: ShredVariant::MerkleCode {
                proof_size: 0,
                resigned: false,
            },
            slot: Slot::MAX,
            index: u32::MAX,
            version: u16::MAX,
            fec_set_index: u32::MAX,
        };
        let data_shred_header = DataShredHeader {
            parent_offset: u16::MAX,
            flags: ShredFlags::all(),
            size: u16::MAX,
        };
        let coding_shred_header = CodingShredHeader {
            num_data_shreds: u16::MAX,
            num_coding_shreds: u16::MAX,
            position: u16::MAX,
        };
        assert_eq!(
            SIZE_OF_COMMON_SHRED_HEADER,
            wincode::serialized_size(&common_header).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_CODING_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            wincode::serialized_size(&coding_shred_header).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_DATA_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            wincode::serialized_size(&data_shred_header).unwrap() as usize
        );
        let data_shred_header_with_size = DataShredHeader {
            size: 1000,
            ..data_shred_header
        };
        assert_eq!(
            SIZE_OF_DATA_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            wincode::serialized_size(&data_shred_header_with_size).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SIGNATURE,
            Pod::<Signature>::serialized_size(&Signature::default()).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_VARIANT,
            wincode::serialized_size(&ShredVariant::MerkleCode {
                proof_size: 15,
                resigned: true
            })
            .unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_SLOT,
            wincode::serialized_size(&Slot::default()).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_INDEX,
            wincode::serialized_size(&common_header.index).unwrap() as usize
        );
    }

    #[test]
    fn test_shred_flags_reference_tick_saturates() {
        const MAX_REFERENCE_TICK: u8 = ShredFlags::SHRED_TICK_REFERENCE_MASK.bits();
        for tick in 0..=u8::MAX {
            let flags = ShredFlags::from_reference_tick(tick);
            assert_eq!(flags.bits(), tick.min(MAX_REFERENCE_TICK));
        }
    }

    #[test]
    fn test_invalid_parent_offset() {
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(10, &keypair);
        assert_matches!(shred.parent(), Ok(9));
        let mut packet = Packet::default();
        shred.copy_to_packet(&mut packet);
        wire::corrupt_and_set_parent_offset(packet.buffer_mut(), 1000);
        let shred_res = Shred::new_from_serialized_shred(packet.data(..).unwrap().to_vec());
        assert_matches!(
            shred_res,
            Err(Error::InvalidParentOffset {
                slot: 10,
                parent_offset: 1000
            })
        );
    }

    #[test_case(true ; "last_in_slot")]
    #[test_case(false ; "not_last_in_slot")]
    fn test_should_discard_shred(is_last_in_slot: bool) {
        agave_logger::setup();
        let mut rng = rand::rng();
        let slot = 18_291;
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            is_last_in_slot,
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);

        assert_matches!(shreds[0].shred_type(), ShredType::Data);
        let parent_slot = shreds[0].parent().unwrap();
        let shred_version = shreds[0].common_header().version;

        let root = rng.random_range(0..parent_slot);
        let max_slot = slot + rng.random_range(1..65536);
        let mut packet = Packet::default();

        // Data shred sanity checks!
        {
            let shred = shreds.first().unwrap();
            assert_eq!(shred.shred_type(), ShredType::Data);
            shred.copy_to_packet(&mut packet);
            let mut stats = ShredFetchStats::default();
            assert!(!should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
        }
        {
            let mut packet = packet.clone();
            let mut stats = ShredFetchStats::default();
            packet.meta_mut().size = OFFSET_OF_SHRED_VARIANT;
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_overrun, 1);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX;
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_overrun, 2);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + 1;
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_overrun, 3);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX - 1;
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_overrun, 4);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX + 2;
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_overrun, 5);
        }
        {
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version.wrapping_add(1),
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.shred_version_mismatch, 1);
        }
        {
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                parent_slot + 1, // root
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.slot_out_of_range, 1);
        }
        {
            let parent_offset = 0u16;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor.seek(SeekFrom::Start(83)).unwrap();
                cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            }
            assert_eq!(
                layout::get_parent_offset(packet.data(..).unwrap()),
                Some(parent_offset)
            );
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.slot_out_of_range, 1);
        }
        {
            let parent_offset = u16::try_from(slot + 1).unwrap();
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor.seek(SeekFrom::Start(83)).unwrap();
                cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            }
            assert_eq!(
                layout::get_parent_offset(packet.data(..).unwrap()),
                Some(parent_offset)
            );
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.bad_parent_offset, 1);
        }
        {
            let index = u32::MAX - 10;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&index.to_le_bytes()).unwrap();
            }
            assert_eq!(layout::get_index(packet.data(..).unwrap()), Some(index));
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| true,
                &mut stats
            ));
            assert_eq!(stats.index_out_of_bounds, 1);
        }

        // Coding shred sanity checks!
        {
            let shred = shreds.last().unwrap();
            assert_eq!(shred.shred_type(), ShredType::Code);
            shreds.last().unwrap().copy_to_packet(&mut packet);
            let mut stats = ShredFetchStats::default();
            assert!(!should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
        }
        {
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version.wrapping_add(1),
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.shred_version_mismatch, 1);
        }
        {
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                slot, // root
                max_slot,
                shred_version,
                |_| true,
                |_| true,
                &mut stats
            ));
            assert_eq!(stats.slot_out_of_range, 1);
        }
        {
            let index = u32::try_from(MAX_CODE_SHREDS_PER_SLOT).unwrap();
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&index.to_le_bytes()).unwrap();
            }
            assert_eq!(layout::get_index(packet.data(..).unwrap()), Some(index));
            let mut stats = ShredFetchStats::default();
            assert!(should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| true,
                |_| false,
                &mut stats
            ));
            assert_eq!(stats.index_out_of_bounds, 1);
        }
    }

    #[test_case(true; "enforce_fixed_fec_set")]
    #[test_case(false ; "do_not_enforce_fixed_fec_set")]
    fn test_should_discard_shred_fec_set_checks(enforce_fixed_fec_set: bool) {
        agave_logger::setup();
        let mut rng = rand::rng();
        let slot = 18_291;
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            false,    // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);

        assert_matches!(shreds[0].shred_type(), ShredType::Data);
        let parent_slot = shreds[0].parent().unwrap();
        let shred_version = shreds[0].common_header().version;
        let root = rng.random_range(0..parent_slot);
        let max_slot = slot + rng.random_range(1..65536);

        // fec_set_index not multiple of 32
        {
            let mut packet = Packet::default();
            shreds[0].copy_to_packet(&mut packet);

            let bad_fec_set_index = 5u32;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                    .unwrap();
                cursor.write_all(&bad_fec_set_index.to_le_bytes()).unwrap();
            }

            let mut stats = ShredFetchStats::default();
            let should_discard = should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| enforce_fixed_fec_set,
                |_| false,
                &mut stats,
            );
            assert_eq!(should_discard, enforce_fixed_fec_set);
            assert_eq!(stats.misaligned_fec_set, 1);
        }

        // index not in range [fec_set_index, fec_set_index + 32)
        {
            let mut packet = Packet::default();
            shreds[0].copy_to_packet(&mut packet);

            let fec_set_index = 64u32; // Multiple of 32
            let bad_index = 100u32; // Outside [64, 96)
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&bad_index.to_le_bytes()).unwrap();
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                    .unwrap();
                cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
            }

            let mut stats = ShredFetchStats::default();
            let should_discard = should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| enforce_fixed_fec_set,
                |_| false,
                &mut stats,
            );
            assert_eq!(should_discard, enforce_fixed_fec_set);
            assert_eq!(stats.misaligned_fec_set, 1);
        }

        // bad erasure config 16:32
        {
            let code_shred = shreds
                .iter()
                .find(|s| s.shred_type() == ShredType::Code)
                .unwrap();
            let mut packet = Packet::default();
            code_shred.copy_to_packet(&mut packet);

            let bad_num_data = 16u16;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_NUM_DATA as u64))
                    .unwrap();
                cursor.write_all(&bad_num_data.to_le_bytes()).unwrap();
            }

            let mut stats = ShredFetchStats::default();
            let should_discard = should_discard_shred(
                &packet,
                root,
                max_slot,
                shred_version,
                |_| enforce_fixed_fec_set,
                |_| false,
                &mut stats,
            );
            assert_eq!(should_discard, enforce_fixed_fec_set);
            assert_eq!(stats.misaligned_erasure_config, 1);
        }

        // data shred with LAST_SHRED_IN_SLOT flag on shred 30
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            true,     // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        let parent_slot = shreds[0].parent().unwrap();
        let shred_version = shreds[0].common_header().version;
        let root = rng.random_range(0..parent_slot);
        let data_shreds: Vec<_> = shreds
            .iter()
            .filter(|s| s.shred_type() == ShredType::Data)
            .collect();
        let last_data_shred = data_shreds.last().unwrap();
        assert!(last_data_shred.last_in_slot());
        let mut packet = Packet::default();
        last_data_shred.copy_to_packet(&mut packet);

        let bad_last_index = 30u32;
        let fec_set_index = 0u32;
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&bad_last_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
        }

        let mut stats = ShredFetchStats::default();
        let should_discard = should_discard_shred(
            &packet,
            root,
            max_slot,
            shred_version,
            |_| enforce_fixed_fec_set,
            |_| false,
            &mut stats,
        );
        assert_eq!(should_discard, enforce_fixed_fec_set);
        assert_eq!(stats.misaligned_last_data_index, 1);
    }

    // Asserts that ShredType is backward compatible with u8.
    #[test]
    fn test_shred_type_compat() {
        assert_eq!(std::mem::size_of::<ShredType>(), std::mem::size_of::<u8>());
        assert_matches!(ShredType::try_from(0u8), Err(_));
        assert_matches!(ShredType::try_from(1u8), Err(_));
        assert_matches!(wincode::deserialize::<ShredType>(&[0u8]), Err(_));
        assert_matches!(wincode::deserialize::<ShredType>(&[1u8]), Err(_));
        // data shred
        assert_eq!(ShredType::Data as u8, 0b1010_0101);
        assert_eq!(u8::from(ShredType::Data), 0b1010_0101);
        assert_eq!(ShredType::try_from(0b1010_0101), Ok(ShredType::Data));
        let buf = wincode::serialize(&ShredType::Data).unwrap();
        assert_eq!(buf, vec![0b1010_0101]);
        assert_matches!(
            wincode::deserialize::<ShredType>(&[0b1010_0101]),
            Ok(ShredType::Data)
        );
        // coding shred
        assert_eq!(ShredType::Code as u8, 0b0101_1010);
        assert_eq!(u8::from(ShredType::Code), 0b0101_1010);
        assert_eq!(ShredType::try_from(0b0101_1010), Ok(ShredType::Code));
        let buf = wincode::serialize(&ShredType::Code).unwrap();
        assert_eq!(buf, vec![0b0101_1010]);
        assert_matches!(
            wincode::deserialize::<ShredType>(&[0b0101_1010]),
            Ok(ShredType::Code)
        );
    }

    #[test]
    fn test_shred_variant_compat() {
        assert_matches!(ShredVariant::try_from(0u8), Err(_));
        assert_matches!(ShredVariant::try_from(1u8), Err(_));
        assert_matches!(ShredVariant::try_from(0b0101_0000), Err(_));
        assert_matches!(ShredVariant::try_from(0b1010_0000), Err(_));
        assert_matches!(wincode::deserialize::<ShredVariant>(&[0b0101_0000]), Err(_));
        assert_matches!(wincode::deserialize::<ShredVariant>(&[0b1010_0000]), Err(_));
        assert_matches!(ShredVariant::try_from(0b0101_1010), Err(_));
        assert_matches!(wincode::deserialize::<ShredVariant>(&[0b0101_1010]), Err(_));
        assert_matches!(ShredVariant::try_from(0b1010_0101), Err(_));
        assert_matches!(wincode::deserialize::<ShredVariant>(&[0b1010_0101]), Err(_));
    }

    #[test_case(false, 0b0110_0000)]
    #[test_case(true, 0b0111_0000)]
    fn test_shred_variant_compat_merkle_code(resigned: bool, byte: u8) {
        for proof_size in 0..=15u8 {
            let byte = byte | proof_size;
            assert_eq!(
                u8::from(ShredVariant::MerkleCode {
                    proof_size,
                    resigned,
                }),
                byte
            );
            assert_eq!(
                ShredType::from(ShredVariant::MerkleCode {
                    proof_size,
                    resigned,
                }),
                ShredType::Code
            );
            assert_eq!(
                ShredVariant::try_from(byte).unwrap(),
                ShredVariant::MerkleCode {
                    proof_size,
                    resigned,
                },
            );
            let wincode_buf = wincode::serialize(&ShredVariant::MerkleCode {
                proof_size,
                resigned,
            })
            .unwrap();
            assert_eq!(wincode_buf, vec![byte]);
            assert_eq!(
                wincode::deserialize::<ShredVariant>(&[byte]).unwrap(),
                ShredVariant::MerkleCode {
                    proof_size,
                    resigned,
                }
            );
        }
    }

    #[test_case(false, 0b1001_0000)]
    #[test_case(true, 0b1011_0000)]
    fn test_shred_variant_compat_merkle_data(resigned: bool, byte: u8) {
        for proof_size in 0..=15u8 {
            let byte = byte | proof_size;
            assert_eq!(
                u8::from(ShredVariant::MerkleData {
                    proof_size,
                    resigned,
                }),
                byte
            );
            assert_eq!(
                ShredType::from(ShredVariant::MerkleData {
                    proof_size,
                    resigned,
                }),
                ShredType::Data
            );
            assert_eq!(
                ShredVariant::try_from(byte).unwrap(),
                ShredVariant::MerkleData {
                    proof_size,
                    resigned
                }
            );
            let wincode_buf = wincode::serialize(&ShredVariant::MerkleData {
                proof_size,
                resigned,
            })
            .unwrap();
            assert_eq!(wincode_buf, vec![byte]);
            assert_eq!(
                wincode::deserialize::<ShredVariant>(&[byte]).unwrap(),
                ShredVariant::MerkleData {
                    proof_size,
                    resigned
                }
            );
        }
    }

    #[test]
    fn test_shred_seed() {
        let mut rng = ChaChaRng::from_seed([147u8; 32]);
        let leader = Pubkey::new_from_array(rng.random());
        let key = ShredId(
            141939602, // slot
            28685,     // index
            ShredType::Data,
        );
        assert_eq!(
            bs58::encode(key.seed(&leader)).into_string(),
            "Gp4kUM4ZpWGQN5XSCyM9YHYWEBCAZLa94ZQuSgDE4r56"
        );
        let leader = Pubkey::new_from_array(rng.random());
        let key = ShredId(
            141945197, // slot
            23418,     // index
            ShredType::Code,
        );
        assert_eq!(
            bs58::encode(key.seed(&leader)).into_string(),
            "G1gmFe1QUM8nhDApk6BqvPgw3TQV2Qc5bpKppa96qbVb"
        );
    }

    fn verify_shred_layout(shred: &Shred, packet: &Packet) {
        let data = layout::get_shred(packet).unwrap();
        assert_eq!(data, packet.data(..).unwrap());
        assert_eq!(layout::get_slot(data), Some(shred.slot()));
        assert_eq!(layout::get_index(data), Some(shred.index()));
        assert_eq!(layout::get_version(data), Some(shred.version()));
        assert_eq!(layout::get_shred_id(data), Some(shred.id()));
        assert_eq!(layout::get_signature(data), Some(*shred.signature()));
        assert_eq!(layout::get_shred_type(data).unwrap(), shred.shred_type());
        match shred.shred_type() {
            ShredType::Code => {
                assert_matches!(
                    layout::get_reference_tick(data),
                    Err(Error::InvalidShredType)
                );
            }
            ShredType::Data => {
                assert_eq!(
                    layout::get_reference_tick(data).unwrap(),
                    shred.reference_tick()
                );
                let parent_offset = layout::get_parent_offset(data).unwrap();
                let slot = layout::get_slot(data).unwrap();
                let parent = slot.checked_sub(Slot::from(parent_offset)).unwrap();
                assert_eq!(parent, shred.parent().unwrap());
            }
        }
    }

    #[test]
    fn test_serde_compat_shred_data() {
        // bytes of a serialized merkle data shred
        const PAYLOAD: &str = "aX2ovF3sZRfd6HyqMow9kkrtL3MyJd52m7gvuSjcvA4qayXZ\
        cVPhjURcs4JX86YQM8wVrKXqdneqdEUJwBWhFrxSkegDSov6NQoK89SzZi9auEXHHr35dmN\
        4zQbxuNdPjKM2K7b7WKRWaHyoMKQfG9jDbJGcWqwVkAxBmUXZQKryHvAqyNdBuRTdWrMtPK\
        DiJWhqVWTmokpyGNceL7mqVr3VrLby6dEuiEUCBHCkhbsXBjfpFZk4yRoSKosb7BViTWWdt\
        pWd7NrbDSiE97sBppEU1nWTPaVQh3bu91x8dEoYk696k532MxnhRLcKeL4XzG6P2HzypAck\
        JdXiRJDn5E3woA8aiPojqdN9ScthJ8yXq1h4HhvzTRWkRxRBpJL8HEYPBcshwuMLDZ9iBsW\
        SFZLmj5v1xH3kDnMuNYJg6Dau6PKHnZyD15tTyFtFtMaXaBc35RqYhsM7s8JuQ9tJ1UfFwd\
        khHa1wdrmTWGcvq9DDmALuTtejH1ccoW43GiYSs1TmByJWjRtupvLzMRifZZ7meaGbUBgHU\
        kA6t1VN3akoZ9BhdX561KpFGABxTU4NxyFqztEy1EB5EJYtTHwtbJQb1NmNMwKFkazXkn1o\
        uKK6drH5y19roH3mMo2JykapbvzYPDBSXUwKQWe1RqSvogapwPxm1EzSRDeXNDP6EYUJJjj\
        TAnckNatpT5UZDz4EhpaSbUzd9b5ztqsdPp9HxeBTm412GopAXKN5iSXSPS2WvrEdnANFD7\
        tRV3a6PM2SfwpF6eFM5J7xXGJSoPm5TWJSPBMbxttxVFUETSRrBubEsd24aymYZZePJtHr7\
        Q8S1deygcyXH5WhhYAmR23hNPv3nUUHe8iwJfaFg73Ncjr8fQBVjwePEy9JKT5jNG5sm87q\
        e2RrHEWEwkNKnNgUknoVMbL7y3wmGFpP8VoKTgP51EjMDz7JTxnVsZeRsSp29STteGKbq4i\
        wiC5EmMS5K86CAJ86FYt1kXXHJBSw4D79wAMgxRDDycp5PgdowdLxAbwySgpmwdfnxnSD4h\
        Y8mo4jLGWokP1mGdgjnPmtMbzndiQCLPjpUcbZoVc6SQrTDCufupkJhy1ewo64yA1db6T2T\
        ASTWSHJkjzaWt7QtFfnBo8WoXQrNKw5pyKAQsmP7n6r1SVD7tASfcZAjfaFHxkVvMpKwTQF\
        dy9WHxREeCPK3yeN7ACT75RgRuRT1shC1PRCuAu4EFGnBmr3nWuDrYNCG5WrWuW6RRoMyB3\
        YaXqjYMXRUVuwb5h2PBP9euBb96Ntung8ihWXa2mbKMYMtmaoYCDhYYrFYszYfdgQH68JYz\
        AXZvjFH1SxCETfiXAWGD1aYDa33rXZLcLVx637igoydr77qmzo5YozRQnuXUiJ19PScLWic\
        8jWeVmQ6Mm7BLoGhVPyYbJBeyX5HRwh8CNeLK2ekmhFz9MypB1rM2PXUfcnr2MXS9WRK8bh\
        sy47awNdApPdN3RxmuyPLnvmN6FsG5fUNqF8rsz9KUiJh9C4ziYf6NSZvVG2c1KFsQRyFrS\
        BzyjqqxBrH1xereV9YNr1gNamFjhZTncpGcPQf9oAoA4LQeSAZXR1dMtfktCs1fFWVbA67F\
        dQ1GrpZVGTsZCbuw7Tspns8WoL158AdS7";

        let mut rng = {
            let seed = [1u8; 32];
            ChaChaRng::from_seed(seed)
        };
        let mut seed = [0u8; Keypair::SECRET_KEY_LENGTH];
        rng.fill(&mut seed[..]);
        let mut data = [0u8; 4096];
        rng.fill(&mut data[..]);
        let keypair = keypair_from_seed(&seed).unwrap();
        let slot = 142076266;
        let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 42).unwrap();
        let reed_solomon_cache = ReedSolomonCache::default();
        let mut shred = shredder
            .make_shreds_from_data_slice(
                &keypair,
                &data,
                false,
                Hash::default(),
                64,
                64,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            )
            .unwrap()
            .next()
            .unwrap();
        shred.sign(&keypair);
        assert!(shred.verify(&keypair.pubkey()));
        assert_matches!(shred.sanitize(), Ok(()));
        let payload = bs58::decode(PAYLOAD).into_vec().unwrap();
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta_mut().size = payload.len();
        assert_eq!(
            shred,
            Shred::new_from_serialized_shred(payload.to_vec()).unwrap()
        );
        verify_shred_layout(&shred, &packet);
    }

    #[test]
    fn test_serde_compat_shred_data_empty() {
        // bytes of a serialized merkle data shred
        const PAYLOAD: &str = "HV7qJBe3jCM8aRd4HAXJnJzyDvNYDYsPjjaaK2tTFTxJU2Qj\
        7i87e45TzCg2Vv4rrcBznfs8212svH8aXsM2WYDPst43KyDz99FesBZ8aasxhkUgHGg3Smc\
        Pa7opSARcYBpQAG2UHRYFmoPsj3hXADsX5C8JBM3jyHLtbQ78CH11J2dh7J4ps8JxCcUsq7\
        E7PVs7NgFku54c8gBuhBuAMykvvSyhGRjyXCL17feubvA8WQyMJz27eXk8hE6LGs98ucsV9\
        pScMuXVbAL4rT2cW9gN77QP4mBohJ8miWbMYbi7eLxiXJ9nA6i7XZd32GKscf3Ln8PK7NFT\
        JpKxinptw2vd3MuVQRQjNuyZLTEjJSiaxoR1mhSKet27PCTkPWRAxPfMCvLNY2mtdixnFfk\
        BdKj1nrwHcQYqNnKHjg3axRhx58QQ7VX5LSNGRtLByZRFMq9wCy7zz7HiMKvMZzddqixHcx\
        EFeaM6YFh2sfmW3AKz4pS9s6XL3mAD9MtFvRJUXupNK9P3XtA7BGU8Z8AyYnVi4wxfaWoSd\
        22nhDmMuHTRNHgUDxCEPHfE5enuR3PG4q1DGpVyV9rM6678qFG3cUKdTbJw85uFAkedEhAn\
        RsU5u7H2DUTMrZ3AsLGuRmRoLZdxcFGe9GJmWY3WYEBJsdrJUVTnQVDsgvcc7HiFCoGBmGG\
        KPS19ncA7Ynie1iokXCjwZNaQf3fpUMksGzkqjfYiFfGQDMNTsPUdcHFAae5Nmh3p8bduG6\
        TNHn3A4LoSMX8wwM6Sn4XL3fFLHrkWTV8CJfg67AoqpzgGiWQNdeK2HRABGRbamUxXSzWhA\
        tJ7yEPrh9tKTX851mpPjrFHxpu68xDL3t5nd18mtALJ5n5gmTsXMwUxpt9GfGrZyHXteZfH\
        jaMLmSqvmDAFH1xADU2SZucRDbAWsuaWUMwaPXCNtiozgJ6uRnyxmwBhikChgtxDSZmRALA\
        uYwEnCE8uj4NWvryeVpzfy6m9tqYutCkRsbNodaGmBZ2KGtg4iaQjj4iCK9jKKAxpYVnxbr\
        n2jtFezUTsR9dqFh94c9Aa4NdPgkr19hcqabqBZyzan3xP3Jvs78Z5uqkSUVtXP3t3b5ozd\
        qjYRMkgwsgGpNqcmBLGANiPXrQ8SseoNsCs3Xtv6Vf7oGg1St3teXSrtWMbsouK3uF8DPgn\
        S14yUtrs8cyXX53QMCEuY1wKcoQZwSWqL86FTvZUA3vT6SYvjVKYxAXwNP1ouKdwtePfwdH\
        wdM37RT9SMht4BLEXutSCcBys1K3pTWWwqGB87A26apuG1TiqeEugv1FrjprEKyt84S3FgM\
        5eJfdN5NDArvw9bBR81UoYmyZgX5pEY6gNg2xw28Gd7gH9TVe5Y31iggni2oJ3GuBj3R7Ma\
        umdo8rE1S4tBWVGsXikv9KFDtMmT2sMmeFuAwDbUZEGsBCAfK7EQpKcYSv7KajtgLpqi6JW\
        RP8nBR7FkKR9qv5khhiBLuRzfSxwtADXZknFRU6bh8Ba6JmLhjhqkCYETizXmZVrjvy4gLZ\
        we5YHZW5uhbthAzfcLRxuTxnPbyW7LehgwkYde64b8PzqYepYUtxqFHuJSwddis1VuoA2Lb\
        M3SeyTe262Q7gUiPEjwQdRXKwUAgGrxVu";
        let mut rng = {
            let seed = [1u8; 32];
            ChaChaRng::from_seed(seed)
        };
        let mut seed = [0u8; Keypair::SECRET_KEY_LENGTH];
        rng.fill(&mut seed[..]);
        let keypair = keypair_from_seed(&seed).unwrap();
        let mut shred = Shredder::single_shred_for_tests(142076266, &keypair);
        shred.sign(&keypair);
        assert!(shred.verify(&keypair.pubkey()));
        assert_matches!(shred.sanitize(), Ok(()));
        let payload = bs58::decode(PAYLOAD).into_vec().unwrap();
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta_mut().size = payload.len();
        assert_eq!(
            shred,
            Shred::new_from_serialized_shred(payload.to_vec()).unwrap()
        );
        verify_shred_layout(&shred, &packet);
    }

    #[test]
    fn test_shred_flags_serde() {
        use wincode::{Deserialize as _, Serialize as _};

        let flags = Pod::<ShredFlags>::deserialize(&[0b0001_0101]).unwrap();
        assert_eq!(flags, ShredFlags::from_bits(0b0001_0101).unwrap());
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 21u8);
        assert_eq!(Pod::<ShredFlags>::serialize(&flags).unwrap(), [0b0001_0101]);

        let flags = Pod::<ShredFlags>::deserialize(&[0b0111_0001]).unwrap();
        assert_eq!(flags, ShredFlags::from_bits(0b0111_0001).unwrap());
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 49u8);
        assert_eq!(Pod::<ShredFlags>::serialize(&flags).unwrap(), [0b0111_0001]);

        let flags = Pod::<ShredFlags>::deserialize(&[0b1110_0101]).unwrap();
        assert_eq!(flags, ShredFlags::from_bits(0b1110_0101).unwrap());
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 37u8);
        assert_eq!(Pod::<ShredFlags>::serialize(&flags).unwrap(), [0b1110_0101]);

        let flags = Pod::<ShredFlags>::deserialize(&[0b1011_1101]).unwrap();
        assert_eq!(flags, ShredFlags::from_bits(0b1011_1101).unwrap());
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 61u8);
        assert_eq!(Pod::<ShredFlags>::serialize(&flags).unwrap(), [0b1011_1101]);
    }

    // Verifies that LAST_SHRED_IN_SLOT also implies DATA_COMPLETE_SHRED.
    #[test]
    fn test_shred_flags_data_complete() {
        use wincode::Deserialize as _;

        let mut flags = ShredFlags::empty();
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        flags.insert(ShredFlags::LAST_SHRED_IN_SLOT);
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));

        let mut flags = ShredFlags::from_bits(0b0011_1111).unwrap();
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        flags |= ShredFlags::LAST_SHRED_IN_SLOT;
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));

        let mut flags = Pod::<ShredFlags>::deserialize(&[0b1011_1111]).unwrap();
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        flags.insert(ShredFlags::LAST_SHRED_IN_SLOT);
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_is_shred_duplicate(is_last_in_slot: bool) {
        fn fill_retransmitter_signature<R: Rng>(
            rng: &mut R,
            shred: Shred,
            is_last_in_slot: bool,
        ) -> Shred {
            let mut shred = shred.into_payload();
            let mut signature = [0u8; SIGNATURE_BYTES];
            rng.fill(&mut signature[..]);
            let out = layout::set_retransmitter_signature(
                &mut shred.as_mut(),
                &Signature::from(signature),
            );
            if is_last_in_slot {
                assert_matches!(out, Ok(()));
            } else {
                assert_matches!(out, Err(Error::InvalidShredVariant));
            }
            Shred::new_from_serialized_shred(shred).unwrap()
        }

        let mut rng = rand::rng();
        let slot = 285_376_049 + rng.random_range(0..100_000);
        let shreds: Vec<_> = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            is_last_in_slot,
        )
        .unwrap()
        .into_iter()
        .map(Shred::from)
        .map(|shred| fill_retransmitter_signature(&mut rng, shred, is_last_in_slot))
        .collect();
        {
            let num_data_shreds = shreds.iter().filter(|shred| shred.is_data()).count();
            let num_coding_shreds = shreds.iter().filter(|shred| shred.is_code()).count();
            assert!(num_data_shreds > if is_last_in_slot { 31 } else { 5 });
            assert!(num_coding_shreds > if is_last_in_slot { 31 } else { 20 });
        }
        // Shreds of different (slot, index, shred-type) are not duplicate.
        // A shred is not a duplicate of itself either.
        for shred in &shreds {
            for other in &shreds {
                assert!(!shred.is_shred_duplicate(other));
            }
        }
        // Different retransmitter signature does not make shreds duplicate.
        for shred in &shreds {
            let other = fill_retransmitter_signature(&mut rng, shred.clone(), is_last_in_slot);
            if is_last_in_slot {
                assert_ne!(shred.payload(), other.payload());
            }
            assert!(!shred.is_shred_duplicate(&other));
            assert!(!other.is_shred_duplicate(shred));
        }
        // Shreds of the same (slot, index, shred-type) with different payload
        // (ignoring retransmitter signature) are duplicate.
        for shred in &shreds {
            let mut other = shred.payload().clone();
            other.as_mut()[90] = other[90].wrapping_add(1);
            let other = Shred::new_from_serialized_shred(other).unwrap();
            assert_ne!(shred.payload(), other.payload());
            assert_eq!(
                layout::get_retransmitter_signature(shred.payload()).ok(),
                layout::get_retransmitter_signature(other.payload()).ok()
            );
            assert!(shred.is_shred_duplicate(&other));
            assert!(other.is_shred_duplicate(shred));
        }
    }

    #[test]
    fn test_data_complete_shred_index_validation() {
        agave_logger::setup();
        let mut rng = rand::rng();
        let slot = 18_291;
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            false,    // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();

        let data_shred = shreds
            .iter()
            .find(|s| s.shred_type() == ShredType::Data)
            .unwrap();

        let parent_slot = data_shred.parent().unwrap();
        let shred_version = data_shred.common_header().version;
        let root = rng.random_range(0..parent_slot);
        let max_slot = slot + rng.random_range(1..65536);

        // Test case where DATA_COMPLETE_SHRED flag is set but index is not at expected position
        let mut packet = Packet::default();
        data_shred.copy_to_packet(&mut packet);

        let fec_set_index = 64u32;
        let wrong_index = fec_set_index + 10; // Should be fec_set_index + 31 for DATA_COMPLETE_SHRED

        // Modify the packet to have DATA_COMPLETE_SHRED flag with wrong index
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&wrong_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();

            // Flags offset is at 85, where:
            //
            //     85 = SIZE_OF_COMMON_SHRED_HEADER (83) + size of parent offset (i.e., 2)
            //
            // See the top-level comments, which articulate data shred layout, for more details.
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_FLAGS as u64))
                .unwrap(); // flags offset
            cursor
                .write_all(&[ShredFlags::DATA_COMPLETE_SHRED.bits()])
                .unwrap();
        }

        let mut stats = ShredFetchStats::default();
        let should_discard = should_discard_shred(
            &packet,
            root,
            max_slot,
            shred_version,
            |_| true,
            |_| true,
            &mut stats,
        );
        assert!(should_discard);
        assert_eq!(stats.unexpected_data_complete_shred, 1);

        // Test case where DATA_COMPLETE_SHRED flag is set with correct index
        let correct_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32 - 1;
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&correct_index.to_le_bytes()).unwrap();
        }

        let mut stats = ShredFetchStats::default();
        let should_discard = should_discard_shred(
            &packet,
            root,
            max_slot,
            shred_version,
            |_| true,
            |_| true,
            &mut stats,
        );
        assert!(!should_discard);
        assert_eq!(stats.unexpected_data_complete_shred, 0);
    }
}
