/// Block components using wincode serialization.
///
/// A `BlockComponent` represents either an entry batch or a special block marker.
/// Most of the time, a block component contains a vector of entries. However, periodically,
/// there are special messages that a block needs to contain. To accommodate these special
/// messages, `BlockComponent` allows for the inclusion of special data via `VersionedBlockMarker`.
///
/// ## Serialization Layouts
///
/// All numeric fields use little-endian encoding.
///
/// ### BlockComponent with EntryBatch
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Entry Count                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry 0           (variable)    │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry 1           (variable)    │
/// ├─────────────────────────────────────────┤
/// │ ...                                     │
/// ├─────────────────────────────────────────┤
/// │ bincode Entry N-1         (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockComponent with BlockMarker
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Entry Count = 0              (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Marker Version               (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Marker Data               (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockMarkerV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Variant ID                   (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Byte Length                  (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Variant Data              (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockHeaderV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Parent Slot                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Parent Block ID             (32 bytes)  │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### UpdateParentV1 Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Parent Slot                  (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Parent Block ID             (32 bytes)  │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### BlockFooterV1 Layout
/// ```text
/// ┌──────────────────────────────────────────────┐
/// │ Bank Hash                        (32 bytes)  │
/// ├──────────────────────────────────────────────┤
/// │ Producer Time Nanos               (8 bytes)  │
/// ├──────────────────────────────────────────────┤
/// │ User Agent Length                 (1 byte)   │
/// ├──────────────────────────────────────────────┤
/// │ User Agent Bytes               (0-255 bytes) │
/// ├──────────────────────────────────────────────┤
/// │ Block Final Cert Present          (1 byte)   │
/// ├──────────────────────────────────────────────┤
/// │ BlockFinalizationCert (if present, variable) │
/// ├──────────────────────────────────────────────┤
/// │ Skip reward cert Present     (1 byte)        │
/// ├──────────────────────────────────────────────┤
/// │ SkipRewardCert (if present, variable)        │
/// ├──────────────────────────────────────────────┤
/// │ Notar reward cert Present    (1 byte)        │
/// ├──────────────────────────────────────────────┤
/// │ NotarRewardCert (if present, variable)       │
/// └──────────────────────────────────────────────┘
/// ```
///
/// ### BlockFinalizationCert Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Slot                         (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Block ID                    (32 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Final Aggregate (VotesAggregate)        │
/// ├─────────────────────────────────────────┤
/// │ Notar Aggregate Present      (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ Notar Aggregate (if present)            │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### VotesAggregate Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ BLS Signature Compressed    (96 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Bitmap Length                (2 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Bitmap                    (variable)    │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### GenesisCertificate Layout
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Genesis Slot                  (8 bytes) │
/// ├─────────────────────────────────────────┤
/// │ Genesis Block ID             (32 bytes) │
/// ├─────────────────────────────────────────┤
/// │ BLS Signature               (192 bytes) │
/// ├─────────────────────────────────────────┤
/// │ Bitmap length (max 512)       (8 bytes) │
/// ├─────────────────────────────────────────┤
/// │ Bitmap                (up to 512 bytes) │
/// └─────────────────────────────────────────┘
/// ```
use {
    crate::entry::{Entry, MaxDataShredsLen},
    agave_votor_messages::{
        certificate::{CertSignature, CertificateType, GenesisCert},
        consensus_message::Block,
        reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
        unverified_vote_message::UnverifiedCertificate,
    },
    solana_bls_signatures::{
        BlsError, Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        signature::AsSignatureAffine,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_packet::PACKET_DATA_SIZE,
    std::mem::MaybeUninit,
    wincode::{
        ReadResult, SchemaRead, SchemaWrite, TypeMeta, WriteResult,
        config::{Config, ConfigCore, DefaultConfig},
        containers::Vec as WincodeVec,
        error::write_length_encoding_overflow,
        io::{Reader, Writer},
        len::{BincodeLen, FixIntLen},
        pod_wrapper,
    },
};

pod_wrapper! {
    // Use `BLSSignature` directly once `BLSSignature` wincode support
    // is released in solana-sdk.
    unsafe struct PodBLSSignature(BLSSignature);
    // Use `BLSSignatureCompressed` directly once `BLSSignature` wincode support
    // is released in solana-sdk.
    unsafe struct PodBLSSignatureCompressed(BLSSignatureCompressed);
}

/// Wraps a value with a u16 length prefix for TLV-style serialization.
///
/// The length prefix represents the serialized byte size of the inner value.
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite)]
pub struct LengthPrefixed<T> {
    len: u16,
    inner: T,
}

unsafe impl<'de, T, C: ConfigCore> SchemaRead<'de, C> for LengthPrefixed<T>
where
    T: SchemaRead<'de, C, Dst = T> + SchemaWrite<C, Src = T>,
{
    type Dst = Self;

    const TYPE_META: TypeMeta = TypeMeta::join_types([
        <u16 as SchemaRead<'de, C>>::TYPE_META,
        <T as SchemaRead<'de, C>>::TYPE_META,
    ])
    .keep_zero_copy(false);

    fn read(mut reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
        let len = <u16 as SchemaRead<'de, C>>::get(reader.by_ref())?;
        let inner = T::get(reader)?;
        let inner_size = T::size_of(&inner)
            .map_err(|_| wincode::ReadError::Custom("LengthPrefixed: inner size_of overflow"))?;

        if inner_size != usize::from(len) {
            return Err(wincode::ReadError::Custom(
                "LengthPrefixed: inner serialized size does not match length prefix",
            ));
        }

        dst.write(Self { len, inner });
        Ok(())
    }
}

impl<T> LengthPrefixed<T>
where
    T: SchemaWrite<DefaultConfig, Src = T>,
{
    pub fn new(inner: T) -> Self {
        let inner_size = T::size_of(&inner).unwrap();
        let len = inner_size
            .try_into()
            .map_err(|_| write_length_encoding_overflow("u16::MAX"))
            .unwrap();
        Self { len, inner }
    }
}

impl<T> LengthPrefixed<T> {
    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlockComponentError {
    #[error("Entry count {count} exceeds max {max}")]
    TooManyEntries { count: usize, max: usize },
    #[error("Entry batch cannot be empty")]
    EmptyEntryBatch,
}

/// Block production metadata. User agent is capped at 255 bytes.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct BlockFooterV1 {
    pub bank_hash: Hash,
    pub block_producer_time_nanos: u64,
    #[wincode(with = "WincodeVec<u8, FixIntLen<u8>>")]
    pub block_user_agent: Vec<u8>,
    pub block_final_cert: Option<BlockFinalizationCert>,
    pub skip_reward_cert: Option<SkipRewardCertificate>,
    pub notar_reward_cert: Option<NotarRewardCertificate>,
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct BlockHeaderV1 {
    pub parent_slot: Slot,
    pub parent_block_id: Hash,
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct UpdateParentV1 {
    pub new_parent_slot: Slot,
    pub new_parent_block_id: Hash,
}

/// Attests to genesis block finalization with a BLS aggregate signature.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct GenesisCertBlockMarker {
    pub slot: Slot,
    pub block_id: Hash,
    #[wincode(with = "PodBLSSignature")]
    pub bls_signature: BLSSignature,
    #[wincode(with = "WincodeVec<u8, BincodeLen>")]
    pub bitmap: Vec<u8>,
}

impl GenesisCertBlockMarker {
    /// Max bitmap size in bytes (supports up to 4096 validators).
    pub const MAX_BITMAP_SIZE: usize = 512;
}

impl TryFrom<GenesisCert> for GenesisCertBlockMarker {
    type Error = String;

    fn try_from(cert: GenesisCert) -> Result<Self, Self::Error> {
        if cert.signature.bitmap.len() > Self::MAX_BITMAP_SIZE {
            return Err(format!(
                "bitmap size {} exceeds max {}",
                cert.signature.bitmap.len(),
                Self::MAX_BITMAP_SIZE
            ));
        }
        Ok(Self {
            slot: cert.block.slot,
            block_id: cert.block.block_id,
            bls_signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct BlockFinalizationCert {
    pub slot: Slot,
    pub block_id: Hash,
    pub final_aggregate: VotesAggregate,
    pub notar_aggregate: Option<VotesAggregate>,
}

impl BlockFinalizationCert {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests() -> BlockFinalizationCert {
        BlockFinalizationCert {
            slot: 1234567890,
            block_id: Hash::new_from_array([1u8; 32]),
            final_aggregate: VotesAggregate {
                signature: BLSSignatureCompressed(
                    [0; solana_bls_signatures::BLS_SIGNATURE_COMPRESSED_SIZE],
                ),
                bitmap: vec![42; 64],
            },
            notar_aggregate: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct VotesAggregate {
    #[wincode(with = "PodBLSSignatureCompressed")]
    signature: BLSSignatureCompressed,
    #[wincode(with = "WincodeVec<u8, FixIntLen<u16>>")]
    bitmap: Vec<u8>,
}

impl VotesAggregate {
    /// Creates a VotesAggregate from a Certificate's signature and bitmap.
    ///
    /// # Panics
    /// Panics if the signature cannot be converted to compressed format.
    /// This should never happen for valid certificates from the consensus pool.
    pub fn from_cert_signature(signature: CertSignature) -> Self {
        Self {
            signature: BLSSignatureCompressed::try_from(&signature.signature)
                .expect("valid certificate signature should convert to compressed format"),
            bitmap: signature.bitmap,
        }
    }

    /// Uncompresses the signature.
    pub fn uncompress_signature(&self) -> Result<BLSSignature, BlsError> {
        Ok(BLSSignature::from(self.signature.try_as_affine()?))
    }

    /// Consumes self and returns the bitmap.
    pub fn into_bitmap(self) -> Vec<u8> {
        self.bitmap
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub enum VersionedBlockFooter {
    #[wincode(tag = 1)]
    V1(BlockFooterV1),
}

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub enum VersionedBlockHeader {
    #[wincode(tag = 1)]
    V1(BlockHeaderV1),
}

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub enum VersionedUpdateParent {
    #[wincode(tag = 1)]
    V1(UpdateParentV1),
}

/// TLV-encoded marker variants.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub enum BlockMarkerV1 {
    BlockFooter(LengthPrefixed<VersionedBlockFooter>),
    BlockHeader(LengthPrefixed<VersionedBlockHeader>),
    UpdateParent(LengthPrefixed<VersionedUpdateParent>),
    GenesisCertificate(LengthPrefixed<GenesisCertBlockMarker>),
}

impl BlockMarkerV1 {
    pub fn new_block_footer(f: VersionedBlockFooter) -> Self {
        Self::BlockFooter(LengthPrefixed::new(f))
    }

    pub fn new_block_header(h: VersionedBlockHeader) -> Self {
        Self::BlockHeader(LengthPrefixed::new(h))
    }

    pub fn new_update_parent(u: VersionedUpdateParent) -> Self {
        Self::UpdateParent(LengthPrefixed::new(u))
    }

    pub fn new_genesis_certificate(c: GenesisCertBlockMarker) -> Self {
        Self::GenesisCertificate(LengthPrefixed::new(c))
    }

    pub fn as_block_footer(&self) -> Option<&VersionedBlockFooter> {
        match self {
            Self::BlockFooter(lp) => Some(lp.inner()),
            _ => None,
        }
    }

    pub fn as_block_header(&self) -> Option<&VersionedBlockHeader> {
        match self {
            Self::BlockHeader(lp) => Some(lp.inner()),
            _ => None,
        }
    }

    pub fn as_update_parent(&self) -> Option<&VersionedUpdateParent> {
        match self {
            Self::UpdateParent(lp) => Some(lp.inner()),
            _ => None,
        }
    }

    pub fn as_genesis_certificate(&self) -> Option<&GenesisCertBlockMarker> {
        match self {
            Self::GenesisCertificate(lp) => Some(lp.inner()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u16")]
pub enum VersionedBlockMarker {
    #[wincode(tag = 1)]
    V1(BlockMarkerV1),
}

impl VersionedBlockMarker {
    pub const fn new(marker: BlockMarkerV1) -> Self {
        Self::V1(marker)
    }

    pub fn from_block_footer(f: BlockFooterV1) -> Self {
        let f = VersionedBlockFooter::V1(f);
        let f = BlockMarkerV1::BlockFooter(LengthPrefixed::new(f));
        Self::new(f)
    }

    pub fn from_block_header(h: BlockHeaderV1) -> Self {
        let h = VersionedBlockHeader::V1(h);
        let h = BlockMarkerV1::BlockHeader(LengthPrefixed::new(h));
        Self::new(h)
    }

    pub fn from_update_parent(u: UpdateParentV1) -> Self {
        let u = VersionedUpdateParent::V1(u);
        let u = BlockMarkerV1::UpdateParent(LengthPrefixed::new(u));
        Self::new(u)
    }

    pub fn from_genesis_cert_block_marker(g: GenesisCertBlockMarker) -> Self {
        let g = BlockMarkerV1::GenesisCertificate(LengthPrefixed::new(g));
        Self::new(g)
    }

    pub fn is_update_parent(&self) -> bool {
        match self {
            Self::V1(BlockMarkerV1::UpdateParent(_)) => true,
            Self::V1(_) => false,
        }
    }

    pub fn is_footer(&self) -> bool {
        match self {
            Self::V1(BlockMarkerV1::BlockFooter(_)) => true,
            Self::V1(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum BlockComponent {
    EntryBatch(Vec<Entry>),
    BlockMarker(VersionedBlockMarker),
}

impl BlockComponent {
    const MAX_ENTRIES: usize = u32::MAX as usize;
    const ENTRY_COUNT_SIZE: usize = 8;
    const EMPTY_ENTRY_BATCH: [u8; Self::ENTRY_COUNT_SIZE] = 0u64.to_le_bytes();

    pub fn new_entry_batch(entries: Vec<Entry>) -> Result<Self, BlockComponentError> {
        if entries.is_empty() {
            return Err(BlockComponentError::EmptyEntryBatch);
        }

        if entries.len() >= Self::MAX_ENTRIES {
            return Err(BlockComponentError::TooManyEntries {
                count: entries.len(),
                max: Self::MAX_ENTRIES,
            });
        }

        Ok(Self::EntryBatch(entries))
    }

    pub const fn new_block_marker(marker: VersionedBlockMarker) -> Self {
        Self::BlockMarker(marker)
    }

    pub fn new_block_header(parent_slot: Slot, parent_block_id: Hash) -> Self {
        let header = BlockHeaderV1 {
            parent_slot,
            parent_block_id,
        };
        Self::new_block_marker(VersionedBlockMarker::from_block_header(header))
    }

    pub const fn as_marker(&self) -> Option<&VersionedBlockMarker> {
        match self {
            Self::BlockMarker(m) => Some(m),
            _ => None,
        }
    }

    pub fn infer_is_entry_batch(data: &[u8]) -> Option<bool> {
        data.get(..Self::ENTRY_COUNT_SIZE)?
            .try_into()
            .ok()
            .map(|b| u64::from_le_bytes(b) != 0)
    }

    pub fn infer_is_block_marker(data: &[u8]) -> Option<bool> {
        Self::infer_is_entry_batch(data).map(|is_entry_batch| !is_entry_batch)
    }

    /// In Alpenglow an empty entry batch will fail to deserialize
    /// Leader's should only use an empty entry batch to indicate that they
    /// are aborting the block
    pub fn infer_is_empty_entry_batch(data: &[u8]) -> bool {
        *data == Self::EMPTY_ENTRY_BATCH
    }
}

unsafe impl<C: Config> SchemaWrite<C> for BlockComponent {
    type Src = Self;

    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        match src {
            Self::EntryBatch(entries) => {
                <WincodeVec<Entry, MaxDataShredsLen> as SchemaWrite<C>>::size_of(entries)
            }
            Self::BlockMarker(marker) => {
                let marker_size = <VersionedBlockMarker as SchemaWrite<C>>::size_of(marker)?;
                Ok(Self::ENTRY_COUNT_SIZE + marker_size)
            }
        }
    }

    fn write(mut writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
        match src {
            Self::EntryBatch(entries) => {
                <WincodeVec<Entry, MaxDataShredsLen> as SchemaWrite<C>>::write(writer, entries)
            }
            Self::BlockMarker(marker) => {
                writer.write(&0u64.to_le_bytes())?;
                <VersionedBlockMarker as SchemaWrite<C>>::write(writer, marker)
            }
        }
    }
}

unsafe impl<'de, C: Config> SchemaRead<'de, C> for BlockComponent {
    type Dst = Self;

    fn read(mut reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
        let entries =
            <WincodeVec<Entry, MaxDataShredsLen> as SchemaRead<'de, C>>::get(reader.by_ref())?;

        if entries.is_empty() {
            dst.write(Self::BlockMarker(<VersionedBlockMarker as SchemaRead<
                C,
            >>::get(reader)?));
        } else if entries.len() >= Self::MAX_ENTRIES {
            return Err(wincode::ReadError::Custom("Too many entries"));
        } else {
            dst.write(Self::EntryBatch(entries));
        }

        Ok(())
    }
}

/// Try to parse a Genesis certificate from a data shred payload
pub fn genesis_certificate_from_shred(
    payload: &[u8],
    shred_version: u16,
) -> Option<UnverifiedCertificate> {
    if !BlockComponent::infer_is_block_marker(payload).unwrap_or(false) {
        return None;
    }
    let BlockComponent::BlockMarker(VersionedBlockMarker::V1(marker)) =
        wincode::config::deserialize_exact(
            payload,
            DefaultConfig::default().with_preallocation_size_limit::<{ PACKET_DATA_SIZE }>(),
        )
        .ok()?
    else {
        return None;
    };
    let BlockMarkerV1::GenesisCertificate(marker) = marker else {
        return None;
    };
    let GenesisCertBlockMarker {
        slot,
        block_id,
        bls_signature,
        bitmap,
    } = marker.into_inner();
    if bitmap.len() > GenesisCertBlockMarker::MAX_BITMAP_SIZE {
        return None;
    }
    Some(UnverifiedCertificate {
        cert_type: CertificateType::Genesis(Block { slot, block_id }),
        signature: bls_signature,
        bitmap,
        shred_version,
    })
}

/// Converts a block footer finalization certificate into certificates ready for BLS verification.
///
/// A fast-finalization footer produces one `FinalizeFast` certificate. A slow-finalization footer
/// produces the corresponding `Notarize` and `Finalize` certificates.
pub fn finalization_certificates_from_footer(
    block_final_cert: BlockFinalizationCert,
    shred_version: u16,
) -> Option<Vec<UnverifiedCertificate>> {
    let BlockFinalizationCert {
        slot,
        block_id,
        final_aggregate,
        notar_aggregate,
    } = block_final_cert;
    let block = Block { slot, block_id };

    let final_signature = final_aggregate.uncompress_signature().ok()?;
    let final_bitmap = final_aggregate.into_bitmap();
    if let Some(notar_aggregate) = notar_aggregate {
        let notar_signature = notar_aggregate.uncompress_signature().ok()?;
        let notar_bitmap = notar_aggregate.into_bitmap();
        Some(vec![
            UnverifiedCertificate {
                cert_type: CertificateType::Notarize(block),
                signature: notar_signature,
                bitmap: notar_bitmap,
                shred_version,
            },
            UnverifiedCertificate {
                cert_type: CertificateType::Finalize(slot),
                signature: final_signature,
                bitmap: final_bitmap,
                shred_version,
            },
        ])
    } else {
        Some(vec![UnverifiedCertificate {
            cert_type: CertificateType::FinalizeFast(block),
            signature: final_signature,
            bitmap: final_bitmap,
            shred_version,
        }])
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, Keypair as BlsKeypair, Signature as BlsSignature,
        },
        std::iter::repeat_n,
        wincode::config::DEFAULT_PREALLOCATION_SIZE_LIMIT,
    };

    fn mock_entries(n: usize) -> Vec<Entry> {
        repeat_n(Entry::default(), n).collect()
    }

    fn sample_footer() -> BlockFooterV1 {
        BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 1234567890,
            block_user_agent: b"test-agent".to_vec(),
            block_final_cert: Some(BlockFinalizationCert::new_for_tests()),
            skip_reward_cert: None,
            notar_reward_cert: None,
        }
    }

    fn test_votes_aggregate(payload: &[u8], bitmap: Vec<u8>) -> (VotesAggregate, BlsSignature) {
        let signature: BlsSignature = BlsKeypair::new().sign(payload).into();
        let aggregate = VotesAggregate::from_cert_signature(CertSignature { signature, bitmap });
        (aggregate, signature)
    }

    #[test]
    fn parse_genesis_certificate_from_shred() {
        let parent_slot = 41;
        let block_id = Hash::new_unique();
        let shred_version = 123;
        let signature: BlsSignature = BlsKeypair::new().sign(b"genesis").into();
        let bitmap = vec![0xa5; 64];
        let marker = GenesisCertBlockMarker {
            slot: parent_slot,
            block_id,
            bls_signature: signature,
            bitmap: bitmap.clone(),
        };
        let component = BlockComponent::new_block_marker(
            VersionedBlockMarker::from_genesis_cert_block_marker(marker),
        );
        let payload = wincode::serialize(&component).unwrap();

        let certificate = genesis_certificate_from_shred(&payload, shred_version).unwrap();
        assert_eq!(
            certificate.cert_type,
            CertificateType::Genesis(Block {
                slot: parent_slot,
                block_id,
            })
        );
        assert_eq!(certificate.signature, signature);
        assert_eq!(certificate.bitmap, bitmap);
        assert_eq!(certificate.shred_version, shred_version);

        let mut payload_with_trailing_data = payload;
        payload_with_trailing_data.push(0);
        assert!(
            genesis_certificate_from_shred(&payload_with_trailing_data, shred_version,).is_none()
        );
    }

    #[test]
    fn finalization_certificates_from_fast_footer() {
        let slot = 42;
        let block_id = Hash::new_unique();
        let shred_version = 123;
        let final_bitmap = vec![0x11; 64];
        let (final_aggregate, final_signature) =
            test_votes_aggregate(b"fast-finalize", final_bitmap.clone());
        let certificates = finalization_certificates_from_footer(
            BlockFinalizationCert {
                slot,
                block_id,
                final_aggregate,
                notar_aggregate: None,
            },
            shred_version,
        )
        .unwrap();

        let [certificate] = certificates.as_slice() else {
            panic!("expected one fast-finalization certificate");
        };
        assert_eq!(
            certificate.cert_type,
            CertificateType::FinalizeFast(Block { slot, block_id })
        );
        assert_eq!(certificate.signature, final_signature);
        assert_eq!(certificate.bitmap, final_bitmap);
        assert_eq!(certificate.shred_version, shred_version);
    }

    #[test]
    fn finalization_certificates_from_slow_footer() {
        let slot = 42;
        let block_id = Hash::new_unique();
        let shred_version = 123;
        let final_bitmap = vec![0x11; 64];
        let notar_bitmap = vec![0x22; 64];
        let (final_aggregate, final_signature) =
            test_votes_aggregate(b"finalize", final_bitmap.clone());
        let (notar_aggregate, notar_signature) =
            test_votes_aggregate(b"notarize", notar_bitmap.clone());
        let certificates = finalization_certificates_from_footer(
            BlockFinalizationCert {
                slot,
                block_id,
                final_aggregate,
                notar_aggregate: Some(notar_aggregate),
            },
            shred_version,
        )
        .unwrap();

        let [notarize, finalize] = certificates.as_slice() else {
            panic!("expected notarize and finalize certificates");
        };
        assert_eq!(
            notarize.cert_type,
            CertificateType::Notarize(Block { slot, block_id })
        );
        assert_eq!(notarize.signature, notar_signature);
        assert_eq!(notarize.bitmap, notar_bitmap);
        assert_eq!(notarize.shred_version, shred_version);
        assert_eq!(finalize.cert_type, CertificateType::Finalize(slot));
        assert_eq!(finalize.signature, final_signature);
        assert_eq!(finalize.bitmap, final_bitmap);
        assert_eq!(finalize.shred_version, shred_version);
    }

    #[test]
    fn round_trips() {
        let header = BlockHeaderV1 {
            parent_slot: 12345,
            parent_block_id: Hash::new_unique(),
        };
        let bytes = wincode::serialize(&header).unwrap();
        assert_eq!(
            header,
            wincode::deserialize::<BlockHeaderV1>(&bytes).unwrap()
        );

        let footer = sample_footer();
        let bytes = wincode::serialize(&footer).unwrap();
        assert_eq!(
            footer,
            wincode::deserialize::<BlockFooterV1>(&bytes).unwrap()
        );

        let marker = GenesisCertBlockMarker {
            slot: 999,
            block_id: Hash::new_unique(),
            bls_signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![1, 2, 3],
        };
        let bytes = wincode::serialize(&marker).unwrap();
        assert_eq!(
            marker,
            wincode::deserialize::<GenesisCertBlockMarker>(&bytes).unwrap()
        );

        let marker = VersionedBlockMarker::from_block_footer(footer.clone());
        let bytes = wincode::serialize(&marker).unwrap();
        assert_eq!(
            marker,
            wincode::deserialize::<VersionedBlockMarker>(&bytes).unwrap()
        );

        let comp = BlockComponent::new_entry_batch(mock_entries(5)).unwrap();
        let bytes = wincode::serialize(&comp).unwrap();
        let deser: BlockComponent = wincode::deserialize(&bytes).unwrap();
        assert_eq!(comp, deser);

        let comp = BlockComponent::new_block_marker(marker);
        let bytes = wincode::serialize(&comp).unwrap();
        let deser: BlockComponent = wincode::deserialize(&bytes).unwrap();
        assert_eq!(comp, deser);
    }

    #[test]
    fn length_prefixed_rejects_inner_size_mismatch() {
        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 12345,
            parent_block_id: Hash::new_unique(),
        });
        let prefixed = LengthPrefixed::new(header);
        let mut bytes = wincode::serialize(&prefixed).unwrap();
        let wrong_len = prefixed.len + 1;
        bytes[..std::mem::size_of::<u16>()].copy_from_slice(&wrong_len.to_le_bytes());

        assert!(matches!(
            wincode::deserialize::<LengthPrefixed<VersionedBlockHeader>>(&bytes),
            Err(wincode::ReadError::Custom(
                "LengthPrefixed: inner serialized size does not match length prefix"
            ))
        ));
    }

    #[test]
    fn length_prefixed_rejects_oversized_deserialized_inner() {
        let marker = GenesisCertBlockMarker {
            slot: 999,
            block_id: Hash::new_unique(),
            bls_signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![0xAB; usize::from(u16::MAX) + 1],
        };
        let wire = LengthPrefixed {
            len: 7,
            inner: marker,
        };
        let bytes = wincode::serialize(&wire).unwrap();

        assert!(matches!(
            wincode::deserialize::<LengthPrefixed<GenesisCertBlockMarker>>(&bytes),
            Err(wincode::ReadError::Custom(
                "LengthPrefixed: inner serialized size does not match length prefix"
            ))
        ));
    }

    #[test]
    fn large_entry_batch_round_trips() {
        // Ensure an EntryBatch that exceeds wincode's default 4 MiB prealloc
        // limit can still round-trip.
        let num_entries = DEFAULT_PREALLOCATION_SIZE_LIMIT / std::mem::size_of::<Entry>() + 1;

        let comp = BlockComponent::new_entry_batch(mock_entries(num_entries)).unwrap();
        let bytes = wincode::serialize(&comp).unwrap();
        let deser: BlockComponent = wincode::deserialize(&bytes).unwrap();
        assert_eq!(comp, deser);
    }
}
