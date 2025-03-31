#![allow(unused)] // HANA

use {
    crate::{
        account_loader::{
            collect_rent_from_account, load_transaction, validate_fee_payer, AccountLoader,
            CheckedTransactionDetails, LoadedTransaction, TransactionCheckResult,
            TransactionLoadResult, ValidatedTransactionDetails,
        },
        transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
        transaction_processing_callback::TransactionProcessingCallback,
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
};

// HANA WOW OK this is the fifth time ive tried to impl this
// i have given up all hope for an elegant, indirect solution that hides this from svm
// it is time to force svm to enter the real world. it must eat from the tree of the knowledge of spl-token
//
// so far we have three greatest hits. i dont even remember was 2 was:
// * bals1: put `enable_balance_recording` on the recording config
//   rather messy balance collection in txp directly with no helpers
//   the idea was we used a callback to get token bals but otherwise did the work ourselves
//   tacked them onto the processing result to get picked up by whoever
//   this was bad because it very poorly separated concerns and did too much in svm
// * bals3: the gigafunction in `ledger/` which attempted to construct balances post-hoc
//   this was a clever machine which attempted to allow svm to have no hand in any of this
//   unfortunately it is inherently imperfect and has tons of gimmickry to emulate balance changes stepwise
//   we crashed into the rocks because to get decimals right we need to either
//   - ignore mints that didnt exist before, but this produces bad balances for mint hijinks
//     it also doesnt necessarily cover 100% of usecases, what if you want to see balances of mints you created
//   - load mint before and after commit and discard any that changed
//     this fixes most hijinks but is still wrong for extreme hijinks
//     it lets us show more balances but forces a weird two-stage flow where we have to load all accounts up front
//   - fully track mints as we advance through the batch. this is fully correct and gets ideal coverage
//     but it is a nightmare to code and maintain, the most rube goldberg situation of any option
//   furthermore all of these have the problem of, for blockstore, we need to put it in the precommit callback
//   and this code has some unusual interactions with bank locking for the unified scheduler
// * bals4: the trait object hand grenade. we package up a machine that does the work and lob into svm
//   this was an inspired option but it is fiendish. we have a trait in svm and impl in `ledger/`
//   but the svm trait depends on acocunt loading, and passing in the trait object is very difficult
//   because the balance collector is generic over concrete impls of either a new loader trait or txpcb
//   both of which are created below blockstore, so they cannot be unified by rustc
//
// so. what is my approach now?
// i would like to impl this in svm. i want helper functions similar to bals4, so the txp flow is minimally inconvenienced
// i should probably use a stateful object that accumulates all this nonsense internally
// i want the recording config option, it is by *far* the cleanest addition to the external interface
// im undecided on a couple things:
// 1. should the fns be inside the matches or should they be outside and do their own matching if necessary
// 2. should the fns accept loaded and processed tx (and generate bals smartly), or just the static svmtx (and load all)
// 3. should we parse the mints or just pass the account data up
//
// i think for 1 and 2 i want to accept loded and processed. do not inconvenience poor txp. two simple callsites
// theyre related because we need to know if loading failed somehow even if we dont get clever with processed
// for 3 just pass the acocunts up for now, we could add it later
//
// ok so that means what im doing now is:
// * define an enum with a real balance collector or a noop empty variant
//   alternatively, a trait and impl for Option<BalanceCollector>
// * impl pre- and post-bal collection for it using svmtx, loaded/processed tx, loader (for mints)
// * define a struct for token balance containing: acocunt index, owner, u64 balance, mint asd
// * store, internally to the collector, four vecvecs total of pre/post bals
// * add Option<BalanceCollector> to the transaction return info. we need Option in case the batch aborts
// * in blockstore/banking, parse the mint, transform to TransactionTokenBalance with uiamount
//   and construct the TransactionBalancesSet and TransactionTokenBalancesSet
//   there are very natural places to do this in both crates
// and we can consider later whether to parse the mint in svm to get decimals and drop the asd

pub(crate) trait BalanceCollectionRoutines {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        load_result: &TransactionLoadResult,
    );

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        processing_result: &TransactionProcessingResult,
    );
}

pub struct BalanceCollector {
    native_pre: Vec<Vec<u64>>,
    native_post: Vec<Vec<u64>>,
    token_pre: Vec<Vec<SvmTokenInfo>>,
    token_post: Vec<Vec<SvmTokenInfo>>,
}

impl BalanceCollector {
    pub(crate) fn new_with_transaction_count(transaction_count: usize) -> Self {
        Self {
            native_pre: Vec::with_capacity(transaction_count),
            native_post: Vec::with_capacity(transaction_count),
            token_pre: Vec::with_capacity(transaction_count),
            token_post: Vec::with_capacity(transaction_count),
        }
    }
}

impl BalanceCollectionRoutines for BalanceCollector {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        load_result: &TransactionLoadResult,
    ) {
        match load_result {
            TransactionLoadResult::NotLoaded(_) => {
                self.native_pre.push(vec![]);
                self.token_pre.push(vec![]);
            }
            _ => {
                let mut native_balances = Vec::with_capacity(transaction.account_keys().len());
                let mut token_balances = vec![];

                for key in transaction.account_keys().iter() {
                    let lamports = account_loader
                        .load_account(key, false)
                        .map(|loaded| loaded.account.lamports())
                        .unwrap_or(0);
                    native_balances.push(lamports);

                    // HANA TODO token
                }

                self.native_pre.push(native_balances);
                self.token_pre.push(token_balances);
            }
        }
    }

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        processing_result: &TransactionProcessingResult,
    ) {
        // HANA NOTE these are basically the same but would diverge if we optimized
        // eg, failed or fee-only we can clone the pre-bal vecs and edit fee-payer
        // can also get bals off the result... maybe not worth it tho

        let mut native_balances = Vec::with_capacity(transaction.account_keys().len());
        let mut token_balances = vec![];

        for key in transaction.account_keys().iter() {
            let lamports = account_loader
                .load_account(key, false)
                .map(|loaded| loaded.account.lamports())
                .unwrap_or(0);
            native_balances.push(lamports);

            // HANA TODO token
        }

        self.native_post.push(native_balances);
        self.token_post.push(token_balances);
    }
}

impl BalanceCollectionRoutines for Option<BalanceCollector> {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        load_result: &TransactionLoadResult,
    ) {
        if let Some(inner) = self {
            inner.collect_pre_balances(account_loader, transaction, load_result)
        }
    }

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        processing_result: &TransactionProcessingResult,
    ) {
        if let Some(inner) = self {
            inner.collect_post_balances(account_loader, transaction, processing_result)
        }
    }
}

pub struct SvmTokenInfo {
    pub index: u8,
    pub mint: Pubkey,
    pub amount: u64,
    pub owner: Pubkey,
    pub mint_data: AccountSharedData,
}
