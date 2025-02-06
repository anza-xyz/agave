#[cfg(test)]
use crate::shred::ShredType;
use {
    crate::{
        shred::{
            self,
            common::impl_shred_common,
            dispatch,
            payload::Payload,
            shred_code, shred_data,
            traits::{
                Shred as ShredTrait, ShredCode as ShredCodeTrait, ShredData as ShredDataTrait,
            },
            CodingShredHeader, DataShredHeader, Error, ProcessShredsStats, ShredCommonHeader,
            ShredFlags, ShredVariant, DATA_SHREDS_PER_FEC_BLOCK, SIZE_OF_CODING_SHRED_HEADERS,
            SIZE_OF_DATA_SHRED_HEADERS, SIZE_OF_SIGNATURE,
        },
        shredder::{self, ReedSolomonCache},
    },
    assert_matches::debug_assert_matches,
    itertools::{Either, Itertools},
    rayon::{prelude::*, ThreadPool},
    reed_solomon_erasure::Error::{InvalidIndex, TooFewParityShards},
    solana_perf::packet::deserialize_from_with_limit,
    solana_sdk::{
        clock::Slot,
        hash::{hashv, Hash},
        pubkey::Pubkey,
        signature::{Signature, Signer},
        signer::keypair::Keypair,
    },
    static_assertions::const_assert_eq,
    std::{
        cmp::Ordering,
        io::{Cursor, Write},
        iter::successors,
        ops::Range,
        time::Instant,
    },
};

const_assert_eq!(SIZE_OF_MERKLE_ROOT, 32);
pub(crate) const SIZE_OF_MERKLE_ROOT: usize = std::mem::size_of::<Hash>();
const_assert_eq!(SIZE_OF_MERKLE_PROOF_ENTRY, 20);
const SIZE_OF_MERKLE_PROOF_ENTRY: usize = std::mem::size_of::<MerkleProofEntry>();
const_assert_eq!(ShredData::SIZE_OF_PAYLOAD, 1203);

// Defense against second preimage attack:
// https://en.wikipedia.org/wiki/Merkle_tree#Second_preimage_attack
// Following Certificate Transparency, 0x00 and 0x01 bytes are prepended to
// hash data when computing leaf and internal node hashes respectively.
const MERKLE_HASH_PREFIX_LEAF: &[u8] = b"\x00SOLANA_MERKLE_SHREDS_LEAF";
const MERKLE_HASH_PREFIX_NODE: &[u8] = b"\x01SOLANA_MERKLE_SHREDS_NODE";

type MerkleProofEntry = [u8; 20];

// Layout: {common, data} headers | data buffer
//     | [Merkle root of the previous erasure batch if chained]
//     | Merkle proof
//     | [Retransmitter's signature if resigned]
// The slice past signature till the end of the data buffer is erasure coded.
// The slice past signature and before the merkle proof is hashed to generate
// the Merkle tree. The root of the Merkle tree is signed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShredData {
    common_header: ShredCommonHeader,
    data_header: DataShredHeader,
    payload: Payload,
}

// Layout: {common, coding} headers | erasure coded shard
//     | [Merkle root of the previous erasure batch if chained]
//     | Merkle proof
//     | [Retransmitter's signature if resigned]
// The slice past signature and before the merkle proof is hashed to generate
// the Merkle tree. The root of the Merkle tree is signed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShredCode {
    common_header: ShredCommonHeader,
    coding_header: CodingShredHeader,
    payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Shred {
    ShredCode(ShredCode),
    ShredData(ShredData),
}

impl Shred {
    dispatch!(fn erasure_shard_index(&self) -> Result<usize, Error>);
    dispatch!(fn erasure_shard_mut(&mut self) -> Result<&mut [u8], Error>);
    dispatch!(fn merkle_node(&self) -> Result<Hash, Error>);
    dispatch!(fn sanitize(&self) -> Result<(), Error>);
    dispatch!(fn set_chained_merkle_root(&mut self, chained_merkle_root: &Hash) -> Result<(), Error>);
    dispatch!(fn set_signature(&mut self, signature: Signature));
    dispatch!(fn signed_data(&self) -> Result<Hash, Error>);
    dispatch!(pub(super) fn common_header(&self) -> &ShredCommonHeader);
    dispatch!(pub(super) fn payload(&self) -> &Payload);
    dispatch!(pub(super) fn set_retransmitter_signature(&mut self, signature: &Signature) -> Result<(), Error>);

    #[inline]
    fn fec_set_index(&self) -> u32 {
        self.common_header().fec_set_index
    }

    #[inline]
    fn merkle_proof(&self) -> Result<impl Iterator<Item = &MerkleProofEntry>, Error> {
        match self {
            Self::ShredCode(shred) => shred.merkle_proof().map(Either::Left),
            Self::ShredData(shred) => shred.merkle_proof().map(Either::Right),
        }
    }

    #[inline]
    fn set_merkle_proof<'a, I>(&mut self, proof: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = Result<&'a MerkleProofEntry, Error>>,
    {
        match self {
            Self::ShredCode(shred) => shred.set_merkle_proof(proof),
            Self::ShredData(shred) => shred.set_merkle_proof(proof),
        }
    }

    #[must_use]
    fn verify(&self, pubkey: &Pubkey) -> bool {
        match self.signed_data() {
            Ok(data) => self.signature().verify(pubkey.as_ref(), data.as_ref()),
            Err(_) => false,
        }
    }

    #[inline]
    fn signature(&self) -> &Signature {
        &self.common_header().signature
    }

    pub(super) fn from_payload<T: AsRef<[u8]>>(shred: T) -> Result<Self, Error>
    where
        Payload: From<T>,
    {
        match shred::layout::get_shred_variant(shred.as_ref())? {
            ShredVariant::LegacyCode | ShredVariant::LegacyData => Err(Error::InvalidShredVariant),
            ShredVariant::MerkleCode { .. } => Ok(Self::ShredCode(ShredCode::from_payload(shred)?)),
            ShredVariant::MerkleData { .. } => Ok(Self::ShredData(ShredData::from_payload(shred)?)),
        }
    }
}

#[cfg(test)]
impl Shred {
    dispatch!(fn erasure_shard(&self) -> Result<&[u8], Error>);
    dispatch!(fn proof_size(&self) -> Result<u8, Error>);
    dispatch!(pub(super) fn chained_merkle_root(&self) -> Result<Hash, Error>);
    dispatch!(pub(super) fn merkle_root(&self) -> Result<Hash, Error>);
    dispatch!(pub(super) fn retransmitter_signature(&self) -> Result<Signature, Error>);
    dispatch!(pub(super) fn retransmitter_signature_offset(&self) -> Result<usize, Error>);

    fn index(&self) -> u32 {
        self.common_header().index
    }

    fn shred_type(&self) -> ShredType {
        ShredType::from(self.common_header().shred_variant)
    }
}

impl ShredData {
    impl_merkle_shred!(MerkleData);

    // Offset into the payload where the erasure coded slice begins.
    const ERASURE_SHARD_START_OFFSET: usize = SIZE_OF_SIGNATURE;

    // Given shred payload, ShredVariant{..} and DataShredHeader.size, returns
    // the slice storing ledger entries in the shred.
    pub(super) fn get_data(
        shred: &[u8],
        proof_size: u8,
        chained: bool,
        resigned: bool,
        size: u16, // DataShredHeader.size
    ) -> Result<&[u8], Error> {
        let size = usize::from(size);
        let data_buffer_size = Self::capacity(proof_size, chained, resigned)?;
        (Self::SIZE_OF_HEADERS..=Self::SIZE_OF_HEADERS + data_buffer_size)
            .contains(&size)
            .then(|| shred.get(Self::SIZE_OF_HEADERS..size))
            .flatten()
            .ok_or_else(|| Error::InvalidDataSize {
                size: size as u16,
                payload: shred.len(),
            })
    }

    pub(super) fn get_merkle_root(
        shred: &[u8],
        proof_size: u8,
        chained: bool,
        resigned: bool,
    ) -> Option<Hash> {
        debug_assert_eq!(
            shred::layout::get_shred_variant(shred).unwrap(),
            ShredVariant::MerkleData {
                proof_size,
                chained,
                resigned,
            },
        );
        // Shred index in the erasure batch.
        let index = {
            let fec_set_index = <[u8; 4]>::try_from(shred.get(79..83)?)
                .map(u32::from_le_bytes)
                .ok()?;
            shred::layout::get_index(shred)?
                .checked_sub(fec_set_index)
                .map(usize::try_from)?
                .ok()?
        };
        let proof_offset = Self::get_proof_offset(proof_size, chained, resigned).ok()?;
        let proof = get_merkle_proof(shred, proof_offset, proof_size).ok()?;
        let node = get_merkle_node(shred, SIZE_OF_SIGNATURE..proof_offset).ok()?;
        get_merkle_root(index, node, proof).ok()
    }
}

impl ShredCode {
    impl_merkle_shred!(MerkleCode);

    // Offset into the payload where the erasure coded slice begins.
    const ERASURE_SHARD_START_OFFSET: usize = Self::SIZE_OF_HEADERS;

