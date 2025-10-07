pub use target_arch::*;
use {
    crate::scalar::PodScalar,
    bytemuck_derive::{Pod, Zeroable},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodG1Compressed(pub [u8; 48]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodG1Affine(pub [u8; 96]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PodG1Projective(pub [u8; 144]);

unsafe impl bytemuck::Zeroable for PodG1Projective {}
unsafe impl bytemuck::Pod for PodG1Projective {}

#[cfg(not(target_os = "solana"))]
mod target_arch {
    use {
        super::*,
        blst::{
            blst_fp, blst_fp_from_lendian, blst_lendian_from_fp, blst_p1, blst_p1_add,
            blst_p1_cneg, blst_p1_mult,
        },
        solana_curve_traits::GroupOperations,
    };

    pub fn add(
        left_point: &PodG1Projective,
        right_point: &PodG1Projective,
    ) -> Option<PodG1Projective> {
        PodG1Projective::add(left_point, right_point)
    }

    pub fn subtract(
        left_point: &PodG1Projective,
        right_point: &PodG1Projective,
    ) -> Option<PodG1Projective> {
        PodG1Projective::subtract(left_point, right_point)
    }

    pub fn multiply(scalar: &PodScalar, point: &PodG1Projective) -> Option<PodG1Projective> {
        PodG1Projective::multiply(scalar, point)
    }

    impl GroupOperations for PodG1Projective {
        type Scalar = PodScalar;
        type Point = Self;

        fn add(left_point: &Self, right_point: &Self) -> Option<Self> {
            let mut result = blst_p1::default();
            // TODO: this conversion makes a copy of bytes
            //   see if it is possible to make zero-copy conversion
            let left_point: blst_p1 = left_point.into();
            let right_point: blst_p1 = right_point.into();

            unsafe {
                blst_p1_add(
                    &mut result as *mut blst_p1,
                    &left_point as *const blst_p1,
                    &right_point as *const blst_p1,
                );
            }
            Some(result.into())
        }

        fn subtract(left_point: &Self, right_point: &Self) -> Option<Self> {
            let mut result = blst_p1::default();
            let left_point: blst_p1 = left_point.into();
            let right_point: blst_p1 = right_point.into();
            unsafe {
                let mut right_point_negated = right_point;
                blst_p1_cneg(&mut right_point_negated as *mut blst_p1, true);
                blst_p1_add(
                    &mut result as *mut blst_p1,
                    &left_point as *const blst_p1,
                    &right_point_negated as *const blst_p1,
                );
            }
            Some(result.into())
        }

        fn multiply(scalar: &PodScalar, point: &Self) -> Option<Self> {
            let mut result = blst_p1::default();
            let point: blst_p1 = point.into();
            unsafe {
                blst_p1_mult(
                    &mut result as *mut blst_p1,
                    &point as *const blst_p1,
                    scalar.0.as_ptr(),
                    256,
                );
            }
            Some(result.into())
        }
    }

    impl From<blst_p1> for PodG1Projective {
        fn from(point: blst_p1) -> Self {
            let mut bytes = [0u8; 144];
            // TODO: this is unchecked; check if on curve and in the correct coset
            unsafe {
                blst_lendian_from_fp(bytes[0..48].as_mut_ptr(), &point.x as *const blst_fp);
                blst_lendian_from_fp(bytes[48..96].as_mut_ptr(), &point.y as *const blst_fp);
                blst_lendian_from_fp(bytes[96..144].as_mut_ptr(), &point.z as *const blst_fp);
            }
            Self(bytes)
        }
    }

    impl From<PodG1Projective> for blst_p1 {
        fn from(point: PodG1Projective) -> Self {
            let mut x = blst_fp::default();
            let mut y = blst_fp::default();
            let mut z = blst_fp::default();
            unsafe {
                blst_fp_from_lendian(&mut x as *mut blst_fp, point.0[0..48].as_ptr());
                blst_fp_from_lendian(&mut y as *mut blst_fp, point.0[48..96].as_ptr());
                blst_fp_from_lendian(&mut z as *mut blst_fp, point.0[96..144].as_ptr());
            }
            blst_p1 { x, y, z }
        }
    }

    impl From<&PodG1Projective> for blst_p1 {
        fn from(point: &PodG1Projective) -> Self {
            let mut x = blst_fp::default();
            let mut y = blst_fp::default();
            let mut z = blst_fp::default();
            unsafe {
                blst_fp_from_lendian(&mut x as *mut blst_fp, point.0[0..48].as_ptr());
                blst_fp_from_lendian(&mut y as *mut blst_fp, point.0[48..96].as_ptr());
                blst_fp_from_lendian(&mut z as *mut blst_fp, point.0[96..144].as_ptr());
            }
            blst_p1 { x, y, z }
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
        left_point: &PodG1Projective,
        right_point: &PodG1Projective,
    ) -> Option<PodG1Projective> {
        let mut result_point = PodG1Projective::zeroed();
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
        left_point: &PodG1Projective,
        right_point: &PodG1Projective,
    ) -> Option<PodG1Projective> {
        let mut result_point = PodG1Projective::zeroed();
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

    pub fn multiply(scalar: &PodScalar, point: &PodG1Projective) -> Option<PodG1Projective> {
        let mut result_point = PodG1Projective::zeroed();
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
        blst::{blst_p1, blst_p1_affine},
        solana_curve_traits::GroupOperations,
    };

    unsafe fn decompress(compressed: &PodG1Compressed) -> PodG1Projective {
        let point_ptr = &compressed.0 as *const u8;

        let mut point_affine = blst_p1_affine::default();
        let point_affine_ptr = &mut point_affine as *mut blst_p1_affine;
        blst::blst_p1_uncompress(point_affine_ptr, point_ptr);

        let mut point_full = blst_p1::default();
        let point_full_ptr = &mut point_full as *mut blst_p1;
        blst::blst_p1_from_affine(point_full_ptr, point_affine_ptr);

        point_full.into()
    }

    unsafe fn compress(projective: &PodG1Projective) -> PodG1Compressed {
        let mut compressed = [0u8; 48];
        let point_ptr = &projective.0 as *const u8 as *mut blst_p1;
        blst::blst_p1_compress(compressed.as_mut_ptr(), point_ptr);
        PodG1Compressed(compressed)
    }

    #[test]
    fn test_add_subtract_bls_12_381() {
        let identity: PodG1Projective = blst_p1::default().into();

        let point_a_compressed = PodG1Compressed([
            140, 112, 74, 2, 254, 123, 212, 72, 73, 122, 106, 93, 64, 7, 172, 236, 36, 227, 96,
            130, 121, 240, 41, 205, 62, 7, 207, 15, 94, 159, 7, 91, 99, 57, 241, 162, 136, 81, 90,
            5, 179, 98, 6, 98, 41, 146, 195, 14,
        ]);

        let point_b_compressed = PodG1Compressed([
            149, 247, 195, 10, 243, 121, 148, 92, 212, 118, 110, 34, 133, 35, 193, 161, 225, 85,
            122, 150, 192, 175, 136, 69, 63, 0, 146, 159, 103, 117, 89, 145, 171, 184, 105, 135,
            75, 231, 97, 247, 162, 101, 208, 175, 198, 222, 35, 102,
        ]);

        let point_c_compressed = PodG1Compressed([
            137, 46, 171, 236, 48, 64, 85, 76, 96, 91, 201, 87, 53, 133, 184, 211, 4, 113, 227,
            145, 17, 134, 71, 182, 72, 39, 55, 230, 145, 29, 216, 20, 52, 247, 57, 191, 255, 53,
            57, 150, 221, 59, 52, 78, 171, 240, 129, 39,
        ]);

        let point_a = unsafe { decompress(&point_a_compressed) };
        let point_b = unsafe { decompress(&point_b_compressed) };
        let point_c = unsafe { decompress(&point_c_compressed) };

        // identity
        assert_eq!(PodG1Projective::add(&point_a, &identity).unwrap(), point_a);

        // associativity
        unsafe {
            assert_eq!(
                compress(
                    &PodG1Projective::add(
                        &PodG1Projective::add(&point_a, &point_b).unwrap(),
                        &point_c
                    )
                    .unwrap()
                ),
                compress(
                    &PodG1Projective::add(
                        &point_a,
                        &PodG1Projective::add(&point_b, &point_c).unwrap()
                    )
                    .unwrap()
                ),
            )
        };

        unsafe {
            assert_eq!(
                compress(
                    &PodG1Projective::subtract(
                        &PodG1Projective::subtract(&point_a, &point_b).unwrap(),
                        &point_c
                    )
                    .unwrap()
                ),
                compress(
                    &PodG1Projective::subtract(
                        &point_a,
                        &PodG1Projective::add(&point_b, &point_c).unwrap()
                    )
                    .unwrap()
                ),
            )
        };

        // commutativity
        unsafe {
            assert_eq!(
                compress(&PodG1Projective::add(&point_a, &point_b).unwrap()),
                compress(&PodG1Projective::add(&point_b, &point_a).unwrap())
            )
        };

        // subtraction
        unsafe {
            assert_eq!(
                compress(&PodG1Projective::subtract(&point_a, &point_a).unwrap()),
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

        let point_a_compressed = PodG1Compressed([
            140, 112, 74, 2, 254, 123, 212, 72, 73, 122, 106, 93, 64, 7, 172, 236, 36, 227, 96,
            130, 121, 240, 41, 205, 62, 7, 207, 15, 94, 159, 7, 91, 99, 57, 241, 162, 136, 81, 90,
            5, 179, 98, 6, 98, 41, 146, 195, 14,
        ]);

        let point_b_compressed = PodG1Compressed([
            149, 247, 195, 10, 243, 121, 148, 92, 212, 118, 110, 34, 133, 35, 193, 161, 225, 85,
            122, 150, 192, 175, 136, 69, 63, 0, 146, 159, 103, 117, 89, 145, 171, 184, 105, 135,
            75, 231, 97, 247, 162, 101, 208, 175, 198, 222, 35, 102,
        ]);

        let point_a = unsafe { decompress(&point_a_compressed) };
        let point_b = unsafe { decompress(&point_b_compressed) };

        let ax = PodG1Projective::multiply(&scalar, &point_a).unwrap();
        let bx = PodG1Projective::multiply(&scalar, &point_b).unwrap();

        unsafe {
            assert_eq!(
                compress(&PodG1Projective::add(&ax, &bx).unwrap()),
                compress(
                    &PodG1Projective::multiply(
                        &scalar,
                        &PodG1Projective::add(&point_a, &point_b).unwrap()
                    )
                    .unwrap()
                ),
            )
        };
    }
}
