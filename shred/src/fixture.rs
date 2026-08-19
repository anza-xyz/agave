//! Test vectors: real shreds captured off the wire, for exercising the cascade without a shredder.
//!
//! Gated behind `dev-context-only-utils`, so this data is never compiled into a validator.

use {bytes::Bytes, solana_pubkey::Pubkey, std::str::FromStr};

/// A real 1203-byte Merkle data shred, captured from `solana-ledger`'s wire-format test.
///
/// Slot 142076266, signed by [`FIXTURE_LEADER_BASE58`].
pub const DATA_SHRED_BASE58: &str = concat!(
    "aX2ovF3sZRfd6HyqMow9kkrtL3MyJd52m7gvuSjcvA4qayXZcVPhjURcs4JX",
    "86YQM8wVrKXqdneqdEUJwBWhFrxSkegDSov6NQoK89SzZi9auEXHHr35dmN4",
    "zQbxuNdPjKM2K7b7WKRWaHyoMKQfG9jDbJGcWqwVkAxBmUXZQKryHvAqyNdB",
    "uRTdWrMtPKDiJWhqVWTmokpyGNceL7mqVr3VrLby6dEuiEUCBHCkhbsXBjfp",
    "FZk4yRoSKosb7BViTWWdtpWd7NrbDSiE97sBppEU1nWTPaVQh3bu91x8dEoY",
    "k696k532MxnhRLcKeL4XzG6P2HzypAckJdXiRJDn5E3woA8aiPojqdN9Scth",
    "J8yXq1h4HhvzTRWkRxRBpJL8HEYPBcshwuMLDZ9iBsWSFZLmj5v1xH3kDnMu",
    "NYJg6Dau6PKHnZyD15tTyFtFtMaXaBc35RqYhsM7s8JuQ9tJ1UfFwdkhHa1w",
    "drmTWGcvq9DDmALuTtejH1ccoW43GiYSs1TmByJWjRtupvLzMRifZZ7meaGb",
    "UBgHUkA6t1VN3akoZ9BhdX561KpFGABxTU4NxyFqztEy1EB5EJYtTHwtbJQb",
    "1NmNMwKFkazXkn1ouKK6drH5y19roH3mMo2JykapbvzYPDBSXUwKQWe1RqSv",
    "ogapwPxm1EzSRDeXNDP6EYUJJjjTAnckNatpT5UZDz4EhpaSbUzd9b5ztqsd",
    "Pp9HxeBTm412GopAXKN5iSXSPS2WvrEdnANFD7tRV3a6PM2SfwpF6eFM5J7x",
    "XGJSoPm5TWJSPBMbxttxVFUETSRrBubEsd24aymYZZePJtHr7Q8S1deygcyX",
    "H5WhhYAmR23hNPv3nUUHe8iwJfaFg73Ncjr8fQBVjwePEy9JKT5jNG5sm87q",
    "e2RrHEWEwkNKnNgUknoVMbL7y3wmGFpP8VoKTgP51EjMDz7JTxnVsZeRsSp2",
    "9STteGKbq4iwiC5EmMS5K86CAJ86FYt1kXXHJBSw4D79wAMgxRDDycp5Pgdo",
    "wdLxAbwySgpmwdfnxnSD4hY8mo4jLGWokP1mGdgjnPmtMbzndiQCLPjpUcbZ",
    "oVc6SQrTDCufupkJhy1ewo64yA1db6T2TASTWSHJkjzaWt7QtFfnBo8WoXQr",
    "NKw5pyKAQsmP7n6r1SVD7tASfcZAjfaFHxkVvMpKwTQFdy9WHxREeCPK3yeN",
    "7ACT75RgRuRT1shC1PRCuAu4EFGnBmr3nWuDrYNCG5WrWuW6RRoMyB3YaXqj",
    "YMXRUVuwb5h2PBP9euBb96Ntung8ihWXa2mbKMYMtmaoYCDhYYrFYszYfdgQ",
    "H68JYzAXZvjFH1SxCETfiXAWGD1aYDa33rXZLcLVx637igoydr77qmzo5Yoz",
    "RQnuXUiJ19PScLWic8jWeVmQ6Mm7BLoGhVPyYbJBeyX5HRwh8CNeLK2ekmhF",
    "z9MypB1rM2PXUfcnr2MXS9WRK8bhsy47awNdApPdN3RxmuyPLnvmN6FsG5fU",
    "NqF8rsz9KUiJh9C4ziYf6NSZvVG2c1KFsQRyFrSBzyjqqxBrH1xereV9YNr1",
    "gNamFjhZTncpGcPQf9oAoA4LQeSAZXR1dMtfktCs1fFWVbA67FdQ1GrpZVGT",
    "sZCbuw7Tspns8WoL158AdS7",
);

/// Base58 of the leader that signed [`DATA_SHRED_BASE58`].
///
/// The shred was produced by a keypair seeded from `ChaChaRng::from_seed([1u8; 32])`, matching
/// `solana-ledger`'s `test_serde_compat_shred_data`.
pub const FIXTURE_LEADER_BASE58: &str = "6Ciokjck2UiKvBgMkgvu2jq6FA4kN4Wr2PHaF4kYHBBD";

/// Slot of [`DATA_SHRED_BASE58`].
pub const FIXTURE_SLOT: u64 = 142_076_266;

/// Decodes [`DATA_SHRED_BASE58`] into the bytes a parser would see on the wire.
pub fn data_shred() -> Bytes {
    Bytes::from(
        bs58::decode(DATA_SHRED_BASE58)
            .into_vec()
            .expect("the fixture is valid base58"),
    )
}

/// The leader that signed [`DATA_SHRED_BASE58`].
pub fn leader() -> Pubkey {
    Pubkey::from_str(FIXTURE_LEADER_BASE58).expect("the fixture leader is a valid pubkey")
}