    pub(super) fn get_merkle_root(
        shred: &[u8],
        proof_size: u8,
        chained: bool,
        resigned: bool,
    ) -> Option<Hash> {
        debug_assert_eq!(
            shred::layout::get_shred_variant(shred).unwrap(),
            ShredVariant::MerkleCode {
                proof_size,
                chained,
                resigned,
            },
        );
        // Shred index in the erasure batch.
        let index = {
            let num_data_shreds = <[u8; 2]>::try_from(shred.get(83..85)?)
                .map(u16::from_le_bytes)
                .map(usize::from)
                .ok()?;
            let position = <[u8; 2]>::try_from(shred.get(87..89)?)
                .map(u16::from_le_bytes)
                .map(usize::from)
                .ok()?;
            num_data_shreds.checked_add(position)?
        };
        let proof_offset = Self::get_proof_offset(proof_size, chained, resigned).ok()?;
        let proof = get_merkle_proof(shred, proof_offset, proof_size).ok()?;
        let node = get_merkle_node(shred, SIZE_OF_SIGNATURE..proof_offset).ok()?;
        get_merkle_root(index, node, proof).ok()
    }
}

macro_rules! impl_merkle_shred {
    ($variant:ident) => {
        // proof_size is the number of merkle proof entries.
        #[inline]
        fn proof_size(&self) -> Result<u8, Error> {
            match self.common_header.shred_variant {
                ShredVariant::$variant { proof_size, .. } => Ok(proof_size),
                _ => Err(Error::InvalidShredVariant),
            }
        }

        // For ShredCode, size of buffer embedding erasure codes.
        // For ShredData, maximum size of ledger data that can be embedded in a
        // data-shred, which is also equal to:
        //   ShredCode::capacity(proof_size, chained, resigned).unwrap()
        //       - ShredData::SIZE_OF_HEADERS
        //       + SIZE_OF_SIGNATURE
        pub(super) fn capacity(
            proof_size: u8,
            chained: bool,
            resigned: bool,
        ) -> Result<usize, Error> {
            debug_assert!(chained || !resigned);
            // Merkle proof is generated and signed after coding shreds are
            // generated. Coding shred headers cannot be erasure coded either.
            Self::SIZE_OF_PAYLOAD
                .checked_sub(
                    Self::SIZE_OF_HEADERS
                        + if chained { SIZE_OF_MERKLE_ROOT } else { 0 }
                        + usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY
                        + if resigned { SIZE_OF_SIGNATURE } else { 0 },
                )
                .ok_or(Error::InvalidProofSize(proof_size))
        }

        // Where the merkle proof starts in the shred binary.
        fn proof_offset(&self) -> Result<usize, Error> {
            let ShredVariant::$variant {
                proof_size,
                chained,
                resigned,
            } = self.common_header.shred_variant
            else {
                return Err(Error::InvalidShredVariant);
            };
            Self::get_proof_offset(proof_size, chained, resigned)
        }

        fn get_proof_offset(proof_size: u8, chained: bool, resigned: bool) -> Result<usize, Error> {
            Ok(Self::SIZE_OF_HEADERS
                + Self::capacity(proof_size, chained, resigned)?
                + if chained { SIZE_OF_MERKLE_ROOT } else { 0 })
        }

        fn chained_merkle_root_offset(&self) -> Result<usize, Error> {
            let ShredVariant::$variant {
                proof_size,
                chained,
                resigned,
            } = self.common_header.shred_variant
            else {
                return Err(Error::InvalidShredVariant);
            };
            Self::get_chained_merkle_root_offset(proof_size, chained, resigned)
        }

        pub(super) fn get_chained_merkle_root_offset(
            proof_size: u8,
            chained: bool,
            resigned: bool,
        ) -> Result<usize, Error> {
            if !chained {
                return Err(Error::InvalidShredVariant);
            }
            debug_assert!(chained);
            Ok(Self::SIZE_OF_HEADERS + Self::capacity(proof_size, chained, resigned)?)
        }

        pub(super) fn chained_merkle_root(&self) -> Result<Hash, Error> {
            let offset = self.chained_merkle_root_offset()?;
            self.payload
                .get(offset..offset + SIZE_OF_MERKLE_ROOT)
                .map(|chained_merkle_root| {
                    <[u8; SIZE_OF_MERKLE_ROOT]>::try_from(chained_merkle_root)
                        .map(Hash::new_from_array)
                        .unwrap()
                })
                .ok_or(Error::InvalidPayloadSize(self.payload.len()))
        }

        fn set_chained_merkle_root(&mut self, chained_merkle_root: &Hash) -> Result<(), Error> {
            let offset = self.chained_merkle_root_offset()?;
            let Some(buffer) = self.payload.get_mut(offset..offset + SIZE_OF_MERKLE_ROOT) else {
                return Err(Error::InvalidPayloadSize(self.payload.len()));
            };
            buffer.copy_from_slice(chained_merkle_root.as_ref());
            Ok(())
        }

        pub(super) fn merkle_root(&self) -> Result<Hash, Error> {
            let proof_size = self.proof_size()?;
            let index = self.erasure_shard_index()?;
            let proof_offset = self.proof_offset()?;
            let proof = get_merkle_proof(&self.payload, proof_offset, proof_size)?;
            let node = get_merkle_node(&self.payload, SIZE_OF_SIGNATURE..proof_offset)?;
            get_merkle_root(index, node, proof)
        }

        fn merkle_proof(&self) -> Result<impl Iterator<Item = &MerkleProofEntry>, Error> {
            let proof_size = self.proof_size()?;
            let proof_offset = self.proof_offset()?;
            get_merkle_proof(&self.payload, proof_offset, proof_size)
        }

        fn merkle_node(&self) -> Result<Hash, Error> {
            let proof_offset = self.proof_offset()?;
            get_merkle_node(&self.payload, SIZE_OF_SIGNATURE..proof_offset)
        }

        fn set_merkle_proof<'a, I>(&mut self, proof: I) -> Result<(), Error>
        where
            I: IntoIterator<Item = Result<&'a MerkleProofEntry, Error>>,
        {
            let proof_size = self.proof_size()?;
            let proof_offset = self.proof_offset()?;
            let mut cursor = Cursor::new(
                self.payload
                    .get_mut(proof_offset..)
                    .ok_or(Error::InvalidProofSize(proof_size))?,
            );
            let proof_size = usize::from(proof_size);
            proof.into_iter().enumerate().try_for_each(|(k, entry)| {
                if k >= proof_size {
                    return Err(Error::InvalidMerkleProof);
                }
                Ok(cursor.write_all(&entry?[..])?)
            })?;
            // Verify that exactly proof_size many entries are written.
            if cursor.position() as usize != proof_size * SIZE_OF_MERKLE_PROOF_ENTRY {
                return Err(Error::InvalidMerkleProof);
            }
            Ok(())
        }

        pub(super) fn retransmitter_signature(&self) -> Result<Signature, Error> {
            let offset = self.retransmitter_signature_offset()?;
            self.payload
                .get(offset..offset + SIZE_OF_SIGNATURE)
                .map(|bytes| <[u8; SIZE_OF_SIGNATURE]>::try_from(bytes).unwrap())
                .map(Signature::from)
                .ok_or(Error::InvalidPayloadSize(self.payload.len()))
        }

        fn set_retransmitter_signature(&mut self, signature: &Signature) -> Result<(), Error> {
            let offset = self.retransmitter_signature_offset()?;
            let Some(buffer) = self.payload.get_mut(offset..offset + SIZE_OF_SIGNATURE) else {
                return Err(Error::InvalidPayloadSize(self.payload.len()));
            };
            buffer.copy_from_slice(signature.as_ref());
            Ok(())
        }

        pub(super) fn retransmitter_signature_offset(&self) -> Result<usize, Error> {
            let ShredVariant::$variant {
                proof_size,
                chained,
                resigned,
            } = self.common_header.shred_variant
            else {
                return Err(Error::InvalidShredVariant);
            };
            Self::get_retransmitter_signature_offset(proof_size, chained, resigned)
        }

        pub(super) fn get_retransmitter_signature_offset(
            proof_size: u8,
            chained: bool,
            resigned: bool,
        ) -> Result<usize, Error> {
            if !resigned {
                return Err(Error::InvalidShredVariant);
            }
            let proof_offset = Self::get_proof_offset(proof_size, chained, resigned)?;
            Ok(proof_offset + usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY)
        }

        // Returns the offsets into the payload which are erasure coded.
        fn erausre_shard_offsets(&self) -> Result<Range<usize>, Error> {
            if self.payload.len() != Self::SIZE_OF_PAYLOAD {
                return Err(Error::InvalidPayloadSize(self.payload.len()));
            }
            let ShredVariant::$variant {
                proof_size,
                chained,
                resigned,
            } = self.common_header.shred_variant
            else {
                return Err(Error::InvalidShredVariant);
            };
            let offset = Self::SIZE_OF_HEADERS + Self::capacity(proof_size, chained, resigned)?;
            Ok(Self::ERASURE_SHARD_START_OFFSET..offset)
        }

        // Returns the erasure coded slice as an immutable reference.
        fn erasure_shard(&self) -> Result<&[u8], Error> {
            self.payload
                .get(self.erausre_shard_offsets()?)
                .ok_or(Error::InvalidPayloadSize(self.payload.len()))
        }

        // Returns the erasure coded slice as a mutable reference.
        fn erasure_shard_mut(&mut self) -> Result<&mut [u8], Error> {
            let offsets = self.erausre_shard_offsets()?;
            let payload_size = self.payload.len();
            self.payload
                .get_mut(offsets)
                .ok_or(Error::InvalidPayloadSize(payload_size))
        }
    };
}

use impl_merkle_shred;

impl<'a> ShredTrait<'a> for ShredData {
    type SignedData = Hash;

    impl_shred_common!();

