use {
    solana_account_decoder::{
        parse_account_data::SplTokenAdditionalDataV2, parse_token::token_amount_to_ui_amount_v3,
    },
    solana_runtime::bank::TransactionBalancesSet,
    solana_svm::transaction_balances::{BalanceCollector, SvmTokenInfo},
    solana_transaction_status::{
        token_balances::TransactionTokenBalancesSet, TransactionTokenBalance,
    },
};

// decompose the contents of BalanceCollector into the two structs required by TransactionStatusSender
pub fn compile_collected_balances(
    balance_collector: BalanceCollector,
) -> (TransactionBalancesSet, TransactionTokenBalancesSet) {
    let (native_pre, native_post, token_pre, token_post) = balance_collector.into_vecs();

    let native_balances = TransactionBalancesSet::new(native_pre, native_post);
    let token_balances = TransactionTokenBalancesSet::new(
        collected_token_infos_to_token_balances(token_pre),
        collected_token_infos_to_token_balances(token_post),
    );

    (native_balances, token_balances)
}

fn collected_token_infos_to_token_balances(
    svm_infos: Vec<Vec<SvmTokenInfo>>,
) -> Vec<Vec<TransactionTokenBalance>> {
    svm_infos
        .into_iter()
        .map(|infos| {
            infos
                .into_iter()
                .map(svm_token_info_to_token_balance)
                .collect()
        })
        .collect()
}

fn svm_token_info_to_token_balance(svm_info: SvmTokenInfo) -> TransactionTokenBalance {
    let SvmTokenInfo {
        account_index,
        mint,
        amount,
        owner,
        program_id,
        decimals,
    } = svm_info;
    TransactionTokenBalance {
        account_index,
        mint: mint.to_string(),
        ui_token_amount: token_amount_to_ui_amount_v3(
            amount,
            // NOTE: Same as parsed instruction data, ledger data always uses
            // the raw token amount, and does not calculate the UI amount with
            // any consideration for interest.
            &SplTokenAdditionalDataV2::with_decimals(decimals),
        ),
        owner: owner.to_string(),
        program_id: program_id.to_string(),
    }
}
