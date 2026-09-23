pub mod filter;
mod stats;

pub use {
    self::stats::{ProcessShredsStats, ShredFetchStats},
    crate::shredder::{DeshredError, Shredder},
    agave_shred::{
        MerkleError,
        constants::{
            Nonce, SIZE_OF_CODE_PAYLOAD, SIZE_OF_DATA_PAYLOAD, SIZE_OF_NONCE, SIZE_OF_SHRED_BUFFER,
        },
        error::{BuildError, ParseError, RecoverError, RejectReason},
        headers::{AnyHeader, CodeHeader, CommonHeader, DataHeader, ShredFlags},
        id::{ErasureSetId, ShredId},
        kind::{Code, Data, ShredLayout},
        merkle, merkle_tree,
        policy::AdmissionPolicy,
        provenance::{Provenance, ShredSource},
        shred::{AnyShred, CodeShred, DataShred, parse_repair, parse_turbine},
        shred_variant::{ShredKind, ShredVariant},
        state::{Admissible, Parsed, Verified},
        view::{self, AnyShredView, ShredView},
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {solana_clock::Slot, solana_pubkey::Pubkey};
use {
    solana_cost_model::shred_limit::{
        DEFAULT_MAX_CODE_SHREDS_PER_SLOT, DEFAULT_MAX_DATA_SHREDS_PER_SLOT,
    },
    solana_entry::entry::{Entry, create_ticks},
    solana_hash::Hash,
};

pub type Shred = AnyShred<Verified>;
pub type ShredType = ShredKind;

pub const DATA_SHREDS_PER_FEC_BLOCK: usize = agave_shred::constants::DATA_SHREDS;
pub const CODING_SHREDS_PER_FEC_BLOCK: usize = agave_shred::constants::CODE_SHREDS;
pub const SHREDS_PER_FEC_BLOCK: usize = agave_shred::constants::SHARDS;

pub const MAX_DATA_SHREDS_PER_SLOT: usize = DEFAULT_MAX_DATA_SHREDS_PER_SLOT as usize;
pub const MAX_CODE_SHREDS_PER_SLOT: usize = DEFAULT_MAX_CODE_SHREDS_PER_SLOT as usize;

pub const MAX_FEC_SETS_PER_SLOT: u32 =
    MAX_DATA_SHREDS_PER_SLOT as u32 / DATA_SHREDS_PER_FEC_BLOCK as u32;

pub const fn get_data_shred_bytes_per_batch_typical() -> u64 {
    (DATA_SHREDS_PER_FEC_BLOCK * Data::SIZE_OF_BODY) as u64
}

pub fn max_ticks_per_n_shreds(num_shreds: u64, shred_data_size: Option<usize>) -> u64 {
    let ticks = create_ticks(1, 0, Hash::default());
    max_entries_per_n_shred(&ticks[0], num_shreds, shred_data_size)
}

#[cfg(feature = "dev-context-only-utils")]
pub fn max_entries_per_n_shred_last_or_not(
    entry: &Entry,
    num_shreds: u64,
    is_last_in_slot: bool,
) -> u64 {
    let vec_size = wincode::serialized_size(&vec![entry]).unwrap();
    let entry_size = wincode::serialized_size(entry).unwrap();
    let count_size = vec_size - entry_size;

    let unsigned = Data::SIZE_OF_BODY as u64;
    let signed = Data::SIZE_OF_BODY_RESIGNED as u64;
    if !is_last_in_slot {
        (unsigned * num_shreds - count_size) / entry_size
    } else {
        let per_block = DATA_SHREDS_PER_FEC_BLOCK as u64;
        (unsigned * (num_shreds - per_block) + signed * per_block - count_size) / entry_size
    }
}

pub fn max_entries_per_n_shred(
    entry: &Entry,
    num_shreds: u64,
    shred_data_size: Option<usize>,
) -> u64 {
    let shred_data_size = shred_data_size.unwrap_or(Data::SIZE_OF_BODY_RESIGNED) as u64;
    let vec_size = wincode::serialized_size(&vec![entry]).unwrap();
    let entry_size = wincode::serialized_size(entry).unwrap();
    let count_size = vec_size - entry_size;

    (shred_data_size * num_shreds - count_size) / entry_size
}

#[cfg(feature = "dev-context-only-utils")]
pub fn verify_test_data_shred(
    shred: &Shred,
    index: u32,
    slot: Slot,
    parent: Slot,
    pk: &Pubkey,
    verify: bool,
    is_last_in_slot: bool,
    is_last_data: bool,
) {
    assert!(shred.is_data());
    assert_eq!(shred.index(), index);
    assert_eq!(shred.slot(), slot);
    assert_eq!(shred.parent_slot().unwrap(), parent);
    let root = shred.merkle_root().unwrap();
    assert_eq!(verify, agave_shred::verify(shred.signature(), pk, &root));
    assert_eq!(shred.last_in_slot(), is_last_in_slot);
    assert_eq!(shred.data_complete(), is_last_data);
}
