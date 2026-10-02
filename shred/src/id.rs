use {
    crate::{constants::DATA_SHREDS_PER_FEC_BLOCK, shred_variant::ShredKind},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sha256_hasher::hashv,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ShredId(Slot, u32, ShredKind);

impl ShredId {
    #[inline]
    pub const fn new(slot: Slot, index: u32, kind: ShredKind) -> Self {
        Self(slot, index, kind)
    }

    #[inline]
    pub const fn slot(&self) -> Slot {
        self.0
    }

    #[inline]
    pub const fn index(&self) -> u32 {
        self.1
    }

    #[inline]
    pub const fn kind(&self) -> ShredKind {
        self.2
    }

    #[inline]
    pub const fn unpack(&self) -> (Slot, u32, ShredKind) {
        (self.0, self.1, self.2)
    }

    pub fn seed(&self, leader: &Pubkey) -> [u8; 32] {
        let ShredId(slot, index, kind) = self;
        hashv(&[
            &slot.to_le_bytes(),
            &u8::from(*kind).to_le_bytes(),
            &index.to_le_bytes(),
            leader.as_ref(),
        ])
        .to_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct ErasureSetId(Slot, u32);

impl ErasureSetId {
    #[inline]
    pub const fn new(slot: Slot, fec_set_index: u32) -> Self {
        Self(slot, fec_set_index)
    }

    #[inline]
    pub const fn slot(&self) -> Slot {
        self.0
    }

    #[inline]
    pub const fn fec_set_index(&self) -> u32 {
        self.1
    }

    pub fn previous_fec_set(&self) -> Option<Self> {
        self.1
            .checked_sub(DATA_SHREDS_PER_FEC_BLOCK)
            .map(|fec_set_index| Self::new(self.0, fec_set_index))
    }

    pub fn next_fec_set(&self) -> Option<Self> {
        self.1
            .checked_add(DATA_SHREDS_PER_FEC_BLOCK)
            .map(|fec_set_index| Self::new(self.0, fec_set_index))
    }

    #[inline]
    pub const fn store_key(&self) -> (Slot, u32) {
        (self.0, self.1)
    }
}
