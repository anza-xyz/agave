//! Rebuilding an erasure batch in full, as shreds rather than as bytes.
//!
//! The rebuild itself is `agave-fec-set-recovery`, which works in payload bytes. What is here is the
//! trust boundary for external users.

use {
    crate::{
        error::RecoverError,
        shred::{CodeShred, DataShred},
        shredder::{CODE_SHREDS, DATA_SHREDS, FecSet},
        state::Verified,
    },
    agave_fec_set_recovery::RebuiltFecSet,
    bytes::Bytes,
};

/// Rebuilds the whole FEC set `data` and `code` are survivors of.
///
/// Needs at least [`DATA_SHREDS`] survivors. The set comes back complete, every shred in index
/// order: survivors as they were given, with their own provenance, and every other shred with
/// [`Provenance::Recovered`](crate::provenance::Provenance::Recovered).
pub fn rebuild_fec_set(
    data: &[DataShred<Verified>],
    code: &[CodeShred<Verified>],
) -> Result<FecSet, RecoverError> {
    let survivors: Vec<Bytes> = data
        .iter()
        .map(|shred| shred.bytes().clone())
        .chain(code.iter().map(|shred| shred.bytes().clone()))
        .collect();
    let RebuiltFecSet {
        payloads,
        merkle_root,
    } = agave_fec_set_recovery::rebuild_fec_set(&survivors)?;

    // The rebuild placed every survivor at its shard index and rejected any it could not, so the
    // lookups below cannot miss.
    let mut data_survivors = vec![None; DATA_SHREDS];
    for shred in data {
        *data_survivors
            .get_mut(shred.erasure_shard_index())
            .expect("the rebuild checked every data survivor's shard index") = Some(shred);
    }
    let mut code_survivors = vec![None; CODE_SHREDS];
    for shred in code {
        let position = shred
            .erasure_shard_index()
            .checked_sub(DATA_SHREDS)
            .expect("the rebuild checked every code survivor's shard index");
        *code_survivors
            .get_mut(position)
            .expect("the rebuild checked every code survivor's shard index") = Some(shred);
    }

    let mut payloads = payloads.into_iter();
    let data = data_survivors
        .into_iter()
        .zip(payloads.by_ref())
        .map(|(survivor, payload)| match survivor {
            Some(shred) => Ok(shred.clone()),
            None => DataShred::assume_recovered(payload),
        })
        .collect::<Result<_, _>>()?;
    let code = code_survivors
        .into_iter()
        .zip(payloads)
        .map(|(survivor, payload)| match survivor {
            Some(shred) => Ok(shred.clone()),
            None => CodeShred::assume_recovered(payload),
        })
        .collect::<Result<_, _>>()?;
    Ok(FecSet {
        data,
        code,
        merkle_root,
    })
}
