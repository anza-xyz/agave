use {
    crate::{
        Version,
        encoding::{
            Endianness, PodG1Compressed, PodG1Point, PodG2Compressed, PodG2Point,
            swap_fq_endianness, swap_g2_c0_c1,
        },
    },
    blstrs::{G1Affine, G2Affine},
};

/// Decompresses a compressed G1 point.
pub fn bls12_381_g1_decompress(
    _version: Version,
    input: &PodG1Compressed,
    endianness: Endianness,
) -> Option<PodG1Point> {
    let p1 = match endianness {
        // `G1Affine::from_compressed_unchecked` performs field and on-curve checks
        Endianness::BE => G1Affine::from_compressed_unchecked(&input.0).into_option()?,
        Endianness::LE => {
            let mut bytes = input.0;
            swap_fq_endianness(&mut bytes);
            // After reversal, the flag byte (originally at end) is now at index 0.
            // This matches the [Zcash BE format][zcash-be-format] expected by
            // `G1Affine::from_compressed_unchecked`.
            //
            // [zcash-be-format]: https://github.com/zkcrypto/pairing/blob/34aa52b0f7bef705917252ea63e5a13fa01af551/src/bls12_381/README.md#serialization
            G1Affine::from_compressed_unchecked(&bytes).into_option()?
        }
    };

    // field and on-curve checks are already performed, so just check subgroup
    if !bool::from(p1.is_torsion_free()) {
        return None;
    }

    let mut result = PodG1Point(p1.to_uncompressed());
    if matches!(endianness, Endianness::LE) {
        swap_fq_endianness(&mut result.0);
    }
    Some(result)
}

