use crate::shred::{
    traits::{Shred, ShredData as ShredDataTrait},
    Error, ShredType, MAX_DATA_SHREDS_PER_SLOT,
};

// Possibly zero pads bytes stored in blockstore.
pub(crate) fn resize_stored_shred(shred: Vec<u8>) -> Result<Vec<u8>, Error> {
    use crate::shred::{merkle, ShredVariant};
    match crate::shred::layout::get_shred_variant(&shred)? {
        ShredVariant::MerkleCode { .. } => Err(Error::InvalidShredType),
        ShredVariant::MerkleData { .. } => {
            if shred.len() != <merkle::ShredData as Shred>::SIZE_OF_PAYLOAD {
                return Err(Error::InvalidPayloadSize(shred.len()));
            }
            Ok(shred)
        }
    }
}

#[inline]
pub(super) fn erasure_shard_index<T: ShredDataTrait>(shred: &T) -> Option<usize> {
    let fec_set_index = shred.common_header().fec_set_index;
    let index = shred.common_header().index.checked_sub(fec_set_index)?;
    usize::try_from(index).ok()
}

pub(super) fn sanitize<T: ShredDataTrait>(shred: &T) -> Result<(), Error> {
    use crate::shred::ShredFlags;
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
