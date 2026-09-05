//! BLS12-381 fixtures.
//!
//! The worst-case vectors below are copied verbatim from
//! `curves/bls12-381/benches/test_vectors.rs` in this workspace. They were
//! chosen by the implementers to maximise the runtime of each operation, which
//! is exactly what a pricing study needs: a flat price has to cover the slowest
//! valid input, not an average one.
//!
//! Byte layouts, per `encoding.rs` of that crate:
//!   BE  - Zcash/IETF standard, fed straight to blstrs.
//!   LE  - each 48-byte Fq reversed; for G2, the c0/c1 halves of each Fq2 also
//!         swapped.
//!   G1 point 96 B, G2 point 192 B, G1 compressed 48 B, G2 compressed 96 B,
//!   scalar 32 B, Gt element 576 B.

#![allow(dead_code)]

use {
    hex_literal::hex,
    solana_bls12_381_syscall::{
        bls12_381_g1_decompress, bls12_381_g1_point_validation, bls12_381_g2_decompress,
        bls12_381_g2_point_validation, Endianness, PodG1Compressed, PodG1Point, PodG2Compressed,
        PodG2Point, PodScalar, Version,
    },
    std::mem::size_of,
};

pub const G1_POINT_SIZE: usize = 96;
pub const G2_POINT_SIZE: usize = 192;
pub const G1_COMPRESSED_SIZE: usize = 48;
pub const G2_COMPRESSED_SIZE: usize = 96;
pub const SCALAR_SIZE: usize = 32;
pub const GT_SIZE: usize = 576;

pub fn endianness(le: bool) -> Endianness {
    if le {
        Endianness::LE
    } else {
        Endianness::BE
    }
}

// ---------------------------------------------------------------- vectors

const INPUT_BE_G1_ADD_WORST_CASE: &[u8] = &hex!(
    "0408fcfce79d55404c279812a8bdfc02525f215e70f717cbf9f97c305f6845d6661f3a5a46c513a24cf706410e878a2c12aca07fea5564f2bdcf28ba9f12f2d88c85ae28b1a2c33f965cad4a1af3c796ee6d33fc7e322cc8954b223f53febd330eea76a913985d932eea073ebf8e86d21c6e4d4c8bfbef927c006abe860b51282629eea7b197385deb9057e603aa9c6e1104b5ffaabfe1d55de81f8acc668c913479611296668293535b602ab8cf24797522ad15ca4a2e8f898855d94d0a58fe"
);

const INPUT_LE_G1_ADD_WORST_CASE: &[u8] = &hex!(
    "2c8a870e4106f74ca213c5465a3a1f66d645685f307cf9f9cb17f7705e215f5202fcbda81298274c40559de7fcfc080433bdfe533f224b95c82c327efc336dee96c7f31a4aad5c963fc3a2b128ae858cd8f2129fba28cfbdf26455ea7fa0ac126e9caa03e65790eb5d3897b1a7ee292628510b86be6a007c92effb8b4c4d6e1cd2868ebf3e07ea2e935d9813a976ea0efe580a4dd95588898f2e4aca15ad22757924cfb82a605b539382669612617934918c66cc8a1fe85dd5e1bfaaffb50411"
);

const INPUT_BE_G1_SUB_WORST_CASE: &[u8] = &hex!(
    "0408fcfce79d55404c279812a8bdfc02525f215e70f717cbf9f97c305f6845d6661f3a5a46c513a24cf706410e878a2c12aca07fea5564f2bdcf28ba9f12f2d88c85ae28b1a2c33f965cad4a1af3c796ee6d33fc7e322cc8954b223f53febd330eea76a913985d932eea073ebf8e86d21c6e4d4c8bfbef927c006abe860b51282629eea7b197385deb9057e603aa9c6e1104b5ffaabfe1d55de81f8acc668c913479611296668293535b602ab8cf24797522ad15ca4a2e8f898855d94d0a58fe"
);

