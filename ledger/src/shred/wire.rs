// Helper methods to extract pieces of the shred from the payload without
// deserializing the entire payload.
#![deny(clippy::indexing_slicing)]
use {
    crate::{
        blockstore_meta::ErasureConfig,
        shred::{
            self, merkle_tree::SIZE_OF_MERKLE_ROOT, traits::Shred, Error, Nonce, ShredFlags,
            ShredId, ShredType, ShredVariant, SIZE_OF_COMMON_SHRED_HEADER,
        },
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_perf::packet::{PacketRef, PacketRefMut},
    solana_signature::{Signature, SIGNATURE_BYTES},
    solana_signer::Signer,
};
#[cfg(test)]
use {
    rand::{prelude::IndexedMutRandom as _, Rng},
    solana_perf::packet::Packet,
    std::collections::HashMap,
    std::ops::Range,
};

#[inline]
fn get_shred_size(shred: &[u8]) -> Option<usize> {
    match get_shred_variant(shred).ok()? {
        ShredVariant::MerkleCode { .. } => Some(shred::merkle::ShredCode::SIZE_OF_PAYLOAD),
        ShredVariant::MerkleData { .. } => Some(shred::merkle::ShredData::SIZE_OF_PAYLOAD),
    }
}

#[inline]
pub fn get_shred<'a, P>(packet: P) -> Option<&'a [u8]>
where
    P: Into<PacketRef<'a>>,
{
    let data = packet.into().data(..)?;
    data.get(..get_shred_size(data)?)
}

#[inline]
pub fn get_shred_mut(buffer: &mut [u8]) -> Option<&mut [u8]> {
    buffer.get_mut(..get_shred_size(buffer)?)
}

#[inline]
pub fn get_shred_and_repair_nonce(packet: PacketRef<'_>) -> Option<(&[u8], Option<Nonce>)> {
    let data = packet.data(..)?;
    let shred = data.get(..get_shred_size(data)?)?;
    if !packet.meta().repair() {
        return Some((shred, None));
    }
    let offset = data.len().checked_sub(4)?;
    let nonce = <[u8; 4]>::try_from(data.get(offset..)?).ok()?;
    let nonce = u32::from_le_bytes(nonce);
    Some((shred, Some(nonce)))
}

#[inline]
pub fn get_common_header_bytes(shred: &[u8]) -> Option<&[u8]> {
    shred.get(..SIZE_OF_COMMON_SHRED_HEADER)
}

#[inline]
pub(crate) fn get_signature(shred: &[u8]) -> Option<Signature> {
    let bytes = <[u8; 64]>::try_from(shred.get(..64)?).unwrap();
    Some(Signature::from(bytes))
}

#[cfg(test)]
pub(crate) const fn get_signature_range() -> Range<usize> {
    0..SIGNATURE_BYTES
}

#[inline]
pub(super) fn get_shred_variant(shred: &[u8]) -> Result<ShredVariant, Error> {
    let Some(&shred_variant) = shred.get(64) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    ShredVariant::try_from(shred_variant).map_err(|_| Error::InvalidShredVariant)
}

#[inline]
pub(super) fn get_shred_type(shred: &[u8]) -> Result<ShredType, Error> {
    get_shred_variant(shred).map(ShredType::from)
}

#[inline]
pub fn get_slot(shred: &[u8]) -> Option<Slot> {
    let bytes = <[u8; 8]>::try_from(shred.get(65..65 + 8)?).unwrap();
    Some(Slot::from_le_bytes(bytes))
}

#[inline]
pub fn get_index(shred: &[u8]) -> Option<u32> {
    let bytes = <[u8; 4]>::try_from(shred.get(73..73 + 4)?).unwrap();
    Some(u32::from_le_bytes(bytes))
}

#[inline]
pub(super) fn get_version(shred: &[u8]) -> Option<u16> {
    let bytes = <[u8; 2]>::try_from(shred.get(77..77 + 2)?).unwrap();
    Some(u16::from_le_bytes(bytes))
}

#[inline]
pub fn get_fec_set_index(shred: &[u8]) -> Option<u32> {
    let bytes = <[u8; 4]>::try_from(shred.get(79..79 + 4)?).unwrap();
    Some(u32::from_le_bytes(bytes))
}

// The caller should verify first that the shred is data and not code!
#[inline]
pub(super) fn get_parent_offset(shred: &[u8]) -> Option<u16> {
    debug_assert_eq!(get_shred_type(shred).unwrap(), ShredType::Data);
    let bytes = <[u8; 2]>::try_from(shred.get(83..83 + 2)?).unwrap();
    Some(u16::from_le_bytes(bytes))
}

#[cfg(test)]
/// this will corrupt the shred by setting parent offset bytes
pub(crate) fn corrupt_and_set_parent_offset(shred: &mut [u8], parent_offset: u16) {
    let bytes = parent_offset.to_le_bytes();
    assert_eq!(get_shred_type(shred).unwrap(), ShredType::Data);
    shred.get_mut(83..83 + 2).unwrap().copy_from_slice(&bytes);
}

// Returns DataShredHeader.flags if the shred is data.
// Returns Error::InvalidShredType for coding shreds.
#[inline]
pub fn get_flags(shred: &[u8]) -> Result<ShredFlags, Error> {
    match get_shred_type(shred)? {
        ShredType::Code => Err(Error::InvalidShredType),
        ShredType::Data => {
            let Some(flags) = shred.get(85).copied() else {
                return Err(Error::InvalidPayloadSize(shred.len()));
            };
            ShredFlags::from_bits(flags).ok_or(Error::InvalidShredFlags(flags))
        }
    }
}

// Returns DataShredHeader.size for data shreds.
// The caller should verify first that the shred is data and not code!
#[inline]
fn get_data_size(shred: &[u8]) -> Result<u16, Error> {
    debug_assert_eq!(get_shred_type(shred).unwrap(), ShredType::Data);
    let Some(bytes) = shred.get(86..86 + 2) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    let bytes = <[u8; 2]>::try_from(bytes).unwrap();
    Ok(u16::from_le_bytes(bytes))
}

#[inline]
pub(crate) fn get_data(shred: &[u8]) -> Result<&[u8], Error> {
    match get_shred_variant(shred)? {
        ShredVariant::MerkleCode { .. } => Err(Error::InvalidShredType),
        ShredVariant::MerkleData {
            proof_size,
            resigned,
        } => shred::merkle::ShredData::get_data(shred, proof_size, resigned, get_data_size(shred)?),
    }
}

/// Returns the ErasureConfig specified by the coding shred, or an Error if
/// the shred is a data shred
#[inline]
pub(crate) fn get_erasure_config(shred: &[u8]) -> Result<ErasureConfig, Error> {
    if !matches!(get_shred_type(shred).unwrap(), ShredType::Code) {
        return Err(Error::InvalidShredType);
    }
    let Some(num_data_bytes) = shred.get(83..83 + 2) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    let Some(num_coding_bytes) = shred.get(85..85 + 2) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    let num_data = <[u8; 2]>::try_from(num_data_bytes)
        .map(u16::from_le_bytes)
        .map(usize::from)
        .map_err(|_| Error::InvalidErasureConfig)?;
    let num_coding = <[u8; 2]>::try_from(num_coding_bytes)
        .map(u16::from_le_bytes)
        .map(usize::from)
        .map_err(|_| Error::InvalidErasureConfig)?;

    Ok(ErasureConfig {
        num_data,
        num_coding,
    })
}

#[inline]
pub fn get_shred_id(shred: &[u8]) -> Option<ShredId> {
    Some(ShredId(
        get_slot(shred)?,
        get_index(shred)?,
        get_shred_type(shred).ok()?,
    ))
}

pub fn get_reference_tick(shred: &[u8]) -> Result<u8, Error> {
    if get_shred_type(shred)? != ShredType::Data {
        return Err(Error::InvalidShredType);
    }
    let Some(flags) = shred.get(85) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    Ok(flags & ShredFlags::SHRED_TICK_REFERENCE_MASK.bits())
}

pub fn get_merkle_root(shred: &[u8]) -> Option<Hash> {
    match get_shred_variant(shred).ok()? {
        ShredVariant::MerkleCode {
            proof_size,
            resigned,
        } => shred::merkle::ShredCode::get_merkle_root(shred, proof_size, resigned),
        ShredVariant::MerkleData {
            proof_size,
            resigned,
        } => shred::merkle::ShredData::get_merkle_root(shred, proof_size, resigned),
    }
}

pub(crate) fn get_chained_merkle_root(shred: &[u8]) -> Option<Hash> {
    let offset = match get_shred_variant(shred).ok()? {
        ShredVariant::MerkleCode {
            proof_size,
            resigned,
        } => shred::merkle::ShredCode::get_chained_merkle_root_offset(proof_size, resigned),
        ShredVariant::MerkleData {
            proof_size,
            resigned,
        } => shred::merkle::ShredData::get_chained_merkle_root_offset(proof_size, resigned),
    }
    .ok()?;
    let merkle_root = shred.get(offset..offset + SIZE_OF_MERKLE_ROOT)?;
    Some(Hash::from(
        <[u8; SIZE_OF_MERKLE_ROOT]>::try_from(merkle_root).unwrap(),
    ))
}

fn get_retransmitter_signature_offset(shred: &[u8]) -> Result<usize, Error> {
    match get_shred_variant(shred)? {
        ShredVariant::MerkleCode {
            proof_size,
            resigned,
        } => shred::merkle::ShredCode::get_retransmitter_signature_offset(proof_size, resigned),
        ShredVariant::MerkleData {
            proof_size,
            resigned,
        } => shred::merkle::ShredData::get_retransmitter_signature_offset(proof_size, resigned),
    }
}

pub fn get_retransmitter_signature(shred: &[u8]) -> Result<Signature, Error> {
    let offset = get_retransmitter_signature_offset(shred)?;
    let Some(bytes) = shred.get(offset..offset + 64) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    Ok(Signature::from(<[u8; 64]>::try_from(bytes).unwrap()))
}

pub fn is_retransmitter_signed_variant(shred: &[u8]) -> Result<bool, Error> {
    match get_shred_variant(shred)? {
        ShredVariant::MerkleCode {
            proof_size: _,
            resigned,
        } => Ok(resigned),
        ShredVariant::MerkleData {
            proof_size: _,
            resigned,
        } => Ok(resigned),
    }
}

pub fn set_retransmitter_signature(shred: &mut [u8], signature: &Signature) -> Result<(), Error> {
    let offset = get_retransmitter_signature_offset(shred)?;
    let Some(buffer) = shred.get_mut(offset..offset + SIGNATURE_BYTES) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    buffer.copy_from_slice(signature.as_ref());
    Ok(())
}

/// Resigns the packet's Merkle root as the retransmitter node in the
/// Turbine broadcast tree. This signature is in addition to leader's
/// signature which is left intact.
pub fn resign_packet(packet: &mut PacketRefMut, keypair: &Keypair) -> Result<(), Error> {
    match packet {
        PacketRefMut::Packet(packet) => {
            let shred = get_shred_mut(packet.buffer_mut()).ok_or(Error::InvalidPacketSize)?;
            resign_shred(shred, keypair)
        }
        // `Bytes` are immutable. Therefore, to resign the shred from
        // `BytesPacket`, we need to copy the packet's buffer, then modify that
        // copy and assign it to the packet.
        // We resign only the last FEC set in the block. For 50mbps coming to
        // turbine, only around 2mbps are resigned. For now, we accept the
        // necessity of copying that minority of packets.
        PacketRefMut::Bytes(packet) => {
            let mut buffer = packet.buffer().to_vec();
            let shred = get_shred_mut(&mut buffer).ok_or(Error::InvalidPacketSize)?;

            resign_shred(shred, keypair)?;

            packet.set_buffer(buffer);

            Ok(())
        }
    }
}

/// Resigns the shred's Merkle root as the retransmitter node in the
/// Turbine broadcast tree. This signature is in addition to leader's
/// signature which is left intact.
pub fn resign_shred(shred: &mut [u8], keypair: &Keypair) -> Result<(), Error> {
    let (offset, merkle_root) = match get_shred_variant(shred)? {
        ShredVariant::MerkleCode {
            proof_size,
            resigned,
        } => (
            shred::merkle::ShredCode::get_retransmitter_signature_offset(proof_size, resigned)?,
            shred::merkle::ShredCode::get_merkle_root(shred, proof_size, resigned)
                .ok_or(Error::InvalidMerkleRoot)?,
        ),
        ShredVariant::MerkleData {
            proof_size,
            resigned,
        } => (
            shred::merkle::ShredData::get_retransmitter_signature_offset(proof_size, resigned)?,
            shred::merkle::ShredData::get_merkle_root(shred, proof_size, resigned)
                .ok_or(Error::InvalidMerkleRoot)?,
        ),
    };
    let Some(buffer) = shred.get_mut(offset..offset + SIGNATURE_BYTES) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    let signature = keypair.sign_message(merkle_root.as_ref());
    buffer.copy_from_slice(signature.as_ref());
    Ok(())
}

// Minimally corrupts the packet so that the signature no longer verifies.
#[cfg(test)]
#[allow(clippy::indexing_slicing)]
pub(crate) fn corrupt_packet<R: Rng>(
    rng: &mut R,
    packet: &mut Packet,
    keypairs: &HashMap<Slot, Keypair>,
) {
    fn modify_packet<R: Rng>(rng: &mut R, packet: &mut Packet, offsets: Range<usize>) {
        let buffer = packet.buffer_mut();
        let byte = buffer[offsets].choose_mut(rng).unwrap();
        *byte = rng.random::<u8>().max(1u8).wrapping_add(*byte);
    }
    // We need to re-borrow the `packet` here, otherwise compiler considers it
    // as moved.
    let shred = get_shred(&*packet).unwrap();
    let merkle_variant = match get_shred_variant(shred).unwrap() {
        ShredVariant::MerkleCode {
            proof_size,
            resigned,
            ..
        }
        | ShredVariant::MerkleData {
            proof_size,
            resigned,
            ..
        } => Some((proof_size, resigned)),
    };
    let coin_flip: bool = rng.random();
    if coin_flip {
        // Corrupt one byte within the signature offsets.
        modify_packet(rng, packet, 0..SIGNATURE_BYTES);
    } else {
        // Corrupt one byte within the signed data offsets.
        let offsets = merkle_variant
            .map(|(proof_size, resigned)| {
                // Need to corrupt the merkle proof.
                // Proof entries are each 20 bytes at the end of shreds.
                let offset = usize::from(proof_size) * 20;
                let size = shred.len() - if resigned { SIGNATURE_BYTES } else { 0 };
                size - offset..size
            })
            .expect("Only merkle shreds are possible");
        modify_packet(rng, packet, offsets);
    }
    // Assert that the signature no longer verifies.
    let shred = get_shred(packet).unwrap();
    let slot = get_slot(shred).unwrap();
    let signature = get_signature(shred).unwrap();
    if coin_flip {
        let pubkey = keypairs[&slot].pubkey();
        let data = get_merkle_root(shred).unwrap();
        assert!(!signature.verify(pubkey.as_ref(), data.as_ref()));
    } else {
        // Slot may have been corrupted and no longer mapping to a keypair.
        let pubkey = keypairs.get(&slot).map(Keypair::pubkey).unwrap_or_default();
        if let Some(data) = get_merkle_root(shred) {
            assert!(!signature.verify(pubkey.as_ref(), data.as_ref()));
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::shred::{
            tests::make_merkle_shreds_for_tests, traits::ShredData, SHREDS_PER_FEC_BLOCK,
        },
        assert_matches::assert_matches,
        rand::Rng,
        solana_perf::packet::PacketFlags,
        test_case::test_matrix,
    };

    fn make_dummy_signature<R: Rng>(rng: &mut R) -> Signature {
        let mut signature = [0u8; 64];
        rng.fill(&mut signature[..]);
        Signature::from(signature)
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_resign_packet(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::rng();
        let slot = 318_230_963 + rng.random_range(0..318_230_963);
        let data_size = 1200 * rng.random_range(32..64);
        let mut shreds =
            make_merkle_shreds_for_tests(&mut rng, slot, data_size, is_last_in_slot).unwrap();
        // enumerate the shreds so that I have index of each shred
        let shreds_len = shreds.len();
        for (index, shred) in shreds.iter_mut().enumerate() {
            let keypair = Keypair::new();
            let signature = make_dummy_signature(&mut rng);
            let nonce = repaired.then(|| rng.random::<Nonce>());
            let is_last_batch = index >= shreds_len - SHREDS_PER_FEC_BLOCK;
            if is_last_in_slot && is_last_batch {
                shred.set_retransmitter_signature(&signature).unwrap();

                let packet = &mut shred.payload().to_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                resign_packet(&mut packet.into(), &keypair).unwrap();

                let packet = &mut shred.payload().to_bytes_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                resign_packet(&mut packet.as_mut(), &keypair).unwrap();
            } else {
                assert_matches!(
                    shred.set_retransmitter_signature(&signature),
                    Err(Error::InvalidShredVariant)
                );

                let packet = &mut shred.payload().to_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                assert_matches!(
                    resign_packet(&mut packet.into(), &keypair),
                    Err(Error::InvalidShredVariant)
                );

                let packet = &mut shred.payload().to_bytes_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                assert_matches!(
                    resign_packet(&mut packet.as_mut(), &keypair),
                    Err(Error::InvalidShredVariant)
                );
            }
        }
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_merkle_shred_wire_layout(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::rng();
        let slot = 318_230_963 + rng.random_range(0..318_230_963);
        let data_size = 1200 * rng.random_range(32..64);
        let mut shreds =
            make_merkle_shreds_for_tests(&mut rng, slot, data_size, is_last_in_slot).unwrap();
        let shreds_len = shreds.len();
        for (index, shred) in shreds.iter_mut().enumerate() {
            let signature = make_dummy_signature(&mut rng);
            let is_last_batch = index >= shreds_len - SHREDS_PER_FEC_BLOCK;
            if is_last_in_slot && is_last_batch {
                shred.set_retransmitter_signature(&signature).unwrap();
            } else {
                assert_matches!(
                    shred.set_retransmitter_signature(&signature),
                    Err(Error::InvalidShredVariant)
                );
            }
        }

        for (index, shred) in shreds.iter().enumerate() {
            let nonce = repaired.then(|| rng.random::<Nonce>());
            let is_last_batch = index >= shreds_len - SHREDS_PER_FEC_BLOCK;
            let mut packet = shred.payload().to_packet(nonce);
            if repaired {
                packet.meta_mut().flags |= PacketFlags::REPAIR;
            }
            let packet = PacketRef::Packet(&packet);
            assert_eq!(
                packet.data(..).map(get_shred_size).unwrap().unwrap(),
                shred.payload().len()
            );
            let bytes = get_shred(packet).unwrap();
            assert_eq!(bytes, shred.payload().as_ref());
            assert_eq!(
                get_shred_and_repair_nonce(packet).unwrap(),
                (shred.payload().as_ref(), nonce),
            );
            let shred_common_header = shred.common_header();
            assert_eq!(
                get_common_header_bytes(bytes).unwrap(),
                wincode::serialize(shred_common_header).unwrap(),
            );
            assert_eq!(get_signature(bytes).unwrap(), shred_common_header.signature,);
            assert_eq!(
                get_shred_variant(bytes).unwrap(),
                shred_common_header.shred_variant,
            );
            assert_eq!(
                get_shred_type(bytes).unwrap(),
                ShredType::from(shred_common_header.shred_variant)
            );
            assert_eq!(get_slot(bytes).unwrap(), shred_common_header.slot);
            assert_eq!(get_index(bytes).unwrap(), shred_common_header.index);
            assert_eq!(get_version(bytes).unwrap(), shred_common_header.version);
            assert_eq!(get_shred_id(bytes).unwrap(), {
                let shred_type = ShredType::from(shred_common_header.shred_variant);
                ShredId::new(
                    shred_common_header.slot,
                    shred_common_header.index,
                    shred_type,
                )
            });
            assert_eq!(
                get_merkle_root(bytes).unwrap(),
                shred.merkle_root().unwrap()
            );
            assert_eq!(
                get_merkle_root(bytes).unwrap(),
                shred.merkle_root().unwrap(),
            );
            assert_eq!(
                get_chained_merkle_root(bytes).unwrap(),
                shred.chained_merkle_root().unwrap(),
            );
            assert_eq!(
                is_retransmitter_signed_variant(bytes).unwrap(),
                is_last_in_slot && is_last_batch,
            );
            if is_last_in_slot && is_last_batch {
                assert_eq!(
                    get_retransmitter_signature_offset(bytes).unwrap(),
                    shred.retransmitter_signature_offset().unwrap(),
                );
                assert_eq!(
                    get_retransmitter_signature(bytes).unwrap(),
                    shred.retransmitter_signature().unwrap(),
                );
                {
                    let mut bytes = bytes.to_vec();
                    let signature = make_dummy_signature(&mut rng);
                    assert_matches!(set_retransmitter_signature(&mut bytes, &signature), Ok(()));
                    assert_eq!(get_retransmitter_signature(&bytes).unwrap(), signature);
                    let shred = shred::merkle::Shred::from_payload(bytes).unwrap();
                    assert_eq!(shred.retransmitter_signature().unwrap(), signature);
                }
                {
                    let mut bytes = bytes.to_vec();
                    let keypair = Keypair::new();
                    let signature = keypair.sign_message(shred.merkle_root().unwrap().as_ref());
                    assert_matches!(resign_shred(&mut bytes, &keypair), Ok(()));
                    assert_eq!(get_retransmitter_signature(&bytes).unwrap(), signature);
                    let shred = shred::merkle::Shred::from_payload(bytes).unwrap();
                    assert_eq!(shred.retransmitter_signature().unwrap(), signature);
                }
            } else {
                assert_matches!(
                    get_retransmitter_signature_offset(bytes),
                    Err(Error::InvalidShredVariant)
                );
                assert_matches!(
                    get_retransmitter_signature(bytes),
                    Err(Error::InvalidShredVariant)
                );
                let mut bytes = bytes.to_vec();
                let signature = make_dummy_signature(&mut rng);
                assert_matches!(
                    set_retransmitter_signature(&mut bytes, &signature),
                    Err(Error::InvalidShredVariant)
                );
                assert_eq!(bytes, shred.payload().as_ref());
                let keypair = Keypair::new();
                assert_matches!(
                    resign_shred(&mut bytes, &keypair),
                    Err(Error::InvalidShredVariant)
                );
                assert_eq!(bytes, shred.payload().as_ref());
            }
            if let shred::merkle::Shred::ShredCode(_) = shred {
                assert_matches!(get_flags(bytes), Err(Error::InvalidShredType));
                assert_matches!(get_data(bytes), Err(Error::InvalidShredType));
                assert_matches!(get_reference_tick(bytes), Err(Error::InvalidShredType));
            }
            if let shred::merkle::Shred::ShredData(shred) = shred {
                let shred_data_header = shred.data_header();
                assert_eq!(
                    get_parent_offset(bytes).unwrap(),
                    shred_data_header.parent_offset
                );
                assert_eq!(get_flags(bytes).unwrap(), shred_data_header.flags);
                assert_eq!(get_data_size(bytes).unwrap(), shred_data_header.size);
                assert_eq!(get_data(bytes).unwrap(), shred.data().unwrap());
                assert_eq!(get_reference_tick(bytes).unwrap(), {
                    shred.reference_tick()
                });
            }
        }
    }
}
