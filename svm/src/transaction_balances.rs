#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::field_qualifiers;
use {
    crate::{
        account_loader::AccountLoader,
        transaction_processing_callback::TransactionProcessingCallback,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    solana_svm_transaction::svm_transaction::SVMTransaction,
    spl_generic_token::{generic_token, is_known_spl_token_id},
};

// we use internal aliases for clarity, the external type aliases are often confusing
type TxNativeBalances = Vec<u64>;
type TxTokenBalances = Vec<SvmTokenInfo>;
type BatchNativeBalances = Vec<TxNativeBalances>;
type BatchTokenBalances = Vec<TxTokenBalances>;

// to operate cleanly over Option<BalanceCollector> we use a trait impled on the outer and inner type
pub(crate) trait BalanceCollectionRoutines {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    );

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    );
}

#[derive(Debug, Default)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(native_pre(pub), native_post(pub), token_pre(pub), token_post(pub),)
)]
pub struct BalanceCollector {
    native_pre: BatchNativeBalances,
    native_post: BatchNativeBalances,
    token_pre: BatchTokenBalances,
    token_post: BatchTokenBalances,
}

impl BalanceCollector {
    // we always provide one vec for every transaction, even if the vecs are empty
    pub(crate) fn new_with_transaction_count(transaction_count: usize) -> Self {
        Self {
            native_pre: Vec::with_capacity(transaction_count),
            native_post: Vec::with_capacity(transaction_count),
            token_pre: Vec::with_capacity(transaction_count),
            token_post: Vec::with_capacity(transaction_count),
        }
    }

    // we use this pattern to prevent anything outside svm mutating BalanceCollector internals
    // with no public constructor, and only private fields, non-svm code can only disassemble the struct
    pub fn into_vecs(
        self,
    ) -> (
        BatchNativeBalances,
        BatchNativeBalances,
        BatchTokenBalances,
        BatchTokenBalances,
    ) {
        (
            self.native_pre,
            self.native_post,
            self.token_pre,
            self.token_post,
        )
    }

    // gather native lamport balances for all accounts
    // and token balances for valid, initialized token accounts with valid, initialized mints
    fn collect_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    ) -> (TxNativeBalances, TxTokenBalances) {
        let mut native_balances = Vec::with_capacity(transaction.account_keys().len());
        let mut token_balances = vec![];

        let has_token_program = transaction.account_keys().iter().any(is_known_spl_token_id);

        for (index, key) in transaction.account_keys().iter().enumerate() {
            let Some(account) = account_loader.load_account(key) else {
                native_balances.push(0);
                continue;
            };

            native_balances.push(account.lamports());

            if has_token_program
                && !transaction.is_invoked(index)
                && !is_known_spl_token_id(key)
                && is_known_spl_token_id(account.owner())
                && let Some(token_info) =
                    SvmTokenInfo::unpack_token_account(account_loader, &account, index)
            {
                token_balances.push(token_info);
            }
        }

        (native_balances, token_balances)
    }

    pub(crate) fn lengths_match_expected(&self, expected_len: usize) -> bool {
        self.native_pre.len() == expected_len
            && self.native_post.len() == expected_len
            && self.token_pre.len() == expected_len
            && self.token_post.len() == expected_len
    }
}

impl BalanceCollectionRoutines for BalanceCollector {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    ) {
        let (native_balances, token_balances) = self.collect_balances(account_loader, transaction);
        self.native_pre.push(native_balances);
        self.token_pre.push(token_balances);
    }

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    ) {
        let (native_balances, token_balances) = self.collect_balances(account_loader, transaction);
        self.native_post.push(native_balances);
        self.token_post.push(token_balances);
    }
}

impl BalanceCollectionRoutines for Option<BalanceCollector> {
    fn collect_pre_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    ) {
        if let Some(inner) = self {
            inner.collect_pre_balances(account_loader, transaction)
        }
    }

    fn collect_post_balances<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
    ) {
        if let Some(inner) = self {
            inner.collect_post_balances(account_loader, transaction)
        }
    }
}

// this contains all the information we can provide to construct TransactionTokenBalance
// that type, in ledger, depends on UiTokenAmount from account-decoder, so we cannot build it here
#[derive(Debug, Clone, PartialEq)]
pub struct SvmTokenInfo {
    pub account_index: u8,
    pub mint: Pubkey,
    pub amount: u64,
    pub owner: Pubkey,
    pub program_id: Pubkey,
    pub decimals: u8,
}

impl SvmTokenInfo {
    fn unpack_token_account<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        account: &AccountSharedData,
        index: usize,
    ) -> Option<Self> {
        let program_id = *account.owner();
        let generic_token::Account {
            mint,
            owner,
            amount,
        } = generic_token::Account::unpack(account.data(), &program_id)?;

        let mint_account = account_loader.load_account(&mint)?;
        if *mint_account.owner() != program_id {
            return None;
        }

        let generic_token::Mint { decimals, .. } =
            generic_token::Mint::unpack(mint_account.data(), &program_id)?;

        Some(Self {
            account_index: index.try_into().ok()?,
            mint,
            amount,
            owner,
            program_id,
            decimals,
        })
    }
}
