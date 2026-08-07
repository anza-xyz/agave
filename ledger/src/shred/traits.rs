use {
    crate::shred::{
        CodingShredHeader, DataShredHeader, Error, ShredCommonHeader, payload::Payload,
    },
    solana_clock::Slot,
    solana_signature::Signature,
};

pub(super) trait Shred<'a>: Sized {
    // Total size of payload including headers, merkle
    // branches (if any), zero paddings, etc.
    const SIZE_OF_PAYLOAD: usize;
    // Size of common and code/data headers.
    const SIZE_OF_HEADERS: usize;

    type SignedData: AsRef<[u8]>;

    fn from_payload<T>(shred: T) -> Result<Self, Error>
    where
        Payload: From<T>;
    fn common_header(&self) -> &ShredCommonHeader;
    fn sanitize(&self) -> Result<(), Error>;

    fn set_signature(&mut self, signature: Signature);

    fn payload(&self) -> &Payload;
    fn into_payload(self) -> Payload;

    /// Returns the shard index within the erasure coding set: this shred's
    /// index within its own erasure batch, in
    /// `[0, num_data_shreds + num_coding_shreds)` — so `[0, 64)` under the
    /// current 32:32 configuration.
    ///
    /// This is what the Reed-Solomon coding and the Merkle tree address a shred
    /// by, and is unrelated in scale to [`ShredCommonHeader::index`], which is a
    /// position within the whole slot. Data shreds occupy the lower range of the
    /// batch and coding shreds the upper, so the two types derive it
    /// differently:
    ///
    /// * Data shreds: `index - fec_set_index`, in `[0, num_data_shreds)`.
    /// * Coding shreds: `position + num_data_shreds`, in
    ///   `[num_data_shreds, num_data_shreds + num_coding_shreds)`, computed
    ///   entirely from the coding header.
    ///
    /// Fails with [`Error::InvalidErasureShardIndex`] if the headers do not
    /// yield an index inside the batch.
    fn erasure_shard_index(&self) -> Result<usize, Error>;
    // Returns the portion of the shred's payload which is erasure coded.
    fn erasure_shard(&self) -> Result<&[u8], Error>;

    // Portion of the payload which is signed.
    fn signed_data(&'a self) -> Result<Self::SignedData, Error>;
}

pub(super) trait ShredData: for<'a> Shred<'a> {
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

pub(super) trait ShredCode: for<'a> Shred<'a> {
    fn coding_header(&self) -> &CodingShredHeader;

    fn first_coding_index(&self) -> Option<u32> {
        let position = u32::from(self.coding_header().position);
        self.common_header().index.checked_sub(position)
    }
}
