use super::*;

/// Legacy shredding logic, which emits the trailing FEC set(s) of a slot as
/// resigned shreds. Current leaders no longer do so, but resigned shreds
/// must still be admitted, so tests need a way to generate them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_shreds_from_data(
    keypair: &Keypair,
    chained_merkle_root: Hash,
    mut data: &[u8], // Serialized &[Entry]
    slot: Slot,
    parent_slot: Slot,
    shred_version: u16,
    reference_tick: u8,
    is_last_in_slot: bool,
    next_shred_index: u32,
    next_code_index: u32,
    stats: &mut ProcessShredsStats,
) -> Result<Vec<Shred>, Error> {
    let now = Instant::now();
    let proof_size = PROOF_ENTRIES_FOR_32_32_BATCH;

    // unsigned data_buffer size
    let data_buffer_per_shred_size = ShredData::capacity(proof_size, false)?;
    let data_buffer_total_size = DATA_SHREDS_PER_FEC_BLOCK * data_buffer_per_shred_size;

    // signed data_buffer size
    let data_buffer_per_shred_size_signed = if is_last_in_slot {
        ShredData::capacity(proof_size, true)?
    } else {
        0
    };
    let data_buffer_total_size_signed =
        DATA_SHREDS_PER_FEC_BLOCK * data_buffer_per_shred_size_signed;

    // Common header for the data shreds.
    let mut common_header_data = ShredCommonHeader {
        signature: Signature::default(),
        shred_variant: ShredVariant::MerkleData {
            proof_size,
            resigned: false,
        },
        slot,
        index: next_shred_index,
        version: shred_version,
        fec_set_index: next_shred_index,
    };

    // Common header for the coding shreds.
    let mut common_header_code = ShredCommonHeader {
        shred_variant: ShredVariant::MerkleCode {
            proof_size,
            resigned: false,
        },
        index: next_code_index,
        ..common_header_data
    };

    // Data header for the data shreds.
    let data_header = {
        let parent_offset = slot
            .checked_sub(parent_slot)
            .and_then(|offset| u16::try_from(offset).ok())
            .ok_or(Error::InvalidParentSlot { slot, parent_slot })?;
        let flags = ShredFlags::from_reference_tick(reference_tick);
        DataShredHeader {
            parent_offset,
            flags,
            size: 0u16,
        }
    };

    stats.data_bytes += data.len();

    // Data is split into full FEC sets, with the remainder going into a final,
    // padded, FEC set. Every FEC set but the last one has to be full: a set
    // containing a data shred below maximum size must carry the batch complete
    // flag on its last data shred, and the only place a batch may end is at the
    // end of the data.
    let (last_set_buffer_size, last_set_total_size) = if is_last_in_slot {
        (
            data_buffer_per_shred_size_signed,
            data_buffer_total_size_signed,
        )
    } else {
        (data_buffer_per_shred_size, data_buffer_total_size)
    };
    // +1 for the final, potentially empty, FEC set that we always emit: when the data
    // exactly fills the preceding sets we still have to emit one to carry the batch
    // complete flag, resigned and with last_in_slot set if it also completes the slot.
    let number_of_fec_sets = 1 + data
        .len()
        .saturating_sub(last_set_total_size)
        .div_ceil(data_buffer_total_size);
    let mut shreds = Vec::<Shred>::with_capacity(SHREDS_PER_FEC_BLOCK * number_of_fec_sets);

    while data.len() > last_set_total_size {
        // When the data is too short to fill a non-resigned FEC set, but still too long for
        // the final resigned one, a full resigned set is emitted instead.
        let (resigned, buffer_size, total_size) = if data.len() > data_buffer_total_size {
            (false, data_buffer_per_shred_size, data_buffer_total_size)
        } else {
            debug_assert!(
                is_last_in_slot,
                "only the last batch in a slot may emit a resigned FEC set"
            );
            (
                true,
                data_buffer_per_shred_size_signed,
                data_buffer_total_size_signed,
            )
        };
        let (chunk, rest) = data.split_at(total_size);
        shred_fec_set(
            proof_size,
            resigned,
            chunk,
            buffer_size,
            &mut common_header_data,
            &mut common_header_code,
            data_header,
            &mut shreds,
        );
        data = rest;
    }
    stats.padding_bytes += last_set_total_size - data.len();
    shred_fec_set(
        proof_size,
        is_last_in_slot,
        data,
        last_set_buffer_size,
        &mut common_header_data,
        &mut common_header_code,
        data_header,
        &mut shreds,
    );

    // Adjust flags for the very last data shred.
    if let Some(Shred::ShredData(shred)) = shreds
        .iter_mut()
        .rev()
        .find(|shred| matches!(shred, Shred::ShredData(_)))
    {
        shred.data_header.flags |= if is_last_in_slot {
            ShredFlags::LAST_SHRED_IN_SLOT // also implies DATA_COMPLETE_SHRED
        } else {
            ShredFlags::DATA_COMPLETE_SHRED
        };

        // Record metrics for number data shreds generated.
        let num_data_shreds = shred.common_header.index - next_shred_index;
        stats.record_num_data_shreds(num_data_shreds as usize);
    }
    stats.gen_data_elapsed += now.elapsed().as_micros() as u64;

    // Generate Merkle for all erasure batches.
    let now = Instant::now();
    // Group shreds by their respective erasure-batch.
    let mut batches = shreds.chunk_by_mut(|a, b| a.fec_set_index() == b.fec_set_index());

    // We have to process erasure batches serially because the Merkle tree
    // (and so the signature) cannot be computed without the Merkle root of
    // the previous erasure batch.
    batches.try_fold(chained_merkle_root, |chained_merkle_root, batch| {
        finish_erasure_batch(keypair, batch, chained_merkle_root)
    })?;
    stats.gen_coding_elapsed += now.elapsed().as_micros() as u64;
    Ok(shreds)
}

#[allow(clippy::too_many_arguments)]
fn shred_fec_set(
    proof_size: u8,
    resigned: bool,
    data: &[u8],
    data_buffer_per_shred_size: usize,
    common_header_data: &mut ShredCommonHeader,
    common_header_code: &mut ShredCommonHeader,
    data_header: DataShredHeader,
    shreds: &mut Vec<Shred>,
) {
    common_header_data.shred_variant = ShredVariant::MerkleData {
        proof_size,
        resigned,
    };
    common_header_code.shred_variant = ShredVariant::MerkleCode {
        proof_size,
        resigned,
    };
    super::shred_fec_set(
        data,
        data_buffer_per_shred_size,
        common_header_data,
        common_header_code,
        data_header,
        shreds,
    );
}