const INPUT_LE_G1_SUB_WORST_CASE: &[u8] = &hex!(
    "2c8a870e4106f74ca213c5465a3a1f66d645685f307cf9f9cb17f7705e215f5202fcbda81298274c40559de7fcfc080433bdfe533f224b95c82c327efc336dee96c7f31a4aad5c963fc3a2b128ae858cd8f2129fba28cfbdf26455ea7fa0ac126e9caa03e65790eb5d3897b1a7ee292628510b86be6a007c92effb8b4c4d6e1cd2868ebf3e07ea2e935d9813a976ea0efe580a4dd95588898f2e4aca15ad22757924cfb82a605b539382669612617934918c66cc8a1fe85dd5e1bfaaffb50411"
);

const INPUT_BE_G1_MUL_WORST_CASE: &[u8] = &hex!(
    "0408fcfce79d55404c279812a8bdfc02525f215e70f717cbf9f97c305f6845d6661f3a5a46c513a24cf706410e878a2c12aca07fea5564f2bdcf28ba9f12f2d88c85ae28b1a2c33f965cad4a1af3c796ee6d33fc7e322cc8954b223f53febd3373eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000"
);

const INPUT_LE_G1_MUL_WORST_CASE: &[u8] = &hex!(
    "2c8a870e4106f74ca213c5465a3a1f66d645685f307cf9f9cb17f7705e215f5202fcbda81298274c40559de7fcfc080433bdfe533f224b95c82c327efc336dee96c7f31a4aad5c963fc3a2b128ae858cd8f2129fba28cfbdf26455ea7fa0ac1200000000fffffffffe5bfeff02a4bd5305d8a10908d83933487d9d2953a7ed73"
);

const INPUT_BE_G1_DECOMPRESS_WORST_CASE: &[u8] = &hex!(
    "a408fcfce79d55404c279812a8bdfc02525f215e70f717cbf9f97c305f6845d6661f3a5a46c513a24cf706410e878a2c"
);

const INPUT_LE_G1_DECOMPRESS_WORST_CASE: &[u8] = &hex!(
    "2c8a870e4106f74ca213c5465a3a1f66d645685f307cf9f9cb17f7705e215f5202fcbda81298274c40559de7fcfc08a4"
);

const INPUT_BE_G1_VALIDATE_WORST_CASE: &[u8] = &hex!(
    "0408fcfce79d55404c279812a8bdfc02525f215e70f717cbf9f97c305f6845d6661f3a5a46c513a24cf706410e878a2c12aca07fea5564f2bdcf28ba9f12f2d88c85ae28b1a2c33f965cad4a1af3c796ee6d33fc7e322cc8954b223f53febd33"
);

const INPUT_LE_G1_VALIDATE_WORST_CASE: &[u8] = &hex!(
    "2c8a870e4106f74ca213c5465a3a1f66d645685f307cf9f9cb17f7705e215f5202fcbda81298274c40559de7fcfc080433bdfe533f224b95c82c327efc336dee96c7f31a4aad5c963fc3a2b128ae858cd8f2129fba28cfbdf26455ea7fa0ac12"
);

const INPUT_BE_G2_ADD_WORST_CASE: &[u8] = &hex!(
    "03bd018dadd58e94c755ae7e3ca3a2359582282475a85ad70d94d051298f2cbcc86d5440032e927e432f2c7b2f28dcb90169a750864019b9aaaf37c7e5a005a525b70e3e52d04ea58874a79af8b835b68eba547558e9627d6e2051f080b560d307a3d35f55ddb9732ee23283533f7aed5b6b702f932ef3cad72cc79d31cb953700d0bd3fe12230c989e9d199ef580989119af50a55aeedc4b0a7972bbcb2d4a1da1a53f469b49f89458271bb84d341065afca90fae7b9cabf2245622cc2e08730377679657a9200ab2be940cb280e114de3ec7ba85bd35f33b7abb97844ae37d4dc01f39cbb981e6ca64b561c42e28b8074753c2c1ae0f67b96f5eccee6db19581a8d8d72d736929e1126970d0dfb62c64022f37b10fb65328ceb8282d56afc10457daadf9144fb306599b420c3787b5d388ea5eb48d997873a5c0316a71564132fa3c08122f85948c926b98e5074a2f048865ccd58cf6e3afc55b59b382ff03df30d63fa09f1622f3f62663e0ff5203c370fb2019e5caa9eeba9d29b17470ed"
);

