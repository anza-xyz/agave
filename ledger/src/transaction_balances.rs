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
    balance_collector: Option<BalanceCollector>,
    batch_len: usize,
) -> (TransactionBalancesSet, TransactionTokenBalancesSet) {
    // if the batch was aborted due to blowing the program cache, balance_collector may be None
    // it isnt reasonable to expect svm to track where in the batch it is and load all accounts in this case
    // we provide balance sets that have an empty vec for each transaction, rather than simple empty vecs
    // this is because the balance set must be broken down into individual TransactionStatusMeta structs
    // it is the responsibility of consumers to robustly handle empty balances in the case of a full batch discard
    let (native_pre, native_post, token_pre, token_post) =
        if let Some(balance_collector) = balance_collector {
            balance_collector.into_vecs()
        } else {
            (
                vec![vec![]; batch_len],
                vec![vec![]; batch_len],
                vec![vec![]; batch_len],
                vec![vec![]; batch_len],
            )
        };

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
