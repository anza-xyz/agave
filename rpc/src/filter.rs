use {
    solana_rpc_client_api::filter::RpcFilterType,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    spl_generic_token::{token::GenericTokenAccount, token_2022::Account},
};

pub fn filter_allows(filter: &RpcFilterType, account: &AccountSharedData) -> bool {
    match filter {
        RpcFilterType::DataSize(size) => account.data().len() as u64 == *size,
        RpcFilterType::Memcmp(compare) => compare.bytes_match(account.data()),
        RpcFilterType::TokenAccountState => Account::valid_account_data(account.data()),
    }
}
