use {
    solana_account_decoder::{
        parse_account_data::SplTokenAdditionalDataV2, parse_token::token_amount_to_ui_amount_v3,
    },
    solana_runtime::bank::TransactionBalancesSet,
    solana_svm::transaction_status_meta_collector::{SvmTokenInfo, TxStatusMetaCollector},
    solana_transaction_status::{
        TransactionTokenBalance, account_sizes::TransactionAccountSizesSet,
        token_balances::TransactionTokenBalancesSet,
    },
};

// Decompose the SVM collector into status-writer transport structs.
pub fn compile_collected_tx_status_meta(
    tx_status_meta_collector: TxStatusMetaCollector,
) -> (
    TransactionBalancesSet,
    TransactionAccountSizesSet,
    TransactionTokenBalancesSet,
) {
    let (native_pre, native_post, acc_size_pre, acc_size_post, token_pre, token_post) =
        tx_status_meta_collector.into_vecs();

    let native_balances = TransactionBalancesSet::new(native_pre, native_post);
    let account_sizes = TransactionAccountSizesSet::new(acc_size_pre, acc_size_post);
    let token_balances = TransactionTokenBalancesSet::new(
        collected_token_infos_to_token_balances(token_pre),
        collected_token_infos_to_token_balances(token_post),
    );

    (native_balances, account_sizes, token_balances)
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

pub fn svm_token_info_to_token_balance(svm_info: SvmTokenInfo) -> TransactionTokenBalance {
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account_decoder::parse_token::UiTokenAmount,
        solana_pubkey::Pubkey,
        spl_generic_token::{token, token_2022},
    };

    #[test]
    fn test_compile_collected_balances() {
        let native_pre = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let native_post = vec![vec![7, 8, 9], vec![10, 11, 0]];

        let account_index = 1;
        let mint1 = Pubkey::new_unique();
        let mint2 = Pubkey::new_unique();
        let owner1 = Pubkey::new_unique();
        let owner2 = Pubkey::new_unique();
        let amount1 = 10;
        let amount2 = 200;
        let decimals1 = 1;
        let decimals2 = 2;

        let token_info_before = SvmTokenInfo {
            account_index,
            mint: mint1,
            amount: amount1,
            owner: owner1,
            program_id: token::id(),
            decimals: decimals1,
        };
        let token_info_after = SvmTokenInfo {
            account_index,
            mint: mint2,
            amount: amount2,
            owner: owner2,
            program_id: token_2022::id(),
            decimals: decimals2,
        };

        let token_pre = vec![vec![token_info_before], vec![]];
        let token_post = vec![vec![token_info_after], vec![]];

        let token_balance_before = TransactionTokenBalance {
            account_index,
            mint: mint1.to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: Some(1.0),
                decimals: decimals1,
                amount: amount1.to_string(),
                ui_amount_string: "1".to_string(),
            },
            owner: owner1.to_string(),
            program_id: token::id().to_string(),
        };
        let token_balance_after = TransactionTokenBalance {
            account_index,
            mint: mint2.to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: Some(2.0),
                decimals: decimals2,
                amount: amount2.to_string(),
                ui_amount_string: "2".to_string(),
            },
            owner: owner2.to_string(),
            program_id: token_2022::id().to_string(),
        };

        let acc_size_pre = vec![vec![10, 20, 30], vec![40, 50, 60]];
        let acc_size_post = vec![vec![11, 21, 31], vec![41, 51, 61]];
        let expected_native = TransactionBalancesSet::new(native_pre.clone(), native_post.clone());
        let expected_sizes =
            TransactionAccountSizesSet::new(acc_size_pre.clone(), acc_size_post.clone());
        let expected_token = TransactionTokenBalancesSet::new(
            vec![vec![token_balance_before], vec![]],
            vec![vec![token_balance_after], vec![]],
        );

        let tx_status_meta_collector = TxStatusMetaCollector {
            native_pre,
            native_post,
            acc_size_pre,
            acc_size_post,
            token_pre,
            token_post,
        };

        let (actual_native, actual_sizes, actual_token) =
            compile_collected_tx_status_meta(tx_status_meta_collector);

        assert_eq!(expected_native.pre_balances, actual_native.pre_balances);
        assert_eq!(expected_native.post_balances, actual_native.post_balances);
        assert_eq!(expected_sizes.pre_acc_sizes, actual_sizes.pre_acc_sizes);
        assert_eq!(expected_sizes.post_acc_sizes, actual_sizes.post_acc_sizes);

        assert_eq!(
            expected_token.pre_token_balances,
            actual_token.pre_token_balances
        );
        assert_eq!(
            expected_token.post_token_balances,
            actual_token.post_token_balances
        );
    }
}