    // Also equal to:
    // ShredData::SIZE_OF_HEADERS
    //       + ShredData::capacity(proof_size, chained, resigned).unwrap()
    //       + if chained { SIZE_OF_MERKLE_ROOT } else { 0 }
    //       + usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY
    //       + if resigned { SIZE_OF_SIGNATURE } else { 0 }
    const SIZE_OF_PAYLOAD: usize =
        ShredCode::SIZE_OF_PAYLOAD - ShredCode::SIZE_OF_HEADERS + SIZE_OF_SIGNATURE;
    const SIZE_OF_HEADERS: usize = SIZE_OF_DATA_SHRED_HEADERS;

    fn from_payload<T>(payload: T) -> Result<Self, Error>
    where
        Payload: From<T>,
    {
        let mut payload = Payload::from(payload);
        // see: https://github.com/solana-labs/solana/pull/10109
        if payload.len() < Self::SIZE_OF_PAYLOAD {
            return Err(Error::InvalidPayloadSize(payload.len()));
        }
        payload.truncate(Self::SIZE_OF_PAYLOAD);
        let (common_header, data_header): (ShredCommonHeader, _) =
            deserialize_from_with_limit(&payload[..])?;
        if !matches!(common_header.shred_variant, ShredVariant::MerkleData { .. }) {
            return Err(Error::InvalidShredVariant);
        }
        let shred = Self {
            common_header,
            data_header,
            payload,
        };
        shred.sanitize()?;
        Ok(shred)
    }

    fn erasure_shard_index(&self) -> Result<usize, Error> {
        shred_data::erasure_shard_index(self).ok_or_else(|| {
            let headers = Box::new((self.common_header, self.data_header));
            Error::InvalidErasureShardIndex(headers)
        })
    }

    fn erasure_shard(&self) -> Result<&[u8], Error> {
        Self::erasure_shard(self)
    }

    fn sanitize(&self) -> Result<(), Error> {
        let shred_variant = self.common_header.shred_variant;
        if !matches!(shred_variant, ShredVariant::MerkleData { .. }) {
            return Err(Error::InvalidShredVariant);
        }
        let _ = self.merkle_proof()?;
        shred_data::sanitize(self)
    }

    fn signed_data(&'a self) -> Result<Self::SignedData, Error> {
        self.merkle_root()
    }
}

impl<'a> ShredTrait<'a> for ShredCode {
    type SignedData = Hash;

    impl_shred_common!();
    const SIZE_OF_PAYLOAD: usize = shred_code::ShredCode::SIZE_OF_PAYLOAD;
    const SIZE_OF_HEADERS: usize = SIZE_OF_CODING_SHRED_HEADERS;

    fn from_payload<T>(payload: T) -> Result<Self, Error>
    where
        Payload: From<T>,
    {
        let mut payload = Payload::from(payload);
        let (common_header, coding_header): (ShredCommonHeader, _) =
            deserialize_from_with_limit(&payload[..])?;
        if !matches!(common_header.shred_variant, ShredVariant::MerkleCode { .. }) {
            return Err(Error::InvalidShredVariant);
        }
        // see: https://github.com/solana-labs/solana/pull/10109
        if payload.len() < Self::SIZE_OF_PAYLOAD {
            return Err(Error::InvalidPayloadSize(payload.len()));
        }
        payload.truncate(Self::SIZE_OF_PAYLOAD);
        let shred = Self {
            common_header,
            coding_header,
            payload,
        };
        shred.sanitize()?;
        Ok(shred)
    }

    fn erasure_shard_index(&self) -> Result<usize, Error> {
        shred_code::erasure_shard_index(self).ok_or_else(|| {
            let headers = Box::new((self.common_header, self.coding_header));
            Error::InvalidErasureShardIndex(headers)
        })
    }

    fn erasure_shard(&self) -> Result<&[u8], Error> {
        Self::erasure_shard(self)
    }

    fn sanitize(&self) -> Result<(), Error> {
        let shred_variant = self.common_header.shred_variant;
        if !matches!(shred_variant, ShredVariant::MerkleCode { .. }) {
            return Err(Error::InvalidShredVariant);
        }
        let _ = self.merkle_proof()?;
        shred_code::sanitize(self)
    }

    fn signed_data(&'a self) -> Result<Self::SignedData, Error> {
        self.merkle_root()
    }
}

impl ShredDataTrait for ShredData {
    #[inline]
    fn data_header(&self) -> &DataShredHeader {
        &self.data_header
    }

    #[inline]
    fn data(&self) -> Result<&[u8], Error> {
        let ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } = self.common_header.shred_variant
        else {
            return Err(Error::InvalidShredVariant);
        };
        Self::get_data(
            &self.payload,
            proof_size,
            chained,
            resigned,
            self.data_header.size,
        )
    }
}

impl ShredCodeTrait for ShredCode {
    #[inline]
    fn coding_header(&self) -> &CodingShredHeader {
        &self.coding_header
    }
}

// Obtains parent's hash by joining two sibling nodes in merkle tree.
fn join_nodes<S: AsRef<[u8]>, T: AsRef<[u8]>>(node: S, other: T) -> Hash {
    let node = &node.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
    let other = &other.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
    hashv(&[MERKLE_HASH_PREFIX_NODE, node, other])
}

// Recovers root of the merkle tree from a leaf node
// at the given index and the respective proof.
fn get_merkle_root<'a, I>(index: usize, node: Hash, proof: I) -> Result<Hash, Error>
where
    I: IntoIterator<Item = &'a MerkleProofEntry>,
{
    let (index, root) = proof
        .into_iter()
        .fold((index, node), |(index, node), other| {
            let parent = if index % 2 == 0 {
                join_nodes(node, other)
            } else {
                join_nodes(other, node)
            };
            (index >> 1, parent)
        });
    (index == 0)
        .then_some(root)
        .ok_or(Error::InvalidMerkleProof)
}

fn get_merkle_proof(
    shred: &[u8],
    proof_offset: usize, // Where the merkle proof starts.
    proof_size: u8,      // Number of proof entries.
) -> Result<impl Iterator<Item = &MerkleProofEntry>, Error> {
    let proof_size = usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY;
    Ok(shred
        .get(proof_offset..proof_offset + proof_size)
        .ok_or(Error::InvalidPayloadSize(shred.len()))?
        .chunks(SIZE_OF_MERKLE_PROOF_ENTRY)
        .map(<&MerkleProofEntry>::try_from)
        .map(Result::unwrap))
}

fn get_merkle_node(shred: &[u8], offsets: Range<usize>) -> Result<Hash, Error> {
    let node = shred
        .get(offsets)
        .ok_or(Error::InvalidPayloadSize(shred.len()))?;
    Ok(hashv(&[MERKLE_HASH_PREFIX_LEAF, node]))
}

fn make_merkle_tree<I>(shreds: I) -> Result<Vec<Hash>, Error>
where
    I: IntoIterator<Item = Result<Hash, Error>>,
    <I as IntoIterator>::IntoIter: ExactSizeIterator,
{
    let shreds = shreds.into_iter();
    let num_shreds = shreds.len();
    let capacity = get_merkle_tree_size(num_shreds);
    let mut nodes = Vec::with_capacity(capacity);
    for shred in shreds {
        nodes.push(shred?);
    }
    let init = (num_shreds > 1).then_some(num_shreds);
    for size in successors(init, |&k| (k > 2).then_some((k + 1) >> 1)) {
        let offset = nodes.len() - size;
        for index in (offset..offset + size).step_by(2) {
            let node = &nodes[index];
            let other = &nodes[(index + 1).min(offset + size - 1)];
            let parent = join_nodes(node, other);
            nodes.push(parent);
        }
    }
    debug_assert_eq!(nodes.len(), capacity);
    Ok(nodes)
}

// Given number of shreds, returns the number of nodes in the Merkle tree.
fn get_merkle_tree_size(num_shreds: usize) -> usize {
    successors(Some(num_shreds), |&k| (k > 1).then_some((k + 1) >> 1)).sum()
}

fn make_merkle_proof(
    mut index: usize, // leaf index ~ shred's erasure shard index.
    mut size: usize,  // number of leaves ~ erasure batch size.
    tree: &[Hash],
) -> impl Iterator<Item = Result<&MerkleProofEntry, Error>> {
    let mut offset = 0;
    if index >= size {
        // Force below iterator to return Error.
        (size, offset) = (0, tree.len());
    }
    std::iter::from_fn(move || {
        if size > 1 {
            let Some(node) = tree.get(offset + (index ^ 1).min(size - 1)) else {
                return Some(Err(Error::InvalidMerkleProof));
            };
            offset += size;
            size = (size + 1) >> 1;
            index >>= 1;
            let entry = &node.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
            let entry = <&MerkleProofEntry>::try_from(entry).unwrap();
            Some(Ok(entry))
        } else if offset + 1 == tree.len() {
            None
        } else {
            Some(Err(Error::InvalidMerkleProof))
        }
    })
}

