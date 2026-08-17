use {
    crate::shred::{
        CodingShredHeader, DataShredHeader, Error, ShredCommonHeader, payload::Payload,
    },
    solana_clock::Slot,
};

/// Read-only access to a shred's headers and payload bytes.
///
/// This is deliberately narrower than [`ShredWithPayload`]: it says nothing
/// about how the payload is stored or whether it is shareable, so it can be
/// implemented both by a finished shred and by a shred still under
/// construction.
pub(super) trait Shred {
    /// Total size of payload including headers, merkle branches (if any), zero
    /// paddings, etc.
    const SIZE_OF_PAYLOAD: usize;
    /// Size of common and code/data headers.
    const SIZE_OF_HEADERS: usize;

    fn common_header(&self) -> &ShredCommonHeader;
    fn payload_bytes(&self) -> &[u8];
    fn sanitize(&self) -> Result<(), Error>;

    /// Returns the shard index within the erasure coding set.
    fn erasure_shard_index(&self) -> Result<usize, Error>;
    /// Returns the portion of the shred's payload which is erasure coded.
    fn erasure_shard(&self) -> Result<&[u8], Error>;
}

/// A finished shred, whose payload is immutable and cheaply shareable.
///
/// Only implemented for fully constructed shreds; a shred under construction
/// implements just [`Shred`], because it has no shareable [`Payload`] to hand
/// out yet.
pub(super) trait ShredWithPayload<'a>: Shred + Sized {
    type SignedData: AsRef<[u8]>;

    fn from_payload<T>(shred: T) -> Result<Self, Error>
    where
        Payload: From<T>;

    fn payload(&self) -> &Payload;
    fn into_payload(self) -> Payload;

    /// Portion of the payload which is signed.
    fn signed_data(&'a self) -> Result<Self::SignedData, Error>;
}

pub(super) trait ShredData: Shred {
    fn data_header(&self) -> &DataShredHeader;

    fn parent(&self) -> Result<Slot, Error> {
        let slot = self.common_header().slot;
        let parent_offset = self.data_header().parent_offset;
        if parent_offset == 0 && slot != 0 {
            return Err(Error::InvalidParentOffset {
                slot,
                parent_offset,
            });
        }
        slot.checked_sub(Slot::from(parent_offset))
            .ok_or(Error::InvalidParentOffset {
                slot,
                parent_offset,
            })
    }

    fn data(&self) -> Result<&[u8], Error>;
}

pub(super) trait ShredCode: Shred {
    fn coding_header(&self) -> &CodingShredHeader;

    fn first_coding_index(&self) -> Option<u32> {
        let position = u32::from(self.coding_header().position);
        self.common_header().index.checked_sub(position)
    }
}
