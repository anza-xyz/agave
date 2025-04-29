//! Rebuilding the missing shreds of an erasure batch, as shreds rather than as bytes.
//!
//! Recovery itself is `solana-fec-set-recovery`, which works in payload bytes. What is here is the
//! trust boundary for external users.

use {
    crate::{
        error::RecoverError,
        shred::{CodeShred, DataShred},
        shred_variant::ShredKind,
        state::Verified,
        view,
    },
    bytes::Bytes,
};

/// The shreds of one FEC set that recovery rebuilt, which are the ones that were missing.
#[derive(Clone, Debug)]
pub struct Recovery {
    /// The rebuilt data shreds, in shard order.
    pub data: Vec<DataShred<Verified>>,
    /// The rebuilt code shreds, in shard order.
    pub code: Vec<CodeShred<Verified>>,
}

/// Rebuilds whatever is missing from the FEC set `data` and `code` are the survivors of.
///
/// Resulting shreds have [`Provenance::Recovered`](crate::provenance::Provenance::Recovered).
pub fn recover(
    data: &[DataShred<Verified>],
    code: &[CodeShred<Verified>],
) -> Result<Recovery, RecoverError> {
    let survivors: Vec<Bytes> = data
        .iter()
        .map(|shred| shred.bytes().clone())
        .chain(code.iter().map(|shred| shred.bytes().clone()))
        .collect();
    let mut recovery = Recovery {
        data: Vec::new(),
        code: Vec::new(),
    };
    for payload in solana_fec_set_recovery::recover_payloads(&survivors)? {
        // The rebuilt payload carries its own variant byte, which is what says where it belongs.
        match view::peek_variant(&payload)?.shred_kind() {
            ShredKind::Data => recovery.data.push(DataShred::assume_recovered(payload)?),
            ShredKind::Code => recovery.code.push(CodeShred::assume_recovered(payload)?),
        }
    }
    Ok(recovery)
}
