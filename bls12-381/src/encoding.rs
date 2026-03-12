use {
    blst::{blst_fp12, blst_lendian_from_fp},
    blstrs::{G1Affine, G2Affine, Gt, Scalar},
    bytemuck::Zeroable,
    bytemuck_derive::{Pod, Zeroable as DeriveZeroable},
};

/// Size of a base field element (`Fq`) in bytes.
pub const FQ_SIZE: usize = 48;
/// Size of a quadratic extension field element (`Fq2`) in bytes.
pub const FQ2_SIZE: usize = 2 * FQ_SIZE;
/// Size of a target group element (`Gt` or `Fq12`) in bytes.
pub const GT_SIZE: usize = 12 * FQ_SIZE;

/// G1 affine point size (uncompressed coordinates).
/// G1 uncompressed = x (48) + y (48) = 96 bytes.
pub const G1_UNCOMPRESSED_SIZE: usize = 2 * FQ_SIZE;

/// G1 compressed point size.
/// G1 compressed = x (48 bytes) with flags encoded in MSB.
pub const G1_COMPRESSED_SIZE: usize = FQ_SIZE;

/// G2 affine point size (uncompressed coordinates).
/// G2 uncompressed = x (96) + y (96) = 192 bytes.
pub const G2_UNCOMPRESSED_SIZE: usize = 2 * FQ2_SIZE;

/// G2 compressed point size.
/// G2 compressed = x (96 bytes) with flags encoded in MSB.
pub const G2_COMPRESSED_SIZE: usize = FQ2_SIZE;

/// Scalar size in bytes.
pub const SCALAR_SIZE: usize = 32;

/// G1 compressed point (48 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodG1Compressed(pub [u8; G1_COMPRESSED_SIZE]);

/// G1 affine point (96 bytes).
/// Represents `x` (48 bytes) and `y` (48 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodG1Point(pub [u8; G1_UNCOMPRESSED_SIZE]);

impl PodG1Point {
    /// Deserializes to an affine point, skipping the subgroup check.
    ///
    /// Checks: Field validity, Curve equation (`y^2 = x^3 + 4`).
    /// Skips: Subgroup membership
    pub fn to_affine_subgroup_unchecked(&self, endianness: Endianness) -> Option<G1Affine> {
        match endianness {
            // `G1Affine::from_uncompressed_unchecked` already performs field and on-curve checks
            Endianness::BE => G1Affine::from_uncompressed_unchecked(&self.0).into_option(),
            Endianness::LE => {
                let mut bytes = self.0;
                swap_fq_endianness(&mut bytes);
                G1Affine::from_uncompressed_unchecked(&bytes).into_option()
            }
        }
    }

    /// Deserializes to an affine point with full validation.
    ///
    /// Checks: Field validity, Curve equation (`y^2 = x^3 + 4`), Subgroup membership.
    pub fn to_affine(&self, endianness: Endianness) -> Option<G1Affine> {
        let affine = self.to_affine_subgroup_unchecked(endianness)?;
        if bool::from(affine.is_torsion_free()) {
            Some(affine)
        } else {
            None
        }
    }
}

/// G2 compressed point (96 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodG2Compressed(pub [u8; G2_COMPRESSED_SIZE]);

/// G2 affine point (192 bytes).
/// Represents `x` (96 bytes) and `y` (96 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodG2Point(pub [u8; G2_UNCOMPRESSED_SIZE]);

impl PodG2Point {
    /// Deserializes to an affine point, skipping the subgroup check.
    ///
    /// Checks: Field validity, Curve equation (`y^2 = x^3 + 4(1+u)^{-1}`).
    /// Skips: Subgroup membership
    pub fn to_affine_subgroup_unchecked(&self, endianness: Endianness) -> Option<G2Affine> {
        match endianness {
            // `G2Affine::from_uncompressed_unchecked` already performs field and on-curve checks
            Endianness::BE => G2Affine::from_uncompressed_unchecked(&self.0).into_option(),
            Endianness::LE => {
                let mut bytes = self.0;
                swap_fq_endianness(&mut bytes);
                swap_g2_c0_c1(&mut bytes);
                G2Affine::from_uncompressed_unchecked(&bytes).into_option()
            }
        }
    }

    /// Deserializes to an affine point with full validation.
    ///
    /// Checks: Field validity, Curve equation (`y^2 = x^3 + 4(1+u)^{-1}`), Subgroup membership.
    pub fn to_affine(&self, endianness: Endianness) -> Option<G2Affine> {
        let affine = self.to_affine_subgroup_unchecked(endianness)?;
        if bool::from(affine.is_torsion_free()) {
            Some(affine)
        } else {
            None
        }
    }
}

/// Scalar value (32 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodScalar(pub [u8; SCALAR_SIZE]);

impl PodScalar {
    pub fn to_scalar(&self, endianness: Endianness) -> Option<Scalar> {
        match endianness {
            Endianness::BE => Scalar::from_bytes_be(&self.0).into_option(),
            Endianness::LE => Scalar::from_bytes_le(&self.0).into_option(),
        }
    }
}

/// Target group element (Gt).
/// Represents an element in the extension field Fq12 (576 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, DeriveZeroable)]
#[repr(transparent)]
pub struct PodGtElement(pub [u8; GT_SIZE]);

/// Specifies the byte ordering for BLS12-381 field elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endianness {
    /// Canonical Big-Endian encoding, following the [Zcash][zcash] and [IETF][ietf]
    /// standards.
    ///
    /// [zcash]: https://github.com/zkcrypto/pairing/tree/master/src/bls12_381#serialization
    /// [ieff]: https://www.ietf.org/archive/id/draft-irtf-cfrg-pairing-friendly-curves-11.html#name-bls-curves-for-the-128-bit-
    BE,
    /// Little-Endian encoding.
    /// Base field elements (`Fq`) are reversed compared to BE.
    LE,
}

