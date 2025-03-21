#![allow(unused)] // HANA

use {
    crate::{
        blockstore_processor::TransactionStatusSender, token_balances::{TokenBalanceData, collect_token_balances, collect_token_balance_from_account},
    },
solana_account_decoder::parse_token::is_known_spl_token_id,
    itertools::Itertools,
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
    spl_token_2022::{
        extension::StateWithExtensions,
        state::{Account as TokenAccount, Mint},
    },
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

// HANA idk struct maybe
pub type BalanceInfo = (TransactionBalancesSet, TransactionTokenBalancesSet);

pub fn calculate_transaction_balances(
    batch: &TransactionBatch<impl TransactionWithMeta>,
    processing_results: &[TransactionProcessingResult],
    bank: &Arc<Bank>,
) -> BalanceInfo {
    // running pre-balances and current mint decimals as we step through results
    let mut native: HashMap<Pubkey, u64> = HashMap::default();
    let mut token: HashMap<Pubkey, Option<TokenBalanceData>> = HashMap::default();
    let mut mint_decimals: HashMap<Pubkey, Option<u8>> = HashMap::default();

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

        native_pre_balances.push(tx_native_pre);
        native_post_balances.push(tx_native_post);

        let mut tx_token_pre: Vec<TransactionTokenBalance> = vec![];
        let mut tx_token_post: Vec<TransactionTokenBalance> = vec![];
        let has_token_program = transaction.account_keys().iter().any(is_known_spl_token_id);

        // next get token balances if the transaction was successful and included the token program
        if result.was_processed_with_successful_result() && has_token_program {
            let result_accounts =  match result {
                Ok(ProcessedTransaction::Executed(ref executed)) if executed.was_successful() => {
                    &executed.loaded_transaction.accounts
                }
                // unreachable
                _ => continue,
            };

            // get pre- and post-balances together. we rely on the presence of the mint in the transaction
            // we ignore pre-balances under a variety of scenarios where mints or accounts change in pathological ways
            // this allows us to produce trustworthy post-balances even in extreme scenarios
            for (index, (key, account)) in result_accounts.into_iter().enumerate() {
                // if the transaction succeeded this cannot be a token account
                if transaction.is_invoked(index) || is_known_spl_token_id(key) {
                    continue;
                }

                // we ignore anything that isnt an open, valid token account *after* the transaction
                if !is_known_spl_token_id(account.owner()) || account.lamports() == 0 {
                    continue;
                }

                // parse the post-transaction account state
                let Ok(token_account) = StateWithExtensions::<TokenAccount>::unpack(account.data()) else {
                    continue;
                };

                // parse the mint. if it is not present, the............
                // nevermind. this doesnt work. rack up another failure
                // transfers dont require the mint. the *only* way is to track *all* mints in the batch
                // which means all my efforts toward doing this outside of svm... are complicated
            }

/*
            // first get pre-balances from prior transactions or bank, using prior or bank decimals
            for (index, key) in transaction.account_keys().iter().enumerate() {
                // this cannot be a token account if the transaction succeeded
                if transaction.is_invoked(index) || is_known_spl_token_id(key) {
                    continue;
                }

                if let Some(balance) = collect_token_balance_from_account(&mut token, bank, key, &mut mint_decimals) {
                    tx_token_pre.push(balance.to_transaction_balance(index as u8));
                }
            }

            // next update all mint decimals
            for (index, (key, account)) in result_accounts.into_iter().enumerate() {
                // this cannot be a mint if the transaction succeeded
                if transaction.is_invoked(index) || is_known_spl_token_id(key) {
                    continue;
                }

                // HANA this is maddening. we have to screen anything that *could have been* a mint
                // otherwise in a case where we see an account where a previous transaction edited the mint
                // we would fall back to the bank state. this is the one thing 

                || !is_known_spl_token_id(account.owner()) {

                //let result_account = result
            }
*/
        }
    }

    (
        TransactionBalancesSet::new(native_pre_balances, native_post_balances),
        TransactionTokenBalancesSet::new(token_pre_balances, token_post_balances),
    )
}
