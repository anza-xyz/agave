use {
    agave_feature_set::FeatureSet,
    openssl::{
        bn::{BigNum, BigNumContext},
        ec::{EcGroup, EcKey, EcPoint},
        nid::Nid,
        pkey::PKey,
        sign::Verifier,
    },
    solana_precompile_error::PrecompileError,
    solana_secp256r1_program::{
        Secp256r1SignatureOffsets, COMPRESSED_PUBKEY_SERIALIZED_SIZE, FIELD_SIZE,
        SECP256R1_HALF_ORDER, SECP256R1_ORDER_MINUS_ONE, SIGNATURE_OFFSETS_SERIALIZED_SIZE,
        SIGNATURE_OFFSETS_START, SIGNATURE_SERIALIZED_SIZE,
    },
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
    if num_signatures == 0 {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    if num_signatures > 8 {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }

    let expected_data_size = num_signatures
        .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
        .saturating_add(SIGNATURE_OFFSETS_START);

    // We do not check or use the byte at data[1]
    if data.len() < expected_data_size {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }

    // Parse half order from constant
    let half_order: BigNum =
        BigNum::from_slice(&SECP256R1_HALF_ORDER).map_err(|_| PrecompileError::InvalidSignature)?;

    // Parse order - 1 from constant
    let order_minus_one: BigNum = BigNum::from_slice(&SECP256R1_ORDER_MINUS_ONE)
        .map_err(|_| PrecompileError::InvalidSignature)?;

    // Create a BigNum for 1
    let one = BigNum::from_u32(1).map_err(|_| PrecompileError::InvalidSignature)?;

    // Define curve group
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
        .map_err(|_| PrecompileError::InvalidSignature)?;
    let mut ctx = BigNumContext::new().map_err(|_| PrecompileError::InvalidSignature)?;

    for i in 0..num_signatures {
        let start = i
            .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
            .saturating_add(SIGNATURE_OFFSETS_START);

        // SAFETY:
        // - data[start..] is guaranteed to be >= size of Secp256r1SignatureOffsets
        // - Secp256r1SignatureOffsets is a POD type, so we can safely read it as an unaligned struct
        let offsets = unsafe {
            core::ptr::read_unaligned(data.as_ptr().add(start) as *const Secp256r1SignatureOffsets)
        };

        // Parse out signature
        let signature = get_data_slice(
            data,
            instruction_datas,
            offsets.signature_instruction_index,
            offsets.signature_offset,
            SIGNATURE_SERIALIZED_SIZE,
        )?;

        // Parse out pubkey
        let pubkey = get_data_slice(
            data,
            instruction_datas,
            offsets.public_key_instruction_index,
            offsets.public_key_offset,
            COMPRESSED_PUBKEY_SERIALIZED_SIZE,
        )?;

        // Parse out message
        let message = get_data_slice(
            data,
            instruction_datas,
            offsets.message_instruction_index,
            offsets.message_data_offset,
            offsets.message_data_size as usize,
        )?;

        let r_bignum = BigNum::from_slice(&signature[..FIELD_SIZE])
            .map_err(|_| PrecompileError::InvalidSignature)?;
        let s_bignum = BigNum::from_slice(&signature[FIELD_SIZE..])
            .map_err(|_| PrecompileError::InvalidSignature)?;

        // Check that the signature is generally in range
        let within_range = r_bignum >= one
            && r_bignum <= order_minus_one
            && s_bignum >= one
            && s_bignum <= half_order;

        if !within_range {
            return Err(PrecompileError::InvalidSignature);
        }

        // Create an ECDSA signature object from the ASN.1 integers
        let ecdsa_sig = openssl::ecdsa::EcdsaSig::from_private_components(r_bignum, s_bignum)
            .and_then(|sig| sig.to_der())
            .map_err(|_| PrecompileError::InvalidSignature)?;

        let public_key_point = EcPoint::from_bytes(&group, pubkey, &mut ctx)
            .map_err(|_| PrecompileError::InvalidPublicKey)?;
        let public_key = EcKey::from_public_key(&group, &public_key_point)
            .map_err(|_| PrecompileError::InvalidPublicKey)?;
        let public_key_as_pkey =
            PKey::from_ec_key(public_key).map_err(|_| PrecompileError::InvalidPublicKey)?;

        let mut verifier =
            Verifier::new(openssl::hash::MessageDigest::sha256(), &public_key_as_pkey)
                .map_err(|_| PrecompileError::InvalidSignature)?;
        verifier
            .update(message)
            .map_err(|_| PrecompileError::InvalidSignature)?;

        if !verifier
            .verify(&ecdsa_sig)
            .map_err(|_| PrecompileError::InvalidSignature)?
        {
            return Err(PrecompileError::InvalidSignature);
        }
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
mod tests {
    use {
        super::*,
        crate::test_verify_with_alignment,
        bytemuck::bytes_of,
        solana_secp256r1_program::{
            new_secp256r1_instruction_with_signature, sign_message, DATA_START, SECP256R1_ORDER,
        },
    };

    fn test_case(
        num_signatures: u16,
        offsets: &Secp256r1SignatureOffsets,
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
        let offsets = Secp256r1SignatureOffsets::default();
        instruction_data[0..SIGNATURE_OFFSETS_START].copy_from_slice(bytes_of(&1u16));
        instruction_data[SIGNATURE_OFFSETS_START..DATA_START].copy_from_slice(bytes_of(&offsets));
        instruction_data.truncate(instruction_data.len() - 1);

        assert_eq!(
            test_verify_with_alignment(
                verify,
                &instruction_data,
                &[&[0u8; 100]],
                &FeatureSet::all_enabled()
            ),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        let offsets = Secp256r1SignatureOffsets {
            signature_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            public_key_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_invalid_signature_data_size() {
        agave_logger::setup();

        // Test data.len() < SIGNATURE_OFFSETS_START
        let small_data = vec![0u8; SIGNATURE_OFFSETS_START - 1];
        assert_eq!(
            test_verify_with_alignment(verify, &small_data, &[&[]], &FeatureSet::all_enabled()),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        // Test num_signatures == 0
        let mut zero_sigs_data = vec![0u8; DATA_START];
        zero_sigs_data[0] = 0; // Set num_signatures to 0
        assert_eq!(
            test_verify_with_alignment(verify, &zero_sigs_data, &[&[]], &FeatureSet::all_enabled()),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        // Test num_signatures > 8
        let mut too_many_sigs = vec![0u8; DATA_START];
        too_many_sigs[0] = 9; // Set num_signatures to 9
        assert_eq!(
            test_verify_with_alignment(verify, &too_many_sigs, &[&[]], &FeatureSet::all_enabled()),
            Err(PrecompileError::InvalidInstructionDataSize)
        );
    }
    #[test]
    fn test_message_data_offsets() {
        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 99,
            message_data_size: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1000,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: u16::MAX,
            message_data_size: u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_pubkey_offset() {
        let offsets = Secp256r1SignatureOffsets {
            public_key_offset: u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            public_key_offset: 100 - (COMPRESSED_PUBKEY_SERIALIZED_SIZE as u16) + 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_signature_offset() {
        let offsets = Secp256r1SignatureOffsets {
            signature_offset: u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            signature_offset: 100 - (SIGNATURE_SERIALIZED_SIZE as u16) + 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_secp256r1() {
        agave_logger::setup();
        let message_arr = b"hello";
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let signing_key = EcKey::generate(&group).unwrap();
        let signature =
            sign_message(message_arr, &signing_key.private_key_to_der().unwrap()).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let pubkey = signing_key
            .public_key()
            .to_bytes(
                &group,
                openssl::ec::PointConversionForm::COMPRESSED,
                &mut ctx,
            )
            .unwrap();
        let mut instruction = new_secp256r1_instruction_with_signature(
            message_arr,
            &signature,
            &pubkey.try_into().unwrap(),
        );
        let feature_set = FeatureSet::all_enabled();

        assert!(test_verify_with_alignment(
            verify,
            &instruction.data,
            &[&instruction.data],
            &feature_set
        )
        .is_ok());

        // The message is the last field in the instruction data so
        // changing its last byte will also change the signature validity
        let message_byte_index = instruction.data.len() - 1;
        instruction.data[message_byte_index] =
            instruction.data[message_byte_index].wrapping_add(12);

        assert!(test_verify_with_alignment(
            verify,
            &instruction.data,
            &[&instruction.data],
            &feature_set
        )
        .is_err());
    }

    #[test]
    fn test_secp256r1_high_s() {
        agave_logger::setup();
        let message_arr = b"hello";
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let signing_key = EcKey::generate(&group).unwrap();
        let signature =
            sign_message(message_arr, &signing_key.private_key_to_der().unwrap()).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let pubkey = signing_key
            .public_key()
            .to_bytes(
                &group,
                openssl::ec::PointConversionForm::COMPRESSED,
                &mut ctx,
            )
            .unwrap();
        let mut instruction = new_secp256r1_instruction_with_signature(
            message_arr,
            &signature,
            &pubkey.try_into().unwrap(),
        );

        // To double check that the untampered low-S value signature passes
        let feature_set = FeatureSet::all_enabled();
        let tx_pass = test_verify_with_alignment(
            verify,
            instruction.data.as_slice(),
            &[instruction.data.as_slice()],
            &feature_set,
        );
        assert!(tx_pass.is_ok());

        // Determine offsets at which to perform the S-value manipulation
        let public_key_offset = DATA_START;
        let signature_offset = public_key_offset + COMPRESSED_PUBKEY_SERIALIZED_SIZE;
        let s_offset = signature_offset + FIELD_SIZE;

        // Create a high S value by doing order - s
        let order = BigNum::from_slice(&SECP256R1_ORDER).unwrap();
        let current_s =
            BigNum::from_slice(&instruction.data[s_offset..s_offset + FIELD_SIZE]).unwrap();
        let mut high_s = BigNum::new().unwrap();
        high_s.checked_sub(&order, &current_s).unwrap();

        // Replace the S value in the signature with our high S
        instruction.data[s_offset..s_offset + FIELD_SIZE].copy_from_slice(&high_s.to_vec());

        let tx_fail = test_verify_with_alignment(
            verify,
            &instruction.data,
            &[&instruction.data],
            &feature_set,
        );
        assert!(tx_fail.unwrap_err() == PrecompileError::InvalidSignature);
    }
    #[test]
    fn test_new_secp256r1_instruction_31byte_components() {
        agave_logger::setup();
        let message_arr = b"hello";
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let signing_key = EcKey::generate(&group).unwrap();

        // Keep generating signatures until we get one with a 31-byte component
        loop {
            let signature =
                sign_message(message_arr, &signing_key.private_key_to_der().unwrap()).unwrap();
            let mut ctx = BigNumContext::new().unwrap();
            let pubkey = signing_key
                .public_key()
                .to_bytes(
                    &group,
                    openssl::ec::PointConversionForm::COMPRESSED,
                    &mut ctx,
                )
                .unwrap();
            let instruction = new_secp256r1_instruction_with_signature(
                message_arr,
                &signature,
                &pubkey.try_into().unwrap(),
            );

            // Extract r and s from the signature
            let signature_offset = DATA_START + COMPRESSED_PUBKEY_SERIALIZED_SIZE;
            let r = &instruction.data[signature_offset..signature_offset + FIELD_SIZE];
            let s =
                &instruction.data[signature_offset + FIELD_SIZE..signature_offset + 2 * FIELD_SIZE];

            // Convert to BigNum and back to get byte representation
            let r_bn = BigNum::from_slice(r).unwrap();
            let s_bn = BigNum::from_slice(s).unwrap();
            let r_bytes = r_bn.to_vec();
            let s_bytes = s_bn.to_vec();

            if r_bytes.len() == 31 || s_bytes.len() == 31 {
                // Once found, verify the signature and break out of the loop
                let feature_set = FeatureSet::all_enabled();
                assert!(test_verify_with_alignment(
                    verify,
                    &instruction.data,
                    &[&instruction.data],
                    &feature_set
                )
                .is_ok());
                break;
            }
        }
    }

    #[test]
    fn test_secp256r1_order() {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let mut openssl_order = BigNum::new().unwrap();
        group.order(&mut openssl_order, &mut ctx).unwrap();

        let our_order = BigNum::from_slice(&SECP256R1_ORDER).unwrap();
        assert_eq!(our_order, openssl_order);
    }

    #[test]
    fn test_secp256r1_order_minus_one() {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let mut openssl_order = BigNum::new().unwrap();
        group.order(&mut openssl_order, &mut ctx).unwrap();

        let mut expected_order_minus_one = BigNum::new().unwrap();
        expected_order_minus_one
            .checked_sub(&openssl_order, &BigNum::from_u32(1).unwrap())
            .unwrap();

        let our_order_minus_one = BigNum::from_slice(&SECP256R1_ORDER_MINUS_ONE).unwrap();
        assert_eq!(our_order_minus_one, expected_order_minus_one);
    }

    #[test]
    fn test_secp256r1_half_order() {
        // Get the secp256r1 curve group
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();

        // Get the order from OpenSSL
        let mut ctx = BigNumContext::new().unwrap();
        let mut openssl_order = BigNum::new().unwrap();
        group.order(&mut openssl_order, &mut ctx).unwrap();

        // Calculate half order
        let mut calculated_half_order = BigNum::new().unwrap();
        let two = BigNum::from_u32(2).unwrap();
        calculated_half_order
            .checked_div(&openssl_order, &two, &mut ctx)
            .unwrap();

        // Get our constant half order
        let our_half_order = BigNum::from_slice(&SECP256R1_HALF_ORDER).unwrap();

        // Compare the calculated half order with our constant
        assert_eq!(calculated_half_order, our_half_order);
    }

    #[test]
    fn test_secp256r1_order_relationships() {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let mut openssl_order = BigNum::new().unwrap();
        group.order(&mut openssl_order, &mut ctx).unwrap();

        let our_order = BigNum::from_slice(&SECP256R1_ORDER).unwrap();
        let our_order_minus_one = BigNum::from_slice(&SECP256R1_ORDER_MINUS_ONE).unwrap();
        let our_half_order = BigNum::from_slice(&SECP256R1_HALF_ORDER).unwrap();

        // Verify our order matches OpenSSL's order
        assert_eq!(our_order, openssl_order);

        // Verify order - 1
        let mut expected_order_minus_one = BigNum::new().unwrap();
        expected_order_minus_one
            .checked_sub(&openssl_order, &BigNum::from_u32(1).unwrap())
            .unwrap();
        assert_eq!(our_order_minus_one, expected_order_minus_one);

        // Verify half order
        let mut expected_half_order = BigNum::new().unwrap();
        expected_half_order
            .checked_div(&openssl_order, &BigNum::from_u32(2).unwrap(), &mut ctx)
            .unwrap();
        assert_eq!(our_half_order, expected_half_order);

        // Verify half order * 2 = order - 1
        let mut double_half_order = BigNum::new().unwrap();
        double_half_order
            .checked_mul(&our_half_order, &BigNum::from_u32(2).unwrap(), &mut ctx)
            .unwrap();
        assert_eq!(double_half_order, expected_order_minus_one);
    }
}
