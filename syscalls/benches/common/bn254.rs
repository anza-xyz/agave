//! BN254 point encoding shared across the alt_bn128 bench targets.

use {
    ark_bn254::{Fq, Fr, G1Affine, G2Affine},
    ark_ec::AffineRepr,
    ark_ff::{BigInteger, PrimeField},
    solana_bn254::versioned::{
        alt_bn128_versioned_g2_addition, Endianness, VersionedG2Addition,
        ALT_BN128_G1_POINT_SIZE, ALT_BN128_G2_POINT_SIZE,
    },
    std::sync::OnceLock,
};

pub fn endian(le: bool) -> Endianness {
    if le {
        Endianness::LE
    } else {
        Endianness::BE
    }
}

pub fn fq_bytes(x: &Fq, le: bool) -> [u8; 32] {
    let mut out = [0u8; 32];
    let be = x.into_bigint().to_bytes_be();
    out[32 - be.len()..].copy_from_slice(&be);
    if le {
        out.reverse();
    }
    out
}

pub fn fr_bytes(s: &Fr, le: bool) -> [u8; 32] {
    let mut out = [0u8; 32];
    let be = s.into_bigint().to_bytes_be();
    out[32 - be.len()..].copy_from_slice(&be);
    if le {
        out.reverse();
    }
    out
}

pub fn g1_bytes(p: &G1Affine, le: bool) -> [u8; ALT_BN128_G1_POINT_SIZE] {
    let mut out = [0u8; ALT_BN128_G1_POINT_SIZE];
    out[..32].copy_from_slice(&fq_bytes(&p.x, le));
    out[32..].copy_from_slice(&fq_bytes(&p.y, le));
    out
}

pub fn g2_bytes_with(
    p: &G2Affine,
    le: bool,
    c1_first: bool,
) -> [u8; ALT_BN128_G2_POINT_SIZE] {
    let parts = if c1_first {
        [p.x.c1, p.x.c0, p.y.c1, p.y.c0]
    } else {
        [p.x.c0, p.x.c1, p.y.c0, p.y.c1]
    };
    let mut out = [0u8; ALT_BN128_G2_POINT_SIZE];
    for (i, part) in parts.iter().enumerate() {
        out[i * 32..(i + 1) * 32].copy_from_slice(&fq_bytes(part, le));
    }
    out
}

/// EIP-197 puts the imaginary component of each Fq2 first. Rather than assume
/// solana-bn254 matches, probe it once per endianness by feeding the generator
/// through G2 addition and seeing which ordering deserializes.
pub fn g2_bytes(p: &G2Affine, le: bool) -> [u8; ALT_BN128_G2_POINT_SIZE] {
    static BE_C1_FIRST: OnceLock<bool> = OnceLock::new();
    static LE_C1_FIRST: OnceLock<bool> = OnceLock::new();

    let cell = if le { &LE_C1_FIRST } else { &BE_C1_FIRST };
    let c1_first = *cell.get_or_init(|| {
        for candidate in [true, false] {
            let g = g2_bytes_with(&G2Affine::generator(), le, candidate);
            let mut input = [0u8; ALT_BN128_G2_POINT_SIZE * 2];
            input[..ALT_BN128_G2_POINT_SIZE].copy_from_slice(&g);
            input[ALT_BN128_G2_POINT_SIZE..].copy_from_slice(&g);
            if alt_bn128_versioned_g2_addition(VersionedG2Addition::V0, &input, endian(le)).is_ok()
            {
                return candidate;
            }
        }
        panic!("solana-bn254 rejected both G2 field orderings (le={le})");
    });
    g2_bytes_with(p, le, c1_first)
}
