use {
    super::leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
    itertools::Itertools,
    solana_ledger::{
        blockstore_processor::TransactionStatusSender, token_balances::collect_token_balances,
    },
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
    std::{collections::HashMap, sync::Arc},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitTransactionDetails {
    Committed {
        compute_units: u64,
        loaded_accounts_data_size: u32,
    },
    NotCommitted,
}

#[derive(Default)]
pub(super) struct PreBalanceInfo {
    pub native: Vec<Vec<u64>>,
    pub token: Vec<Vec<TransactionTokenBalance>>,
    pub mint_decimals: HashMap<Pubkey, u8>,
}

// HANA ok i am doing something similar to the prebal attempt
// using code from the intrabatch attempt because it is generally of higher quality
// the idea is this time i want a mutable object that collects initial prebals
// * init from batch: create four empty internal vecs and two hashmaps using bank
// * accumulate bals: fill the vecs using the post-commit state
// * finalize: drop the struct and return the needed balance structs with ownerhip of the vecs
// for tokens i need to think what happens if the account doesnt exist, is dropped and recreated, etc
// basically if it doesnt exist or doesnt parse i need to drop it from the balance store
// programid need to be part of the value rather than the key
// i dont think i need an intermediate struct. im never comparing values or anythig
// we also have to be careful with mints... we might need another hashmap for decimals
// otherwise dropping and recreating mints can fuck us up
// its all relatively straightforward tho just loop through each tx twice
// we need all mint decimals to be correct before we calculate ui token bals
// it does this sort of already, it always gets the mint from bank
// the logic isnt usable, it sort of has a bug but probably no real impact
// if you closed a mint and reopened it with different decimals the post-bal would be wrong
// but this cant really affect anyone since it would need to have or be brought to 0 supply
//
// HANA ok my entire perspective on this is changed
// we can do all balance fuckery BEFORE commit
// that means we dont actually need to gather prebals systematically
// because bank will always contain the pre-state for an account first seen mid-batch
// that means our true desired flow is... one function that takes results (possibly also batch?) and bank
// honestly just loop on accts threetimes and optimize down to two if it makes sense after
// * first loop: get and push prelamps from hashmap, fall back to bank if not found
//   get postlamps from acct, push and insert
// * second loop: go through keys. get any that exist in token hashmap, fall back to bank and try parse
//   if we have a token account, use mint key on acct, decs in hashmap, fall back to bank (and insert?)
//   convert to ui and push to pretoken
// * third loop: go through keys. try to parse the result accounts themselves as token accounts
//   if we succeed look for the mint *in the results*, parse for decimals, insert. fall back to hashmap for decs
//   fall back to bank *and insert* if mint not in result accts. use the decs we have *not* for ui convert
//   push to posttokens
// we can combine the first and second loops but get it right b4 i fuck with it
// this all means we DO NOT NEED a stateful object. we just need an ugly fucking function

type BalanceInfo = (TransactionBalancesSet, TransactionTokenBalancesSet);

#[allow(unused_variables)] // HANA
#[allow(unused_mut)] // HANA
fn calculate_balances(
    batch: &TransactionBatch<impl TransactionWithMeta>,
    processing_results: &Vec<TransactionProcessingResult>,
    bank: &Arc<Bank>,
) -> BalanceInfo {
    // running pre-balances and current mint decimals as we step through results
    let mut native: HashMap<Pubkey, u64> = HashMap::default();
    let mut token: HashMap<Pubkey, TransactionTokenBalance> = HashMap::default();
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

            let native_pre_balance = *native.entry(*key).or_insert_with(|| bank.get_balance(key));
            tx_native_pre.push(native_pre_balance);

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

        // TODO tokens are significantly more complicated
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
    }

    (
        TransactionBalancesSet::new(native_pre_balances, native_post_balances),
        TransactionTokenBalancesSet::new(token_pre_balances, token_post_balances),
    )
}

#[derive(Clone)]
pub struct Committer {
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: ReplayVoteSender,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
}

