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
/// ┌─────────────────────────────────────────┐
/// │ Bank Hash                   (32 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ Producer Time Nanos          (8 bytes)  │
/// ├─────────────────────────────────────────┤
/// │ User Agent Length            (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ User Agent Bytes          (0-255 bytes) │
/// ├─────────────────────────────────────────┤
/// │ Final Cert Present           (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ FinalCertificate (if present, variable) │
/// ├─────────────────────────────────────────┤
/// │ Skip reward cert Present     (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ SkipRewardCert (if present, variable)   │
/// ├─────────────────────────────────────────┤
/// │ Notar reward cert Present    (1 byte)   │
/// ├─────────────────────────────────────────┤
/// │ NotarRewardCert (if present, variable)  │
/// └─────────────────────────────────────────┘
/// ```
///
/// ### FinalCertificate Layout
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
    agave_votor_messages::consensus_message::{Certificate, CertificateType},
    solana_bls_signatures::{
        Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    std::mem::MaybeUninit,
    wincode::{
        config::{Config, DefaultConfig},
        containers::{Pod, Vec as WincodeVec},
        error::write_length_encoding_overflow,
        io::{Reader, Writer},
        len::{BincodeLen, FixIntLen},
        ReadResult, SchemaRead, SchemaWrite, WriteResult,
    },
};

/// Placeholder for skip reward certificate.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct SkipRewardCertificate {
    pub data: Vec<u8>,
}

/// Placeholder for notar reward certificate.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct NotarRewardCertificate {
    pub data: Vec<u8>,
}

