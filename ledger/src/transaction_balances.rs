#![allow(unused)] // HANA

use {
    crate::{
        blockstore_processor::TransactionStatusSender,
        token_balances::{
            collect_token_balance_from_account, collect_token_balances,
            process_and_store_token_balance, TokenBalanceData,
        },
    },
    itertools::Itertools,
    solana_account_decoder::parse_token::is_known_spl_token_id,
    solana_measure::measure_us,
    solana_runtime::{
        bank::{Bank, ProcessedTransactionCounts, TransactionBalancesSet},
        bank_utils,
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_batch::TransactionBatch,
        vote_sender_types::ReplayVoteSender,
    },
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::{account::ReadableAccount, pubkey::Pubkey, saturating_add_assign},
    solana_svm::{
        transaction_commit_result::{TransactionCommitResult, TransactionCommitResultExtensions},
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResult,
            TransactionProcessingResultExtensions,
        },
    },
    solana_transaction_status::{
        token_balances::TransactionTokenBalancesSet, TransactionTokenBalance,
    },
    spl_token_2022::{
        extension::StateWithExtensions,
        state::{Account as TokenAccount, Mint},
    },
    std::{collections::HashMap, sync::Arc},
};

// TODO lamports done, tokens are significantly more complicated
// * for pre it isnt so bad. step through accounts, getting balance details from hashmap
//   and decimals from hashmap, falling back to bank (and inserting) in both cases if we miss
// * next we need to refresh decimals for any mints we find on successful executed transactions
//   this uses the AccountSharedData from the LoadedTransaction. remember to check lamports
//   mints may be opened, closed, and remade. need a null placeholder
// * finally we need post balances. for executed-failed, processed-failed, and discarded, its the pre
//   for executed-succeeded, if lamports is 0 balance is 0. otherwise parse the account
//   this is structurally the same as the pre case except we use our own account instead of bank result
//   insert it into the hashmap, this concludes our token experiment
// note the balance asserts are that there are the same number of *transactions*
// it is normal and expected that the number of token balances per transaction may be unbalanced
// to preserve existing behavior... we do *not* return a balance if the account is opened (pre) or closed (post)
// the existing design is that it fails ownership checks and gets skipped. not that it returns a 0 balance
//
// ok two notes:
// * remember to gate this on tx status sender
// * theres also code in bstore proc and the bank functions it calls to collect balances
//   if we put this function in ledger/ instead it should cover all usecases
//
// starry suggested we might be able to avoid balances for failed txns
// im going to code the token part assuming that is kosher
// in other words, the flow is much simpler:
// * if txn failed in any way, skip. otherwise...
// * step through and collect prebals from hashmap falling back to bank and inserting
//   get decimals from hashmap falling back to bank
// * step through and update decimals
// * step through getting postbals from raw account data
// and remember to check lamports plus parse fails are no balance not 0 balance
//
// altho that raises an interesting question about how these are used, ask rpc person:
// * is it fine to exclude failed transactions
// * how does it cope with missing balances from new (pre) and closed (post) accounts
//   because the existing code does not capture 0 bals for these
//
// HANA LATEST ok we back up in here whats good
// new me. new rules. new hatred for edge cases and no mercy shown
// the new rule i am going to enforce is we *only* report token balances if the mint existed already
// we dont do any mint tracking. that shit is like 80% of the problem with this

