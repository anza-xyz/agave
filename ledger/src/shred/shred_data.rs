use {
    crate::shred::{
        traits::ShredData as ShredDataTrait, Error, ShredFlags, ShredType, ShredVariant,
        MAX_DATA_SHREDS_PER_SLOT, SIZE_OF_CODING_SHRED_HEADERS, SIZE_OF_NONCE, SIZE_OF_SIGNATURE,
    },
    solana_packet::PACKET_DATA_SIZE,
    static_assertions::const_assert_eq,
};

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

// ShredData payload size = ShredCode::SIZE_OF_PAYLOAD - SIZE_OF_CODING_SHRED_HEADERS + SIZE_OF_SIGNATURE
// = (PACKET_DATA_SIZE - SIZE_OF_NONCE) - SIZE_OF_CODING_SHRED_HEADERS + SIZE_OF_SIGNATURE
const SIZE_OF_PAYLOAD: usize =
    PACKET_DATA_SIZE - SIZE_OF_NONCE - SIZE_OF_CODING_SHRED_HEADERS + SIZE_OF_SIGNATURE;

// Compile-time assertions to verify the shred payload relationships.
const_assert_eq!(PACKET_DATA_SIZE - SIZE_OF_NONCE, 1228);
const_assert_eq!(SIZE_OF_PAYLOAD, 1203);

// Possibly zero pads bytes stored in blockstore.
pub(crate) fn resize_stored_shred(shred: Vec<u8>) -> Result<Vec<u8>, Error> {
    match crate::shred::layout::get_shred_variant(&shred)? {
        ShredVariant::MerkleCode { .. } => Err(Error::InvalidShredType),
        ShredVariant::MerkleData { .. } => {
            if shred.len() != SIZE_OF_PAYLOAD {
                return Err(Error::InvalidPayloadSize(shred.len()));
            }
            Ok(shred)
        }
    }
}
