//! Account state conversions for protobuf support.

use {protosol::protos::AcctState as ProtoAccount, solana_account::Account, solana_pubkey::Pubkey};

// Default `rent_epoch` field value for all accounts.
const RENT_EXEMPT_RENT_EPOCH: u64 = u64::MAX;

/// Convert a protobuf account state into a `(Pubkey, Account)` pair.
pub fn account_from_proto(value: ProtoAccount) -> (Pubkey, Account) {
    let ProtoAccount {
        address,
        owner,
        lamports,
        data,
        executable,
        ..
    } = value;

    let pubkey = Pubkey::try_from(address).expect("invalid account address bytes");
    let owner = Pubkey::try_from(owner).expect("invalid account owner bytes");

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

/// Convert a `(Pubkey, Account)` pair into a protobuf account state.
pub fn account_to_proto(value: (Pubkey, Account)) -> ProtoAccount {
    let Account {
        lamports,
        data,
        owner,
        executable,
        ..
    } = value.1;

    ProtoAccount {
        address: value.0.to_bytes().to_vec(),
        owner: owner.to_bytes().to_vec(),
        lamports,
        data,
        executable,
    }
}
