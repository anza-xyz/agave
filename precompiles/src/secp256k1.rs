use {
    agave_feature_set::FeatureSet,
    digest::Digest,
    solana_precompile_error::PrecompileError,
    solana_secp256k1_program::{
        construct_eth_pubkey, SecpSignatureOffsets, HASHED_PUBKEY_SERIALIZED_SIZE,
        SIGNATURE_OFFSETS_SERIALIZED_SIZE, SIGNATURE_SERIALIZED_SIZE,
    },
};

/// Verifies the signatures specified in the secp256k1 instruction data.
///
/// This is the same as the verification routine executed by the runtime's secp256k1 native program,
/// and is primarily of use to the runtime.
///
/// `data` is the secp256k1 program's instruction data. `instruction_datas` is
/// the full slice of instruction datas for all instructions in the transaction,
/// including the secp256k1 program's instruction data.
///
/// `feature_set` is the set of active Solana features. It is used to enable or
/// disable a few minor additional checks that were activated on chain
/// subsequent to the addition of the secp256k1 native program. For many
/// purposes passing `FeatureSet::all_enabled()` is reasonable.
pub fn verify(
    data: &[u8],
    instruction_datas: &[&[u8]],
    _feature_set: &FeatureSet,
) -> Result<(), PrecompileError> {
    if data.is_empty() {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let count = data[0] as usize;
    if count == 0 && data.len() > 1 {
        // count is zero but the instruction data indicates that is probably not
        // correct, fail the instruction to catch probable invalid secp256k1
        // instruction construction.
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let expected_data_size = count
        .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
        .saturating_add(1);
    if data.len() < expected_data_size {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    for i in 0..count {
        let start = i
            .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
            .saturating_add(1);
        let end = start.saturating_add(SIGNATURE_OFFSETS_SERIALIZED_SIZE);

        let offsets: SecpSignatureOffsets = bincode::deserialize(&data[start..end])
            .map_err(|_| PrecompileError::InvalidSignature)?;

        // Parse out signature
        let signature_index = offsets.signature_instruction_index as usize;
        if signature_index >= instruction_datas.len() {
            return Err(PrecompileError::InvalidInstructionDataSize);
        }
        let signature_instruction = instruction_datas[signature_index];
        let sig_start = offsets.signature_offset as usize;
        let sig_end = sig_start.saturating_add(SIGNATURE_SERIALIZED_SIZE);
        if sig_end >= signature_instruction.len() {
            return Err(PrecompileError::InvalidSignature);
        }

        let signature = libsecp256k1::Signature::parse_standard_slice(
            &signature_instruction[sig_start..sig_end],
        )
        .map_err(|_| PrecompileError::InvalidSignature)?;

        let recovery_id = libsecp256k1::RecoveryId::parse(signature_instruction[sig_end])
            .map_err(|_| PrecompileError::InvalidRecoveryId)?;

        // Parse out pubkey
        let eth_address_slice = get_data_slice(
            instruction_datas,
            offsets.eth_address_instruction_index,
            offsets.eth_address_offset,
            HASHED_PUBKEY_SERIALIZED_SIZE,
        )?;

        // Parse out message
        let message_slice = get_data_slice(
            instruction_datas,
            offsets.message_instruction_index,
            offsets.message_data_offset,
            offsets.message_data_size as usize,
        )?;

        let mut hasher = sha3::Keccak256::new();
        hasher.update(message_slice);
        let message_hash = hasher.finalize();

        let pubkey = libsecp256k1::recover(
            &libsecp256k1::Message::parse_slice(&message_hash).unwrap(),
            &signature,
            &recovery_id,
        )
        .map_err(|_| PrecompileError::InvalidSignature)?;
        let eth_address = construct_eth_pubkey(&pubkey);

        if eth_address_slice != eth_address {
            return Err(PrecompileError::InvalidSignature);
        }
    }
    Ok(())
}

fn get_data_slice<'a>(
    instruction_datas: &'a [&[u8]],
    instruction_index: u8,
    offset_start: u16,
    size: usize,
) -> Result<&'a [u8], PrecompileError> {
    let signature_index = instruction_index as usize;
    if signature_index >= instruction_datas.len() {
        return Err(PrecompileError::InvalidDataOffsets);
    }
    let signature_instruction = &instruction_datas[signature_index];
    let start = offset_start as usize;
    let end = start.saturating_add(size);
    if end > signature_instruction.len() {
        return Err(PrecompileError::InvalidSignature);
    }

    Ok(&instruction_datas[signature_index][start..end])
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        rand0_7::{thread_rng, Rng},
        solana_keccak_hasher as keccak,
        solana_secp256k1_program::{new_secp256k1_instruction, DATA_START},
    };

    fn test_case(
        num_signatures: u8,
        offsets: &SecpSignatureOffsets,
    ) -> Result<(), PrecompileError> {
        let mut instruction_data = vec![0u8; DATA_START];
        instruction_data[0] = num_signatures;
        let writer = std::io::Cursor::new(&mut instruction_data[1..]);
        bincode::serialize_into(writer, &offsets).unwrap();
        let feature_set = FeatureSet::all_enabled();
        verify(&instruction_data, &[&[0u8; 100]], &feature_set)
    }

    #[test]
    fn test_invalid_offsets() {
        solana_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = SecpSignatureOffsets::default();
        instruction_data[0] = 1;
        let writer = std::io::Cursor::new(&mut instruction_data[1..]);
        bincode::serialize_into(writer, &offsets).unwrap();
        instruction_data.truncate(instruction_data.len() - 1);
        let feature_set = FeatureSet::all_enabled();

        assert_eq!(
            verify(&instruction_data, &[&[0u8; 100]], &feature_set),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        let offsets = SecpSignatureOffsets {
            signature_instruction_index: 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        let offsets = SecpSignatureOffsets {
            message_instruction_index: 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = SecpSignatureOffsets {
            eth_address_instruction_index: 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_message_data_offsets() {
        let offsets = SecpSignatureOffsets {
            message_data_offset: 99,
            message_data_size: 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = SecpSignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = SecpSignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1000,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = SecpSignatureOffsets {
            message_data_offset: u16::MAX,
            message_data_size: u16::MAX,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );
    }

    #[test]
    fn test_eth_offset() {
        let offsets = SecpSignatureOffsets {
            eth_address_offset: u16::MAX,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = SecpSignatureOffsets {
            eth_address_offset: 100 - HASHED_PUBKEY_SERIALIZED_SIZE as u16 + 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );
    }

    #[test]
    fn test_signature_offset() {
        let offsets = SecpSignatureOffsets {
            signature_offset: u16::MAX,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = SecpSignatureOffsets {
            signature_offset: 100 - SIGNATURE_SERIALIZED_SIZE as u16 + 1,
            ..SecpSignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );
    }

    #[test]
    fn test_count_is_zero_but_sig_data_exists() {
        solana_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = SecpSignatureOffsets::default();
        instruction_data[0] = 0;
        let writer = std::io::Cursor::new(&mut instruction_data[1..]);
        bincode::serialize_into(writer, &offsets).unwrap();
        let feature_set = FeatureSet::all_enabled();

        assert_eq!(
            verify(&instruction_data, &[&[0u8; 100]], &feature_set),
            Err(PrecompileError::InvalidInstructionDataSize)
        );
    }

    #[test]
    fn test_secp256k1() {
        solana_logger::setup();
        let offsets = SecpSignatureOffsets::default();
        assert_eq!(
            bincode::serialized_size(&offsets).unwrap() as usize,
            SIGNATURE_OFFSETS_SERIALIZED_SIZE
        );

        let secp_privkey = libsecp256k1::SecretKey::random(&mut thread_rng());
        let message_arr = b"hello";
        let mut instruction = new_secp256k1_instruction(&secp_privkey, message_arr);
        let feature_set = FeatureSet::all_enabled();
        assert!(verify(&instruction.data, &[&instruction.data], &feature_set).is_ok());

        let index = thread_rng().gen_range(0, instruction.data.len());
        instruction.data[index] = instruction.data[index].wrapping_add(12);
        assert!(verify(&instruction.data, &[&instruction.data], &feature_set).is_err());
    }

    // Signatures are malleable.
    #[test]
    fn test_malleability() {
        solana_logger::setup();

        let secret_key = libsecp256k1::SecretKey::random(&mut thread_rng());
        let public_key = libsecp256k1::PublicKey::from_secret_key(&secret_key);
        let eth_address = construct_eth_pubkey(&public_key);

        let message = b"hello";
        let message_hash = {
            let mut hasher = keccak::Hasher::default();
            hasher.hash(message);
            hasher.result()
        };

        let secp_message = libsecp256k1::Message::parse(&message_hash.0);
        let (signature, recovery_id) = libsecp256k1::sign(&secp_message, &secret_key);

        // Flip the S value in the signature to make a different but valid signature.
        let mut alt_signature = signature;
        alt_signature.s = -alt_signature.s;
        let alt_recovery_id = libsecp256k1::RecoveryId::parse(recovery_id.serialize() ^ 1).unwrap();

        let mut data: Vec<u8> = vec![];
        let mut both_offsets = vec![];

        // Verify both signatures of the same message.
        let sigs = [(signature, recovery_id), (alt_signature, alt_recovery_id)];
        for (signature, recovery_id) in sigs.iter() {
            let signature_offset = data.len();
            data.extend(signature.serialize());
            data.push(recovery_id.serialize());
            let eth_address_offset = data.len();
            data.extend(eth_address);
            let message_data_offset = data.len();
            data.extend(message);

            let data_start = 1 + SIGNATURE_OFFSETS_SERIALIZED_SIZE * 2;

            let offsets = SecpSignatureOffsets {
                signature_offset: (signature_offset + data_start) as u16,
                signature_instruction_index: 0,
                eth_address_offset: (eth_address_offset + data_start) as u16,
                eth_address_instruction_index: 0,
                message_data_offset: (message_data_offset + data_start) as u16,
                message_data_size: message.len() as u16,
                message_instruction_index: 0,
            };

            both_offsets.push(offsets);
        }

        let mut instruction_data: Vec<u8> = vec![2];

        for offsets in both_offsets {
            let offsets = bincode::serialize(&offsets).unwrap();
            instruction_data.extend(offsets);
        }

        instruction_data.extend(data);

        verify(
            &instruction_data,
            &[&instruction_data],
            &FeatureSet::all_enabled(),
        )
        .unwrap();
    }
}
