//! Shared test fixtures.

use {
    protosol::protos::AcctState as ProtoAcctState,
    solana_svm::conformance::account_state::account_to_proto,
    solana_sysvar_account::keyed_sysvar_account, solana_sysvar_id::SysvarId,
};

/// The proto account for the sysvar holding `value`.
pub(crate) fn proto_sysvar_account<T>(value: &T) -> ProtoAcctState
where
    T: wincode::Serialize<Src = T> + SysvarId,
{
    let (pubkey, account) = keyed_sysvar_account(value);
    account_to_proto((pubkey, account.into()))
}
