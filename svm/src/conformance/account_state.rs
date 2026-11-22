//! Account state conversions for protobuf support.

use {
    crate::conformance::fd_hash::fd_hash_or_zero,
    protosol::protos::{AcctState as ProtoAccount, acct_state::DataRepr},
    solana_account::Account,
    solana_pubkey::Pubkey,
};

// Default `rent_epoch` field value for all accounts.
const RENT_EXEMPT_RENT_EPOCH: u64 = u64::MAX;

/// Convert a protobuf account state into a `(Pubkey, Account)` pair.
pub fn account_from_proto(value: ProtoAccount) -> (Pubkey, Account) {
    let ProtoAccount {
        address,
        owner,
        lamports,
        data_repr,
        executable,
    } = value;

    let pubkey = Pubkey::try_from(address).expect("invalid account address bytes");
    let owner = Pubkey::try_from(owner).expect("invalid account owner bytes");
    let data = match data_repr {
        Some(DataRepr::Data(data)) => data,
        Some(DataRepr::DataHash(_)) => {
            panic!("input account carries a data hash, expected raw data")
        }
        None => Vec::new(),
    };

    (
        pubkey,
        Account {
            data,
            executable,
            lamports,
            owner,
            rent_epoch: RENT_EXEMPT_RENT_EPOCH,
        },
    )
}

/// Convert a `(Pubkey, Account)` pair into a protobuf account state for
/// effects, summarizing the account data as a hash.
pub fn account_to_proto(pubkey: &Pubkey, account: &Account) -> ProtoAccount {
    ProtoAccount {
        address: pubkey.to_bytes().to_vec(),
        owner: account.owner.to_bytes().to_vec(),
        lamports: account.lamports,
        data_repr: Some(DataRepr::DataHash(fd_hash_or_zero(&account.data))),
        executable: account.executable,
    }
}
