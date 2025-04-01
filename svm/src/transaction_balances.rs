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
    solana_inline_spl::{
        is_known_spl_token_id,
        token::{self, Account as TokenAccount, GenericTokenAccount},
        token_2022::{self, Account as Token2022Account},
    },
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
//   but the svm trait depends on account loading, and passing in the trait object is very difficult
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
// for 3 just pass the accounts up for now, we could add it later
//
// ok so that means what im doing now is:
// * define an enum with a real balance collector or a noop empty variant
//   alternatively, a trait and impl for Option<BalanceCollector>
// * impl pre- and post-bal collection for it using svmtx, loaded/processed tx, loader (for mints)
// * define a struct for token balance containing: account index, owner, u64 balance, mint asd
// * store, internally to the collector, four vecvecs total of pre/post bals
// * add Option<BalanceCollector> to the transaction return info. we need Option in case the batch aborts
// * in blockstore/banking, parse the mint, transform to TransactionTokenBalance with uiamount
//   and construct the TransactionBalancesSet and TransactionTokenBalancesSet
//   there are very natural places to do this in both crates
// and we can consider later whether to parse the mint in svm to get decimals and drop the asd
//
// ok i mostly finished, fairly well in-line with the above though some minor changes
// i just need to collect SvmTokenInfo now. whats that look like. bassically like the ledger token fn
// * do nothing for who tx if no token program
// * skip account if invoked or is token program
// * load account (actually we already did this for lamports, hold the account)
// * skip account if owner not a token program
// * parse
// * load the mint and check its owner or else skip
// * now we have the info we need, push the struct
// as noted elsewhere, it could be nice to add mint parsing to inline-spl but im undecided
//
// HANA UPDATE first draft is done, this should work
// should because i dont know if the token balances are tested anywhere. bank tests lamport balances
// one thing thats a bit odd is im not sure if we should collect bals for *discarded* transactions
// bank tests expect to see bals for "failed" but they trigger an unrecoverable loading failure on purpose
// ie TransactionLoadResult::NotLoaded rather than TransactionLoadResult::FeesOnly
// we should debate whether the test is wrong. i dont know if discarded transactions get statussenderized
// they dont get written to the ledger and arent confirmed so cant be pulled by getTransaction tho

// HANA the native one of these has an equivalent in runtime/ we could move down here and export
// the token one shouldnt be exported. but i need to take a pass looking for code to delete
// im pretty sure i can delete the entire token balance module in ledger/
type NativeBalances = Vec<Vec<u64>>;
type TokenBalances = Vec<Vec<SvmTokenInfo>>;

pub(crate) trait BalanceCollectionRoutines {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        _load_result: &TransactionLoadResult,
    );

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        _processing_result: &TransactionProcessingResult,
    );
}

pub struct BalanceCollector {
    native_pre: NativeBalances,
    native_post: NativeBalances,
    token_pre: TokenBalances,
    token_post: TokenBalances,
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

    // we use this pattern to prevent outside tampering
    // with no public constructor and private fields, non-svm code can only consume
    pub fn into_vecs(self) -> (NativeBalances, NativeBalances, TokenBalances, TokenBalances) {
        (
            self.native_pre,
            self.native_post,
            self.token_pre,
            self.token_post,
        )
    }
}

impl BalanceCollectionRoutines for BalanceCollector {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        _load_result: &TransactionLoadResult,
    ) {
        let mut native_balances = Vec::with_capacity(transaction.account_keys().len());
        let mut token_balances = vec![];

        let has_token_program = transaction.account_keys().iter().any(is_known_spl_token_id);

        for (index, key) in transaction.account_keys().iter().enumerate() {
            let Some(account) = account_loader
                .load_account(key, false)
                .map(|loaded| loaded.account)
            else {
                native_balances.push(0);
                continue;
            };

            native_balances.push(account.lamports());

            if has_token_program
                && !transaction.is_invoked(index)
                && !is_known_spl_token_id(key)
                && is_known_spl_token_id(account.owner())
            {
                if let Some(token_info) =
                    SvmTokenInfo::unpack_token_account(account_loader, &account, index)
                {
                    token_balances.push(token_info);
                }
            }
        }

        self.native_pre.push(native_balances);
        self.token_pre.push(token_balances);
    }

    // HANA NOTE this is identical to the above function, but i left it as-is in case we want to optimize
    // eg, failed or fee-only we can clone the pre-bal vecs and edit fee-payer
    // can also get bals off the result... maybe not worth it tho. loader is cheap
    // and we cannot clone the token accounts because the mints might have changed
    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        _processing_result: &TransactionProcessingResult,
    ) {
        let mut native_balances = Vec::with_capacity(transaction.account_keys().len());
        let mut token_balances = vec![];

        let has_token_program = transaction.account_keys().iter().any(is_known_spl_token_id);

        for (index, key) in transaction.account_keys().iter().enumerate() {
            let Some(account) = account_loader
                .load_account(key, false)
                .map(|loaded| loaded.account)
            else {
                native_balances.push(0);
                continue;
            };

            native_balances.push(account.lamports());

            if has_token_program
                && !transaction.is_invoked(index)
                && !is_known_spl_token_id(key)
                && is_known_spl_token_id(account.owner())
            {
                if let Some(token_info) =
                    SvmTokenInfo::unpack_token_account(account_loader, &account, index)
                {
                    token_balances.push(token_info);
                }
            }
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
    pub account_index: u8,
    pub mint: Pubkey,
    pub amount: u64,
    pub owner: Pubkey,
    pub program_id: Pubkey,
    pub mint_account: AccountSharedData,
}

impl SvmTokenInfo {
    fn unpack_token_account<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        account: &AccountSharedData,
        index: usize,
    ) -> Option<Self> {
        // HANA the way inline-spl dispatch is designed is very odd
        // maybe it should have wrappers that take program id and call the right methods
        let program_id = *account.owner();
        let (mint, amount, owner) = if program_id == token::id() {
            (
                *TokenAccount::unpack_account_mint(account.data())?,
                TokenAccount::unpack_account_amount(account.data())?,
                *TokenAccount::unpack_account_owner(account.data())?,
            )
        } else if program_id == token_2022::id() {
            (
                *Token2022Account::unpack_account_mint(account.data())?,
                Token2022Account::unpack_account_amount(account.data())?,
                *Token2022Account::unpack_account_owner(account.data())?,
            )
        } else {
            // NOTE this should be unreachable, we already validated token program id
            // if this is ever triggered, it means a new token program has been added
            // it must be added here (HANA but we should probably do this in inline-spl instead)
            debug_assert!(false);
            return None;
        };

        let mint_account = account_loader.load_account(&mint, false)?.account;
        if mint_account.owner() != account.owner() {
            return None;
        }

        Some(Self {
            account_index: index.try_into().ok()?,
            mint,
            amount,
            owner,
            program_id,
            mint_account,
        })
    }
}