const INPUT_LE_G2_ADD_WORST_CASE: &[u8] = &hex!(
    "d360b580f051206e7d62e9587554ba8eb635b8f89aa77488a54ed0523e0eb725a505a0e5c737afaab919408650a76901b9dc282f7b2c2f437e922e0340546dc8bc2c8f2951d0940dd75aa8752428829535a2a33c7eae55c7948ed5ad8d01bd0373082ecc225624f2ab9c7bae0fa9fc5a0641d384bb718245899fb469f4531adaa1d4b2bc2b97a7b0c4edae550af59a11890958ef99d1e989c93022e13fbdd0003795cb319dc72cd7caf32e932f706b5bed7a3f538332e22e73b9dd555fd3a307c1af562d28b8ce2853b60fb1372f02642cb6dfd0706912e12969732dd7d8a88195b16deecc5e6fb9670faec1c2534707b8282ec461b564cae681b9cb391fc04d7de34a8497bb7a3bf335bd85bac73ede14e180b20c94beb20a20a95796677703ed7074b1299dbaeea9cae51920fb70c30352ffe06326f6f322169fa03fd630df03ff82b3595bc5afe3f68cd5cc6588042f4a07e5986b928c94852f12083cfa324156716a31c0a57378998db45eea88d3b587370c429b5906b34f14f9adda5704"
);

const INPUT_BE_G2_SUB_WORST_CASE: &[u8] = &hex!(
    "03bd018dadd58e94c755ae7e3ca3a2359582282475a85ad70d94d051298f2cbcc86d5440032e927e432f2c7b2f28dcb90169a750864019b9aaaf37c7e5a005a525b70e3e52d04ea58874a79af8b835b68eba547558e9627d6e2051f080b560d307a3d35f55ddb9732ee23283533f7aed5b6b702f932ef3cad72cc79d31cb953700d0bd3fe12230c989e9d199ef580989119af50a55aeedc4b0a7972bbcb2d4a1da1a53f469b49f89458271bb84d341065afca90fae7b9cabf2245622cc2e08730377679657a9200ab2be940cb280e114de3ec7ba85bd35f33b7abb97844ae37d4dc01f39cbb981e6ca64b561c42e28b8074753c2c1ae0f67b96f5eccee6db19581a8d8d72d736929e1126970d0dfb62c64022f37b10fb65328ceb8282d56afc10457daadf9144fb306599b420c3787b5d388ea5eb48d997873a5c0316a71564132fa3c08122f85948c926b98e5074a2f048865ccd58cf6e3afc55b59b382ff03df30d63fa09f1622f3f62663e0ff5203c370fb2019e5caa9eeba9d29b17470ed"
);

const INPUT_LE_G2_SUB_WORST_CASE: &[u8] = &hex!(
    "d360b580f051206e7d62e9587554ba8eb635b8f89aa77488a54ed0523e0eb725a505a0e5c737afaab919408650a76901b9dc282f7b2c2f437e922e0340546dc8bc2c8f2951d0940dd75aa8752428829535a2a33c7eae55c7948ed5ad8d01bd0373082ecc225624f2ab9c7bae0fa9fc5a0641d384bb718245899fb469f4531adaa1d4b2bc2b97a7b0c4edae550af59a11890958ef99d1e989c93022e13fbdd0003795cb319dc72cd7caf32e932f706b5bed7a3f538332e22e73b9dd555fd3a307c1af562d28b8ce2853b60fb1372f02642cb6dfd0706912e12969732dd7d8a88195b16deecc5e6fb9670faec1c2534707b8282ec461b564cae681b9cb391fc04d7de34a8497bb7a3bf335bd85bac73ede14e180b20c94beb20a20a95796677703ed7074b1299dbaeea9cae51920fb70c30352ffe06326f6f322169fa03fd630df03ff82b3595bc5afe3f68cd5cc6588042f4a07e5986b928c94852f12083cfa324156716a31c0a57378998db45eea88d3b587370c429b5906b34f14f9adda5704"
);

