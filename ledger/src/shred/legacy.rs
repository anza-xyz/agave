use {
    crate::shred::{
        common::impl_shred_common,
        payload::Payload,
        shred_code, shred_data,
        traits::{Shred, ShredCode as ShredCodeTrait, ShredData as ShredDataTrait},
        CodingShredHeader, DataShredHeader, Error, ShredCommonHeader, ShredVariant,
        SIZE_OF_CODING_SHRED_HEADERS, SIZE_OF_DATA_SHRED_HEADERS, SIZE_OF_SIGNATURE,
    },
    solana_perf::packet::deserialize_from_with_limit,
    solana_signature::Signature,
    static_assertions::const_assert_eq,
    std::io::Cursor,
};

const_assert_eq!(ShredData::SIZE_OF_PAYLOAD, ShredCode::SIZE_OF_PAYLOAD);
const_assert_eq!(ShredData::SIZE_OF_PAYLOAD, 1228);

const SIZE_OF_ERASURE_ENCODED_SLICE: usize =
    ShredCode::SIZE_OF_PAYLOAD - ShredCode::SIZE_OF_HEADERS;

// Layout: {common, data} headers | data | zero padding
// Everything up to ShredCode::SIZE_OF_HEADERS bytes at the end (which is part
// of zero padding) is erasure coded.
// All payload past signature, including the entirety of zero paddings, is
// signed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShredData {
    common_header: ShredCommonHeader,
    data_header: DataShredHeader,
    payload: Payload,
}

// Layout: {common, coding} headers | erasure coded shard
// All payload past signature is singed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShredCode {
    common_header: ShredCommonHeader,
    coding_header: CodingShredHeader,
    payload: Payload,
}

impl<'a> Shred<'a> for ShredData {
    type SignedData = &'a [u8];

    impl_shred_common!();
    // Legacy data shreds are always zero padded and
    // the same size as coding shreds.
    const SIZE_OF_PAYLOAD: usize = shred_code::ShredCode::SIZE_OF_PAYLOAD;
    const SIZE_OF_HEADERS: usize = SIZE_OF_DATA_SHRED_HEADERS;

    fn from_payload<T>(payload: T) -> Result<Self, Error>
    where
        Payload: From<T>,
    {
        let mut payload = Payload::from(payload).into_bytes_mut();
        let mut cursor = Cursor::new(&payload[..]);
        let common_header: ShredCommonHeader = deserialize_from_with_limit(&mut cursor)?;
        if common_header.shred_variant != ShredVariant::LegacyData {
            return Err(Error::InvalidShredVariant);
        }
        let data_header = deserialize_from_with_limit(&mut cursor)?;
        // Shreds stored to blockstore may have trailing zeros trimmed.
        // Repair packets have nonce at the end of packet payload; see:
        // https://github.com/solana-labs/solana/pull/10109
        // https://github.com/solana-labs/solana/pull/16602
        if payload.len() < Self::SIZE_OF_HEADERS {
            return Err(Error::InvalidPayloadSize(payload.len()));
        }
        payload.resize(Self::SIZE_OF_PAYLOAD, 0u8);
        let shred = Self {
            common_header,
            data_header,
            payload: payload.into(),
        };
        shred.sanitize().map(|_| shred)
    }

    fn erasure_shard_index(&self) -> Result<usize, Error> {
        shred_data::erasure_shard_index(self).ok_or_else(|| {
            let headers = Box::new((self.common_header, self.data_header));
            Error::InvalidErasureShardIndex(headers)
        })
    }

    fn erasure_shard(&self) -> Result<&[u8], Error> {
        if self.payload.len() != Self::SIZE_OF_PAYLOAD {
            return Err(Error::InvalidPayloadSize(self.payload.len()));
        }
        Ok(&self.payload[..SIZE_OF_ERASURE_ENCODED_SLICE])
    }

    fn sanitize(&self) -> Result<(), Error> {
        match self.common_header.shred_variant {
            ShredVariant::LegacyData => (),
            _ => return Err(Error::InvalidShredVariant),
        }
        shred_data::sanitize(self)
    }

    fn signed_data(&'a self) -> Result<Self::SignedData, Error> {
        debug_assert_eq!(self.payload.len(), Self::SIZE_OF_PAYLOAD);
        Ok(&self.payload[SIZE_OF_SIGNATURE..])
    }
}

impl<'a> Shred<'a> for ShredCode {
    type SignedData = &'a [u8];

    impl_shred_common!();
    const SIZE_OF_PAYLOAD: usize = shred_code::ShredCode::SIZE_OF_PAYLOAD;
    const SIZE_OF_HEADERS: usize = SIZE_OF_CODING_SHRED_HEADERS;

    fn from_payload<T>(_payload: T) -> Result<Self, Error>
    where
        Payload: From<T>,
    {
        Err(Error::InvalidShredVariant)
    }

    fn erasure_shard_index(&self) -> Result<usize, Error> {
        Err(Error::InvalidShredVariant)
    }

    fn erasure_shard(&self) -> Result<&[u8], Error> {
        Err(Error::InvalidShredVariant)
    }

    fn sanitize(&self) -> Result<(), Error> {
        Err(Error::InvalidShredVariant)
    }

    fn signed_data(&'a self) -> Result<Self::SignedData, Error> {
        Err(Error::InvalidShredVariant)
    }
}

impl ShredDataTrait for ShredData {
    #[inline]
    fn data_header(&self) -> &DataShredHeader {
        &self.data_header
    }

    #[inline]
    fn data(&self) -> Result<&[u8], Error> {
        Err(Error::InvalidShredVariant)
    }
}

impl ShredCodeTrait for ShredCode {
    #[inline]
    fn coding_header(&self) -> &CodingShredHeader {
        &self.coding_header
    }
}

impl ShredData {
    pub(super) fn bytes_to_store(&self) -> &[u8] {
        // Payload will be padded out to Self::SIZE_OF_PAYLOAD.
        // But only need to store the bytes within data_header.size.
        &self.payload[..self.data_header.size as usize]
    }

    pub(super) fn resize_stored_shred(mut shred: Vec<u8>) -> Result<Vec<u8>, Error> {
        // Old shreds might have been extra zero padded.
        if !(Self::SIZE_OF_HEADERS..=Self::SIZE_OF_PAYLOAD).contains(&shred.len()) {
            return Err(Error::InvalidPayloadSize(shred.len()));
        }
        shred.resize(Self::SIZE_OF_PAYLOAD, 0u8);
        Ok(shred)
    }
}