pub fn calculate_transaction_balances(
    batch: &TransactionBatch<impl TransactionWithMeta>,
    processing_results: &[TransactionProcessingResult],
    bank: &Arc<Bank>,
) -> (TransactionBalancesSet, TransactionTokenBalancesSet) {
    // running pre-balances and current mint decimals as we step through results
    let mut native: HashMap<Pubkey, u64> = HashMap::default();
    let mut token: HashMap<Pubkey, Option<TokenBalanceData>> = HashMap::default();
    let mut mint_decimals: HashMap<Pubkey, u8> = HashMap::default();

    // accumulated pre/post lamport balances for each transaction
    let mut native_pre_balances: Vec<Vec<u64>> = Vec::with_capacity(processing_results.len());
    let mut native_post_balances: Vec<Vec<u64>> = Vec::with_capacity(processing_results.len());

    // accumulated pre/post token balances for each transaction
    let mut token_pre_balances: Vec<Vec<TransactionTokenBalance>> =
        Vec::with_capacity(processing_results.len());
    let mut token_post_balances: Vec<Vec<TransactionTokenBalance>> =
        Vec::with_capacity(processing_results.len());

    // accumulate lamport balances
    for (result, transaction) in processing_results
        .iter()
        .zip(batch.sanitized_transactions())
    {
        let mut tx_native_pre: Vec<u64> = Vec::with_capacity(transaction.account_keys().len());
        let mut tx_native_post: Vec<u64> = Vec::with_capacity(transaction.account_keys().len());

        // first get lamport balances for this transaction
        for (index, key) in transaction.account_keys().iter().enumerate() {
            let is_fee_payer = key == transaction.fee_payer();

            // get pre-balance from past executions or bank
            let native_pre_balance = *native.entry(*key).or_insert_with(|| bank.get_balance(key));
            tx_native_pre.push(native_pre_balance);

            // get post-balance from execution result
            let native_post_balance = match result {
                Ok(ProcessedTransaction::Executed(ref executed)) if executed.was_successful() => {
                    executed.loaded_transaction.accounts[index].1.lamports()
                }
                Ok(ProcessedTransaction::Executed(ref executed)) if is_fee_payer => executed
                    .loaded_transaction
                    .rollback_accounts
                    .fee_payer_balance(),
                Ok(ProcessedTransaction::FeesOnly(ref fees_only)) if is_fee_payer => {
                    fees_only.rollback_accounts.fee_payer_balance()
                }
                _ => native_pre_balance,
            };
            native.insert(*key, native_post_balance);
            tx_native_post.push(native_post_balance);
        }

        // push the native balances for this transaction. these are full regardless of execution result
        native_pre_balances.push(tx_native_pre);
        native_post_balances.push(tx_native_post);

        let mut tx_token_pre: Vec<TransactionTokenBalance> = vec![];
        let mut tx_token_post: Vec<TransactionTokenBalance> = vec![];
        let has_token_program = transaction.account_keys().iter().any(is_known_spl_token_id);

        // next get token balances if the transaction was successful and included the token program
        if result.was_processed_with_successful_result() && has_token_program {
            let result_accounts = match result {
                Ok(ProcessedTransaction::Executed(ref executed)) if executed.was_successful() => {
                    &executed.loaded_transaction.accounts
                }
                _ => unreachable!(),
            };

            // get pre- and post-balances together. we only report balances for mints that exist prior to the batch
            // this can produce incorrect UiAmount in pathological cases of closing and reopening mints
            // but since mints can only be done when supply is 0, this does not affect any real usecase
            for (index, (key, account)) in result_accounts.iter().enumerate() {
                // if the transaction succeeded and either of these are true, this cannot be a token account
                if transaction.is_invoked(index) || is_known_spl_token_id(key) {
                    continue;
                }

                // get the pre-balance. this implicitly stores decimals for the mint, if it exists
                // push it, and keep working with this key regardless of whether it exists
                if let Some(pre_token_state) =
                    collect_token_balance_from_account(&mut token, bank, key, &mut mint_decimals)
                {
                    tx_token_pre.push(pre_token_state.into_transaction_balance(index as u8));
                }

                // we now look at the state post-execution
                // start with the obvious signs it is not a token account; prior state is irrelevant
                if !is_known_spl_token_id(account.owner()) || account.lamports() == 0 {
                    token.insert(*key, None);
                    continue;
                }

                // parse and store the account if it might be a token account so we can retreive it back
                // we pass through this function so we catch if the account address was reused on a different mint
                // the same rule applies that, to emit a balance, the mint must have existed prior to the batch
                let post_token_state = if let Ok(token_account) =
                    StateWithExtensions::<TokenAccount>::unpack(account.data())
                {
                    process_and_store_token_balance(
                        &mut token,
                        bank,
                        key,
                        token_account,
                        account.owner(),
                        &mut mint_decimals,
                    )
                } else {
                    token.insert(*key, None);
                    continue;
                };

                // now get the post-execution token balance
                if let Some(post_token_state) =
                    collect_token_balance_from_account(&mut token, bank, key, &mut mint_decimals)
                {
                    tx_token_post.push(post_token_state.into_transaction_balance(index as u8));
                }
            }
        }

        // push the token balances for this transaction. these are empty if the transaction failed
        // this is intended because it is assumed they can be zipped with the batch
        // if the transaction succeeded, all valid token accounts with pre-batch valid mints are contained
        // pre and post may be different lengths for each transaction if an account was opened or closed
        token_pre_balances.push(tx_token_pre);
        token_post_balances.push(tx_token_post);
    }

    // construct the proper balance sets and return
    (
        TransactionBalancesSet::new(native_pre_balances, native_post_balances),
        TransactionTokenBalancesSet::new(token_pre_balances, token_post_balances),
    )
}