const INPUT_BE_G2_MUL_WORST_CASE: &[u8] = &hex!(
    "03bd018dadd58e94c755ae7e3ca3a2359582282475a85ad70d94d051298f2cbcc86d5440032e927e432f2c7b2f28dcb90169a750864019b9aaaf37c7e5a005a525b70e3e52d04ea58874a79af8b835b68eba547558e9627d6e2051f080b560d307a3d35f55ddb9732ee23283533f7aed5b6b702f932ef3cad72cc79d31cb953700d0bd3fe12230c989e9d199ef580989119af50a55aeedc4b0a7972bbcb2d4a1da1a53f469b49f89458271bb84d341065afca90fae7b9cabf2245622cc2e087373eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000"
);

const INPUT_LE_G2_MUL_WORST_CASE: &[u8] = &hex!(
    "d360b580f051206e7d62e9587554ba8eb635b8f89aa77488a54ed0523e0eb725a505a0e5c737afaab919408650a76901b9dc282f7b2c2f437e922e0340546dc8bc2c8f2951d0940dd75aa8752428829535a2a33c7eae55c7948ed5ad8d01bd0373082ecc225624f2ab9c7bae0fa9fc5a0641d384bb718245899fb469f4531adaa1d4b2bc2b97a7b0c4edae550af59a11890958ef99d1e989c93022e13fbdd0003795cb319dc72cd7caf32e932f706b5bed7a3f538332e22e73b9dd555fd3a30700000000fffffffffe5bfeff02a4bd5305d8a10908d83933487d9d2953a7ed73"
);

const INPUT_BE_G2_DECOMPRESS_WORST_CASE: &[u8] = &hex!(
    "83bd018dadd58e94c755ae7e3ca3a2359582282475a85ad70d94d051298f2cbcc86d5440032e927e432f2c7b2f28dcb90169a750864019b9aaaf37c7e5a005a525b70e3e52d04ea58874a79af8b835b68eba547558e9627d6e2051f080b560d3"
);

const INPUT_LE_G2_DECOMPRESS_WORST_CASE: &[u8] = &hex!(
    "d360b580f051206e7d62e9587554ba8eb635b8f89aa77488a54ed0523e0eb725a505a0e5c737afaab919408650a76901b9dc282f7b2c2f437e922e0340546dc8bc2c8f2951d0940dd75aa8752428829535a2a33c7eae55c7948ed5ad8d01bd83"
);

const INPUT_BE_G2_VALIDATE_WORST_CASE: &[u8] = &hex!(
    "02298dc5b07ca647c4f87741130b540b7c2410f40bc644ffb5de3f85c6b116f09323fdd675774b9507e1898a5e32ae990a01305b6d86169d57048a21f8144f4a7706eadc687e81b2cd47bea2152ca6c467be4fdb4a7ce8038deede79b40e7d9a0e83c50a4e6f3cfb1333cea38add85a7ca08a351ca2e208f9282e6a224081fa16343263da17e1f37132209f399c5b915140a85dd41147c0346f6bae0719c2ad94a1893901e99ea3a493c57e1827bb65db214b403de77e52d050b17f46f8fe2f1"
);

const INPUT_LE_G2_VALIDATE_WORST_CASE: &[u8] = &hex!(
    "9a7d0eb479deee8d03e87c4adb4fbe67c4a62c15a2be47cdb2817e68dcea06774a4f14f8218a04579d16866d5b30010a99ae325e8a89e107954b7775d6fd2393f016b1c6853fdeb5ff44c60bf410247c0b540b134177f8c447a67cb0c58d2902f1e28f6ff4170b052de577de03b414b25db67b82e1573c493aea991e9093184ad92a9c71e0baf646037c1441dd850a1415b9c599f3092213371f7ea13d264363a11f0824a2e682928f202eca51a308caa785dd8aa3ce3313fb3c6f4e0ac5830e"
);

