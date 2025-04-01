#![allow(unused)] // HANA

use {
    solana_account_decoder::{
        parse_account_data::SplTokenAdditionalDataV2,
        parse_token::{is_known_spl_token_id, token_amount_to_ui_amount_v3, UiTokenAmount},
    },
    solana_measure::measure::Measure,
    solana_metrics::datapoint_debug,
    solana_runtime::{
        bank::{Bank, TransactionBalancesSet},
        transaction_batch::TransactionBatch,
    },
    solana_sdk::{account::ReadableAccount, pubkey::Pubkey},
    solana_svm::transaction_balances::{BalanceCollector, SvmTokenInfo},
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_status::{
        token_balances::{TransactionTokenBalances, TransactionTokenBalancesSet},
        TransactionTokenBalance,
    },
    spl_token_2022::{
        extension::StateWithExtensions,
        state::{Account as TokenAccount, Mint},
    },
    std::collections::HashMap,
};

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
                .filter_map(|info| svm_token_info_to_token_balance(info))
                .collect()
        })
        .collect()
}

fn svm_token_info_to_token_balance(svm_info: SvmTokenInfo) -> Option<TransactionTokenBalance> {
    let SvmTokenInfo {
        account_index,
        mint,
        amount,
        owner,
        program_id,
        mint_account,
    } = svm_info;

    let mint_decimals = StateWithExtensions::<Mint>::unpack(mint_account.data())
        .map(|mint| mint.base.decimals)
        .ok()?;

    Some(TransactionTokenBalance {
        account_index,
        mint: mint.to_string(),
        ui_token_amount: token_amount_to_ui_amount_v3(
            amount,
            // NOTE: Same as parsed instruction data, ledger data always uses
            // the raw token amount, and does not calculate the UI amount with
            // any consideration for interest.
            &SplTokenAdditionalDataV2::with_decimals(mint_decimals),
        ),
        owner: owner.to_string(),
        program_id: program_id.to_string(),
    })
}
