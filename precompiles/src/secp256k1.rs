use {
    agave_feature_set::FeatureSet,
    k256::{
        FieldBytes, Scalar,
        ecdsa::{RecoveryId, Signature, VerifyingKey},
        elliptic_curve::PrimeField,
    },
    solana_precompile_error::PrecompileError,
    solana_secp256k1_program::{
        HASHED_PUBKEY_SERIALIZED_SIZE, SIGNATURE_OFFSETS_SERIALIZED_SIZE,
        SIGNATURE_SERIALIZED_SIZE, SecpSignatureOffsets, eth_address_from_pubkey,
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

        let offsets: SecpSignatureOffsets = wincode::deserialize(&data[start..end])
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

        let parsed_signature = ParsedSignature::parse(
            &signature_instruction[sig_start..sig_end],
            signature_instruction[sig_end],
        )?;

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

        let message_hash: [u8; 32] = solana_keccak_hasher::hash(message_slice).to_bytes();
        let pubkey = parsed_signature.recover(&message_hash)?;
        let eth_address = eth_address_from_pubkey(&pubkey);

        if eth_address_slice != eth_address {
            return Err(PrecompileError::InvalidSignature);
        }
    }
    Ok(())
}

/// A signature and recovery id that passed the parse-time checks.
///
/// Parsing is kept separate from recovery so that malformed signatures and
/// recovery ids are reported before the eth address and message offsets are
/// checked, exactly as the `libsecp256k1` backend this replaced always did.
struct ParsedSignature {
    signature: [u8; SIGNATURE_SERIALIZED_SIZE],
    recovery_id: RecoveryId,
}

impl ParsedSignature {
    fn parse(signature: &[u8], recovery_id: u8) -> Result<Self, PrecompileError> {
        let signature: [u8; SIGNATURE_SERIALIZED_SIZE] = signature
            .try_into()
            .map_err(|_| PrecompileError::InvalidSignature)?;
        // `libsecp256k1` rejected `r` or `s` at or above the curve order when
        // parsing, but only rejected a zero `r` or `s` during recovery. Mirror
        // that split so errors are reported at the same step.
        for scalar in [&signature[..32], &signature[32..]] {
            let repr = FieldBytes::from(<[u8; 32]>::try_from(scalar).unwrap());
            if bool::from(Scalar::from_repr(repr).is_none()) {
                return Err(PrecompileError::InvalidSignature);
            }
        }
        let recovery_id =
            RecoveryId::try_from(recovery_id).map_err(|_| PrecompileError::InvalidRecoveryId)?;
        Ok(Self {
            signature,
            recovery_id,
        })
    }