impl Committer {
    pub fn new(
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> Self {
        Self {
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        }
    }

    pub(super) fn transaction_status_sender_enabled(&self) -> bool {
        self.transaction_status_sender.is_some()
    }

    pub(super) fn commit_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        processing_results: Vec<TransactionProcessingResult>,
        starting_transaction_index: Option<usize>,
        bank: &Arc<Bank>,
        pre_balance_info: &mut PreBalanceInfo, // HANA remove
        execute_and_commit_timings: &mut LeaderExecuteAndCommitTimings,
        processed_counts: &ProcessedTransactionCounts,
    ) -> (u64, Vec<CommitTransactionDetails>) {
        let processed_transactions = processing_results
            .iter()
            .zip(batch.sanitized_transactions())
            .filter_map(|(processing_result, tx)| processing_result.was_processed().then_some(tx))
            .collect_vec();

        let balance_info = calculate_balances(batch, &processing_results, bank);

        let (commit_results, commit_time_us) = measure_us!(bank.commit_transactions(
            batch.sanitized_transactions(),
            processing_results,
            processed_counts,
            &mut execute_and_commit_timings.execute_timings,
        ));
        execute_and_commit_timings.commit_us = commit_time_us;

        let commit_transaction_statuses = commit_results
            .iter()
            .map(|commit_result| match commit_result {
                // reports actual execution CUs, and actual loaded accounts size for
                // transaction committed to block. qos_service uses these information to adjust
                // reserved block space.
                Ok(committed_tx) => CommitTransactionDetails::Committed {
                    compute_units: committed_tx.executed_units,
                    loaded_accounts_data_size: committed_tx
                        .loaded_account_stats
                        .loaded_accounts_data_size,
                },
                Err(_) => CommitTransactionDetails::NotCommitted,
            })
            .collect();

        let ((), find_and_send_votes_us) = measure_us!({
            bank_utils::find_and_send_votes(
                batch.sanitized_transactions(),
                &commit_results,
                Some(&self.replay_vote_sender),
            );
            self.collect_balances_and_send_status_batch(
                commit_results,
                bank,
                batch,
                pre_balance_info, // HANA remove
                balance_info,
                starting_transaction_index,
            );
            self.prioritization_fee_cache
                .update(bank, processed_transactions.into_iter());
        });
        execute_and_commit_timings.find_and_send_votes_us = find_and_send_votes_us;
        (commit_time_us, commit_transaction_statuses)
    }

    fn collect_balances_and_send_status_batch(
        &self,
        commit_results: Vec<TransactionCommitResult>,
        bank: &Arc<Bank>,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        pre_balance_info: &mut PreBalanceInfo, // HANA remove
        balance_info: BalanceInfo,
        starting_transaction_index: Option<usize>,
    ) {
        if let Some(transaction_status_sender) = &self.transaction_status_sender {
            // Clone `SanitizedTransaction` out of `RuntimeTransaction`, this is
            // done to send over the status sender.
            let txs = batch
                .sanitized_transactions()
                .iter()
                .map(|tx| tx.as_sanitized_transaction().into_owned())
                .collect_vec();
            let post_token_balances =
                collect_token_balances(bank, batch, &mut pre_balance_info.mint_decimals);
            let mut transaction_index = starting_transaction_index.unwrap_or_default();
            let batch_transaction_indexes: Vec<_> = commit_results
                .iter()
                .map(|commit_result| {
                    if commit_result.was_committed() {
                        let this_transaction_index = transaction_index;
                        saturating_add_assign!(transaction_index, 1);
                        this_transaction_index
                    } else {
                        0
                    }
                })
                .collect();
            transaction_status_sender.send_transaction_status_batch(
                bank.slot(),
                txs,
                commit_results,
                balance_info.0,
                TransactionTokenBalancesSet::new(
                    std::mem::take(&mut pre_balance_info.token),
                    post_token_balances,
                ),
                batch_transaction_indexes,
            );
        }
    }
}
