use {
    blst::{blst_bendian_from_fp, blst_fp12, blst_lendian_from_fp},
    blstrs::Gt,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endianness {
    BE,
    LE,
}

pub(crate) fn reverse_48_byte_chunks(bytes: &mut [u8]) {
    for chunk in bytes.chunks_mut(48) {
        chunk.reverse();
    }
}

pub(crate) fn swap_g2_c0_c1(bytes: &mut [u8]) {
    for fq2_chunk in bytes.chunks_exact_mut(96) {
        let (c0, c1) = fq2_chunk.split_at_mut(48);
        c0.swap_with_slice(c1);
    }
}

/// Helper to serialize Fp12 (Gt) according to SIMD Endianness rules.
/// Fp12 = c0(Fp6) + c1(Fp6)w.
/// Fp6 = c0(Fp2) + c1(Fp2)v + c2(Fp2)v^2.
/// Fp2 = c0(Fp) + c1(Fp)u.
/// SIMD/Zcash BE Rule for Fp2: c1 (imaginary) then c0 (real).
/// SIMD LE Rule for Fp2: c0 then c1.
pub(crate) fn serialize_gt(gt: Gt, endianness: Endianness) -> Vec<u8> {
    // blstrs::Gt is repr(transparent) over blst_fp12.
    // We transmute to access the internal coefficients directly because
    // blstrs does not expose the Fp12/Fp6/Fp2/Fp types publicly.
    let val: blst_fp12 = unsafe { std::mem::transmute(gt) };

    let mut out = Vec::with_capacity(576); // 12 * 48
    let mut ptr = out.as_mut_ptr();

    unsafe {
        for fp6 in val.fp6.iter() {
            for fp2 in fp6.fp2.iter() {
                let c0 = &fp2.fp[0];
                let c1 = &fp2.fp[1];

                let fps = match endianness {
                    Endianness::BE => [c1, c0],
                    Endianness::LE => [c0, c1],
                };

                for fp in fps {
                    match endianness {
                        Endianness::BE => blst_bendian_from_fp(ptr, fp),
                        Endianness::LE => blst_lendian_from_fp(ptr, fp),
                    }
                    ptr = ptr.add(48);
                }
            }
        }
        out.set_len(576);
    }
    out
}

#[cfg(test)]
mod tests {
    use {super::*, blstrs::Gt, group::Group};

    #[test]
    fn test_reverse_48_byte_chunks() {
        // Create a buffer with 2 chunks (96 bytes)
        // Chunk 0: 0..48
        // Chunk 1: 48..96
        let mut input = Vec::new();
        for i in 0..96u8 {
            input.push(i);
        }

        let mut expected = input.clone();
        // Manually reverse the expected chunks
        expected[0..48].reverse();
        expected[48..96].reverse();

        reverse_48_byte_chunks(&mut input);

        assert_eq!(input, expected);
    }

    #[test]
    fn test_swap_g2_c0_c1() {
        // Create a buffer with 1 G2 point (96 bytes)
        // c0: 48 bytes of 0xAA
        // c1: 48 bytes of 0xBB
        let mut input = Vec::new();
        input.extend_from_slice(&[0xAAu8; 48]);
        input.extend_from_slice(&[0xBBu8; 48]);

        // Expected result: c1 then c0
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0xBBu8; 48]);
        expected.extend_from_slice(&[0xAAu8; 48]);

        swap_g2_c0_c1(&mut input);

        assert_eq!(input, expected);
    }

    #[test]
    fn test_serialize_gt_identity() {
        let identity = Gt::identity(); // 1 + 0u + ...

        // Fp12 = c0 + c1*w
        // c0 (Fp6) = c0 + c1*v + c2*v^2
        // c0 (Fp2) = c0 + c1*u
        // Identity has c0.c0.c0 = 1, all others 0.

        // BE (Zcash): Fp2 is [c1, c0].
        // The first Fp2 is c0.c0.
        // So the order is c0.c0.c1 (0), then c0.c0.c0 (1).
        let be_bytes = serialize_gt(identity, Endianness::BE);
        // First 48 bytes should be 0
        assert_eq!(&be_bytes[0..48], &[0u8; 48]);
        // Second 48 bytes should be 1 (canonical, big-endian)
        let mut one_be = [0u8; 48];
        one_be[47] = 1;
        assert_eq!(&be_bytes[48..96], &one_be);

        // LE (SIMD): Fp2 is [c0, c1].
        // The first Fp2 is c0.c0.
        // So the order is c0.c0.c0 (1), then c0.c0.c1 (0).
        let le_bytes = serialize_gt(identity, Endianness::LE);
        // First 48 bytes should be 1 (canonical, little-endian)
        let mut one_le = [0u8; 48];
        one_le[0] = 1;
        assert_eq!(&le_bytes[0..48], &one_le);
        // Second 48 bytes should be 0
        assert_eq!(&le_bytes[48..96], &[0u8; 48]);
    }
}