pub(super) fn recover(
    mut shreds: Vec<Shred>,
    reed_solomon_cache: &ReedSolomonCache,
) -> Result<impl Iterator<Item = Result<Shred, Error>>, Error> {
    // Sort shreds by their erasure shard index.
    // In particular this places all data shreds before coding shreds.
    let is_sorted = |(a, b)| cmp_shred_erasure_shard_index(a, b).is_le();
    if !shreds.iter().tuple_windows().all(is_sorted) {
        shreds.sort_unstable_by(cmp_shred_erasure_shard_index);
    }
    // Grab {common, coding} headers from the last coding shred.
    // Incoming shreds are resigned immediately after signature verification,
    // so we can just grab the retransmitter signature from one of the
    // available shreds and attach it to the recovered shreds.
    let (common_header, coding_header, merkle_root, chained_merkle_root, retransmitter_signature) = {
        // The last shred must be a coding shred by the above sorting logic.
        let Some(Shred::ShredCode(shred)) = shreds.last() else {
            return Err(Error::from(TooFewParityShards));
        };
        let position = u32::from(shred.coding_header.position);
        let index = shred.common_header.index.checked_sub(position);
        let common_header = ShredCommonHeader {
            index: index.ok_or(Error::from(InvalidIndex))?,
            ..shred.common_header
        };
        let coding_header = CodingShredHeader {
            position: 0u16,
            ..shred.coding_header
        };
        (
            common_header,
            coding_header,
            shred.merkle_root()?,
            shred.chained_merkle_root().ok(),
            shred.retransmitter_signature().ok(),
        )
    };
    debug_assert_matches!(common_header.shred_variant, ShredVariant::MerkleCode { .. });
    let (proof_size, chained, resigned) = match common_header.shred_variant {
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => (proof_size, chained, resigned),
        ShredVariant::MerkleData { .. } | ShredVariant::LegacyCode | ShredVariant::LegacyData => {
            return Err(Error::InvalidShredVariant);
        }
    };
    debug_assert!(!resigned || retransmitter_signature.is_some());
    // Verify that shreds belong to the same erasure batch
    // and have consistent headers.
    debug_assert!(shreds.iter().all(|shred| {
        let ShredCommonHeader {
            signature: _, // signature are verified further below.
            shred_variant,
            slot,
            index: _,
            version,
            fec_set_index,
        } = shred.common_header();
        slot == &common_header.slot
            && version == &common_header.version
            && fec_set_index == &common_header.fec_set_index
            && match shred {
                Shred::ShredData(_) => {
                    shred_variant
                        == &ShredVariant::MerkleData {
                            proof_size,
                            chained,
                            resigned,
                        }
                }
                Shred::ShredCode(shred) => {
                    let CodingShredHeader {
                        num_data_shreds,
                        num_coding_shreds,
                        position: _,
                    } = shred.coding_header;
                    shred_variant
                        == &ShredVariant::MerkleCode {
                            proof_size,
                            chained,
                            resigned,
                        }
                        && num_data_shreds == coding_header.num_data_shreds
                        && num_coding_shreds == coding_header.num_coding_shreds
                }
            }
    }));
    let num_data_shreds = usize::from(coding_header.num_data_shreds);
    let num_coding_shreds = usize::from(coding_header.num_coding_shreds);
    let num_shards = num_data_shreds + num_coding_shreds;
    // Identify which shreds are missing and create stub shreds in their place.
    let mut mask = vec![false; num_shards];
    let mut shreds = {
        let make_stub_shred = |erasure_shard_index| {
            make_stub_shred(
                erasure_shard_index,
                &common_header,
                &coding_header,
                &chained_merkle_root,
                &retransmitter_signature,
            )
        };
        let mut batch = Vec::with_capacity(num_shards);
        // By the sorting logic earlier above, this visits shreds in the order
        // of their erasure shard index.
        for shred in shreds {
            // The leader signs the Merkle root and shreds in the same erasure
            // batch have the same Merkle root. So the signatures are the same
            // or shreds are not from the same erasure batch.
            if shred.signature() != &common_header.signature {
                return Err(Error::InvalidMerkleRoot);
            }
            let erasure_shard_index = shred.erasure_shard_index()?;
            if !(batch.len()..num_shards).contains(&erasure_shard_index) {
                return Err(Error::from(InvalidIndex));
            }
            // Push stub shreds as placeholder for the missing shreds in
            // between.
            while batch.len() < erasure_shard_index {
                batch.push(make_stub_shred(batch.len())?);
            }
            mask[erasure_shard_index] = true;
            batch.push(shred);
        }
        // Push stub shreds as placeholder for the missing shreds at the end.
        while batch.len() < num_shards {
            batch.push(make_stub_shred(batch.len())?);
        }
        batch
    };
    // Obtain erasure encoded shards from the shreds and reconstruct shreds.
    let mut shards: Vec<(&mut [u8], bool)> = shreds
        .iter_mut()
        .zip(&mask)
        .map(|(shred, &mask)| Ok((shred.erasure_shard_mut()?, mask)))
        .collect::<Result<_, Error>>()?;
    reed_solomon_cache
        .get(num_data_shreds, num_coding_shreds)?
        .reconstruct(&mut shards)?;
    // Verify and sanitize recovered shreds, re-compute the Merkle tree and set
    // the merkle proof on the recovered shreds.
    let nodes = shreds
        .iter_mut()
        .zip(&mask)
        .enumerate()
        .map(|(index, (shred, mask))| {
            if !mask {
                if index < num_data_shreds {
                    let Shred::ShredData(shred) = shred else {
                        return Err(Error::InvalidRecoveredShred);
                    };
                    let (common_header, data_header) =
                        deserialize_from_with_limit(&shred.payload[..])?;
                    if shred.common_header != common_header {
                        return Err(Error::InvalidRecoveredShred);
                    }
                    shred.data_header = data_header;
                } else if !matches!(shred, Shred::ShredCode(_)) {
                    return Err(Error::InvalidRecoveredShred);
                }
                shred.sanitize()?;
            }
            shred.merkle_node()
        });
    let tree = make_merkle_tree(nodes)?;
    // The attached signature verifies only if we obtain the same Merkle root.
    // Because shreds obtained from turbine or repair are sig-verified, this
    // also means that we don't need to verify signatures for recovered shreds.
    if tree.last() != Some(&merkle_root) {
        return Err(Error::InvalidMerkleRoot);
    }
    let set_merkle_proof = move |(index, (mut shred, mask)): (_, (Shred, _))| {
        if mask {
            debug_assert!({
                let proof = make_merkle_proof(index, num_shards, &tree);
                shred.merkle_proof()?.map(Some).eq(proof.map(Result::ok))
            });
            Ok(None)
        } else {
            let proof = make_merkle_proof(index, num_shards, &tree);
            shred.set_merkle_proof(proof)?;
            // Already sanitized after reconstruct.
            debug_assert_matches!(shred.sanitize(), Ok(()));
            // Assert that shred payload is fully populated.
            debug_assert_eq!(shred, {
                let shred = shred.payload().clone();
                Shred::from_payload(shred).unwrap()
            });
            Ok(Some(shred))
        }
    };
    Ok(shreds
        .into_iter()
        .zip(mask)
        .enumerate()
        .map(set_merkle_proof)
        .filter_map(Result::transpose))
}

// Compares shreds of the same erasure batch by their erasure shard index
// within the erasure batch.
#[inline]
fn cmp_shred_erasure_shard_index(a: &Shred, b: &Shred) -> Ordering {
    debug_assert_eq!(
        a.common_header().fec_set_index,
        b.common_header().fec_set_index
    );
    // Ordering by erasure shard index is equivalent to:
    //   * ShredType::Data < ShredType::Code.
    //   * Tie break by shred index.
    match (a, b) {
        (Shred::ShredCode(_), Shred::ShredData(_)) => Ordering::Greater,
        (Shred::ShredData(_), Shred::ShredCode(_)) => Ordering::Less,
        (Shred::ShredCode(a), Shred::ShredCode(b)) => {
            a.common_header.index.cmp(&b.common_header.index)
        }
        (Shred::ShredData(a), Shred::ShredData(b)) => {
            a.common_header.index.cmp(&b.common_header.index)
        }
    }
}

// Creates a minimally populated shred which will be a placeholder for a
// missing shred when running erasure recovery. This allows us to obtain
// mutable references to erasure coded slices within the shreds and reconstruct
// shreds in place.
fn make_stub_shred(
    erasure_shard_index: usize,
    common_header: &ShredCommonHeader,
    coding_header: &CodingShredHeader,
    chained_merkle_root: &Option<Hash>,
    retransmitter_signature: &Option<Signature>,
) -> Result<Shred, Error> {
    let num_data_shreds = usize::from(coding_header.num_data_shreds);
    let mut shred = if let Some(position) = erasure_shard_index.checked_sub(num_data_shreds) {
        let position = u16::try_from(position).map_err(|_| Error::from(InvalidIndex))?;
        let common_header = ShredCommonHeader {
            index: common_header.index + u32::from(position),
            ..*common_header
        };
        let coding_header = CodingShredHeader {
            position,
            ..*coding_header
        };
        // For coding shreds {common,coding} headers are not part of the
        // erasure coded slice and need to be written to the payload here.
        let mut payload = vec![0u8; ShredCode::SIZE_OF_PAYLOAD];
        bincode::serialize_into(&mut payload[..], &(&common_header, &coding_header))?;
        Shred::ShredCode(ShredCode {
            common_header,
            coding_header,
            payload: Payload::from(payload),
        })
    } else {
        let ShredVariant::MerkleCode { proof_size, .. } = common_header.shred_variant else {
            return Err(Error::InvalidShredVariant);
        };
        let shred_variant = ShredVariant::MerkleData {
            proof_size,
            chained: chained_merkle_root.is_some(),
            resigned: retransmitter_signature.is_some(),
        };
        let index = common_header.fec_set_index
            + u32::try_from(erasure_shard_index).map_err(|_| InvalidIndex)?;
        let common_header = ShredCommonHeader {
            shred_variant,
            index,
            ..*common_header
        };
        // Data header will be overwritten from the recovered shard.
        let data_header = DataShredHeader {
            parent_offset: 0u16,
            flags: ShredFlags::empty(),
            size: 0u16,
        };
        // For data shreds only the signature part of the {common,data} headers
        // is not erasure coded and it needs to be written to the payload here.
        let mut payload = vec![0u8; ShredData::SIZE_OF_PAYLOAD];
        payload[..SIZE_OF_SIGNATURE].copy_from_slice(common_header.signature.as_ref());
        Shred::ShredData(ShredData {
            common_header,
            data_header,
            payload: Payload::from(payload),
        })
    };
    if let Some(chained_merkle_root) = chained_merkle_root {
        shred.set_chained_merkle_root(chained_merkle_root)?;
    }
    if let Some(signature) = retransmitter_signature {
        shred.set_retransmitter_signature(signature)?;
    }
    Ok(shred)
}

