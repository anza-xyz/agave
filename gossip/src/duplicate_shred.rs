#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample, frozen_abi};
use {
    crate::crds_data::sanitize_wallclock,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sanitize::{Sanitize, SanitizeError},
    wincode::{SchemaRead, SchemaWrite},
};

pub(crate) type DuplicateShredIndex = u16;
pub(crate) const MAX_DUPLICATE_SHREDS: DuplicateShredIndex = 512;

// Kept only as the legacy CRDS wire shape. Alpenglow nodes neither produce
// nor consume duplicate-shred proofs, but still need to decode and filter
// values received from older peers.
#[cfg_attr(
    feature = "stable-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "9zVjcmgLcLv1YDhBwDFYgMMjtpoZjk77YoL3sLBnABTf",
        abi_serializer = ["wincode"],
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Clone, Debug, PartialEq, Eq, SchemaWrite, SchemaRead)]
pub struct DuplicateShred {
    pub(crate) from: Pubkey,
    pub(crate) wallclock: u64,
    pub(crate) slot: Slot,
    _unused: u32,
    // This field was previously typed as ShredType. Decode it as a plain byte
    // so malformed legacy data cannot trigger strict enum validation.
    _unused_shred_type: u8,
    num_chunks: u8,
    chunk_index: u8,
    chunk: Vec<u8>,
}

impl DuplicateShred {
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn num_chunks(&self) -> u8 {
        self.num_chunks
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn chunk_index(&self) -> u8 {
        self.chunk_index
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn from(&self) -> &Pubkey {
        &self.from
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn wallclock(&self) -> u64 {
        self.wallclock
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn slot(&self) -> Slot {
        self.slot
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn chunk(&self) -> &[u8] {
        &self.chunk
    }
}

impl Sanitize for DuplicateShred {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        sanitize_wallclock(self.wallclock)?;
        if self.chunk_index >= self.num_chunks {
            return Err(SanitizeError::IndexOutOfBounds);
        }
        self.from.sanitize()
    }
}

#[cfg(test)]
impl DuplicateShred {
    pub(crate) fn new_for_tests(from: Pubkey, wallclock: u64) -> Self {
        Self {
            from,
            wallclock,
            slot: 0,
            _unused: 0,
            _unused_shred_type: 0,
            num_chunks: 1,
            chunk_index: 0,
            chunk: Vec::new(),
        }
    }
}
