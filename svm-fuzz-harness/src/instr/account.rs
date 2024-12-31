//! Account state.

use {
    crate::{error::HarnessError, proto::AcctState as ProtoAcctState},
    solana_account::Account,
    solana_pubkey::Pubkey,
};

impl TryFrom<ProtoAcctState> for (Pubkey, Account) {
    type Error = HarnessError;

    fn try_from(input: ProtoAcctState) -> Result<Self, Self::Error> {
        let pubkey = Pubkey::new_from_array(
            input
                .address
                .try_into()
                .map_err(|_| HarnessError::InvalidPubkeyBytes)?,
        );
        let owner = Pubkey::new_from_array(
            input
                .owner
                .try_into()
                .map_err(|_| HarnessError::InvalidPubkeyBytes)?,
        );

        Ok((
            pubkey,
            Account {
                lamports: input.lamports,
                data: input.data,
                owner,
                executable: input.executable,
                rent_epoch: input.rent_epoch,
            },
        ))
    }
}