/// Wraps a value with a u16 length prefix for TLV-style serialization.
///
/// The length prefix represents the serialized byte size of the inner value.
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct LengthPrefixed<T> {
    len: u16,
    inner: T,
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
    #[wincode(with = "Pod<Hash>")]
    pub bank_hash: Hash,
    pub block_producer_time_nanos: u64,
    #[wincode(with = "WincodeVec<u8, FixIntLen<u8>>")]
    pub block_user_agent: Vec<u8>,
    pub final_cert: Option<FinalCertificate>,
    pub skip_reward_cert: Option<SkipRewardCertificate>,
    pub notar_reward_cert: Option<NotarRewardCertificate>,
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct BlockHeaderV1 {
    pub parent_slot: Slot,
    #[wincode(with = "Pod<Hash>")]
    pub parent_block_id: Hash,
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct UpdateParentV1 {
    pub new_parent_slot: Slot,
    #[wincode(with = "Pod<Hash>")]
    pub new_parent_block_id: Hash,
}

/// Attests to genesis block finalization with a BLS aggregate signature.
#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct GenesisCertificate {
    pub slot: Slot,
    #[wincode(with = "Pod<Hash>")]
    pub block_id: Hash,
    #[wincode(with = "Pod<BLSSignature>")]
    pub bls_signature: BLSSignature,
    #[wincode(with = "WincodeVec<u8, BincodeLen>")]
    pub bitmap: Vec<u8>,
}

impl GenesisCertificate {
    /// Max bitmap size in bytes (supports up to 4096 validators).
    pub const MAX_BITMAP_SIZE: usize = 512;
}

impl TryFrom<Certificate> for GenesisCertificate {
    type Error = String;

    fn try_from(cert: Certificate) -> Result<Self, Self::Error> {
        let CertificateType::Genesis(slot, block_id) = cert.cert_type else {
            return Err("expected genesis certificate".into());
        };
        if cert.bitmap.len() > Self::MAX_BITMAP_SIZE {
            return Err(format!(
                "bitmap size {} exceeds max {}",
                cert.bitmap.len(),
                Self::MAX_BITMAP_SIZE
            ));
        }
        Ok(Self {
            slot,
            block_id,
            bls_signature: cert.signature,
            bitmap: cert.bitmap,
        })
    }
}

impl From<GenesisCertificate> for Certificate {
    fn from(cert: GenesisCertificate) -> Self {
        Self {
            cert_type: CertificateType::Genesis(cert.slot, cert.block_id),
            signature: cert.bls_signature,
            bitmap: cert.bitmap,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaWrite, SchemaRead)]
pub struct FinalCertificate {
    pub slot: Slot,
    #[wincode(with = "Pod<Hash>")]
    pub block_id: Hash,
    pub final_aggregate: VotesAggregate,
    pub notar_aggregate: Option<VotesAggregate>,
}

impl FinalCertificate {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests() -> FinalCertificate {
        FinalCertificate {
            slot: 1234567890,
            block_id: Hash::new_from_array([1u8; 32]),
            final_aggregate: VotesAggregate {
                signature: BLSSignatureCompressed::default(),
                bitmap: vec![42; 64],
            },
            notar_aggregate: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, SchemaRead, SchemaWrite)]
pub struct VotesAggregate {
    #[wincode(with = "Pod<BLSSignatureCompressed>")]
    signature: BLSSignatureCompressed,
    #[wincode(with = "WincodeVec<u8, FixIntLen<u16>>")]
    bitmap: Vec<u8>,
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
    GenesisCertificate(LengthPrefixed<GenesisCertificate>),
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

    pub fn new_genesis_certificate(c: GenesisCertificate) -> Self {
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

    pub fn as_genesis_certificate(&self) -> Option<&GenesisCertificate> {
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

    pub fn new_block_footer(f: BlockFooterV1) -> Self {
        let f = VersionedBlockFooter::V1(f);
        let f = BlockMarkerV1::BlockFooter(LengthPrefixed::new(f));
        VersionedBlockMarker::V1(f)
    }

    pub fn new_block_header(h: BlockHeaderV1) -> Self {
        let h = VersionedBlockHeader::V1(h);
        let h = BlockMarkerV1::BlockHeader(LengthPrefixed::new(h));
        VersionedBlockMarker::V1(h)
    }

    pub fn new_update_parent(u: UpdateParentV1) -> Self {
        let u = VersionedUpdateParent::V1(u);
        let u = BlockMarkerV1::UpdateParent(LengthPrefixed::new(u));
        VersionedBlockMarker::V1(u)
    }

    pub fn new_genesis_certificate(g: GenesisCertificate) -> Self {
        let g = BlockMarkerV1::GenesisCertificate(LengthPrefixed::new(g));
        VersionedBlockMarker::V1(g)
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
        // Read the entry count (first 8 bytes) to determine variant
        let count_bytes = reader.fill_array::<8>()?;
        let entry_count = u64::from_le_bytes(*count_bytes);

        if entry_count == 0 {
            // This is a BlockMarker - consume the count bytes and read the marker
            // SAFETY: fill_array::<8>() above guarantees at least 8 bytes are available
            unsafe { reader.consume_unchecked(8) };
            dst.write(Self::BlockMarker(<VersionedBlockMarker as SchemaRead<
                C,
            >>::get(reader)?));
        } else {
            let entries: Vec<Entry> =
                <WincodeVec<Entry, MaxDataShredsLen> as SchemaRead<'de, C>>::get(reader)?;

            if entries.len() >= Self::MAX_ENTRIES {
                return Err(wincode::ReadError::Custom("Too many entries"));
            }

            dst.write(Self::EntryBatch(entries));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::iter::repeat_n, wincode::config::DEFAULT_PREALLOCATION_SIZE_LIMIT};

    fn mock_entries(n: usize) -> Vec<Entry> {
        repeat_n(Entry::default(), n).collect()
    }

    fn sample_footer() -> BlockFooterV1 {
        BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 1234567890,
            block_user_agent: b"test-agent".to_vec(),
            final_cert: Some(FinalCertificate::new_for_tests()),
            skip_reward_cert: None,
            notar_reward_cert: None,
        }
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

        let cert = GenesisCertificate {
            slot: 999,
            block_id: Hash::new_unique(),
            bls_signature: BLSSignature::default(),
            bitmap: vec![1, 2, 3],
        };
        let bytes = wincode::serialize(&cert).unwrap();
        assert_eq!(
            cert,
            wincode::deserialize::<GenesisCertificate>(&bytes).unwrap()
        );

        let marker = VersionedBlockMarker::new_block_footer(footer.clone());
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