    /// Recovers the public key, accepting high-S (malleable) signatures just
    /// like `libsecp256k1` did.
    fn recover(&self, message_hash: &[u8; 32]) -> Result<[u8; 64], PrecompileError> {
        // Rejects a zero `r` or `s`, like `libsecp256k1::recover`.
        let signature = Signature::try_from(self.signature.as_slice())
            .map_err(|_| PrecompileError::InvalidSignature)?;
        // Recovery only: this neither verifies the signature against the
        // recovered key nor requires a low-S signature.
        let pubkey = VerifyingKey::recover_from_prehash(message_hash, &signature, self.recovery_id)
            .map_err(|_| PrecompileError::InvalidSignature)?;
        let encoded = pubkey.to_sec1_point(false);
        Ok(encoded.as_bytes()[1..65].try_into().unwrap())
    }
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
        crate::test_verify_with_alignment,
        rand::{Rng, RngCore, SeedableRng, rngs::StdRng},
        solana_keccak_hasher as keccak,
        solana_secp256k1_program::{
            DATA_START, new_secp256k1_instruction_with_signature, sign_message,
        },
    };

    const SECP256K1_ORDER: [u8; SIGNATURE_SERIALIZED_SIZE / 2] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36,
        0x41, 0x41,
    ];

    /// Reference recovery with `libsecp256k1`, the backend the precompile
    /// used before switching to `k256`.
    fn recover_pubkey_legacy(
        message_hash: &[u8; 32],
        recovery_id: u8,
        signature: &[u8],
    ) -> Result<[u8; 64], PrecompileError> {
        let signature = libsecp256k1::Signature::parse_standard_slice(signature)
            .map_err(|_| PrecompileError::InvalidSignature)?;
        let recovery_id = libsecp256k1::RecoveryId::parse(recovery_id)
            .map_err(|_| PrecompileError::InvalidRecoveryId)?;
        let pubkey = libsecp256k1::recover(
            &libsecp256k1::Message::parse(message_hash),
            &signature,
            &recovery_id,
        )
        .map_err(|_| PrecompileError::InvalidSignature)?;
        Ok(pubkey.serialize()[1..65].try_into().unwrap())
    }

    fn recover_pubkey_k256(
        message_hash: &[u8; 32],
        recovery_id: u8,
        signature: &[u8],
    ) -> Result<[u8; 64], PrecompileError> {
        ParsedSignature::parse(signature, recovery_id)?.recover(message_hash)
    }

    // The `k256` recovery must agree with the `libsecp256k1` backend it replaced.
    #[test]
    fn test_recovery_consistent_with_libsecp256k1() {
        let secret_bytes: [u8; 32] = rand::random();
        let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
        let message = b"hello";
        let message_hash = {
            let mut hasher = keccak::Hasher::default();
            hasher.hash(message);
            hasher.result()
        };

        let secp_message = libsecp256k1::Message::parse(message_hash.as_bytes());
        let (signature, recovery_id) = libsecp256k1::sign(&secp_message, &secret_key);

        let signature_bytes = signature.serialize();
        let legacy_pubkey = recover_pubkey_legacy(
            message_hash.as_bytes(),
            recovery_id.serialize(),
            &signature_bytes,
        );
        let k256_pubkey = recover_pubkey_k256(
            message_hash.as_bytes(),
            recovery_id.serialize(),
            &signature_bytes,
        );
        assert_eq!(legacy_pubkey, k256_pubkey);

        // Flip the S value in the signature to create another valid signature.
        let mut alt_signature = signature;
        alt_signature.s = -alt_signature.s;
        let alt_recovery_id = libsecp256k1::RecoveryId::parse(recovery_id.serialize() ^ 1).unwrap();

        let alt_signature_bytes = alt_signature.serialize();
        let legacy_pubkey = recover_pubkey_legacy(
            message_hash.as_bytes(),
            alt_recovery_id.serialize(),
            &alt_signature_bytes,
        );
        let k256_pubkey = recover_pubkey_k256(
            message_hash.as_bytes(),
            alt_recovery_id.serialize(),
            &alt_signature_bytes,
        );
        assert!(legacy_pubkey.is_ok());
        assert_eq!(legacy_pubkey, k256_pubkey);

        let invalid_recovery_id = 4;
        assert_eq!(
            recover_pubkey_legacy(
                message_hash.as_bytes(),
                invalid_recovery_id,
                &signature_bytes
            ),
            Err(PrecompileError::InvalidRecoveryId)
        );
        assert_eq!(
            recover_pubkey_k256(
                message_hash.as_bytes(),
                invalid_recovery_id,
                &signature_bytes
            ),
            Err(PrecompileError::InvalidRecoveryId)
        );
    }

    /// Returns a 32-byte value biased towards the edges of the scalar range.
    fn edge_biased_bytes(rng: &mut StdRng) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        match rng.random_range(0..10) {
            0 => {}
            1 => bytes = [0xff; 32],
            2 => bytes = SECP256K1_ORDER,
            3 => {
                bytes = SECP256K1_ORDER;
                bytes[31] = bytes[31].checked_sub(1).unwrap();
            }
            4 => {
                bytes = SECP256K1_ORDER;
                bytes[31] = bytes[31].checked_add(1).unwrap();
            }
            // Small enough that adding the curve order stays below the field
            // modulus, so recovery ids 2 and 3 can succeed.
            5 => rng.fill_bytes(&mut bytes[16..]),
            6 => bytes[31] = rng.random_range(1..8),
            _ => rng.fill_bytes(&mut bytes),
        }
        bytes
    }

    // Differential test against the `libsecp256k1` backend over a fixed-seed
    // set of valid, malleated and malformed inputs.
    #[test]
    fn test_recovery_consistent_with_libsecp256k1_randomized() {
        let mut rng = StdRng::seed_from_u64(0x5ec9_256b);
        let mut recovered = 0usize;
        let mut recovered_x_reduced = 0usize;
        let mut recovered_high_s = 0usize;
        let mut rejected = 0usize;

        for i in 0..3_000 {
            let message_hash = edge_biased_bytes(&mut rng);
            let mut signature = [0u8; SIGNATURE_SERIALIZED_SIZE];
            let recovery_id;
            let mut high_s = false;
            if i % 3 == 0 {
                // A genuine signature, sometimes with `s` negated and with or
                // without the matching recovery id flip.
                let mut secret_bytes = [0u8; 32];
                rng.fill_bytes(&mut secret_bytes);
                let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
                let (mut sig, rec_id) =
                    libsecp256k1::sign(&libsecp256k1::Message::parse(&message_hash), &secret_key);
                let mut rec_id = rec_id.serialize();
                if rng.random() {
                    sig.s = -sig.s;
                    high_s = true;
                    if rng.random() {
                        rec_id ^= 1;
                    }
                }
                signature = sig.serialize();
                recovery_id = rec_id;
            } else {
                signature[..32].copy_from_slice(&edge_biased_bytes(&mut rng));
                signature[32..].copy_from_slice(&edge_biased_bytes(&mut rng));
                recovery_id = rng.random_range(0..6);
            }

            let legacy = recover_pubkey_legacy(&message_hash, recovery_id, &signature);
            let k256 = recover_pubkey_k256(&message_hash, recovery_id, &signature);
            assert_eq!(
                legacy, k256,
                "hash {message_hash:?} recovery id {recovery_id} signature {signature:?}",
            );
            match legacy {
                Ok(_) => {
                    recovered += 1;
                    recovered_x_reduced += usize::from(recovery_id >= 2);
                    recovered_high_s += usize::from(high_s);
                }
                Err(_) => rejected += 1,
            }
        }

        // Make sure the interesting paths were actually exercised.
        assert!(recovered > 500, "{recovered}");
        assert!(recovered_x_reduced > 10, "{recovered_x_reduced}");
        assert!(recovered_high_s > 100, "{recovered_high_s}");
        assert!(rejected > 500, "{rejected}");
    }

    fn test_case(
        num_signatures: u8,
        offsets: &SecpSignatureOffsets,
    ) -> Result<(), PrecompileError> {
        let mut instruction_data = vec![0u8; DATA_START];
        instruction_data[0] = num_signatures;
        wincode::serialize_into(&mut instruction_data[1..], offsets).unwrap();
        let feature_set = FeatureSet::all_enabled();
        test_verify_with_alignment(verify, &instruction_data, &[&[0u8; 100]], &feature_set)
    }

    #[test]
    fn test_invalid_offsets() {
        agave_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = SecpSignatureOffsets::default();
        instruction_data[0] = 1;
        wincode::serialize_into(&mut instruction_data[1..], &offsets).unwrap();
        instruction_data.truncate(instruction_data.len() - 1);
        let feature_set = FeatureSet::all_enabled();

        assert_eq!(
            test_verify_with_alignment(verify, &instruction_data, &[&[0u8; 100]], &feature_set),
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
        agave_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = SecpSignatureOffsets::default();
        instruction_data[0] = 0;
        wincode::serialize_into(&mut instruction_data[1..], &offsets).unwrap();
        let feature_set = FeatureSet::all_enabled();

        assert_eq!(
            test_verify_with_alignment(verify, &instruction_data, &[&[0u8; 100]], &feature_set),
            Err(PrecompileError::InvalidInstructionDataSize)
        );
    }

    #[test]
    fn test_secp256k1() {
        agave_logger::setup();
        let offsets = SecpSignatureOffsets::default();
        assert_eq!(
            wincode::serialized_size(&offsets).unwrap() as usize,
            SIGNATURE_OFFSETS_SERIALIZED_SIZE
        );

        let secret_bytes: [u8; 32] = rand::random();
        let secp_privkey = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
        let message_arr = b"hello";
        let secp_pubkey = libsecp256k1::PublicKey::from_secret_key(&secp_privkey);
        let eth_address =
            eth_address_from_pubkey(&secp_pubkey.serialize()[1..].try_into().unwrap());
        let (signature, recovery_id) =
            sign_message(&secp_privkey.serialize(), message_arr).unwrap();
        let mut instruction = new_secp256k1_instruction_with_signature(
            message_arr,
            &signature,
            recovery_id,
            &eth_address,
        );
        let feature_set = FeatureSet::all_enabled();
        assert!(
            test_verify_with_alignment(
                verify,
                &instruction.data,
                &[&instruction.data],
                &feature_set
            )
            .is_ok()
        );

        let index = rand::rng().random_range(0..instruction.data.len());
        instruction.data[index] = instruction.data[index].wrapping_add(12);
        assert!(
            test_verify_with_alignment(
                verify,
                &instruction.data,
                &[&instruction.data],
                &feature_set
            )
            .is_err()
        );
    }

    // Signatures are malleable.
    #[test]
    fn test_malleability() {
        agave_logger::setup();

        let secret_bytes: [u8; 32] = rand::random();
        let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
        let public_key = libsecp256k1::PublicKey::from_secret_key(&secret_key);
        let eth_address = eth_address_from_pubkey(&public_key.serialize()[1..].try_into().unwrap());

        let message = b"hello";
        let message_hash = {
            let mut hasher = keccak::Hasher::default();
            hasher.hash(message);
            hasher.result()
        };

        let secp_message = libsecp256k1::Message::parse(message_hash.as_bytes());
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
            let offsets = wincode::serialize(&offsets).unwrap();
            instruction_data.extend(offsets);
        }

        instruction_data.extend(data);

        test_verify_with_alignment(
            verify,
            &instruction_data,
            &[&instruction_data],
            &FeatureSet::all_enabled(),
        )
        .unwrap();
    }

    #[test]
    fn test_high_s_malleability_and_invalid_signatures() {
        agave_logger::setup();

        let secret_bytes: [u8; 32] = rand::random();
        let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
        let public_key = libsecp256k1::PublicKey::from_secret_key(&secret_key);
        let eth_address = eth_address_from_pubkey(&public_key.serialize()[1..].try_into().unwrap());

        let message = b"hello";
        let message_hash = {
            let mut hasher = keccak::Hasher::default();
            hasher.hash(message);
            hasher.result()
        };

        let secp_message = libsecp256k1::Message::parse(message_hash.as_bytes());
        let (signature, recovery_id) = libsecp256k1::sign(&secp_message, &secret_key);

        let mut alt_signature = signature;
        alt_signature.s = -alt_signature.s;
        let alt_recovery_id = libsecp256k1::RecoveryId::parse(recovery_id.serialize() ^ 1).unwrap();

        let valid_instruction = new_secp256k1_instruction_with_signature(
            message,
            &alt_signature.serialize(),
            alt_recovery_id.serialize(),
            &eth_address,
        )
        .data;
        assert_verify_result(&valid_instruction, Ok(()));

        let mut zero_r_signature = signature.serialize();
        zero_r_signature[..32].fill(0);
        let zero_r_instruction = new_secp256k1_instruction_with_signature(
            message,
            &zero_r_signature,
            recovery_id.serialize(),
            &eth_address,
        )
        .data;
        assert_verify_result(&zero_r_instruction, Err(PrecompileError::InvalidSignature));

        let mut zero_s_signature = signature.serialize();
        zero_s_signature[32..].fill(0);
        let zero_s_instruction = new_secp256k1_instruction_with_signature(
            message,
            &zero_s_signature,
            recovery_id.serialize(),
            &eth_address,
        )
        .data;
        assert_verify_result(&zero_s_instruction, Err(PrecompileError::InvalidSignature));

        let mut overflow_r_signature = signature.serialize();
        overflow_r_signature[..32].copy_from_slice(&SECP256K1_ORDER);
        let overflow_r_instruction = new_secp256k1_instruction_with_signature(
            message,
            &overflow_r_signature,
            recovery_id.serialize(),
            &eth_address,
        )
        .data;
        assert_verify_result(
            &overflow_r_instruction,
            Err(PrecompileError::InvalidSignature),
        );

        let mut overflow_s_signature = signature.serialize();
        overflow_s_signature[32..].copy_from_slice(&SECP256K1_ORDER);
        let overflow_s_instruction = new_secp256k1_instruction_with_signature(
            message,
            &overflow_s_signature,
            recovery_id.serialize(),
            &eth_address,
        )
        .data;
        assert_verify_result(
            &overflow_s_instruction,
            Err(PrecompileError::InvalidSignature),
        );

        let x_reduced_even_instruction = new_secp256k1_instruction_with_signature(
            message,
            &signature.serialize(),
            2,
            &eth_address,
        )
        .data;
        assert_verify_result(
            &x_reduced_even_instruction,
            Err(PrecompileError::InvalidSignature),
        );

        let x_reduced_odd_instruction = new_secp256k1_instruction_with_signature(
            message,
            &signature.serialize(),
            3,
            &eth_address,
        )
        .data;
        assert_verify_result(
            &x_reduced_odd_instruction,
            Err(PrecompileError::InvalidSignature),
        );
    }

    #[test]
    fn test_signature_parse_errors_take_priority_over_offset_errors() {
        let secret_bytes: [u8; 32] = rand::random();
        let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes).unwrap();
        let public_key = libsecp256k1::PublicKey::from_secret_key(&secret_key);
        let eth_address = eth_address_from_pubkey(&public_key.serialize()[1..].try_into().unwrap());

        let message = b"hello";
        let message_hash = {
            let mut hasher = keccak::Hasher::default();
            hasher.hash(message);
            hasher.result()
        };
        let secp_message = libsecp256k1::Message::parse(message_hash.as_bytes());
        let (signature, recovery_id) = libsecp256k1::sign(&secp_message, &secret_key);

        // Point the eth address at a non-existent instruction so the offset
        // check fails whenever it is reached.
        let with_bad_eth_address_offset = |mut instruction_data: Vec<u8>| {
            let offsets_end = 1usize
                .checked_add(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
                .unwrap();
            let mut offsets: SecpSignatureOffsets =
                wincode::deserialize(&instruction_data[1..offsets_end]).unwrap();
            offsets.eth_address_instruction_index = 1;
            wincode::serialize_into(&mut instruction_data[1..], &offsets).unwrap();
            instruction_data
        };

        // A well-formed signature reaches the offset check.
        let valid_instruction = with_bad_eth_address_offset(
            new_secp256k1_instruction_with_signature(
                message,
                &signature.serialize(),
                recovery_id.serialize(),
                &eth_address,
            )
            .data,
        );
        assert_verify_result(&valid_instruction, Err(PrecompileError::InvalidDataOffsets));

        // An out-of-range recovery id is rejected before the offset check.
        let invalid_recovery_id_instruction = with_bad_eth_address_offset(
            new_secp256k1_instruction_with_signature(
                message,
                &signature.serialize(),
                4,
                &eth_address,
            )
            .data,
        );
        assert_verify_result(
            &invalid_recovery_id_instruction,
            Err(PrecompileError::InvalidRecoveryId),
        );

        // A scalar at or above the curve order is rejected before the offset check.
        let mut overflow_s_signature = signature.serialize();
        overflow_s_signature[32..].copy_from_slice(&SECP256K1_ORDER);
        let overflow_s_instruction = with_bad_eth_address_offset(
            new_secp256k1_instruction_with_signature(
                message,
                &overflow_s_signature,
                recovery_id.serialize(),
                &eth_address,
            )
            .data,
        );
        assert_verify_result(
            &overflow_s_instruction,
            Err(PrecompileError::InvalidSignature),
        );

        // A zero scalar is only rejected during recovery, after the offset check.
        let mut zero_s_signature = signature.serialize();
        zero_s_signature[32..].fill(0);
        let zero_s_instruction = with_bad_eth_address_offset(
            new_secp256k1_instruction_with_signature(
                message,
                &zero_s_signature,
                recovery_id.serialize(),
                &eth_address,
            )
            .data,
        );
        assert_verify_result(
            &zero_s_instruction,
            Err(PrecompileError::InvalidDataOffsets),
        );
    }

    fn assert_verify_result(instruction_data: &[u8], expected: Result<(), PrecompileError>) {
        assert_eq!(
            test_verify_with_alignment(
                verify,
                instruction_data,
                &[instruction_data],
                &FeatureSet::all_enabled(),
            ),
            expected
        );
    }
}
