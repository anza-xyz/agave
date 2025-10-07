pub use target_arch::*;
use {
    crate::scalar::PodScalar,
    bytemuck_derive::{Pod, Zeroable},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodG2Compressed(pub [u8; 96]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PodG2Affine(pub [u8; 192]);

unsafe impl bytemuck::Zeroable for PodG2Affine {}
unsafe impl bytemuck::Pod for PodG2Affine {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PodG2Projective(pub [u8; 288]);

unsafe impl bytemuck::Zeroable for PodG2Projective {}
unsafe impl bytemuck::Pod for PodG2Projective {}

#[cfg(not(target_os = "solana"))]
mod target_arch {
    use {
        super::*,
        blst::{
            blst_fp, blst_fp2, blst_fp_from_lendian, blst_lendian_from_fp, blst_p2, blst_p2_add,
            blst_p2_cneg, blst_p2_mult,
        },
        solana_curve_traits::GroupOperations,
    };

    pub fn add(
        left_point: &PodG2Projective,
        right_point: &PodG2Projective,
    ) -> Option<PodG2Projective> {
        PodG2Projective::add(left_point, right_point)
    }

    pub fn subtract(
        left_point: &PodG2Projective,
        right_point: &PodG2Projective,
    ) -> Option<PodG2Projective> {
        PodG2Projective::subtract(left_point, right_point)
    }

    pub fn multiply(scalar: &PodScalar, point: &PodG2Projective) -> Option<PodG2Projective> {
        PodG2Projective::multiply(scalar, point)
    }

    impl GroupOperations for PodG2Projective {
        type Scalar = PodScalar;
        type Point = Self;

        fn add(left_point: &Self, right_point: &Self) -> Option<Self> {
            let mut result = blst_p2::default();
            // TODO: this conversion makes a copy of bytes
            //   see if it is possible to make zero-copy conversion
            let left_point: blst_p2 = left_point.into();
            let right_point: blst_p2 = right_point.into();

            unsafe {
                blst_p2_add(
                    &mut result as *mut blst_p2,
                    &left_point as *const blst_p2,
                    &right_point as *const blst_p2,
                );
            }
            Some(result.into())
        }

        fn subtract(left_point: &Self, right_point: &Self) -> Option<Self> {
            let mut result = blst_p2::default();
            let left_point: blst_p2 = left_point.into();
            let right_point: blst_p2 = right_point.into();
            unsafe {
                let mut right_point_negated = right_point;
                blst_p2_cneg(&mut right_point_negated as *mut blst_p2, true);
                blst_p2_add(
                    &mut result as *mut blst_p2,
                    &left_point as *const blst_p2,
                    &right_point_negated as *const blst_p2,
                );
            }
            Some(result.into())
        }

        fn multiply(scalar: &PodScalar, point: &Self) -> Option<Self> {
            let mut result = blst_p2::default();
            let point: blst_p2 = point.into();
            unsafe {
                blst_p2_mult(
                    &mut result as *mut blst_p2,
                    &point as *const blst_p2,
                    scalar.0.as_ptr(),
                    256,
                );
            }
            Some(result.into())
        }
    }

    impl From<blst_p2> for PodG2Projective {
        fn from(point: blst_p2) -> Self {
            let mut bytes = [0u8; 288];
            unsafe {
                blst_lendian_from_fp(bytes[0..48].as_mut_ptr(), &point.x.fp[0] as *const blst_fp);
                blst_lendian_from_fp(bytes[48..96].as_mut_ptr(), &point.x.fp[1] as *const blst_fp);
                blst_lendian_from_fp(
                    bytes[96..144].as_mut_ptr(),
                    &point.y.fp[0] as *const blst_fp,
                );
                blst_lendian_from_fp(
                    bytes[144..192].as_mut_ptr(),
                    &point.y.fp[1] as *const blst_fp,
                );
                blst_lendian_from_fp(
                    bytes[192..240].as_mut_ptr(),
                    &point.z.fp[0] as *const blst_fp,
                );
                blst_lendian_from_fp(
                    bytes[240..288].as_mut_ptr(),
                    &point.z.fp[1] as *const blst_fp,
                );
            }
            Self(bytes)
        }
    }

    impl From<PodG2Projective> for blst_p2 {
        fn from(point: PodG2Projective) -> Self {
            let mut x = blst_fp2::default();
            let mut y = blst_fp2::default();
            let mut z = blst_fp2::default();
            unsafe {
                blst_fp_from_lendian(&mut x.fp[0] as *mut blst_fp, point.0[0..48].as_ptr());
                blst_fp_from_lendian(&mut x.fp[1] as *mut blst_fp, point.0[48..96].as_ptr());
                blst_fp_from_lendian(&mut y.fp[0] as *mut blst_fp, point.0[96..144].as_ptr());
                blst_fp_from_lendian(&mut y.fp[1] as *mut blst_fp, point.0[144..192].as_ptr());
                blst_fp_from_lendian(&mut z.fp[0] as *mut blst_fp, point.0[192..240].as_ptr());
                blst_fp_from_lendian(&mut z.fp[1] as *mut blst_fp, point.0[240..288].as_ptr());
            }
            blst_p2 { x, y, z }
        }
    }

    impl From<&PodG2Projective> for blst_p2 {
        fn from(point: &PodG2Projective) -> Self {
            let mut x = blst_fp2::default();
            let mut y = blst_fp2::default();
            let mut z = blst_fp2::default();
            unsafe {
                blst_fp_from_lendian(&mut x.fp[0] as *mut blst_fp, point.0[0..48].as_ptr());
                blst_fp_from_lendian(&mut x.fp[1] as *mut blst_fp, point.0[48..96].as_ptr());
                blst_fp_from_lendian(&mut y.fp[0] as *mut blst_fp, point.0[96..144].as_ptr());
                blst_fp_from_lendian(&mut y.fp[1] as *mut blst_fp, point.0[144..192].as_ptr());
                blst_fp_from_lendian(&mut z.fp[0] as *mut blst_fp, point.0[192..240].as_ptr());
                blst_fp_from_lendian(&mut z.fp[1] as *mut blst_fp, point.0[240..288].as_ptr());
            }
            blst_p2 { x, y, z }
        }
    }
}

#[cfg(target_os = "solana")]
mod target_arch {
    use {
        super::*,
        bytemuck::Zeroable,
        solana_curve_traits::{ADD, BLS12_381_G1_PROJECTIVE, MUL, SUB},
    };

    pub fn add(
        left_point: &PodG2Projective,
        right_point: &PodG2Projective,
    ) -> Option<PodG2Projective> {
        let mut result_point = PodG2Projective::zeroed();
        let result = unsafe {
            solana_define_syscall::definitions::sol_curve_group_op(
                BLS12_381_G1_PROJECTIVE,
                ADD,
                &left_point.0 as *const u8,
                &right_point.0 as *const u8,
                &mut result_point.0 as *mut u8,
            )
        };

        if result == 0 {
            Some(result_point)
        } else {
            None
        }
    }

    pub fn subtract(
        left_point: &PodG2Projective,
        right_point: &PodG2Projective,
    ) -> Option<PodG2Projective> {
        let mut result_point = PodG2Projective::zeroed();
        let result = unsafe {
            solana_define_syscall::definitions::sol_curve_group_op(
                BLS12_381_G1_PROJECTIVE,
                SUB,
                &left_point.0 as *const u8,
                &right_point.0 as *const u8,
                &mut result_point.0 as *mut u8,
            )
        };

        if result == 0 {
            Some(result_point)
        } else {
            None
        }
    }

    pub fn multiply(scalar: &PodScalar, point: &PodG2Projective) -> Option<PodG2Projective> {
        let mut result_point = PodG2Projective::zeroed();
        let result = unsafe {
            solana_define_syscall::definitions::sol_curve_group_op(
                BLS12_381_G1_PROJECTIVE,
                MUL,
                &scalar.0 as *const u8,
                &point.0 as *const u8,
                &mut result_point.0 as *mut u8,
            )
        };

        if result == 0 {
            Some(result_point)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::scalar::PodScalar,
        blst::{blst_p2, blst_p2_affine},
        solana_curve_traits::GroupOperations,
    };

    unsafe fn decompress(compressed: &PodG2Compressed) -> PodG2Projective {
        let point_ptr = &compressed.0 as *const u8;

        let mut point_affine = blst_p2_affine::default();
        let point_affine_ptr = &mut point_affine as *mut blst_p2_affine;
        blst::blst_p2_uncompress(point_affine_ptr, point_ptr);

        let mut point_full = blst_p2::default();
        let point_full_ptr = &mut point_full as *mut blst_p2;
        blst::blst_p2_from_affine(point_full_ptr, point_affine_ptr);

        point_full.into()
    }

    unsafe fn compress(projective: &PodG2Projective) -> PodG2Compressed {
        let mut compressed = [0u8; 96];
        let point_ptr = &projective.0 as *const u8 as *mut blst_p2;
        blst::blst_p2_compress(compressed.as_mut_ptr(), point_ptr);
        PodG2Compressed(compressed)
    }

    #[test]
    fn test_add_subtract_bls_12_381() {
        let identity: PodG2Projective = blst_p2::default().into();

        let point_a_compressed = PodG2Compressed([
            164, 206, 80, 113, 43, 158, 131, 37, 93, 106, 231, 75, 147, 161, 185, 106, 81, 151, 33,
            215, 119, 212, 236, 144, 255, 79, 164, 84, 156, 164, 121, 86, 19, 207, 42, 161, 95, 32,
            22, 141, 21, 250, 100, 154, 134, 50, 186, 209, 12, 208, 242, 49, 189, 146, 166, 202,
            120, 136, 221, 182, 244, 18, 95, 15, 95, 85, 3, 216, 6, 37, 199, 101, 109, 31, 213, 20,
            68, 69, 19, 79, 126, 19, 60, 71, 114, 17, 78, 220, 142, 37, 33, 157, 252, 2, 18, 182,
        ]);

        let point_b_compressed = PodG2Compressed([
            183, 42, 8, 225, 237, 101, 184, 130, 73, 9, 104, 128, 181, 122, 114, 248, 38, 145, 28,
            175, 76, 168, 219, 102, 168, 17, 1, 163, 145, 33, 127, 101, 159, 1, 108, 7, 56, 68,
            142, 7, 151, 2, 220, 149, 227, 134, 194, 231, 9, 6, 86, 227, 163, 72, 228, 151, 235,
            97, 51, 218, 156, 244, 234, 108, 157, 71, 90, 247, 143, 215, 224, 44, 68, 20, 155, 178,
            155, 29, 183, 167, 10, 244, 56, 19, 49, 169, 90, 8, 100, 86, 172, 14, 119, 200, 205,
            193,
        ]);

        let point_c_compressed = PodG2Compressed([
            139, 35, 111, 111, 138, 15, 121, 99, 87, 180, 83, 67, 5, 100, 162, 78, 79, 114, 138,
            150, 244, 249, 138, 213, 44, 122, 179, 155, 36, 156, 121, 98, 76, 57, 109, 116, 219,
            227, 54, 177, 90, 19, 147, 215, 145, 4, 231, 175, 1, 144, 102, 168, 64, 217, 60, 234,
            32, 38, 115, 250, 43, 47, 227, 138, 249, 195, 141, 231, 226, 207, 122, 246, 147, 50,
            72, 230, 22, 215, 146, 161, 209, 111, 221, 185, 53, 103, 4, 224, 151, 54, 60, 94, 65,
            34, 66, 247,
        ]);

        let point_a = unsafe { decompress(&point_a_compressed) };
        let point_b = unsafe { decompress(&point_b_compressed) };
        let point_c = unsafe { decompress(&point_c_compressed) };

        // identity
        assert_eq!(PodG2Projective::add(&point_a, &identity).unwrap(), point_a);

        // associativity
        unsafe {
            assert_eq!(
                compress(
                    &PodG2Projective::add(
                        &PodG2Projective::add(&point_a, &point_b).unwrap(),
                        &point_c
                    )
                    .unwrap()
                ),
                compress(
                    &PodG2Projective::add(
                        &point_a,
                        &PodG2Projective::add(&point_b, &point_c).unwrap()
                    )
                    .unwrap()
                ),
            )
        };

        unsafe {
            assert_eq!(
                compress(
                    &PodG2Projective::subtract(
                        &PodG2Projective::subtract(&point_a, &point_b).unwrap(),
                        &point_c
                    )
                    .unwrap()
                ),
                compress(
                    &PodG2Projective::subtract(
                        &point_a,
                        &PodG2Projective::add(&point_b, &point_c).unwrap()
                    )
                    .unwrap()
                ),
            )
        };

        // commutativity
        unsafe {
            assert_eq!(
                compress(&PodG2Projective::add(&point_a, &point_b).unwrap()),
                compress(&PodG2Projective::add(&point_b, &point_a).unwrap())
            )
        };

        // subtraction
        unsafe {
            assert_eq!(
                compress(&PodG2Projective::subtract(&point_a, &point_a).unwrap()),
                compress(&identity)
            )
        };
    }

    #[test]
    fn test_multiply_bls12_381() {
        let scalar = PodScalar([
            107, 15, 13, 77, 216, 207, 117, 144, 252, 166, 162, 81, 107, 12, 249, 164, 242, 212,
            76, 68, 144, 198, 72, 233, 76, 116, 60, 179, 0, 32, 86, 93,
        ]);

        let point_a_compressed = PodG2Compressed([
            164, 206, 80, 113, 43, 158, 131, 37, 93, 106, 231, 75, 147, 161, 185, 106, 81, 151, 33,
            215, 119, 212, 236, 144, 255, 79, 164, 84, 156, 164, 121, 86, 19, 207, 42, 161, 95, 32,
            22, 141, 21, 250, 100, 154, 134, 50, 186, 209, 12, 208, 242, 49, 189, 146, 166, 202,
            120, 136, 221, 182, 244, 18, 95, 15, 95, 85, 3, 216, 6, 37, 199, 101, 109, 31, 213, 20,
            68, 69, 19, 79, 126, 19, 60, 71, 114, 17, 78, 220, 142, 37, 33, 157, 252, 2, 18, 182,
        ]);

        let point_b_compressed = PodG2Compressed([
            183, 42, 8, 225, 237, 101, 184, 130, 73, 9, 104, 128, 181, 122, 114, 248, 38, 145, 28,
            175, 76, 168, 219, 102, 168, 17, 1, 163, 145, 33, 127, 101, 159, 1, 108, 7, 56, 68,
            142, 7, 151, 2, 220, 149, 227, 134, 194, 231, 9, 6, 86, 227, 163, 72, 228, 151, 235,
            97, 51, 218, 156, 244, 234, 108, 157, 71, 90, 247, 143, 215, 224, 44, 68, 20, 155, 178,
            155, 29, 183, 167, 10, 244, 56, 19, 49, 169, 90, 8, 100, 86, 172, 14, 119, 200, 205,
            193,
        ]);

        let point_a = unsafe { decompress(&point_a_compressed) };
        let point_b = unsafe { decompress(&point_b_compressed) };

        let ax = PodG2Projective::multiply(&scalar, &point_a).unwrap();
        let bx = PodG2Projective::multiply(&scalar, &point_b).unwrap();

        unsafe {
            assert_eq!(
                compress(&PodG2Projective::add(&ax, &bx).unwrap()),
                compress(
                    &PodG2Projective::multiply(
                        &scalar,
                        &PodG2Projective::add(&point_a, &point_b).unwrap()
                    )
                    .unwrap()
                ),
            )
        };
    }
}
