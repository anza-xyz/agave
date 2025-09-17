use {
    crate::shred::{
        self,
        common::dispatch,
        merkle,
        payload::Payload,
        traits::{Shred as _, ShredData as ShredDataTrait},
        DataShredHeader, Error, ShredCommonHeader, ShredFlags, ShredType, ShredVariant,
        MAX_DATA_SHREDS_PER_SLOT,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_signature::Signature,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShredData {
    Merkle(merkle::ShredData),
}

impl ShredData {
    dispatch!(fn data_header(&self) -> &DataShredHeader);

    dispatch!(pub(super) fn common_header(&self) -> &ShredCommonHeader);
    dispatch!(pub(super) fn into_payload(self) -> Payload);
    dispatch!(pub(super) fn parent(&self) -> Result<Slot, Error>);
    dispatch!(pub(super) fn payload(&self) -> &Payload);
    dispatch!(pub(super) fn sanitize(&self) -> Result<(), Error>);
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    dispatch!(pub(super) fn set_signature(&mut self, signature: Signature));

    pub(super) fn signed_data(&self) -> Result<Hash, Error> {
        let Self::Merkle(shred) = self;
        shred.signed_data()
    }

    pub(super) fn chained_merkle_root(&self) -> Result<Hash, Error> {
        match self {
            Self::Merkle(shred) => shred.chained_merkle_root(),
        }
    }

    pub(super) fn merkle_root(&self) -> Result<Hash, Error> {
        match self {
            Self::Merkle(shred) => shred.merkle_root(),
        }
    }

    pub(super) fn last_in_slot(&self) -> bool {
        let flags = self.data_header().flags;
        flags.contains(ShredFlags::LAST_SHRED_IN_SLOT)
    }

    pub(super) fn data_complete(&self) -> bool {
        let flags = self.data_header().flags;
        flags.contains(ShredFlags::DATA_COMPLETE_SHRED)
    }

    pub(super) fn reference_tick(&self) -> u8 {
        let flags = self.data_header().flags;
        (flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits()
    }

    // Possibly trimmed payload;
    // Should only be used when storing shreds to blockstore.
    pub(super) fn bytes_to_store(&self) -> &[u8] {
        match self {
            Self::Merkle(shred) => shred.payload(),
        }
    }

    // Possibly zero pads bytes stored in blockstore.
    pub(crate) fn resize_stored_shred(shred: Vec<u8>) -> Result<Vec<u8>, Error> {
        match shred::layout::get_shred_variant(&shred)? {
            ShredVariant::MerkleCode { .. } => Err(Error::InvalidShredType),
            ShredVariant::MerkleData { .. } => {
                if shred.len() != merkle::ShredData::SIZE_OF_PAYLOAD {
                    return Err(Error::InvalidPayloadSize(shred.len()));
                }
                Ok(shred)
            }
        }
    }

    // Maximum size of ledger data that can be embedded in a data-shred.
    // merkle_proof_size is the number of merkle proof entries.
    // None indicates a legacy data-shred.
    pub fn capacity(
        merkle_variant: Option<(
            u8,   // proof_size
            bool, // chained
            bool, // resigned
        )>,
    ) -> Result<usize, Error> {
        match merkle_variant {
            None => Err(Error::InvalidShredVariant),
            Some((proof_size, chained, resigned)) => {
                debug_assert!(chained || !resigned);
                merkle::ShredData::capacity(proof_size, chained, resigned)
            }
        }
    }

    pub(super) fn retransmitter_signature(&self) -> Result<Signature, Error> {
        match self {
            Self::Merkle(shred) => shred.retransmitter_signature(),
        }
    }
}

impl From<merkle::ShredData> for ShredData {
    fn from(shred: merkle::ShredData) -> Self {
        Self::Merkle(shred)
    }
}

#[inline]
pub(super) fn erasure_shard_index<T: ShredDataTrait>(shred: &T) -> Option<usize> {
    let fec_set_index = shred.common_header().fec_set_index;
    let index = shred.common_header().index.checked_sub(fec_set_index)?;
    usize::try_from(index).ok()
}

pub(super) fn sanitize<T: ShredDataTrait>(shred: &T) -> Result<(), Error> {
    if shred.payload().len() != T::SIZE_OF_PAYLOAD {
        return Err(Error::InvalidPayloadSize(shred.payload().len()));
    }
    let common_header = shred.common_header();
    let data_header = shred.data_header();
    if common_header.index as usize >= MAX_DATA_SHREDS_PER_SLOT {
        return Err(Error::InvalidShredIndex(
            ShredType::Data,
            common_header.index,
        ));
    }
    let flags = data_header.flags;
    if flags.intersects(ShredFlags::LAST_SHRED_IN_SLOT)
        && !flags.contains(ShredFlags::DATA_COMPLETE_SHRED)
    {
        return Err(Error::InvalidShredFlags(data_header.flags.bits()));
    }
    let _data = shred.data()?;
    let _parent = shred.parent()?;
    let _shard_index = shred.erasure_shard_index()?;
    let _erasure_shard = shred.erasure_shard()?;
    Ok(())
}