const INPUT_BE_PAIRING_WORST_CASE: &[u8] = &hex!(
    "023d85d0663f73ae66687d68dc64927752a9ca27de3b1118ba0aaeaa4a6ecb03907530f12c79f8bf2859b6c286f9a93315e8ef5d9d9d224906d22a13683e4419e2463b728ade94b1569e4210c0f6d50765062d57c92027148db03e245d52a57f19dbe2d5c1b709b81868241ce0a838a2ad95c5f3f85f9f619bc5f542f8ec518af5d15ee6b0bcabb81f926ea9b314afdf13df5d30b648ce7429bfc057710c5e936fcfe5330188512344fad45797d34dd278c0ee96353cafec4435eedd5bc0c08508fa6979ce323b0172f90cb94ed927011860bc41e3be0ff6db84427cc750432281f3903f57b291faec6ca7fc438eeb1012ec3ae72921a2fd455262e7e26f3c20f47c2f83a4fffce5d2a562b9e9f35cbb5a689217fa6a18c7e2337f59d4be0876"
);

const INPUT_LE_PAIRING_WORST_CASE: &[u8] = &hex!(
    "33a9f986c2b65928bff8792cf130759003cb6e4aaaae0aba18113bde27caa952779264dc687d6866ae733f66d0853d027fa5525d243eb08d142720c9572d066507d5f6c010429e56b194de8a723b46e219443e68132ad20649229d9d5defe81585c0c05bddee3544ecaf3c3596eec078d24dd39757d4fa442351880133e5cf6f935e0c7157c0bf2974ce48b6305ddf13dfaf14b3a96e921fb8abbcb0e65ed1f58a51ecf842f5c59b619f5ff8f3c595ada238a8e01c246818b809b7c1d5e2db197608bed4597f33e2c7186afa1792685abb5cf3e9b962a5d2e5fcffa4832f7cf4203c6fe2e7625245fda22129e73aec1210eb8e43fca76cecfa91b2573f90f381224350c77c4284dbf60fbee341bc60180127d94eb90cf972013b32ce7969fa08"
);

// ---------------------------------------------------------------- constructors

fn g1_point(bytes: &[u8]) -> PodG1Point {
    assert_eq!(size_of::<PodG1Point>(), G1_POINT_SIZE);
    PodG1Point(bytes.try_into().expect("G1 point must be 96 bytes"))
}

fn g2_point(bytes: &[u8]) -> PodG2Point {
    assert_eq!(size_of::<PodG2Point>(), G2_POINT_SIZE);
    PodG2Point(bytes.try_into().expect("G2 point must be 192 bytes"))
}

fn scalar(bytes: &[u8]) -> PodScalar {
    PodScalar(bytes.try_into().expect("scalar must be 32 bytes"))
}

fn pick(le: bool, be_vec: &'static [u8], le_vec: &'static [u8]) -> &'static [u8] {
    if le {
        le_vec
    } else {
        be_vec
    }
}

// ---------------------------------------------------------------- G1

/// Two G1 points for `GROUP_OP_ADD`. Vector layout is `left || right`.
pub fn g1_add_inputs(le: bool) -> (PodG1Point, PodG1Point) {
    let v = pick(le, INPUT_BE_G1_ADD_WORST_CASE, INPUT_LE_G1_ADD_WORST_CASE);
    let (l, r) = v.split_at(G1_POINT_SIZE);
    (g1_point(l), g1_point(r))
}

/// Two G1 points for `GROUP_OP_SUB`.
pub fn g1_sub_inputs(le: bool) -> (PodG1Point, PodG1Point) {
    let v = pick(le, INPUT_BE_G1_SUB_WORST_CASE, INPUT_LE_G1_SUB_WORST_CASE);
    let (l, r) = v.split_at(G1_POINT_SIZE);
    (g1_point(l), g1_point(r))
}

/// Returned in **syscall argument order**: scalar first, point second.
///
/// The stored vector is `point || scalar`, matching the library function
/// signature, but `SyscallCurveGroupOps` reads the scalar from
/// `left_input_addr` and the point from `right_input_addr`. The swap happens
/// here so no caller has to remember it.
///
/// The scalar is `r - 1`, the largest canonical value, so the multiplication
/// runs its full ladder.
pub fn g1_mul_inputs(le: bool) -> (PodScalar, PodG1Point) {
    let v = pick(le, INPUT_BE_G1_MUL_WORST_CASE, INPUT_LE_G1_MUL_WORST_CASE);
    let (point, s) = v.split_at(G1_POINT_SIZE);
    (scalar(s), g1_point(point))
}