// Maps number of (code + data) shreds to merkle_proof.len().
fn get_proof_size(num_shreds: usize) -> u8 {
    let bits = usize::BITS - num_shreds.leading_zeros();
    let proof_size = if num_shreds.is_power_of_two() {
        bits.checked_sub(1).unwrap()
    } else {
        bits
    };
    u8::try_from(proof_size).unwrap()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn make_shreds_from_data(
    thread_pool: &ThreadPool,
    keypair: &Keypair,
    // The Merkle root of the previous erasure batch if chained.
    chained_merkle_root: Option<Hash>,
    mut data: &[u8], // Serialized &[Entry]
    slot: Slot,
    parent_slot: Slot,
    shred_version: u16,
    reference_tick: u8,
    is_last_in_slot: bool,
    next_shred_index: u32,
    next_code_index: u32,
    reed_solomon_cache: &ReedSolomonCache,
    stats: &mut ProcessShredsStats,
) -> Result<Vec<Shred>, Error> {
    // Generates data shreds for the current erasure batch.
    // Updates ShredCommonHeader.index for data shreds of the next batch.
    fn make_shreds_data<'a>(
        common_header: &'a mut ShredCommonHeader,
        mut data_header: DataShredHeader,
        chunks: impl IntoIterator<Item = &'a [u8]> + 'a,
    ) -> impl Iterator<Item = ShredData> + 'a {
        debug_assert_matches!(common_header.shred_variant, ShredVariant::MerkleData { .. });
        chunks.into_iter().map(move |chunk| {
            debug_assert_matches!(common_header.shred_variant,
                ShredVariant::MerkleData { proof_size, chained, resigned }
                if chunk.len() <= ShredData::capacity(proof_size, chained, resigned).unwrap()
            );
            let size = ShredData::SIZE_OF_HEADERS + chunk.len();
            let mut payload = vec![0u8; ShredData::SIZE_OF_PAYLOAD];
            payload[ShredData::SIZE_OF_HEADERS..size].copy_from_slice(chunk);
            data_header.size = size as u16;
            let shred = ShredData {
                common_header: *common_header,
                data_header,
                payload: Payload::from(payload),
            };
            common_header.index += 1;
            shred
        })
    }
    // Generates coding shreds for the current erasure batch.
    // Updates ShredCommonHeader.index for coding shreds of the next batch.
    fn make_shreds_code(
        common_header: &mut ShredCommonHeader,
        num_data_shreds: usize,
        is_last_in_slot: bool,
    ) -> impl Iterator<Item = ShredCode> + '_ {
        debug_assert_matches!(common_header.shred_variant, ShredVariant::MerkleCode { .. });
        let erasure_batch_size = shredder::get_erasure_batch_size(num_data_shreds, is_last_in_slot);
        let num_coding_shreds = erasure_batch_size - num_data_shreds;
        let mut coding_header = CodingShredHeader {
            num_data_shreds: num_data_shreds as u16,
            num_coding_shreds: num_coding_shreds as u16,
            position: 0,
        };
        std::iter::repeat_with(move || {
            let shred = ShredCode {
                common_header: *common_header,
                coding_header,
                payload: Payload::from(vec![0u8; ShredCode::SIZE_OF_PAYLOAD]),
            };
            common_header.index += 1;
            coding_header.position += 1;
            shred
        })
        .take(num_coding_shreds)
    }
    let now = Instant::now();
    let chained = chained_merkle_root.is_some();
    let resigned = chained && is_last_in_slot;
    let erasure_batch_size =
        shredder::get_erasure_batch_size(DATA_SHREDS_PER_FEC_BLOCK, is_last_in_slot);
    let proof_size = get_proof_size(erasure_batch_size);
    let data_buffer_size = ShredData::capacity(proof_size, chained, resigned)?;
    let chunk_size = DATA_SHREDS_PER_FEC_BLOCK * data_buffer_size;
    // Common header for the data shreds.
    let mut common_header_data = ShredCommonHeader {
        signature: Signature::default(),
        shred_variant: ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        },
        slot,
        index: next_shred_index,
        version: shred_version,
        fec_set_index: next_shred_index,
    };
    // Common header for the coding shreds.
    let mut common_header_code = ShredCommonHeader {
        shred_variant: ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        },
        index: next_code_index,
        ..common_header_data
    };
    let data_header = {
        let parent_offset = slot
            .checked_sub(parent_slot)
            .and_then(|offset| u16::try_from(offset).ok())
            .ok_or(Error::InvalidParentSlot { slot, parent_slot })?;
        let flags = ShredFlags::from_reference_tick(reference_tick);
        DataShredHeader {
            parent_offset,
            flags,
            size: 0u16,
        }
    };
    let mut shreds = {
        let capacity = 2 * DATA_SHREDS_PER_FEC_BLOCK * data.len().div_ceil(chunk_size);
        Vec::<Shred>::with_capacity(capacity)
    };
    // Split the data into erasure batches and initialize
    // data and coding shreds for each batch.
    while data.len() >= 2 * chunk_size || data.len() == chunk_size {
        let (chunk, rest) = data.split_at(chunk_size);
        debug_assert_eq!(chunk.len(), DATA_SHREDS_PER_FEC_BLOCK * data_buffer_size);
        common_header_data.fec_set_index = common_header_data.index;
        common_header_code.fec_set_index = common_header_data.fec_set_index;
        shreds.extend(
            make_shreds_data(
                &mut common_header_data,
                data_header,
                chunk.chunks(data_buffer_size),
            )
            .map(Shred::ShredData),
        );
        shreds.extend(
            make_shreds_code(
                &mut common_header_code,
                DATA_SHREDS_PER_FEC_BLOCK,          // num_data_shreds
                is_last_in_slot && rest.is_empty(), // is_last_in_slot
            )
            .map(Shred::ShredCode),
        );
        data = rest;
    }
    // If shreds.is_empty() then the data argument was empty. In that case we
    // want to generate one data shred with empty data.
    if !data.is_empty() || shreds.is_empty() {
        // Should generate at least one data shred (which may have no data).
        // Last erasure batch should also be padded with empty data shreds to
        // make >= 32 data shreds. This guarantees that the batch cannot be
        // recovered unless 32+ shreds are received from turbine or repair.
        let min_num_data_shreds = if is_last_in_slot {
            DATA_SHREDS_PER_FEC_BLOCK
        } else {
            1
        };
        // Find the Merkle proof_size and data_buffer_size
        // which can embed the remaining data.
        let (proof_size, data_buffer_size, num_data_shreds) = (1u8..32)
            .find_map(|proof_size| {
                let data_buffer_size = ShredData::capacity(proof_size, chained, resigned).ok()?;
                let num_data_shreds = data.len().div_ceil(data_buffer_size);
                let num_data_shreds = num_data_shreds.max(min_num_data_shreds);
                let erasure_batch_size =
                    shredder::get_erasure_batch_size(num_data_shreds, is_last_in_slot);
                (proof_size == get_proof_size(erasure_batch_size)).then_some((
                    proof_size,
                    data_buffer_size,
                    num_data_shreds,
                ))
            })
            .ok_or(Error::UnknownProofSize)?;
        common_header_data.shred_variant = ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        };
        common_header_code.shred_variant = ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        };
        common_header_data.fec_set_index = common_header_data.index;
        common_header_code.fec_set_index = common_header_data.fec_set_index;
        shreds.extend({
            let chunks = data
                .chunks(data_buffer_size)
                .chain(std::iter::repeat(&[][..])) // possible padding
                .take(num_data_shreds);
            make_shreds_data(&mut common_header_data, data_header, chunks).map(Shred::ShredData)
        });
        if let Some(Shred::ShredData(shred)) = shreds.last() {
            stats.data_buffer_residual += data_buffer_size - shred.data()?.len();
        }
        shreds.extend(
            make_shreds_code(&mut common_header_code, num_data_shreds, is_last_in_slot)
                .map(Shred::ShredCode),
        );
    }
    // Only the trailing data shreds may have residual data buffer.
    debug_assert!(shreds
        .iter()
        .rev()
        .filter_map(|shred| match shred {
            Shred::ShredCode(_) => None,
            Shred::ShredData(shred) => Some(shred),
        })
        .skip_while(|shred| is_last_in_slot && shred.data().unwrap().is_empty())
        .skip(1)
        .all(|shred| {
            let proof_size = shred.proof_size().unwrap();
            let capacity = ShredData::capacity(proof_size, chained, resigned).unwrap();
            shred.data().unwrap().len() == capacity
        }));
    // Adjust flags for the very last data shred.
    if let Some(Shred::ShredData(shred)) = shreds
        .iter_mut()
        .rev()
        .find(|shred| matches!(shred, Shred::ShredData(_)))
    {
        shred.data_header.flags |= if is_last_in_slot {
            ShredFlags::LAST_SHRED_IN_SLOT // also implies DATA_COMPLETE_SHRED
        } else {
            ShredFlags::DATA_COMPLETE_SHRED
        };
        let num_data_shreds = shred.common_header.index - next_shred_index;
        stats.record_num_data_shreds(num_data_shreds as usize);
    }
    stats.gen_data_elapsed += now.elapsed().as_micros() as u64;
    let now = Instant::now();
    // Group shreds by their respective erasure-batch.
    let batches: Vec<&mut [Shred]> = shreds
        .chunk_by_mut(|a, b| a.fec_set_index() == b.fec_set_index())
        .collect();
    if let Some(chained_merkle_root) = chained_merkle_root {
        // We have to process erasure batches serially because the Merkle tree
        // (and so the signature) cannot be computed without the Merkle root of
        // the previous erasure batch.
        batches
            .into_iter()
            .try_fold(chained_merkle_root, |chained_merkle_root, batch| {
                finish_erasure_batch(
                    Some(thread_pool),
                    keypair,
                    batch,
                    Some(chained_merkle_root),
                    reed_solomon_cache,
                )
            })?;
    } else if batches.len() <= 1 {
        for batch in batches {
            finish_erasure_batch(
                Some(thread_pool),
                keypair,
                batch,
                None, // chained_merkle_root
                reed_solomon_cache,
            )?;
        }
    } else {
        thread_pool.install(|| {
            batches.into_par_iter().try_for_each(|batch| {
                finish_erasure_batch(
                    None, // thread_pool
                    keypair,
                    batch,
                    None, // chained_merkle_root
                    reed_solomon_cache,
                )
                .map(|_| ())
            })
        })?;
    }
    stats.gen_coding_elapsed += now.elapsed().as_micros() as u64;
    Ok(shreds)
}

