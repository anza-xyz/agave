//! Account state conversions for protobuf support.

#![cfg(feature = "fuzz")]

use {
    super::{error::FixtureError, proto::AcctState as ProtoAccount},
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

        let pubkey = Pubkey::try_from(address.as_slice()).map_err(|_| FixtureError::InvalidPubkeyBytes(address.clone()))?;
        let owner = Pubkey::try_from(owner.as_slice()).map_err(|_| FixtureError::InvalidPubkeyBytes(owner.clone()))?;

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