pub fn g1_decompress_input(le: bool) -> PodG1Compressed {
    let v = pick(
        le,
        INPUT_BE_G1_DECOMPRESS_WORST_CASE,
        INPUT_LE_G1_DECOMPRESS_WORST_CASE,
    );
    let pod = PodG1Compressed(v.try_into().expect("G1 compressed must be 48 bytes"));
    assert!(
        bls12_381_g1_decompress(Version::V0, &pod, endianness(le)).is_some(),
        "G1 decompress vector rejected (le={le})"
    );
    pod
}

pub fn g1_validate_input(le: bool) -> PodG1Point {
    let v = pick(
        le,
        INPUT_BE_G1_VALIDATE_WORST_CASE,
        INPUT_LE_G1_VALIDATE_WORST_CASE,
    );
    let pod = g1_point(v);
    assert!(
        bls12_381_g1_point_validation(Version::V0, &pod, endianness(le)),
        "G1 validate vector rejected (le={le})"
    );
    pod
}

// ---------------------------------------------------------------- G2

pub fn g2_add_inputs(le: bool) -> (PodG2Point, PodG2Point) {
    let v = pick(le, INPUT_BE_G2_ADD_WORST_CASE, INPUT_LE_G2_ADD_WORST_CASE);
    let (l, r) = v.split_at(G2_POINT_SIZE);
    (g2_point(l), g2_point(r))
}

pub fn g2_sub_inputs(le: bool) -> (PodG2Point, PodG2Point) {
    let v = pick(le, INPUT_BE_G2_SUB_WORST_CASE, INPUT_LE_G2_SUB_WORST_CASE);
    let (l, r) = v.split_at(G2_POINT_SIZE);
    (g2_point(l), g2_point(r))
}

/// Syscall argument order: scalar first, point second. See `g1_mul_inputs`.
pub fn g2_mul_inputs(le: bool) -> (PodScalar, PodG2Point) {
    let v = pick(le, INPUT_BE_G2_MUL_WORST_CASE, INPUT_LE_G2_MUL_WORST_CASE);
    let (point, s) = v.split_at(G2_POINT_SIZE);
    (scalar(s), g2_point(point))
}

pub fn g2_decompress_input(le: bool) -> PodG2Compressed {
    let v = pick(
        le,
        INPUT_BE_G2_DECOMPRESS_WORST_CASE,
        INPUT_LE_G2_DECOMPRESS_WORST_CASE,
    );
    let pod = PodG2Compressed(v.try_into().expect("G2 compressed must be 96 bytes"));
    assert!(
        bls12_381_g2_decompress(Version::V0, &pod, endianness(le)).is_some(),
        "G2 decompress vector rejected (le={le})"
    );
    pod
}

pub fn g2_validate_input(le: bool) -> PodG2Point {
    let v = pick(
        le,
        INPUT_BE_G2_VALIDATE_WORST_CASE,
        INPUT_LE_G2_VALIDATE_WORST_CASE,
    );
    let pod = g2_point(v);
    assert!(
        bls12_381_g2_point_validation(Version::V0, &pod, endianness(le)),
        "G2 validate vector rejected (le={le})"
    );
    pod
}

// ---------------------------------------------------------------- pairing

/// One `(G1, G2)` pair. The stored vector is `G1 (96) || G2 (192)`.
pub fn pairing_pair(le: bool) -> (PodG1Point, PodG2Point) {
    let v = pick(le, INPUT_BE_PAIRING_WORST_CASE, INPUT_LE_PAIRING_WORST_CASE);
    let (g1, g2) = v.split_at(G1_POINT_SIZE);
    (g1_point(g1), g2_point(g2))
}

/// `n` copies of the worst-case pair, as two parallel arrays.
///
/// Repeating one pair is what the curve crate's own bench does. The Miller loop
/// cost per pair does not depend on the point values, so this measures the
/// per-pair slope correctly.
pub fn pairing_batch(n: usize, le: bool) -> (Vec<PodG1Point>, Vec<PodG2Point>) {
    let (g1, g2) = pairing_pair(le);
    (vec![g1; n], vec![g2; n])
}
