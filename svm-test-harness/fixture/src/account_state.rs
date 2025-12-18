//! Account state conversions for protobuf support.

#![cfg(feature = "fuzz")]

use {
    crate::{error::FixtureError, proto::AcctState as ProtoAccount},
    solana_account::Account,
    solana_pubkey::Pubkey,
};

// Default `rent_epoch` field value for all accounts.
const RENT_EXEMPT_RENT_EPOCH: u64 = u64::MAX;

impl TryFrom<ProtoAccount> for (Pubkey, Account) {
    type Error = FixtureError;

    fn try_from(value: ProtoAccount) -> Result<Self, Self::Error> {
        let ProtoAccount {
            address,
            owner,
            lamports,
            data,
            executable,
            ..
        } = value;

        let pubkey = Pubkey::try_from(address).map_err(FixtureError::InvalidPubkeyBytes)?;
        let owner = Pubkey::try_from(owner).map_err(FixtureError::InvalidPubkeyBytes)?;

        Ok((
            pubkey,
            Account {
                data,
                executable,
                lamports,
                owner,
                rent_epoch: RENT_EXEMPT_RENT_EPOCH,
            },
        ))
    }
}

impl From<(Pubkey, Account)> for ProtoAccount {
    fn from(value: (Pubkey, Account)) -> Self {
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
}