/// Decompresses a compressed G2 point.
pub fn bls12_381_g2_decompress(
    _version: Version,
    input: &PodG2Compressed,
    endianness: Endianness,
) -> Option<PodG2Point> {
    let p2 = match endianness {
        // `G1Affine::from_compressed_unchecked` performs field and on-curve checks
        Endianness::BE => G2Affine::from_compressed_unchecked(&input.0).into_option()?,
        Endianness::LE => {
            let mut bytes = input.0;
            swap_fq_endianness(&mut bytes);
            swap_g2_c0_c1(&mut bytes); // Swap c0/c1 for G2

            // After reversal, the flag byte (originally at end) is now at index 0.
            // This matches the [Zcash BE format][zcash-be-format] expected by
            // `G2Affine::from_compressed_unchecked`.
            //
            // [zcash-be-format]: https://github.com/zkcrypto/pairing/blob/34aa52b0f7bef705917252ea63e5a13fa01af551/src/bls12_381/README.md#serialization
            G2Affine::from_compressed_unchecked(&bytes).into_option()?
        }
    };

    // field and on-curve checks are already performed, so just check subgroup
    if !bool::from(p2.is_torsion_free()) {
        return None;
    }

    let mut result = PodG2Point(p2.to_uncompressed());
    if matches!(endianness, Endianness::LE) {
        swap_g2_c0_c1(&mut result.0);
        swap_fq_endianness(&mut result.0);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use {super::*, crate::test_vectors::*, bytemuck::pod_read_unaligned};

    fn to_pod_g1(bytes: &[u8]) -> PodG1Point {
        pod_read_unaligned(bytes)
    }

    fn to_pod_g2(bytes: &[u8]) -> PodG2Point {
        pod_read_unaligned(bytes)
    }

    // New helpers for compressed pods
    fn to_pod_g1_compressed(bytes: &[u8]) -> PodG1Compressed {
        pod_read_unaligned(bytes)
    }

    fn to_pod_g2_compressed(bytes: &[u8]) -> PodG2Compressed {
        pod_read_unaligned(bytes)
    }

    fn run_g1_test(
        test_name: &str,
        input_be: &[u8],
        output_be: Option<&[u8]>,
        input_le: &[u8],
        output_le: Option<&[u8]>,
    ) {
        // Test Big Endian
        let input_be_pod = to_pod_g1_compressed(input_be);
        let result_be = bls12_381_g1_decompress(Version::V0, &input_be_pod, Endianness::BE);
        match output_be {
            Some(expected) => assert_eq!(
                result_be,
                Some(to_pod_g1(expected)),
                "G1 {test_name} BE Test Failed",
            ),
            None => assert!(result_be.is_none(), "G1 {test_name} BE expected failure"),
        }

        // Test Little Endian
        let input_le_pod = to_pod_g1_compressed(input_le);
        let result_le = bls12_381_g1_decompress(Version::V0, &input_le_pod, Endianness::LE);
        match output_le {
            Some(expected) => assert_eq!(
                result_le,
                Some(to_pod_g1(expected)),
                "G1 {test_name} LE Test Failed",
            ),
            None => assert!(result_le.is_none(), "G1 {test_name} LE expected failure"),
        }
    }

    fn run_g2_test(
        test_name: &str,
        input_be: &[u8],
        output_be: Option<&[u8]>,
        input_le: &[u8],
        output_le: Option<&[u8]>,
    ) {
        // Test Big Endian
        let input_be_pod = to_pod_g2_compressed(input_be);
        let result_be = bls12_381_g2_decompress(Version::V0, &input_be_pod, Endianness::BE);
        match output_be {
            Some(expected) => assert_eq!(
                result_be,
                Some(to_pod_g2(expected)),
                "G2 {test_name} BE Test Failed",
            ),
            None => assert!(result_be.is_none(), "G2 {test_name} BE expected failure"),
        }

        // Test Little Endian
        let input_le_pod = to_pod_g2_compressed(input_le);
        let result_le = bls12_381_g2_decompress(Version::V0, &input_le_pod, Endianness::LE);
        match output_le {
            Some(expected) => assert_eq!(
                result_le,
                Some(to_pod_g2(expected)),
                "G2 {test_name} LE Test Failed",
            ),
            None => assert!(result_le.is_none(), "G2 {test_name} LE expected failure"),
        }
    }

    #[test]
    fn test_g1_decompress_random() {
        run_g1_test(
            "Decompress: RANDOM",
            INPUT_BE_G1_DECOMPRESS_RANDOM,
            Some(OUTPUT_BE_G1_DECOMPRESS_RANDOM),
            INPUT_LE_G1_DECOMPRESS_RANDOM,
            Some(OUTPUT_LE_G1_DECOMPRESS_RANDOM),
        );
    }

    #[test]
    fn test_g1_decompress_infinity() {
        run_g1_test(
            "Decompress: INFINITY",
            INPUT_BE_G1_DECOMPRESS_INFINITY,
            Some(OUTPUT_BE_G1_DECOMPRESS_INFINITY),
            INPUT_LE_G1_DECOMPRESS_INFINITY,
            Some(OUTPUT_LE_G1_DECOMPRESS_INFINITY),
        );
    }

    #[test]
    fn test_g1_decompress_generator() {
        run_g1_test(
            "Decompress: GENERATOR",
            INPUT_BE_G1_DECOMPRESS_GENERATOR,
            Some(OUTPUT_BE_G1_DECOMPRESS_GENERATOR),
            INPUT_LE_G1_DECOMPRESS_GENERATOR,
            Some(OUTPUT_LE_G1_DECOMPRESS_GENERATOR),
        );
    }

    #[test]
    fn test_g1_decompress_invalid() {
        run_g1_test(
            "Decompress: INVALID_CURVE",
            INPUT_BE_G1_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
            INPUT_LE_G1_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
        );
        run_g1_test(
            "Decompress: INVALID_FIELD",
            INPUT_BE_G1_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
            INPUT_LE_G1_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
        );
    }

    #[test]
    fn test_g2_decompress_random() {
        run_g2_test(
            "Decompress: RANDOM",
            INPUT_BE_G2_DECOMPRESS_RANDOM,
            Some(OUTPUT_BE_G2_DECOMPRESS_RANDOM),
            INPUT_LE_G2_DECOMPRESS_RANDOM,
            Some(OUTPUT_LE_G2_DECOMPRESS_RANDOM),
        );
    }

    #[test]
    fn test_g2_decompress_infinity() {
        run_g2_test(
            "Decompress: INFINITY",
            INPUT_BE_G2_DECOMPRESS_INFINITY,
            Some(OUTPUT_BE_G2_DECOMPRESS_INFINITY),
            INPUT_LE_G2_DECOMPRESS_INFINITY,
            Some(OUTPUT_LE_G2_DECOMPRESS_INFINITY),
        );
    }

    #[test]
    fn test_g2_decompress_generator() {
        run_g2_test(
            "Decompress: GENERATOR",
            INPUT_BE_G2_DECOMPRESS_GENERATOR,
            Some(OUTPUT_BE_G2_DECOMPRESS_GENERATOR),
            INPUT_LE_G2_DECOMPRESS_GENERATOR,
            Some(OUTPUT_LE_G2_DECOMPRESS_GENERATOR),
        );
    }

    #[test]
    fn test_g2_decompress_invalid() {
        run_g2_test(
            "Decompress: INVALID_CURVE",
            INPUT_BE_G2_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
            INPUT_LE_G2_DECOMPRESS_RANDOM_INVALID_CURVE,
            None,
        );
        run_g2_test(
            "Decompress: INVALID_FIELD",
            INPUT_BE_G2_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
            INPUT_LE_G2_DECOMPRESS_FIELD_TOO_LARGE_INVALID,
            None,
        );
    }
}
