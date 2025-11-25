//! Legacy Poseidon hash implementation using older crate versions and not
//! enforcing the explicit padding.
//!
//! This module mirrors the logic from `lib.rs` but imports `ark_bn254_0_4` and
//! `light_poseidon_0_2`.

use crate::{Endianness, Parameters, PoseidonHash, PoseidonSyscallError};
#[cfg(target_os = "solana")]
use {crate::HASH_BYTES, solana_define_syscall::definitions::sol_poseidon};

/// Return a Poseidon hash for the given data with the given elliptic curve and
/// endianness.
///
/// # Examples
///
/// ```rust
/// use solana_poseidon::{hashv, Endianness, Parameters};
///
/// # fn test() {
/// let input1 = [1u8; 32];
/// let input2 = [2u8; 32];
///
/// let hash = hashv(Parameters::Bn254X5, Endianness::BigEndian, &[&input1, &input2]).unwrap();
/// assert_eq!(hash.to_bytes().len(), 32);
/// # }
/// ```
#[allow(unused_variables)]
pub fn hashv(
    // This parameter is not used currently, because we support only one curve
    // (BN254). It should be used in case we add more curves in the future.
    parameters: Parameters,
    endianness: Endianness,
    vals: &[&[u8]],
) -> Result<PoseidonHash, PoseidonSyscallError> {
    // Perform the calculation inline, calling this from within a program is
    // not supported.
    #[cfg(not(target_os = "solana"))]
    {
        use {
            ark_bn254_0_4::Fr,
            light_poseidon_0_2::{Poseidon, PoseidonBytesHasher, PoseidonError},
        };

        #[allow(non_local_definitions)]
        impl From<PoseidonError> for PoseidonSyscallError {
            fn from(error: PoseidonError) -> Self {
                match error {
                    PoseidonError::InvalidNumberOfInputs { .. } => {
                        PoseidonSyscallError::InvalidNumberOfInputs
                    }
                    PoseidonError::EmptyInput => PoseidonSyscallError::EmptyInput,
                    PoseidonError::InvalidInputLength { .. } => {
                        PoseidonSyscallError::InvalidInputLength
                    }
                    PoseidonError::BytesToPrimeFieldElement { .. } => {
                        PoseidonSyscallError::BytesToPrimeFieldElement
                    }
                    PoseidonError::InputLargerThanModulus => {
                        PoseidonSyscallError::InputLargerThanModulus
                    }
                    PoseidonError::VecToArray => PoseidonSyscallError::VecToArray,
                    PoseidonError::U64Tou8 => PoseidonSyscallError::U64Tou8,
                    PoseidonError::BytesToBigInt => PoseidonSyscallError::BytesToBigInt,
                    PoseidonError::InvalidWidthCircom { .. } => {
                        PoseidonSyscallError::InvalidWidthCircom
                    }
                }
            }
        }

        let mut hasher =
            Poseidon::<Fr>::new_circom(vals.len()).map_err(PoseidonSyscallError::from)?;
        let res = match endianness {
            Endianness::BigEndian => hasher.hash_bytes_be(vals),
            Endianness::LittleEndian => hasher.hash_bytes_le(vals),
        }
        .map_err(PoseidonSyscallError::from)?;

        Ok(PoseidonHash(res))
    }
    #[cfg(target_os = "solana")]
    {
        let mut hash_result = [0; HASH_BYTES];
        let result = unsafe {
            sol_poseidon(
                parameters.into(),
                endianness.into(),
                vals as *const _ as *const u8,
                vals.len() as u64,
                &mut hash_result as *mut _ as *mut u8,
            )
        };

        match result {
            0 => Ok(PoseidonHash::new(hash_result)),
            _ => Err(PoseidonSyscallError::Unexpected),
        }
    }
}

