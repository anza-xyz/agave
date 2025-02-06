// Helper methods to extract pieces of the shred from the payload without
// deserializing the entire payload.
#![deny(clippy::indexing_slicing)]
use {
    crate::shred::{
        self, merkle::SIZE_OF_MERKLE_ROOT, traits::Shred, Error, Nonce, ShredFlags, ShredId,
        ShredType, ShredVariant, SignedData, SIZE_OF_COMMON_SHRED_HEADER,
    },
    solana_perf::packet::Packet,
    solana_sdk::{
        clock::Slot,
        hash::Hash,
        signature::{Signature, Signer, SIGNATURE_BYTES},
        signer::keypair::Keypair,
    },
    std::ops::Range,
};
#[cfg(test)]
use {
    rand::{seq::SliceRandom, Rng},
    std::collections::HashMap,
};

#[inline]
fn get_shred_size(shred: &[u8]) -> Option<usize> {
    // Legacy data shreds have zero padding at the end which might have been
    // trimmed. Other variants do not have any trailing zeros.
    Some(match get_shred_variant(shred).ok()? {
        ShredVariant::LegacyCode => shred::legacy::ShredCode::SIZE_OF_PAYLOAD,
        ShredVariant::LegacyData => shred::legacy::ShredData::SIZE_OF_PAYLOAD.min(shred.len()),
        ShredVariant::MerkleCode { .. } => shred::merkle::ShredCode::SIZE_OF_PAYLOAD,
        ShredVariant::MerkleData { .. } => shred::merkle::ShredData::SIZE_OF_PAYLOAD,
    })
}

#[inline]
pub fn get_shred(packet: &Packet) -> Option<&[u8]> {
    let data = packet.data(..)?;
    data.get(..get_shred_size(data)?)
}

#[inline]
pub fn get_shred_mut(packet: &mut Packet) -> Option<&mut [u8]> {
    let buffer = packet.buffer_mut();
    buffer.get_mut(..get_shred_size(buffer)?)
}

#[inline]
pub fn get_shred_and_repair_nonce(packet: &Packet) -> Option<(&[u8], Option<Nonce>)> {
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

// The caller should verify first that the shred is data and not code!
#[inline]
pub(super) fn get_parent_offset(shred: &[u8]) -> Option<u16> {
    debug_assert_eq!(get_shred_type(shred).unwrap(), ShredType::Data);
    let bytes = <[u8; 2]>::try_from(shred.get(83..83 + 2)?).unwrap();
    Some(u16::from_le_bytes(bytes))
}

// Returns DataShredHeader.flags.
#[inline]
pub(crate) fn get_flags(shred: &[u8]) -> Result<ShredFlags, Error> {
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
        ShredVariant::LegacyCode => Err(Error::InvalidShredType),
        ShredVariant::MerkleCode { .. } => Err(Error::InvalidShredType),
        ShredVariant::LegacyData => {
            shred::legacy::ShredData::get_data(shred, get_data_size(shred)?)
        }
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => shred::merkle::ShredData::get_data(
            shred,
            proof_size,
            chained,
            resigned,
            get_data_size(shred)?,
        ),
    }
}

#[inline]
pub fn get_shred_id(shred: &[u8]) -> Option<ShredId> {
    Some(ShredId(
        get_slot(shred)?,
        get_index(shred)?,
        get_shred_type(shred).ok()?,
    ))
}

pub(crate) fn get_signed_data(shred: &[u8]) -> Option<SignedData> {
    let data = match get_shred_variant(shred).ok()? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => {
            let chunk = shred.get(shred::legacy::SIGNED_MESSAGE_OFFSETS)?;
            SignedData::Chunk(chunk)
        }
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => {
            let merkle_root =
                shred::merkle::ShredCode::get_merkle_root(shred, proof_size, chained, resigned)?;
            SignedData::MerkleRoot(merkle_root)
        }
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => {
            let merkle_root =
                shred::merkle::ShredData::get_merkle_root(shred, proof_size, chained, resigned)?;
            SignedData::MerkleRoot(merkle_root)
        }
    };
    Some(data)
}

// Returns offsets within the shred payload which is signed.
pub(crate) fn get_signed_data_offsets(shred: &[u8]) -> Option<Range<usize>> {
    match get_shred_variant(shred).ok()? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => {
            let offsets = shred::legacy::SIGNED_MESSAGE_OFFSETS;
            (offsets.end <= shred.len()).then_some(offsets)
        }
        // Merkle shreds sign merkle tree root which can be recovered from
        // the merkle proof embedded in the payload but itself is not
        // stored the payload.
        ShredVariant::MerkleCode { .. } => None,
        ShredVariant::MerkleData { .. } => None,
    }
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
        ShredVariant::LegacyCode | ShredVariant::LegacyData => None,
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => shred::merkle::ShredCode::get_merkle_root(shred, proof_size, chained, resigned),
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => shred::merkle::ShredData::get_merkle_root(shred, proof_size, chained, resigned),
    }
}

