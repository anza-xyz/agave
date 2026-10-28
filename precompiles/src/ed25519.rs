use {
    agave_feature_set::FeatureSet,
    solana_ed25519::ed_sigs::{Signature, VerificationKey},
    solana_ed25519_program::{
        Ed25519SignatureOffsets, PUBKEY_SERIALIZED_SIZE, SIGNATURE_OFFSETS_SERIALIZED_SIZE,
        SIGNATURE_OFFSETS_START, SIGNATURE_SERIALIZED_SIZE,
    },
    solana_precompile_error::PrecompileError,
};

pub fn verify(
    data: &[u8],
    instruction_datas: &[&[u8]],
    _feature_set: &FeatureSet,
) -> Result<(), PrecompileError> {
    if data.len() < SIGNATURE_OFFSETS_START {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let num_signatures = data[0] as usize;
    if num_signatures == 0 && data.len() > SIGNATURE_OFFSETS_START {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let expected_data_size = num_signatures
        .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
        .saturating_add(SIGNATURE_OFFSETS_START);
    // We do not check or use the byte at data[1]
    if data.len() < expected_data_size {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    for i in 0..num_signatures {
        let start = i
            .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
            .saturating_add(SIGNATURE_OFFSETS_START);

        // SAFETY:
        // - data[start..] is guaranteed to be >= size of Ed25519SignatureOffsets
        // - Ed25519SignatureOffsets is a POD type, so we can safely read it as an unaligned struct
        let offsets = unsafe {
            core::ptr::read_unaligned(data.as_ptr().add(start) as *const Ed25519SignatureOffsets)
        };

        // Parse out signature
        let signature = get_data_slice(
            data,
            instruction_datas,
            offsets.signature_instruction_index,
            offsets.signature_offset,
            SIGNATURE_SERIALIZED_SIZE,
        )?;

        // `ed25519::Signature::from_bytes` used to reject (before parsing the
        // pubkey) any `s` with the top three bits set, which is a cheap partial
        // non-canonical check. `verify_dalek` rejects every non-canonical `s`,
        // but keep the early check so the error variant stays the same when
        // both the pubkey and the signature are malformed.
        if signature[SIGNATURE_SERIALIZED_SIZE - 1] & 0b1110_0000 != 0 {
            return Err(PrecompileError::InvalidSignature);
        }
        let signature =
            Signature::from_slice(signature).map_err(|_| PrecompileError::InvalidSignature)?;

        // Parse out pubkey
        let pubkey = get_data_slice(
            data,
            instruction_datas,
            offsets.public_key_instruction_index,
            offsets.public_key_offset,
            PUBKEY_SERIALIZED_SIZE,
        )?;

        let publickey =
            VerificationKey::try_from(pubkey).map_err(|_| PrecompileError::InvalidPublicKey)?;

        // Parse out message
        let message = get_data_slice(
            data,
            instruction_datas,
            offsets.message_instruction_index,
            offsets.message_data_offset,
            offsets.message_data_size as usize,
        )?;
        // Strict (non-cofactored, canonical-`R`) verification, accept/reject
        // identical to `ed25519_dalek::VerifyingKey::verify_strict`.
        publickey
            .verify_dalek(&signature, message)
            .map_err(|_| PrecompileError::InvalidSignature)?;
    }
    Ok(())
}

fn get_data_slice<'a>(
    data: &'a [u8],
    instruction_datas: &'a [&[u8]],
    instruction_index: u16,
    offset_start: u16,
    size: usize,
) -> Result<&'a [u8], PrecompileError> {
    let instruction = if instruction_index == u16::MAX {
        data
    } else {
        let signature_index = instruction_index as usize;
        if signature_index >= instruction_datas.len() {
            return Err(PrecompileError::InvalidDataOffsets);
        }
        instruction_datas[signature_index]
    };

    let start = offset_start as usize;
    let end = start.saturating_add(size);
    if end > instruction.len() {
        return Err(PrecompileError::InvalidDataOffsets);
    }

    Ok(&instruction[start..end])
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::test_verify_with_alignment,
        bytemuck::bytes_of,
        hex,
        rand::Rng,
        solana_ed25519::ed_sigs::SigningKey,
        solana_ed25519_program::{
            DATA_START, new_ed25519_instruction_with_signature, offsets_to_ed25519_instruction,
        },
        solana_instruction::Instruction,
        std::vec,
    };

    pub fn new_ed25519_instruction_raw(
        pubkey: &[u8],
        signature: &[u8],
        message: &[u8],
    ) -> Instruction {
        assert_eq!(pubkey.len(), PUBKEY_SERIALIZED_SIZE);
        assert_eq!(signature.len(), SIGNATURE_SERIALIZED_SIZE);

        let mut instruction_data = Vec::with_capacity(
            DATA_START
                .saturating_add(SIGNATURE_SERIALIZED_SIZE)
                .saturating_add(PUBKEY_SERIALIZED_SIZE)
                .saturating_add(message.len()),
        );

        let num_signatures: u8 = 1;
        let public_key_offset = DATA_START;
        let signature_offset = public_key_offset.saturating_add(PUBKEY_SERIALIZED_SIZE);
        let message_data_offset = signature_offset.saturating_add(SIGNATURE_SERIALIZED_SIZE);

        // add padding byte so that offset structure is aligned
        instruction_data.extend_from_slice(bytes_of(&[num_signatures, 0]));

        let offsets = Ed25519SignatureOffsets {
            signature_offset: signature_offset as u16,
            signature_instruction_index: u16::MAX,
            public_key_offset: public_key_offset as u16,
            public_key_instruction_index: u16::MAX,
            message_data_offset: message_data_offset as u16,
            message_data_size: message.len() as u16,
            message_instruction_index: u16::MAX,
        };

        instruction_data.extend_from_slice(bytes_of(&offsets));

        debug_assert_eq!(instruction_data.len(), public_key_offset);

        instruction_data.extend_from_slice(pubkey);

        debug_assert_eq!(instruction_data.len(), signature_offset);

        instruction_data.extend_from_slice(signature);

        debug_assert_eq!(instruction_data.len(), message_data_offset);

        instruction_data.extend_from_slice(message);

        Instruction {
            program_id: solana_sdk_ids::ed25519_program::id(),
            accounts: vec![],
            data: instruction_data,
        }
    }

    fn test_case(
        num_signatures: u16,
        offsets: &Ed25519SignatureOffsets,
    ) -> Result<(), PrecompileError> {
        assert_eq!(
            bytemuck::bytes_of(offsets).len(),
            SIGNATURE_OFFSETS_SERIALIZED_SIZE
        );

        let mut instruction_data = vec![0u8; DATA_START];
        instruction_data[0..SIGNATURE_OFFSETS_START].copy_from_slice(bytes_of(&num_signatures));
        instruction_data[SIGNATURE_OFFSETS_START..DATA_START].copy_from_slice(bytes_of(offsets));

        test_verify_with_alignment(
            verify,
            &instruction_data,
            &[&[0u8; 100]],
            &FeatureSet::all_enabled(),
        )
    }

    #[test]
    fn test_invalid_offsets() {
        agave_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = Ed25519SignatureOffsets::default();
        instruction_data[0..SIGNATURE_OFFSETS_START].copy_from_slice(bytes_of(&1u16));
        instruction_data[SIGNATURE_OFFSETS_START..DATA_START].copy_from_slice(bytes_of(&offsets));
        instruction_data.truncate(instruction_data.len() - 1);

        assert_eq!(
            test_verify_with_alignment(
                verify,
                &instruction_data,
                &[&[0u8; 100]],
                &FeatureSet::all_enabled(),
            ),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        let offsets = Ed25519SignatureOffsets {
            signature_instruction_index: 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            message_instruction_index: 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            public_key_instruction_index: 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_message_data_offsets() {
        let offsets = Ed25519SignatureOffsets {
            message_data_offset: 99,
            message_data_size: 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = Ed25519SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1000,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            message_data_offset: u16::MAX,
            message_data_size: u16::MAX,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_pubkey_offset() {
        let offsets = Ed25519SignatureOffsets {
            public_key_offset: u16::MAX,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            public_key_offset: 100 - PUBKEY_SERIALIZED_SIZE as u16 + 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_signature_offset() {
        let offsets = Ed25519SignatureOffsets {
            signature_offset: u16::MAX,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Ed25519SignatureOffsets {
            signature_offset: 100 - SIGNATURE_SERIALIZED_SIZE as u16 + 1,
            ..Ed25519SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_ed25519() {
        agave_logger::setup();

        let secret_bytes: [u8; 32] = rand::random();
        let privkey = SigningKey::from_bytes(&secret_bytes);
        let message_arr = b"hello";
        let signature = privkey.sign(message_arr).to_bytes();
        let pubkey: [u8; 32] = privkey.verification_key().into();
        let mut instruction =
            new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);
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

        let mut rng = rand::rng();
        let index = loop {
            let index = rng.random_range(0..instruction.data.len());
            // byte 1 is not used, so this would not cause the verify to fail
            if index != 1 {
                break index;
            }
        };

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

    #[test]
    fn test_offsets_to_ed25519_instruction() {
        agave_logger::setup();

        let secret_bytes: [u8; 32] = rand::random();
        let privkey = SigningKey::from_bytes(&secret_bytes);
        let messages: [&[u8]; 3] = [b"hello", b"IBRL", b"goodbye"];
        let data_start =
            messages.len() * SIGNATURE_OFFSETS_SERIALIZED_SIZE + SIGNATURE_OFFSETS_START;
        let mut data_offset = data_start + PUBKEY_SERIALIZED_SIZE;
        let (offsets, messages): (Vec<_>, Vec<_>) = messages
            .into_iter()
            .map(|message| {
                let signature_offset = data_offset;
                let message_data_offset = signature_offset + SIGNATURE_SERIALIZED_SIZE;
                data_offset += SIGNATURE_SERIALIZED_SIZE + message.len();

                let offsets = Ed25519SignatureOffsets {
                    signature_offset: signature_offset as u16,
                    signature_instruction_index: u16::MAX,
                    public_key_offset: data_start as u16,
                    public_key_instruction_index: u16::MAX,
                    message_data_offset: message_data_offset as u16,
                    message_data_size: message.len() as u16,
                    message_instruction_index: u16::MAX,
                };

                (offsets, message)
            })
            .unzip();

        let mut instruction = offsets_to_ed25519_instruction(&offsets);

        let pubkey: [u8; 32] = privkey.verification_key().into();
        instruction.data.extend_from_slice(&pubkey);

        for message in messages {
            let signature = privkey.sign(message).to_bytes();
            instruction.data.extend_from_slice(&signature);
            instruction.data.extend_from_slice(message);
        }

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

        let mut rng = rand::rng();
        let index = loop {
            let index = rng.random_range(0..instruction.data.len());
            // byte 1 is not used, so this would not cause the verify to fail
            if index != 1 {
                break index;
            }
        };

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

    #[test]
    fn test_ed25519_malleability() {
        agave_logger::setup();

        // sig created via solana_ed25519: both pass
        let secret_bytes: [u8; 32] = rand::random();
        let privkey = SigningKey::from_bytes(&secret_bytes);
        let message_arr = b"hello";
        let signature = privkey.sign(message_arr).to_bytes();
        let pubkey: [u8; 32] = privkey.verification_key().into();
        let instruction = new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);

        let feature_set = FeatureSet::default();
        assert!(
            test_verify_with_alignment(
                verify,
                &instruction.data,
                &[&instruction.data],
                &feature_set
            )
            .is_ok()
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

        // malleable sig: verify_strict does NOT pass
        // for example, test number 5:
        // https://github.com/C2SP/CCTV/tree/main/ed25519
        // R has low order (in fact R == 0)
        let pubkey =
            &hex::decode("10eb7c3acfb2bed3e0d6ab89bf5a3d6afddd1176ce4812e38d9fd485058fdb1f")
                .unwrap();
        let signature = &hex::decode("00000000000000000000000000000000000000000000000000000000000000009472a69cd9a701a50d130ed52189e2455b23767db52cacb8716fb896ffeeac09").unwrap();
        let message = b"ed25519vectors 3";
        let instruction = new_ed25519_instruction_raw(pubkey, signature, message);

        // verify_strict does NOT pass for malleable signature
        let feature_set = FeatureSet::default();
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

    /// Precompile `verify` must accept exactly what
    /// `ed25519_dalek::VerifyingKey::verify_strict` accepts (the precompile
    /// used to call `verify_strict` directly).
    #[test]
    fn test_ed25519_consistent_with_dalek_verify_strict() {
        use ed25519_dalek::Signer as _;

        let feature_set = FeatureSet::all_enabled();
        let mut rng = rand::rng();

        let check = |pubkey: &[u8; 32], signature: &[u8; 64], message: &[u8]| {
            let instruction = new_ed25519_instruction_raw(pubkey, signature, message);
            let precompile = test_verify_with_alignment(
                verify,
                &instruction.data,
                &[&instruction.data],
                &feature_set,
            )
            .is_ok();
            let dalek = ed25519_dalek::VerifyingKey::from_bytes(pubkey)
                .and_then(|vk| {
                    vk.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature))
                })
                .is_ok();
            assert_eq!(precompile, dalek, "pubkey={pubkey:?} sig={signature:?}");
            precompile
        };

        for _ in 0..64 {
            let secret_bytes: [u8; 32] = rand::random();
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
            let pubkey = signing_key.verifying_key().to_bytes();
            let mut message = vec![0u8; rng.random_range(0..256)];
            rng.fill(message.as_mut_slice());
            let mut signature = signing_key.sign(&message).to_bytes();

            assert!(check(&pubkey, &signature, &message));

            // Tamper with a byte of the signature, pubkey or message.
            let index = rng.random_range(0..64);
            signature[index] = signature[index].wrapping_add(1);
            assert!(!check(&pubkey, &signature, &message));
            signature[index] = signature[index].wrapping_sub(1);

            let mut bad_pubkey = pubkey;
            let index = rng.random_range(0..32);
            bad_pubkey[index] = bad_pubkey[index].wrapping_add(1);
            check(&bad_pubkey, &signature, &message);

            if !message.is_empty() {
                let index = rng.random_range(0..message.len());
                message[index] = message[index].wrapping_add(1);
                assert!(!check(&pubkey, &signature, &message));
            }

            // Non-canonical `s` (s + l) must be rejected.
            let mut s_plus_l = signature;
            let l: [u8; 32] = [
                0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
                0xde, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x10,
            ];
            let mut carry = 0u16;
            for i in 0..32 {
                let sum = u16::from(s_plus_l[32 + i]) + u16::from(l[i]) + carry;
                s_plus_l[32 + i] = sum as u8;
                carry = sum >> 8;
            }
            assert!(!check(&pubkey, &s_plus_l, &message));

            // Random pubkeys / signatures: mostly off-curve or invalid, both
            // implementations must agree on each one.
            let random_pubkey: [u8; 32] = rand::random();
            let random_signature: [u8; 64] = rand::random();
            check(&random_pubkey, &random_signature, &message);
            check(&random_pubkey, &signature, &message);
            check(&pubkey, &random_signature, &message);
        }

        // Small-order public keys and `R` values (every encoding of the 8-torsion
        // points) must be rejected by both implementations whatever the
        // message, including non-canonical encodings.
        let small_order: [[u8; 32]; 14] = [
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128],
            [236, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 127],
            [38, 232, 149, 143, 194, 178, 39, 176, 69, 195, 244, 137, 242, 239, 152, 240, 213, 223, 172, 5, 211, 198, 51, 57, 177, 56, 2, 136, 109, 83, 252, 5],
            [38, 232, 149, 143, 194, 178, 39, 176, 69, 195, 244, 137, 242, 239, 152, 240, 213, 223, 172, 5, 211, 198, 51, 57, 177, 56, 2, 136, 109, 83, 252, 133],
            [199, 23, 106, 112, 61, 77, 216, 79, 186, 60, 11, 118, 13, 16, 103, 15, 42, 32, 83, 250, 44, 57, 204, 198, 78, 199, 253, 119, 146, 172, 3, 122],
            [199, 23, 106, 112, 61, 77, 216, 79, 186, 60, 11, 118, 13, 16, 103, 15, 42, 32, 83, 250, 44, 57, 204, 198, 78, 199, 253, 119, 146, 172, 3, 250],
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128],
            [236, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [237, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 127],
            [237, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            [238, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 127],
            [238, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        ];
        let secret_bytes: [u8; 32] = rand::random();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
        let pubkey = signing_key.verifying_key().to_bytes();
        let message = b"small order";
        let signature = signing_key.sign(message).to_bytes();
        for point in small_order {
            // Small-order (or invalid) public key.
            assert!(!check(&point, &signature, message));
            // Small-order (or invalid) `R`, with `s = 0` and a random `s`.
            let mut sig = [0u8; 64];
            sig[..32].copy_from_slice(&point);
            assert!(!check(&pubkey, &sig, message));
            let s: [u8; 32] = rand::random();
            sig[32..].copy_from_slice(&s);
            sig[63] &= 0x0f;
            assert!(!check(&pubkey, &sig, message));
        }
    }
}