/// Return a Poseidon hash for the given data with the given elliptic curve and
/// endianness.
///
/// # Examples
///
/// ```rust
/// use solana_poseidon::{hash, Endianness, Parameters};
///
/// # fn test() {
/// let input = [1u8; 32];
///
/// let result = hash(Parameters::Bn254X5, Endianness::BigEndian, &input).unwrap();
/// assert_eq!(
///     result.to_bytes(),
///     [
///         5, 191, 172, 229, 129, 238, 97, 119, 204, 25, 198, 197, 99, 99, 166, 136, 130, 241,
///         30, 132, 7, 172, 99, 157, 185, 145, 224, 210, 127, 27, 117, 230
///     ],
/// );
///
/// let hash = hash(Parameters::Bn254X5, Endianness::LittleEndian, &input).unwrap();
/// assert_eq!(
///     hash.to_bytes(),
///     [
///         230, 117, 27, 127, 210, 224, 145, 185, 157, 99, 172, 7, 132, 30, 241, 130, 136,
///         166, 99, 99, 197, 198, 25, 204, 119, 97, 238, 129, 229, 172, 191, 5
///     ],
/// );
/// # }
/// ```
pub fn hash(
    parameters: Parameters,
    endianness: Endianness,
    val: &[u8],
) -> Result<PoseidonHash, PoseidonSyscallError> {
    hashv(parameters, endianness, &[val])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poseidon_input_ones_be() {
        let input = [1u8; 32];

        let hash = hash(Parameters::Bn254X5, Endianness::BigEndian, &input).unwrap();
        assert_eq!(
            hash.to_bytes(),
            [
                5, 191, 172, 229, 129, 238, 97, 119, 204, 25, 198, 197, 99, 99, 166, 136, 130, 241,
                30, 132, 7, 172, 99, 157, 185, 145, 224, 210, 127, 27, 117, 230
            ]
        );
    }

    #[test]
    fn test_poseidon_input_ones_le() {
        let input = [1u8; 32];

        let hash = hash(Parameters::Bn254X5, Endianness::LittleEndian, &input).unwrap();
        assert_eq!(
            hash.to_bytes(),
            [
                230, 117, 27, 127, 210, 224, 145, 185, 157, 99, 172, 7, 132, 30, 241, 130, 136,
                166, 99, 99, 197, 198, 25, 204, 119, 97, 238, 129, 229, 172, 191, 5
            ],
        );
    }

    #[test]
    fn test_poseidon_input_ones_twos_be() {
        let input1 = [1u8; 32];
        let input2 = [2u8; 32];

        let hash = hashv(
            Parameters::Bn254X5,
            Endianness::BigEndian,
            &[&input1, &input2],
        )
        .unwrap();
        assert_eq!(
            hash.to_bytes(),
            [
                13, 84, 225, 147, 143, 138, 140, 28, 125, 235, 94, 3, 85, 242, 99, 25, 32, 123,
                132, 254, 156, 162, 206, 27, 38, 231, 53, 200, 41, 130, 25, 144
            ]
        );
    }

    #[test]
    fn test_poseidon_input_ones_twos_le() {
        let input1 = [1u8; 32];
        let input2 = [2u8; 32];

        let hash = hashv(
            Parameters::Bn254X5,
            Endianness::LittleEndian,
            &[&input1, &input2],
        )
        .unwrap();
        assert_eq!(
            hash.to_bytes(),
            [
                144, 25, 130, 41, 200, 53, 231, 38, 27, 206, 162, 156, 254, 132, 123, 32, 25, 99,
                242, 85, 3, 94, 235, 125, 28, 140, 138, 143, 147, 225, 84, 13
            ]
        );
    }

    #[test]
    fn test_poseidon_input_one() {
        let input = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 1,
        ];

        let expected_hashes = [
            [
                41, 23, 97, 0, 234, 169, 98, 189, 193, 254, 108, 101, 77, 106, 60, 19, 14, 150,
                164, 209, 22, 139, 51, 132, 139, 137, 125, 197, 2, 130, 1, 51,
            ],
            [
                0, 122, 243, 70, 226, 211, 4, 39, 158, 121, 224, 169, 243, 2, 63, 119, 18, 148,
                167, 138, 203, 112, 231, 63, 144, 175, 226, 124, 173, 64, 30, 129,
            ],
            [
                2, 192, 6, 110, 16, 167, 42, 189, 43, 51, 195, 178, 20, 203, 62, 129, 188, 177,
                182, 227, 9, 97, 205, 35, 194, 2, 177, 134, 115, 191, 37, 67,
            ],
            [
                8, 44, 156, 55, 10, 13, 36, 244, 65, 111, 188, 65, 74, 55, 104, 31, 120, 68, 45,
                39, 216, 99, 133, 153, 28, 23, 214, 252, 12, 75, 125, 113,
            ],
            [
                16, 56, 150, 5, 174, 104, 141, 79, 20, 219, 133, 49, 34, 196, 125, 102, 168, 3,
                199, 43, 65, 88, 156, 177, 191, 134, 135, 65, 178, 6, 185, 187,
            ],
            [
                42, 115, 246, 121, 50, 140, 62, 171, 114, 74, 163, 229, 189, 191, 80, 179, 144, 53,
                215, 114, 159, 19, 91, 151, 9, 137, 15, 133, 197, 220, 94, 118,
            ],
            [
                34, 118, 49, 10, 167, 243, 52, 58, 40, 66, 20, 19, 157, 157, 169, 89, 190, 42, 49,
                178, 199, 8, 165, 248, 25, 84, 178, 101, 229, 58, 48, 184,
            ],
            [
                23, 126, 20, 83, 196, 70, 225, 176, 125, 43, 66, 51, 66, 81, 71, 9, 92, 79, 202,
                187, 35, 61, 35, 11, 109, 70, 162, 20, 217, 91, 40, 132,
            ],
            [
                14, 143, 238, 47, 228, 157, 163, 15, 222, 235, 72, 196, 46, 187, 68, 204, 110, 231,
                5, 95, 97, 251, 202, 94, 49, 59, 138, 95, 202, 131, 76, 71,
            ],
            [
                46, 196, 198, 94, 99, 120, 171, 140, 115, 48, 133, 79, 74, 112, 119, 193, 255, 146,
                96, 228, 72, 133, 196, 184, 29, 209, 49, 173, 58, 134, 205, 150,
            ],
            [
                0, 113, 61, 65, 236, 166, 53, 241, 23, 212, 236, 188, 235, 95, 58, 102, 220, 65,
                66, 235, 112, 181, 103, 101, 188, 53, 143, 27, 236, 64, 187, 155,
            ],
            [
                20, 57, 11, 224, 186, 239, 36, 155, 212, 124, 101, 221, 172, 101, 194, 229, 46,
                133, 19, 192, 129, 193, 205, 114, 201, 128, 6, 9, 142, 154, 143, 190,
            ],
        ];

        for (i, expected_hash) in expected_hashes.into_iter().enumerate() {
            let inputs = vec![&input[..]; i + 1];
            let hash = hashv(Parameters::Bn254X5, Endianness::BigEndian, &inputs).unwrap();
            assert_eq!(hash.to_bytes(), expected_hash);
        }
    }

    // This is one of the cases that differentiates the legacy module from the
    // main one.
    #[test]
    fn test_poseidon_input_without_padding_be() {
        let input = [1];

        let expected_hashes = [
            [
                41, 23, 97, 0, 234, 169, 98, 189, 193, 254, 108, 101, 77, 106, 60, 19, 14, 150,
                164, 209, 22, 139, 51, 132, 139, 137, 125, 197, 2, 130, 1, 51,
            ],
            [
                0, 122, 243, 70, 226, 211, 4, 39, 158, 121, 224, 169, 243, 2, 63, 119, 18, 148,
                167, 138, 203, 112, 231, 63, 144, 175, 226, 124, 173, 64, 30, 129,
            ],
            [
                2, 192, 6, 110, 16, 167, 42, 189, 43, 51, 195, 178, 20, 203, 62, 129, 188, 177,
                182, 227, 9, 97, 205, 35, 194, 2, 177, 134, 115, 191, 37, 67,
            ],
            [
                8, 44, 156, 55, 10, 13, 36, 244, 65, 111, 188, 65, 74, 55, 104, 31, 120, 68, 45,
                39, 216, 99, 133, 153, 28, 23, 214, 252, 12, 75, 125, 113,
            ],
            [
                16, 56, 150, 5, 174, 104, 141, 79, 20, 219, 133, 49, 34, 196, 125, 102, 168, 3,
                199, 43, 65, 88, 156, 177, 191, 134, 135, 65, 178, 6, 185, 187,
            ],
            [
                42, 115, 246, 121, 50, 140, 62, 171, 114, 74, 163, 229, 189, 191, 80, 179, 144, 53,
                215, 114, 159, 19, 91, 151, 9, 137, 15, 133, 197, 220, 94, 118,
            ],
            [
                34, 118, 49, 10, 167, 243, 52, 58, 40, 66, 20, 19, 157, 157, 169, 89, 190, 42, 49,
                178, 199, 8, 165, 248, 25, 84, 178, 101, 229, 58, 48, 184,
            ],
            [
                23, 126, 20, 83, 196, 70, 225, 176, 125, 43, 66, 51, 66, 81, 71, 9, 92, 79, 202,
                187, 35, 61, 35, 11, 109, 70, 162, 20, 217, 91, 40, 132,
            ],
            [
                14, 143, 238, 47, 228, 157, 163, 15, 222, 235, 72, 196, 46, 187, 68, 204, 110, 231,
                5, 95, 97, 251, 202, 94, 49, 59, 138, 95, 202, 131, 76, 71,
            ],
            [
                46, 196, 198, 94, 99, 120, 171, 140, 115, 48, 133, 79, 74, 112, 119, 193, 255, 146,
                96, 228, 72, 133, 196, 184, 29, 209, 49, 173, 58, 134, 205, 150,
            ],
            [
                0, 113, 61, 65, 236, 166, 53, 241, 23, 212, 236, 188, 235, 95, 58, 102, 220, 65,
                66, 235, 112, 181, 103, 101, 188, 53, 143, 27, 236, 64, 187, 155,
            ],
            [
                20, 57, 11, 224, 186, 239, 36, 155, 212, 124, 101, 221, 172, 101, 194, 229, 46,
                133, 19, 192, 129, 193, 205, 114, 201, 128, 6, 9, 142, 154, 143, 190,
            ],
        ];

        for (i, expected_hash) in expected_hashes.into_iter().enumerate() {
            let inputs = vec![&input[..]; i + 1];
            let hash = hashv(Parameters::Bn254X5, Endianness::BigEndian, &inputs).unwrap();
            assert_eq!(hash.to_bytes(), expected_hash);
        }
    }

    #[test]
    fn test_poseidon_input_without_padding_le() {
        let input = [1];

        let expected_hashes = [
            [
                51, 1, 130, 2, 197, 125, 137, 139, 132, 51, 139, 22, 209, 164, 150, 14, 19, 60,
                106, 77, 101, 108, 254, 193, 189, 98, 169, 234, 0, 97, 23, 41,
            ],
            [
                129, 30, 64, 173, 124, 226, 175, 144, 63, 231, 112, 203, 138, 167, 148, 18, 119,
                63, 2, 243, 169, 224, 121, 158, 39, 4, 211, 226, 70, 243, 122, 0,
            ],
            [
                67, 37, 191, 115, 134, 177, 2, 194, 35, 205, 97, 9, 227, 182, 177, 188, 129, 62,
                203, 20, 178, 195, 51, 43, 189, 42, 167, 16, 110, 6, 192, 2,
            ],
            [
                113, 125, 75, 12, 252, 214, 23, 28, 153, 133, 99, 216, 39, 45, 68, 120, 31, 104,
                55, 74, 65, 188, 111, 65, 244, 36, 13, 10, 55, 156, 44, 8,
            ],
            [
                187, 185, 6, 178, 65, 135, 134, 191, 177, 156, 88, 65, 43, 199, 3, 168, 102, 125,
                196, 34, 49, 133, 219, 20, 79, 141, 104, 174, 5, 150, 56, 16,
            ],
            [
                118, 94, 220, 197, 133, 15, 137, 9, 151, 91, 19, 159, 114, 215, 53, 144, 179, 80,
                191, 189, 229, 163, 74, 114, 171, 62, 140, 50, 121, 246, 115, 42,
            ],
            [
                184, 48, 58, 229, 101, 178, 84, 25, 248, 165, 8, 199, 178, 49, 42, 190, 89, 169,
                157, 157, 19, 20, 66, 40, 58, 52, 243, 167, 10, 49, 118, 34,
            ],
            [
                132, 40, 91, 217, 20, 162, 70, 109, 11, 35, 61, 35, 187, 202, 79, 92, 9, 71, 81,
                66, 51, 66, 43, 125, 176, 225, 70, 196, 83, 20, 126, 23,
            ],
            [
                71, 76, 131, 202, 95, 138, 59, 49, 94, 202, 251, 97, 95, 5, 231, 110, 204, 68, 187,
                46, 196, 72, 235, 222, 15, 163, 157, 228, 47, 238, 143, 14,
            ],
            [
                150, 205, 134, 58, 173, 49, 209, 29, 184, 196, 133, 72, 228, 96, 146, 255, 193,
                119, 112, 74, 79, 133, 48, 115, 140, 171, 120, 99, 94, 198, 196, 46,
            ],
            [
                155, 187, 64, 236, 27, 143, 53, 188, 101, 103, 181, 112, 235, 66, 65, 220, 102, 58,
                95, 235, 188, 236, 212, 23, 241, 53, 166, 236, 65, 61, 113, 0,
            ],
            [
                190, 143, 154, 142, 9, 6, 128, 201, 114, 205, 193, 129, 192, 19, 133, 46, 229, 194,
                101, 172, 221, 101, 124, 212, 155, 36, 239, 186, 224, 11, 57, 20,
            ],
        ];

        for (i, expected_hash) in expected_hashes.into_iter().enumerate() {
            let inputs = vec![&input[..]; i + 1];
            let hash = hashv(Parameters::Bn254X5, Endianness::LittleEndian, &inputs).unwrap();
            assert_eq!(hash.to_bytes(), expected_hash);
        }
    }
}