pub(crate) fn get_chained_merkle_root(shred: &[u8]) -> Option<Hash> {
    let offset = match get_shred_variant(shred).ok()? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => return None,
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => {
            shred::merkle::ShredCode::get_chained_merkle_root_offset(proof_size, chained, resigned)
        }
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => {
            shred::merkle::ShredData::get_chained_merkle_root_offset(proof_size, chained, resigned)
        }
    }
    .ok()?;
    let merkle_root = shred.get(offset..offset + SIZE_OF_MERKLE_ROOT)?;
    Some(Hash::from(
        <[u8; SIZE_OF_MERKLE_ROOT]>::try_from(merkle_root).unwrap(),
    ))
}

fn get_retransmitter_signature_offset(shred: &[u8]) -> Result<usize, Error> {
    match get_shred_variant(shred)? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => Err(Error::InvalidShredVariant),
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => shred::merkle::ShredCode::get_retransmitter_signature_offset(
            proof_size, chained, resigned,
        ),
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => shred::merkle::ShredData::get_retransmitter_signature_offset(
            proof_size, chained, resigned,
        ),
    }
}

pub fn get_retransmitter_signature(shred: &[u8]) -> Result<Signature, Error> {
    let offset = get_retransmitter_signature_offset(shred)?;
    let Some(bytes) = shred.get(offset..offset + 64) else {
        return Err(Error::InvalidPayloadSize(shred.len()));
    };
    Ok(Signature::from(<[u8; 64]>::try_from(bytes).unwrap()))
}