// Given shreds of the same erasure batch:
// - Writes common and {data,coding} headers into shreds' payload.
// - Fills in erasure code buffers in the coding shreds.
// - Sets the chained_merkle_root for each shred.
// - Computes the Merkle tree for the erasure batch.
// - Signs the root of the Merkle tree.
// - Populates Merkle proof for each shred and attaches the signature.
// Returns the root of the Merkle tree (for chaining Merkle roots).
fn finish_erasure_batch(
    thread_pool: Option<&ThreadPool>,
    keypair: &Keypair,
    shreds: &mut [Shred],
    // The Merkle root of the previous erasure batch if chained.
    chained_merkle_root: Option<Hash>,
    reed_solomon_cache: &ReedSolomonCache,
) -> Result</*Merkle root:*/ Hash, Error> {
    debug_assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);
    // Write common and {data,coding} headers into shreds' payload.
    fn write_headers(shred: &mut Shred) -> Result<(), bincode::Error> {
        match shred {
            Shred::ShredCode(shred) => bincode::serialize_into(
                &mut shred.payload[..],
                &(&shred.common_header, &shred.coding_header),
            ),
            Shred::ShredData(shred) => bincode::serialize_into(
                &mut shred.payload[..],
                &(&shred.common_header, &shred.data_header),
            ),
        }
    }
    match thread_pool {
        None => shreds.iter_mut().try_for_each(write_headers),
        Some(thread_pool) => {
            thread_pool.install(|| shreds.par_iter_mut().try_for_each(write_headers))
        }
    }?;
    // Fill in erasure code buffers in the coding shreds.
    let CodingShredHeader {
        num_data_shreds,
        num_coding_shreds,
        ..
    } = {
        // Last shred in the erasure batch should be a coding shred.
        let Some(Shred::ShredCode(shred)) = shreds.last() else {
            return Err(Error::from(TooFewParityShards));
        };
        shred.coding_header
    };
    let num_data_shreds = usize::from(num_data_shreds);
    let num_coding_shreds = usize::from(num_coding_shreds);
    let erasure_batch_size = num_data_shreds + num_coding_shreds;
    reed_solomon_cache
        .get(num_data_shreds, num_coding_shreds)?
        .encode(
            shreds
                .iter_mut()
                .map(Shred::erasure_shard_mut)
                .collect::<Result<Vec<&mut [u8]>, _>>()?,
        )?;
    // Set the chained_merkle_root for each shred.
    if let Some(chained_merkle_root) = chained_merkle_root {
        for shred in shreds.iter_mut() {
            shred.set_chained_merkle_root(&chained_merkle_root)?;
        }
    }
    // Compute the Merkle tree for the erasure batch.
    let tree = match thread_pool {
        None => {
            let nodes = shreds.iter().map(Shred::merkle_node);
            make_merkle_tree(nodes)
        }
        Some(thread_pool) => make_merkle_tree(thread_pool.install(|| {
            shreds
                .par_iter()
                .map(Shred::merkle_node)
                .collect::<Vec<_>>()
        })),
    }?;
    // Sign the root of the Merkle tree.
    let root = tree.last().copied().ok_or(Error::InvalidMerkleProof)?;
    let signature = keypair.sign_message(root.as_ref());
    // Populate merkle proof for all shreds and attach signature.
    for (index, shred) in shreds.iter_mut().enumerate() {
        let proof = make_merkle_proof(index, erasure_batch_size, &tree);
        shred.set_merkle_proof(proof)?;
        shred.set_signature(signature);
        debug_assert!(shred.verify(&keypair.pubkey()));
        debug_assert_matches!(shred.sanitize(), Ok(()));
        // Assert that shred payload is fully populated.
        debug_assert_eq!(shred, {
            let shred = shred.payload().clone();
            &Shred::from_payload(shred).unwrap()
        });
    }
    Ok(root)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::shred::{ShredFlags, ShredId, SignedData},
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::{seq::SliceRandom, CryptoRng, Rng},
        rayon::ThreadPoolBuilder,
        reed_solomon_erasure::Error::TooFewShardsPresent,
        solana_sdk::{
            packet::PACKET_DATA_SIZE,
            signature::{Keypair, Signer},
        },
        std::{cmp::Ordering, collections::HashMap, iter::repeat_with},
        test_case::test_case,
    };

    // Total size of a data shred including headers and merkle proof.
    fn shred_data_size_of_payload(proof_size: u8, chained: bool, resigned: bool) -> usize {
        assert!(chained || !resigned);
        ShredData::SIZE_OF_HEADERS
            + ShredData::capacity(proof_size, chained, resigned).unwrap()
            + if chained { SIZE_OF_MERKLE_ROOT } else { 0 }
            + usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY
            + if resigned { SIZE_OF_SIGNATURE } else { 0 }
    }

    // Merkle proof is generated and signed after coding shreds are generated.
    // All payload excluding merkle proof and the signature are erasure coded.
    // Therefore the data capacity is equal to erasure encoded shard size minus
    // size of erasure encoded header.
    fn shred_data_capacity(proof_size: u8, chained: bool, resigned: bool) -> usize {
        const SIZE_OF_ERASURE_ENCODED_HEADER: usize =
            ShredData::SIZE_OF_HEADERS - SIZE_OF_SIGNATURE;
        ShredCode::capacity(proof_size, chained, resigned).unwrap() - SIZE_OF_ERASURE_ENCODED_HEADER
    }

    fn shred_data_size_of_erasure_encoded_slice(
        proof_size: u8,
        chained: bool,
        resigned: bool,
    ) -> usize {
        ShredData::SIZE_OF_PAYLOAD
            - SIZE_OF_SIGNATURE
            - if chained { SIZE_OF_MERKLE_ROOT } else { 0 }
            - usize::from(proof_size) * SIZE_OF_MERKLE_PROOF_ENTRY
            - if resigned { SIZE_OF_SIGNATURE } else { 0 }
    }

    #[test_case(false, false)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_shred_data_size_of_payload(chained: bool, resigned: bool) {
        for proof_size in 0..0x15 {
            assert_eq!(
                ShredData::SIZE_OF_PAYLOAD,
                shred_data_size_of_payload(proof_size, chained, resigned)
            );
        }
    }

    #[test_case(false, false)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_shred_data_capacity(chained: bool, resigned: bool) {
        for proof_size in 0..0x15 {
            assert_eq!(
                ShredData::capacity(proof_size, chained, resigned).unwrap(),
                shred_data_capacity(proof_size, chained, resigned)
            );
        }
    }

    #[test_case(false, false)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_shred_code_capacity(chained: bool, resigned: bool) {
        for proof_size in 0..0x15 {
            assert_eq!(
                ShredCode::capacity(proof_size, chained, resigned).unwrap(),
                shred_data_size_of_erasure_encoded_slice(proof_size, chained, resigned),
            );
        }
    }

    #[test]
    fn test_merkle_proof_entry_from_hash() {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 32] = rng.gen();
        let hash = Hash::from(bytes);
        let entry = &hash.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
        let entry = MerkleProofEntry::try_from(entry).unwrap();
        assert_eq!(entry, &bytes[..SIZE_OF_MERKLE_PROOF_ENTRY]);
    }

    #[test]
    fn test_get_merkle_tree_size() {
        const TREE_SIZE: [usize; 15] = [0, 1, 3, 6, 7, 11, 12, 14, 15, 20, 21, 23, 24, 27, 28];
        for (num_shreds, size) in TREE_SIZE.into_iter().enumerate() {
            assert_eq!(get_merkle_tree_size(num_shreds), size);
        }
    }

    #[test]
    fn test_make_merkle_proof_error() {
        let mut rng = rand::thread_rng();
        let nodes = repeat_with(|| rng.gen::<[u8; 32]>()).map(Hash::from);
        let nodes: Vec<_> = nodes.take(5).collect();
        let size = nodes.len();
        let tree = make_merkle_tree(nodes.into_iter().map(Ok)).unwrap();
        for index in size..size + 3 {
            assert_matches!(
                make_merkle_proof(index, size, &tree).next(),
                Some(Err(Error::InvalidMerkleProof))
            );
        }
    }

    fn run_merkle_tree_round_trip<R: Rng>(rng: &mut R, size: usize) {
        let nodes = repeat_with(|| rng.gen::<[u8; 32]>()).map(Hash::from);
        let nodes: Vec<_> = nodes.take(size).collect();
        let tree = make_merkle_tree(nodes.iter().cloned().map(Ok)).unwrap();
        let root = tree.last().copied().unwrap();
        for index in 0..size {
            for (k, &node) in nodes.iter().enumerate() {
                let proof = make_merkle_proof(index, size, &tree).map(Result::unwrap);
                if k == index {
                    assert_eq!(root, get_merkle_root(k, node, proof).unwrap());
                } else {
                    assert_ne!(root, get_merkle_root(k, node, proof).unwrap());
                }
            }
        }
    }

    #[test]
    fn test_merkle_tree_round_trip() {
        let mut rng = rand::thread_rng();
        for size in 1..=143 {
            run_merkle_tree_round_trip(&mut rng, size);
        }
    }

    #[test_case(19, false, false)]
    #[test_case(19, true, false)]
    #[test_case(19, true, true)]
    #[test_case(31, false, false)]
    #[test_case(31, true, false)]
    #[test_case(31, true, true)]
    #[test_case(32, false, false)]
    #[test_case(32, true, false)]
    #[test_case(32, true, true)]
    #[test_case(33, false, false)]
    #[test_case(33, true, false)]
    #[test_case(33, true, true)]
    #[test_case(37, false, false)]
    #[test_case(37, true, false)]
    #[test_case(37, true, true)]
    #[test_case(64, false, false)]
    #[test_case(64, true, false)]
    #[test_case(64, true, true)]
    #[test_case(73, false, false)]
    #[test_case(73, true, false)]
    #[test_case(73, true, true)]
    fn test_recover_merkle_shreds(num_shreds: usize, chained: bool, resigned: bool) {
        let mut rng = rand::thread_rng();
        let reed_solomon_cache = ReedSolomonCache::default();
        for num_data_shreds in 1..num_shreds {
            let num_coding_shreds = num_shreds - num_data_shreds;
            run_recover_merkle_shreds(
                &mut rng,
                chained,
                resigned,
                num_data_shreds,
                num_coding_shreds,
                &reed_solomon_cache,
            );
        }
    }

    fn run_recover_merkle_shreds<R: Rng + CryptoRng>(
        rng: &mut R,
        chained: bool,
        resigned: bool,
        num_data_shreds: usize,
        num_coding_shreds: usize,
        reed_solomon_cache: &ReedSolomonCache,
    ) {
        let keypair = Keypair::new();
        let num_shreds = num_data_shreds + num_coding_shreds;
        let proof_size = get_proof_size(num_shreds);
        let capacity = ShredData::capacity(proof_size, chained, resigned).unwrap();
        let common_header = ShredCommonHeader {
            signature: Signature::default(),
            shred_variant: ShredVariant::MerkleData {
                proof_size,
                chained,
                resigned,
            },
            slot: 145_865_705,
            index: 1835,
            version: rng.gen(),
            fec_set_index: 1835,
        };
        let data_header = {
            let reference_tick = rng.gen_range(0..0x40);
            DataShredHeader {
                parent_offset: rng.gen::<u16>().max(1),
                flags: ShredFlags::from_bits_retain(reference_tick),
                size: 0,
            }
        };
        let coding_header = CodingShredHeader {
            num_data_shreds: num_data_shreds as u16,
            num_coding_shreds: num_coding_shreds as u16,
            position: 0,
        };
        let mut shreds = Vec::with_capacity(num_shreds);
        for i in 0..num_data_shreds {
            let common_header = ShredCommonHeader {
                index: common_header.index + i as u32,
                ..common_header
            };
            let size = ShredData::SIZE_OF_HEADERS + rng.gen_range(0..capacity);
            let data_header = DataShredHeader {
                size: size as u16,
                ..data_header
            };
            let mut payload = vec![0u8; ShredData::SIZE_OF_PAYLOAD];
            bincode::serialize_into(&mut payload[..], &(&common_header, &data_header)).unwrap();
            rng.fill(&mut payload[ShredData::SIZE_OF_HEADERS..size]);
            let shred = ShredData {
                common_header,
                data_header,
                payload: Payload::from(payload),
            };
            shreds.push(Shred::ShredData(shred));
        }
        let data: Vec<_> = shreds
            .iter()
            .map(Shred::erasure_shard)
            .collect::<Result<_, _>>()
            .unwrap();
        let mut parity = vec![vec![0u8; data[0].len()]; num_coding_shreds];
        reed_solomon_cache
            .get(num_data_shreds, num_coding_shreds)
            .unwrap()
            .encode_sep(&data, &mut parity[..])
            .unwrap();
        for (i, code) in parity.into_iter().enumerate() {
            let common_header = ShredCommonHeader {
                shred_variant: ShredVariant::MerkleCode {
                    proof_size,
                    chained,
                    resigned,
                },
                index: common_header.index + i as u32 + 7,
                ..common_header
            };
            let coding_header = CodingShredHeader {
                position: i as u16,
                ..coding_header
            };
            let mut payload = vec![0u8; ShredCode::SIZE_OF_PAYLOAD];
            bincode::serialize_into(&mut payload[..], &(&common_header, &coding_header)).unwrap();
            payload[ShredCode::SIZE_OF_HEADERS..ShredCode::SIZE_OF_HEADERS + code.len()]
                .copy_from_slice(&code);
            let shred = ShredCode {
                common_header,
                coding_header,
                payload: Payload::from(payload),
            };
            shreds.push(Shred::ShredCode(shred));
        }
        let nodes = shreds.iter().map(Shred::merkle_node);
        let tree = make_merkle_tree(nodes).unwrap();
        for (index, shred) in shreds.iter_mut().enumerate() {
            let proof = make_merkle_proof(index, num_shreds, &tree);
            shred.set_merkle_proof(proof).unwrap();
            let data = shred.signed_data().unwrap();
            let signature = keypair.sign_message(data.as_ref());
            shred.set_signature(signature);
            assert!(shred.verify(&keypair.pubkey()));
            assert_matches!(shred.sanitize(), Ok(()));
        }
        verify_erasure_recovery(rng, &shreds, reed_solomon_cache);
    }

    fn verify_erasure_recovery<R: Rng>(
        rng: &mut R,
        shreds: &[Shred],
        reed_solomon_cache: &ReedSolomonCache,
    ) {
        assert_eq!(shreds.iter().map(Shred::signature).dedup().count(), 1);
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);
        let num_shreds = shreds.len();
        let num_data_shreds = shreds
            .iter()
            .filter(|shred| shred.shred_type() == ShredType::Data)
            .count();
        for size in 0..num_shreds {
            let mut shreds = Vec::from(shreds);
            shreds.shuffle(rng);
            let mut removed_shreds = shreds.split_off(size);
            // Should at least contain one coding shred.
            if shreds.iter().all(|shred| {
                matches!(
                    shred.common_header().shred_variant,
                    ShredVariant::MerkleData { .. }
                )
            }) {
                assert_matches!(
                    recover(shreds, reed_solomon_cache).err(),
                    Some(Error::ErasureError(TooFewParityShards))
                );
                continue;
            }
            if shreds.len() < num_data_shreds {
                assert_matches!(
                    recover(shreds, reed_solomon_cache).err(),
                    Some(Error::ErasureError(TooFewShardsPresent))
                );
                continue;
            }
            let recovered_shreds: Vec<_> = recover(shreds, reed_solomon_cache)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(size + recovered_shreds.len(), num_shreds);
            assert_eq!(recovered_shreds.len(), removed_shreds.len());
            removed_shreds.sort_by(|a, b| {
                if a.shred_type() == b.shred_type() {
                    a.index().cmp(&b.index())
                } else if a.shred_type() == ShredType::Data {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            });
            assert_eq!(recovered_shreds, removed_shreds);
        }
    }

    #[test]
    fn test_get_proof_size() {
        assert_eq!(get_proof_size(0), 0);
        assert_eq!(get_proof_size(1), 0);
        assert_eq!(get_proof_size(2), 1);
        assert_eq!(get_proof_size(3), 2);
        assert_eq!(get_proof_size(4), 2);
        assert_eq!(get_proof_size(5), 3);
        assert_eq!(get_proof_size(63), 6);
        assert_eq!(get_proof_size(64), 6);
        assert_eq!(get_proof_size(65), 7);
        assert_eq!(get_proof_size(usize::MAX - 1), 64);
        assert_eq!(get_proof_size(usize::MAX), 64);
        for proof_size in 1u8..9 {
            let max_num_shreds = 1usize << u32::from(proof_size);
            let min_num_shreds = (max_num_shreds >> 1) + 1;
            for num_shreds in min_num_shreds..=max_num_shreds {
                assert_eq!(get_proof_size(num_shreds), proof_size);
            }
        }
    }

    #[test_case(0, false, false)]
    #[test_case(0, false, true)]
    #[test_case(0, true, false)]
    #[test_case(0, true, true)]
    #[test_case(15600, false, false)]
    #[test_case(15600, false, true)]
    #[test_case(15600, true, false)]
    #[test_case(15600, true, true)]
    #[test_case(31200, false, false)]
    #[test_case(31200, false, true)]
    #[test_case(31200, true, false)]
    #[test_case(31200, true, true)]
    #[test_case(46800, false, false)]
    #[test_case(46800, false, true)]
    #[test_case(46800, true, false)]
    #[test_case(46800, true, true)]
    fn test_make_shreds_from_data(data_size: usize, chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let data_size = data_size.saturating_sub(16);
        let reed_solomon_cache = ReedSolomonCache::default();
        for data_size in data_size..data_size + 32 {
            run_make_shreds_from_data(
                &mut rng,
                data_size,
                chained,
                is_last_in_slot,
                &reed_solomon_cache,
            );
        }
    }

    #[test_case(false, false)]
    #[test_case(false, true)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_make_shreds_from_data_rand(chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let reed_solomon_cache = ReedSolomonCache::default();
        for _ in 0..32 {
            let data_size = rng.gen_range(0..31200 * 7);
            run_make_shreds_from_data(
                &mut rng,
                data_size,
                chained,
                is_last_in_slot,
                &reed_solomon_cache,
            );
        }
    }

    #[ignore]
    #[test_case(false, false)]
    #[test_case(false, true)]
    #[test_case(true, false)]
    #[test_case(true, true)]
    fn test_make_shreds_from_data_paranoid(chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let reed_solomon_cache = ReedSolomonCache::default();
        for data_size in 0..=PACKET_DATA_SIZE * 4 * 64 {
            run_make_shreds_from_data(
                &mut rng,
                data_size,
                chained,
                is_last_in_slot,
                &reed_solomon_cache,
            );
        }
    }

    fn run_make_shreds_from_data<R: Rng>(
        rng: &mut R,
        data_size: usize,
        chained: bool,
        is_last_in_slot: bool,
        reed_solomon_cache: &ReedSolomonCache,
    ) {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let keypair = Keypair::new();
        let chained_merkle_root = chained.then(|| Hash::new_from_array(rng.gen()));
        let resigned = chained && is_last_in_slot;
        let slot = 149_745_689;
        let parent_slot = slot - rng.gen_range(1..65536);
        let shred_version = rng.gen();
        let reference_tick = rng.gen_range(1..64);
        let next_shred_index = rng.gen_range(0..671);
        let next_code_index = rng.gen_range(0..781);
        let mut data = vec![0u8; data_size];
        rng.fill(&mut data[..]);
        let shreds = make_shreds_from_data(
            &thread_pool,
            &keypair,
            chained_merkle_root,
            &data[..],
            slot,
            parent_slot,
            shred_version,
            reference_tick,
            is_last_in_slot,
            next_shred_index,
            next_code_index,
            reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .unwrap();
        let data_shreds: Vec<_> = shreds
            .iter()
            .filter_map(|shred| match shred {
                Shred::ShredCode(_) => None,
                Shred::ShredData(shred) => Some(shred),
            })
            .collect();
        // Assert that the input data can be recovered from data shreds.
        assert_eq!(
            data,
            data_shreds
                .iter()
                .flat_map(|shred| shred.data().unwrap())
                .copied()
                .collect::<Vec<_>>()
        );
        // Assert that shreds sanitize and verify.
        let pubkey = keypair.pubkey();
        for shred in &shreds {
            assert!(shred.verify(&pubkey));
            assert_matches!(shred.sanitize(), Ok(()));
            let ShredCommonHeader {
                signature,
                shred_variant,
                slot,
                index,
                version,
                fec_set_index: _,
            } = *shred.common_header();
            let shred_type = ShredType::from(shred_variant);
            let key = ShredId::new(slot, index, shred_type);
            let merkle_root = shred.merkle_root().unwrap();
            let chained_merkle_root = if chained {
                Some(shred.chained_merkle_root().unwrap())
            } else {
                assert_matches!(shred.chained_merkle_root(), Err(Error::InvalidShredVariant));
                None
            };
            assert!(signature.verify(pubkey.as_ref(), merkle_root.as_ref()));
            // Verify shred::layout api.
            let shred = shred.payload();
            assert_eq!(shred::layout::get_signature(shred), Some(signature));
            assert_eq!(
                shred::layout::get_shred_variant(shred).unwrap(),
                shred_variant
            );
            assert_eq!(shred::layout::get_shred_type(shred).unwrap(), shred_type);
            assert_eq!(shred::layout::get_slot(shred), Some(slot));
            assert_eq!(shred::layout::get_index(shred), Some(index));
            assert_eq!(shred::layout::get_version(shred), Some(version));
            assert_eq!(shred::layout::get_shred_id(shred), Some(key));
            assert_eq!(shred::layout::get_merkle_root(shred), Some(merkle_root));
            assert_eq!(
                shred::layout::get_chained_merkle_root(shred),
                chained_merkle_root
            );
            assert_eq!(shred::layout::get_signed_data_offsets(shred), None);
            let data = shred::layout::get_signed_data(shred).unwrap();
            assert_eq!(data, SignedData::MerkleRoot(merkle_root));
            assert!(signature.verify(pubkey.as_ref(), data.as_ref()));
        }
        // Verify common, data and coding headers.
        let mut num_data_shreds = 0;
        let mut num_coding_shreds = 0;
        for shred in &shreds {
            let common_header = shred.common_header();
            assert_eq!(common_header.slot, slot);
            assert_eq!(common_header.version, shred_version);
            let proof_size = shred.proof_size().unwrap();
            match shred {
                Shred::ShredCode(shred) => {
                    assert_eq!(common_header.index, next_code_index + num_coding_shreds);
                    assert_eq!(
                        common_header.shred_variant,
                        ShredVariant::MerkleCode {
                            proof_size,
                            chained,
                            resigned
                        }
                    );
                    num_coding_shreds += 1;
                    let shred = shred.payload();
                    assert_matches!(
                        shred::layout::get_flags(shred),
                        Err(Error::InvalidShredType)
                    );
                    assert_matches!(shred::layout::get_data(shred), Err(Error::InvalidShredType));
                }
                Shred::ShredData(shred) => {
                    assert_eq!(common_header.index, next_shred_index + num_data_shreds);
                    assert_eq!(
                        common_header.shred_variant,
                        ShredVariant::MerkleData {
                            proof_size,
                            chained,
                            resigned
                        }
                    );
                    assert!(common_header.fec_set_index <= common_header.index);
                    assert_eq!(
                        Slot::from(shred.data_header.parent_offset),
                        slot - parent_slot
                    );
                    assert_eq!(
                        (shred.data_header.flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(),
                        reference_tick,
                    );
                    let data_header = shred.data_header;
                    let data = shred.data().unwrap();
                    let shred = shred.payload();
                    assert_eq!(
                        shred::layout::get_parent_offset(shred),
                        Some(u16::try_from(slot - parent_slot).unwrap()),
                    );
                    assert_eq!(
                        shred::layout::get_parent_offset(shred).unwrap(),
                        data_header.parent_offset
                    );
                    assert_eq!(shred::layout::get_flags(shred).unwrap(), data_header.flags);
                    assert_eq!(shred::layout::get_data(shred).unwrap(), data);
                    assert_eq!(
                        shred::layout::get_reference_tick(shred).unwrap(),
                        reference_tick
                    );
                    num_data_shreds += 1;
                }
            }
        }
        assert!(num_coding_shreds >= num_data_shreds);
        // Verify chained Merkle roots.
        if let Some(chained_merkle_root) = chained_merkle_root {
            let chained_merkle_roots: HashMap<u32, Hash> =
                std::iter::once((0, chained_merkle_root))
                    .chain(
                        shreds
                            .iter()
                            .sorted_unstable_by_key(|shred| shred.fec_set_index())
                            .dedup_by(|shred, other| shred.fec_set_index() == other.fec_set_index())
                            .map(|shred| (shred.fec_set_index(), shred.merkle_root().unwrap())),
                    )
                    .tuple_windows()
                    .map(|((_, merkle_root), (fec_set_index, _))| (fec_set_index, merkle_root))
                    .collect();
            for shred in &shreds {
                assert_eq!(
                    shred.chained_merkle_root().unwrap(),
                    chained_merkle_roots[&shred.fec_set_index()]
                );
            }
        }
        // Assert that only the last shred is LAST_SHRED_IN_SLOT.
        assert_eq!(
            data_shreds
                .iter()
                .filter(|shred| shred
                    .data_header
                    .flags
                    .contains(ShredFlags::LAST_SHRED_IN_SLOT))
                .count(),
            if is_last_in_slot { 1 } else { 0 }
        );
        assert_eq!(
            data_shreds
                .last()
                .unwrap()
                .data_header
                .flags
                .contains(ShredFlags::LAST_SHRED_IN_SLOT),
            is_last_in_slot
        );
        // Assert that the last erasure batch has 32+ data shreds.
        if is_last_in_slot {
            let fec_set_index = shreds.iter().map(Shred::fec_set_index).max().unwrap();
            assert!(
                shreds
                    .iter()
                    .filter(|shred| shred.fec_set_index() == fec_set_index)
                    .filter(|shred| shred.shred_type() == ShredType::Data)
                    .count()
                    >= 32
            )
        }
        // Assert that data shreds can be recovered from coding shreds.
        let recovered_data_shreds: Vec<_> = shreds
            .iter()
            .filter_map(|shred| match shred {
                Shred::ShredCode(_) => Some(shred.clone()),
                Shred::ShredData(_) => None,
            })
            .group_by(|shred| shred.common_header().fec_set_index)
            .into_iter()
            .flat_map(|(_, shreds)| {
                recover(shreds.collect(), reed_solomon_cache)
                    .unwrap()
                    .map(Result::unwrap)
            })
            .collect();
        assert_eq!(recovered_data_shreds.len(), data_shreds.len());
        for (shred, other) in recovered_data_shreds.into_iter().zip(data_shreds) {
            match shred {
                Shred::ShredCode(_) => panic!("Invalid shred type!"),
                Shred::ShredData(shred) => assert_eq!(shred, *other),
            }
        }
        // Verify erasure recovery for each erasure batch.
        for shreds in shreds
            .into_iter()
            .into_group_map_by(Shred::fec_set_index)
            .values()
        {
            verify_erasure_recovery(rng, shreds, reed_solomon_cache);
        }
    }
}