/// Toggles the endianness of `Fq` (base field) elements in place.
///
/// Since a BLS12-381 `Fq` element is represented by 48 bytes, this function
/// iterates through the input buffer in 48-byte chunks and reverses the byte
/// order of each chunk.
pub(crate) fn swap_fq_endianness(bytes: &mut [u8]) {
    for chunk in bytes.chunks_mut(FQ_SIZE) {
        chunk.reverse();
    }
}

/// Swaps the real (`c_0`) and imaginary (`c_1`) components of `Fq2` elements in place.
///
/// In the Zcash/IETF Big-Endian standard, `Fq2` elements are ordered as `c1` then `c0`.
/// In the Little-Endian variant, they are ordered as `c0` then `c1` to match
/// standard polynomial representation ($c_0 + c_1 u$).
///
/// This function expects 96-byte chunks (representing one `Fq2` element or a G2 coordinate)
/// and swaps the first 48 bytes with the last 48 bytes.
pub(crate) fn swap_g2_c0_c1(bytes: &mut [u8]) {
    for fq2_chunk in bytes.chunks_exact_mut(FQ2_SIZE) {
        let (c0, c1) = fq2_chunk.split_at_mut(FQ_SIZE);
        c0.swap_with_slice(c1);
    }
}

/// Helper to serialize Fp12 (Gt) according to SIMD Endianness rules.
///
/// `Fp12 = c0(Fp6) + c1(Fp6)w`.
/// `Fp6 = c0(Fp2) + c1(Fp2)v + c2(Fp2)v^2`.
/// `Fp2 = c0(Fp) + c1(Fp)u`.
///
/// # Rules
/// 1. **Fq (`Fp`)**:
///    - BE: Big-Endian bytes.
///    - LE: Little-Endian bytes.
/// 2. **Coefficient Ordering**:
///    - BE: Highest degree coefficients first (reverses the entire tower).
///    - LE: Lowest degree coefficients first (canonical memory order).
pub(crate) fn serialize_gt(gt: Gt, endianness: Endianness) -> PodGtElement {
    // `blstrs::Gt` is `repr(transparent)` over `blst_fp12`.
    // We transmute to access the internal coefficients directly because
    // blstrs does not expose the `Fp12`/`Fp6`/`Fp2`/`Fp` types publicly.
    // Safe because layout is guaranteed by `repr(transparent)` wrapping the blst type.
    // We also verify in `test_gt_layout_safety` that layout sizes and alignment match.
    let val: blst_fp12 = unsafe { std::mem::transmute(gt) };

    let mut out = PodGtElement::zeroed();
    let mut ptr = out.0.as_mut_ptr();

    unsafe {
        for fp6 in val.fp6.iter() {
            for fp2 in fp6.fp2.iter() {
                // In LE, we write c0 (Real) then c1 (Imaginary)
                let c0 = &fp2.fp[0];
                let c1 = &fp2.fp[1];

                blst_lendian_from_fp(ptr, c0);
                ptr = ptr.add(FQ_SIZE);
                blst_lendian_from_fp(ptr, c1);
                ptr = ptr.add(FQ_SIZE);
            }
        }
    }

    // If Big Endian is requested, simply reverse the entire byte array, which
    // achieves both of the following simultaneously:
    // - flips coefficient order (c0...c11 -> c11...c0)
    // - flips coefficient bytes (LE -> BE)
    if matches!(endianness, Endianness::BE) {
        out.0.reverse();
    }

    out
}

#[cfg(test)]
mod tests {
    use {super::*, blstrs::Gt, group::Group};

    #[test]
    fn test_swap_fq_endianness() {
        // Create a buffer with 2 chunks (96 bytes)
        // Chunk 0: 0..48
        // Chunk 1: 48..96
        let mut input = Vec::new();
        for i in 0..96u8 {
            input.push(i);
        }

        let mut expected = input.clone();
        expected[0..48].reverse();
        expected[48..96].reverse();

        swap_fq_endianness(&mut input);

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
        let identity = Gt::identity();

        // BE
        let be_bytes = serialize_gt(identity, Endianness::BE);
        assert_eq!(&be_bytes.0[0..48], &[0u8; 48]);
        let mut one_be = [0u8; 48];
        one_be[47] = 1;
        assert_eq!(&be_bytes.0[528..576], &one_be);

        // LE
        let le_bytes = serialize_gt(identity, Endianness::LE);
        let mut one_le = [0u8; 48];
        one_le[0] = 1; // [01, 00, ... 00]
        assert_eq!(&le_bytes.0[0..48], &one_le);
        assert_eq!(&le_bytes.0[48..96], &[0u8; 48]);
    }

    #[test]
    fn test_gt_layout_safety() {
        // Critical Safety Check:
        // verify that blstrs::Gt is indeed a transparent wrapper around blst_fp12
        // before we rely on unsafe transmutes in the main code.
        assert_eq!(
            std::mem::size_of::<Gt>(),
            std::mem::size_of::<blst_fp12>(),
            "Size mismatch between blstrs::Gt and blst_fp12. Unsafe transmute is invalid."
        );
        assert_eq!(
            std::mem::align_of::<Gt>(),
            std::mem::align_of::<blst_fp12>(),
            "Alignment mismatch between blstrs::Gt and blst_fp12. Unsafe transmute is invalid."
        );
    }
}