pub(crate) fn is_retransmitter_signed_variant(shred: &[u8]) -> Result<bool, Error> {
    match get_shred_variant(shred)? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => Ok(false),
        ShredVariant::MerkleCode {
            proof_size: _,
            chained: _,
            resigned,
        } => Ok(resigned),
        ShredVariant::MerkleData {
            proof_size: _,
            chained: _,
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

/// Resigns the shred's Merkle root as the retransmitter node in the
/// Turbine broadcast tree. This signature is in addition to leader's
/// signature which is left intact.
pub fn resign_shred(shred: &mut [u8], keypair: &Keypair) -> Result<(), Error> {
    let (offset, merkle_root) = match get_shred_variant(shred)? {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => {
            return Err(Error::InvalidShredVariant)
        }
        ShredVariant::MerkleCode {
            proof_size,
            chained,
            resigned,
        } => (
            shred::merkle::ShredCode::get_retransmitter_signature_offset(
                proof_size, chained, resigned,
            )?,
            shred::merkle::ShredCode::get_merkle_root(shred, proof_size, chained, resigned)
                .ok_or(Error::InvalidMerkleRoot)?,
        ),
        ShredVariant::MerkleData {
            proof_size,
            chained,
            resigned,
        } => (
            shred::merkle::ShredData::get_retransmitter_signature_offset(
                proof_size, chained, resigned,
            )?,
            shred::merkle::ShredData::get_merkle_root(shred, proof_size, chained, resigned)
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
        *byte = rng.gen::<u8>().max(1u8).wrapping_add(*byte);
    }
    let shred = get_shred(packet).unwrap();
    let merkle_variant = match get_shred_variant(shred).unwrap() {
        ShredVariant::LegacyCode | ShredVariant::LegacyData => None,
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
    let coin_flip: bool = rng.gen();
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
            .or_else(|| {
                let Range { start, end } = get_signed_data_offsets(shred)?;
                Some(start + 1..end) // +1 to exclude ShredVariant.
            });
        modify_packet(rng, packet, offsets.unwrap());
    }
    // Assert that the signature no longer verifies.
    let shred = get_shred(packet).unwrap();
    let slot = get_slot(shred).unwrap();
    let signature = get_signature(shred).unwrap();
    if coin_flip {
        let pubkey = keypairs[&slot].pubkey();
        let data = get_signed_data(shred).unwrap();
        assert!(!signature.verify(pubkey.as_ref(), data.as_ref()));
        if let Some(offsets) = get_signed_data_offsets(shred) {
            assert!(!signature.verify(pubkey.as_ref(), &shred[offsets]));
        }
    } else {
        // Slot may have been corrupted and no longer mapping to a keypair.
        let pubkey = keypairs.get(&slot).map(Keypair::pubkey).unwrap_or_default();
        if let Some(data) = get_signed_data(shred) {
            assert!(!signature.verify(pubkey.as_ref(), data.as_ref()));
        }
        let offsets = get_signed_data_offsets(shred).unwrap_or_default();
        assert!(!signature.verify(pubkey.as_ref(), &shred[offsets]));
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::shred::{tests::make_merkle_shreds_for_tests, traits::ShredData},
        assert_matches::assert_matches,
        rand::Rng,
        solana_perf::packet::PacketFlags,
        std::io::{Cursor, Write},
        test_case::test_case,
    };

    fn make_dummy_signature<R: Rng>(rng: &mut R) -> Signature {
        let mut signature = [0u8; 64];
        rng.fill(&mut signature[..]);
        Signature::from(signature)
    }

    fn write_shred<R: Rng>(
        rng: &mut R,
        shred: impl AsRef<[u8]>,
        nonce: Option<Nonce>,
        packet: &mut Packet,
    ) {
        let buffer = packet.buffer_mut();
        let capacity = buffer.len();
        let mut cursor = Cursor::new(buffer);
        cursor.write_all(shred.as_ref()).unwrap();
        // Write some random many bytes trailing shred payload.
        let mut bytes = {
            let size = capacity
                - cursor.position() as usize
                - if nonce.is_some() {
                    std::mem::size_of::<Nonce>()
                } else {
                    0
                };
            vec![0u8; rng.gen_range(0..=size)]
        };
        rng.fill(&mut bytes[..]);
        cursor.write_all(&bytes).unwrap();
        // Write nonce after random trailing bytes.
        if let Some(nonce) = nonce {
            cursor.write_all(&nonce.to_le_bytes()).unwrap();
        }
        packet.meta_mut().size = usize::try_from(cursor.position()).unwrap();
    }

    #[test_case(false, false, false)]
    #[test_case(false, false, true)]
    #[test_case(false, true, false)]
    #[test_case(false, true, true)]
    #[test_case(true, false, false)]
    #[test_case(true, false, true)]
    #[test_case(true, true, false)]
    #[test_case(true, true, true)]
    fn test_merkle_shred_wire_layout(repaired: bool, chained: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();
        let slot = 318_230_963 + rng.gen_range(0..318_230_963);
        let data_size = 1200 * rng.gen_range(32..64);
        let mut shreds =
            make_merkle_shreds_for_tests(&mut rng, slot, data_size, chained, is_last_in_slot)
                .unwrap();
        for shred in &mut shreds {
            let signature = make_dummy_signature(&mut rng);
            if chained && is_last_in_slot {
                shred.set_retransmitter_signature(&signature).unwrap();
            } else {
                assert_matches!(
                    shred.set_retransmitter_signature(&signature),
                    Err(Error::InvalidShredVariant)
                );
            }
        }
        let mut packet = Packet::default();
        if repaired {
            packet.meta_mut().flags |= PacketFlags::REPAIR;
        }
        for shred in &shreds {
            let nonce = repaired.then(|| rng.gen::<Nonce>());
            write_shred(&mut rng, shred.payload(), nonce, &mut packet);
            assert_eq!(
                packet.data(..).map(get_shred_size).unwrap().unwrap(),
                shred.payload().len()
            );
            assert_eq!(get_shred(&packet).unwrap(), shred.payload().as_ref());
            assert_eq!(
                get_shred_mut(&mut packet).unwrap(),
                shred.payload().as_ref(),
            );
            assert_eq!(
                get_shred_and_repair_nonce(&packet).unwrap(),
                (shred.payload().as_ref(), nonce),
            );
            let bytes = get_shred(&packet).unwrap();
            let shred_common_header = shred.common_header();
            assert_eq!(
                get_common_header_bytes(bytes).unwrap(),
                bincode::serialize(shred_common_header).unwrap(),
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
                get_signed_data(bytes).unwrap(),
                SignedData::MerkleRoot(shred.merkle_root().unwrap())
            );
            assert_matches!(get_signed_data_offsets(bytes), None);
            assert_eq!(
                get_merkle_root(bytes).unwrap(),
                shred.merkle_root().unwrap(),
            );
            if chained {
                assert_eq!(
                    get_chained_merkle_root(bytes).unwrap(),
                    shred.chained_merkle_root().unwrap(),
                );
            } else {
                assert_matches!(get_chained_merkle_root(bytes), None);
            }
            assert_eq!(
                is_retransmitter_signed_variant(bytes).unwrap(),
                chained && is_last_in_slot
            );
            if chained && is_last_in_slot {
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
                    let shred = shred::shred_data::ShredData::Merkle(shred.clone());
                    shred.reference_tick()
                });
            }
        }
    }
}
