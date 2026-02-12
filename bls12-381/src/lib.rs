#![cfg(feature = "agave-unstable-api")]

//! Implementation of the BLS12-381 Elliptic Curve operations for Solana syscalls.
//!
//! This crate provides the native implementations for the syscalls defined in
//! [SIMD-0388](https://github.com/solana-foundation/solana-improvement-documents/pull/388).
//!
//! # Supported Operations
//!
//! - **Group Operations** (G1 & G2): Addition, Subtraction, Scalar Multiplication.
//! - **Pairing**: Multi-scalar multiplication (Miller loop + Final Exponentiation).
//! - **Validation**: Subgroup and on-curve checks.
//! - **Decompression**: Converting compressed byte representations to affine points.
//!
//! # Encoding and Endianness
//!
//! The operations support two encoding formats defined in [`Endianness`]:
//! 1. **Big-Endian (BE)**: Follows the [Zcash BLS12-381 specification][zcash] and
//!    [IETF draft][ietf].
//! 2. **Little-Endian (LE)**: This mirrors the Zcash structure but utilizes little-endian
//!    byte ordering for base field elements.
//!
//! [zcash]: https://github.com/zkcrypto/pairing/tree/master/src/bls12_381#serialization
//! [ietf]: https://www.ietf.org/archive/id/draft-irtf-cfrg-pairing-friendly-curves-11.html#name-bls-curves-for-the-128-bit-

pub use crate::{
    addition::{bls12_381_g1_addition, bls12_381_g2_addition},
    decompression::{bls12_381_g1_decompress, bls12_381_g2_decompress},
    encoding::{
        Endianness, PodG1Compressed, PodG1Point, PodG2Compressed, PodG2Point, PodGtElement,
        PodScalar,
    },
    multiplication::{bls12_381_g1_multiplication, bls12_381_g2_multiplication},
    pairing::bls12_381_pairing_map,
    subtraction::{bls12_381_g1_subtraction, bls12_381_g2_subtraction},
    validation::{bls12_381_g1_point_validation, bls12_381_g2_point_validation},
};

pub(crate) mod addition;
pub(crate) mod decompression;
pub(crate) mod encoding;
pub(crate) mod multiplication;
pub(crate) mod pairing;
pub(crate) mod subtraction;
#[cfg(test)]
pub(crate) mod test_vectors;
pub(crate) mod validation;

/// Version identifier for the syscall interface.
///
/// Modifying the behavior of syscalls is a consensus-critical operation.
/// Any change in behavior across the network without proper coordination will result
/// in a network fork.
///
/// If a change to the syscall behavior is required:
/// 1. The change must first be proposed and approved via a
///    [Solana Improvement Document (SIMD)](https://github.com/solana-foundation/solana-improvement-documents).
/// 2. Once the SIMD is accepted, a new variant should be added to this enum (e.g., `V1`).
/// 3. The implementation of every function in this crate must be scoped to handle the
///    specific logic for each version variant.
pub enum Version {
    /// SIMD-388: BLS12-381 Elliptic Curve Syscalls
    V0,
}
