#![cfg(not(target_os = "solana"))]
#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]

pub use crate::{
    addition::{bls12_381_g1_addition, bls12_381_g2_addition},
    decompression::{bls12_381_g1_decompress, bls12_381_g2_decompress},
    multiplication::{bls12_381_g1_multiplication, bls12_381_g2_multiplication},
    pairing::bls12_381_pairing_map,
    subtraction::{bls12_381_g1_subtraction, bls12_381_g2_subtraction},
    validation::{bls12_381_g1_point_validation, bls12_381_g2_point_validation},
};

pub(crate) mod addition;
pub(crate) mod decompression;
pub(crate) mod multiplication;
pub(crate) mod pairing;
pub(crate) mod subtraction;
#[cfg(test)]
pub(crate) mod test_vectors;
pub(crate) mod validation;

pub enum Endianness {
    BE,
    LE,
}

pub enum Version {
    /// SIMD-388: BLS12-381 Elliptic Curve Syscalls
    V0,
}

fn reverse_48_byte_chunks(bytes: &mut [u8]) {
    for chunk in bytes.chunks_mut(48) {
        chunk.reverse();
    }
}

fn swap_g2_c0_c1(bytes: &mut [u8]) {
    for fq2_chunk in bytes.chunks_exact_mut(96) {
        let (c0, c1) = fq2_chunk.split_at_mut(48);
        c0.swap_with_slice(c1);
    }
}
