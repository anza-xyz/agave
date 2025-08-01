#![cfg(test)]
#![allow(clippy::arithmetic_side_effects)]

use {
    crate::mock_bank::{
        create_custom_loader, deploy_program_with_upgrade_authority, load_program, program_address,
        program_data_size, register_builtins, MockBankCallback, MockForkGraph, EXECUTION_EPOCH,
        EXECUTION_SLOT, WALLCLOCK_TIME,
    },
    agave_feature_set::{self as feature_set, raise_cpi_nesting_limit_to_8, FeatureSet},
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount, PROGRAM_OWNERS},
    solana_clock::Slot,
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_fee_structure::FeeDetails,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_loader_v3_interface::{
        get_program_data_address, instruction as loaderv3_instruction,
        state::UpgradeableLoaderState,
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_nonce::{self as nonce, state::DurableNonce},
    solana_program_entrypoint::MAX_PERMITTED_DATA_INCREASE,
    solana_program_runtime::execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
    solana_pubkey::{pubkey, Pubkey},
    solana_sdk_ids::{bpf_loader_upgradeable, native_loader},
    solana_signer::Signer,
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        nonce_info::NonceInfo,
        transaction_execution_result::TransactionExecutionDetails,
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResult,
            TransactionProcessingResultExtensions,
        },
        transaction_processor::{
            ExecutionRecordingConfig, LoadAndExecuteSanitizedTransactionsOutput,
            TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_system_transaction as system_transaction,
    solana_sysvar::rent::Rent,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    solana_transaction_context::TransactionReturnData,
    solana_transaction_error::TransactionError,
    solana_type_overrides::sync::{Arc, RwLock},
    std::collections::HashMap,
    test_case::test_case,
};

// This module contains the implementation of TransactionProcessingCallback
mod mock_bank;

const DEPLOYMENT_SLOT: u64 = 0;
const LAMPORTS_PER_SIGNATURE: u64 = 5000;
const LAST_BLOCKHASH: Hash = Hash::new_from_array([7; 32]); // Arbitrary constant hash for advancing nonces

pub type AccountsMap = HashMap<Pubkey, AccountSharedData>;

// container for everything needed to execute a test entry
// care should be taken if reused, because we update bank account states, but otherwise leave it as-is
// the environment is made available for tests that check it after processing
pub struct SvmTestEnvironment<'a> {
    pub mock_bank: MockBankCallback,
    pub fork_graph: Arc<RwLock<MockForkGraph>>,
    pub batch_processor: TransactionBatchProcessor<MockForkGraph>,
    pub processing_config: TransactionProcessingConfig<'a>,
    pub processing_environment: TransactionProcessingEnvironment<'a>,
    pub test_entry: SvmTestEntry,
}

impl SvmTestEnvironment<'_> {
    pub fn create(test_entry: SvmTestEntry) -> Self {
        let mock_bank = MockBankCallback::default();

        for (name, slot, authority) in &test_entry.initial_programs {
            deploy_program_with_upgrade_authority(name.to_string(), *slot, &mock_bank, *authority);
        }

        for (pubkey, account) in &test_entry.initial_accounts {
            mock_bank
                .account_shared_data
                .write()
                .unwrap()
                .insert(*pubkey, account.clone());
        }

        let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
        let batch_processor = TransactionBatchProcessor::new(
            EXECUTION_SLOT,
            EXECUTION_EPOCH,
            Arc::downgrade(&fork_graph),
            Some(Arc::new(create_custom_loader())),
            None, // We are not using program runtime v2.
        );

        // The sysvars must be put in the cache
        mock_bank.configure_sysvars();
        batch_processor.fill_missing_sysvar_cache_entries(&mock_bank);
        register_builtins(&mock_bank, &batch_processor, test_entry.with_loader_v4);

        let processing_config = TransactionProcessingConfig {
            recording_config: ExecutionRecordingConfig {
                enable_log_recording: true,
                enable_return_data_recording: true,
                enable_cpi_recording: false,
                enable_transaction_balance_recording: false,
            },
            ..Default::default()
        };

        let feature_set = test_entry.feature_set();
        let processing_environment = TransactionProcessingEnvironment {
            blockhash: LAST_BLOCKHASH,
            feature_set: feature_set.runtime_features(),
            blockhash_lamports_per_signature: LAMPORTS_PER_SIGNATURE,
            ..TransactionProcessingEnvironment::default()
        };

        Self {
            mock_bank,
            fork_graph,
            batch_processor,
            processing_config,
            processing_environment,
            test_entry,
        }
    }

    pub fn execute(&self) -> LoadAndExecuteSanitizedTransactionsOutput {
        let (transactions, check_results) = self.test_entry.prepare_transactions();
        let batch_output = self
            .batch_processor
            .load_and_execute_sanitized_transactions(
                &self.mock_bank,
                &transactions,
                check_results,
                &self.processing_environment,
                &self.processing_config,
            );

        // build a hashmap of final account states incrementally
        // starting with all initial states, updating to all final states
        // with SIMD83, an account might change multiple times in the same batch
        // but it might not exist on all transactions
        let mut final_accounts_actual = self.test_entry.initial_accounts.clone();
        let update_or_dealloc_account =
            |final_accounts: &mut AccountsMap, pubkey, account: AccountSharedData| {
                if account.lamports() == 0 {
                    final_accounts.insert(pubkey, AccountSharedData::default());
                } else {
                    final_accounts.insert(pubkey, account);
                }
            };

        for (tx_index, processed_transaction) in batch_output.processing_results.iter().enumerate()
        {
            let sanitized_transaction = &transactions[tx_index];

            match processed_transaction {
                Ok(ProcessedTransaction::Executed(executed_transaction)) => {
                    for (index, (pubkey, account_data)) in executed_transaction
                        .loaded_transaction
                        .accounts
                        .iter()
                        .enumerate()
                    {
                        if sanitized_transaction.is_writable(index) {
                            update_or_dealloc_account(
                                &mut final_accounts_actual,
                                *pubkey,
                                account_data.clone(),
                            );
                        }
                    }
                }
                Ok(ProcessedTransaction::FeesOnly(fees_only_transaction)) => {
                    for (pubkey, account_data) in &fees_only_transaction.rollback_accounts {
                        update_or_dealloc_account(
                            &mut final_accounts_actual,
                            *pubkey,
                            account_data.clone(),
                        );
                    }
                }
                Err(_) => {}
            }
        }

        // first assert all transaction states together, it makes test-driven development much less of a headache
        let (actual_statuses, expected_statuses): (Vec<_>, Vec<_>) = batch_output
            .processing_results
            .iter()
            .zip(self.test_entry.asserts())
            .map(|(processing_result, test_item_assert)| {
                (
                    ExecutionStatus::from(processing_result),
                    test_item_assert.status,
                )
            })
            .unzip();
        assert_eq!(
            expected_statuses,
            actual_statuses,
            "mismatch between expected and actual statuses. execution details:\n{}",
            batch_output
                .processing_results
                .iter()
                .enumerate()
                .map(|(i, tx)| match tx {
                    Ok(ProcessedTransaction::Executed(executed)) => {
                        format!("{} (executed): {:#?}", i, executed.execution_details)
                    }
                    Ok(ProcessedTransaction::FeesOnly(fee_only)) => {
                        format!("{} (fee-only): {:?}", i, fee_only.load_error)
                    }
                    Err(e) => format!("{i} (discarded): {e:?}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );

        // check that all the account states we care about are present and correct
        for (pubkey, expected_account_data) in self.test_entry.final_accounts.iter() {
            let actual_account_data = final_accounts_actual.get(pubkey);
            assert_eq!(
                Some(expected_account_data),
                actual_account_data,
                "mismatch on account {pubkey}"
            );
        }

        // now run our transaction-by-transaction checks
        for (processing_result, test_item_asserts) in batch_output
            .processing_results
            .iter()
            .zip(self.test_entry.asserts())
        {
            match processing_result {
                Ok(ProcessedTransaction::Executed(executed_transaction)) => test_item_asserts
                    .check_executed_transaction(&executed_transaction.execution_details),
                Ok(ProcessedTransaction::FeesOnly(_)) => {
                    assert!(test_item_asserts.processed());
                    assert!(!test_item_asserts.executed());
                }
                Err(_) => assert!(test_item_asserts.discarded()),
            }
        }

        // merge new account states into the bank for multi-batch tests
        let mut mock_bank_accounts = self.mock_bank.account_shared_data.write().unwrap();
        mock_bank_accounts.extend(final_accounts_actual);

        // update global program cache
        for processing_result in batch_output.processing_results.iter() {
            if let Some(ProcessedTransaction::Executed(executed_tx)) =
                processing_result.processed_transaction()
            {
                let programs_modified_by_tx = &executed_tx.programs_modified_by_tx;
                if executed_tx.was_successful() && !programs_modified_by_tx.is_empty() {
                    self.batch_processor
                        .program_cache
                        .write()
                        .unwrap()
                        .merge(programs_modified_by_tx);
                }
            }
        }

        batch_output
    }

    pub fn is_program_blocked(&self, program_id: &Pubkey) -> bool {
        let (_, program_cache_entry) = self
            .batch_processor
            .program_cache
            .read()
            .unwrap()
            .get_flattened_entries_for_tests()
            .into_iter()
            .rev()
            .find(|(key, _)| key == program_id)
            .unwrap();

        // in the same batch, a new valid loaderv3 program may have a Loaded entry with a later execution slot
        // in a later batch, the same loaderv3 program will have a DelayedVisibility tombstone
        // a new loaderv1/v2 account will have a FailedVerification tombstone
        // and a closed loaderv3 program or any loaderv3 buffer will have a Closed tombstone
        program_cache_entry.effective_slot > EXECUTION_SLOT || program_cache_entry.is_tombstone()
    }
}

// container for a transaction batch and all data needed to run and verify it against svm
#[derive(Clone, Debug)]
pub struct SvmTestEntry {
    // features are enabled by default; these will be disabled
    pub disabled_features: Vec<Pubkey>,

    // until LoaderV4 is live on mainnet, we default to omitting it, but can also test it
    pub with_loader_v4: bool,

    // programs to deploy to the new svm
    pub initial_programs: Vec<(String, Slot, Option<Pubkey>)>,

    // accounts to deploy to the new svm before transaction execution
    pub initial_accounts: AccountsMap,

    // transactions to execute and transaction-specific checks to perform on the results from svm
    pub transaction_batch: Vec<TransactionBatchItem>,

    // expected final account states, checked after transaction execution
    pub final_accounts: AccountsMap,
}

impl SvmTestEntry {
    pub fn with_loader_v4() -> Self {
        Self {
            with_loader_v4: true,
            ..Self::default()
        }
    }

    // add a new a rent-exempt account that exists before the batch
    // inserts it into both account maps, assuming it lives unchanged (except for svm fixing rent epoch)
    // rent-paying accounts must be added by hand because svm will not set rent epoch to u64::MAX
    pub fn add_initial_account(&mut self, pubkey: Pubkey, account: &AccountSharedData) {
        assert!(self
            .initial_accounts
            .insert(pubkey, account.clone())
            .is_none());

        self.create_expected_account(pubkey, account);
    }

    // add an immutable program that will have been deployed before the slot we execute transactions in
    pub fn add_initial_program(&mut self, program_name: &str) {
        self.initial_programs
            .push((program_name.to_string(), DEPLOYMENT_SLOT, None));
    }

    // add a new rent-exempt account that is created by the transaction
    // inserts it only into the post account map
    pub fn create_expected_account(&mut self, pubkey: Pubkey, account: &AccountSharedData) {
        let mut account = account.clone();
        account.set_rent_epoch(u64::MAX);

        assert!(self.final_accounts.insert(pubkey, account).is_none());
    }

    // edit an existing account to reflect changes you expect the transaction to make to it
    pub fn update_expected_account_data(&mut self, pubkey: Pubkey, account: &AccountSharedData) {
        let mut account = account.clone();
        account.set_rent_epoch(u64::MAX);

        assert!(self.final_accounts.insert(pubkey, account).is_some());
    }

    // indicate that an existing account is expected to be deallocated
    pub fn drop_expected_account(&mut self, pubkey: Pubkey) {
        assert!(self
            .final_accounts
            .insert(pubkey, AccountSharedData::default())
            .is_some());
    }

    // add lamports to an existing expected final account state
    pub fn increase_expected_lamports(&mut self, pubkey: &Pubkey, lamports: u64) {
        self.final_accounts
            .get_mut(pubkey)
            .unwrap()
            .checked_add_lamports(lamports)
            .unwrap();
    }

    // subtract lamports from an existing expected final account state
    pub fn decrease_expected_lamports(&mut self, pubkey: &Pubkey, lamports: u64) {
        self.final_accounts
            .get_mut(pubkey)
            .unwrap()
            .checked_sub_lamports(lamports)
            .unwrap();
    }

    // convenience function that adds a transaction that is expected to succeed
    pub fn push_transaction(&mut self, transaction: Transaction) {
        self.push_transaction_with_status(transaction, ExecutionStatus::Succeeded)
    }

    // convenience function that adds a transaction with an expected execution status
    pub fn push_transaction_with_status(
        &mut self,
        transaction: Transaction,
        status: ExecutionStatus,
    ) {
        self.transaction_batch.push(TransactionBatchItem {
            transaction,
            asserts: TransactionBatchItemAsserts {
                status,
                ..TransactionBatchItemAsserts::default()
            },
            ..TransactionBatchItem::default()
        });
    }

    // convenience function that adds a nonce transaction that is expected to succeed
    // we accept the prior nonce state and advance it for the check status, since this happens before svm
    pub fn push_nonce_transaction(&mut self, transaction: Transaction, nonce_info: NonceInfo) {
        self.push_nonce_transaction_with_status(transaction, nonce_info, ExecutionStatus::Succeeded)
    }

    // convenience function that adds a nonce transaction with an expected execution status
    // we accept the prior nonce state and advance it for the check status, since this happens before svm
    pub fn push_nonce_transaction_with_status(
        &mut self,
        transaction: Transaction,
        mut nonce_info: NonceInfo,
        status: ExecutionStatus,
    ) {
        if status != ExecutionStatus::Discarded {
            nonce_info
                .try_advance_nonce(
                    DurableNonce::from_blockhash(&LAST_BLOCKHASH),
                    LAMPORTS_PER_SIGNATURE,
                )
                .unwrap();
        }

        self.transaction_batch.push(TransactionBatchItem {
            transaction,
            asserts: TransactionBatchItemAsserts {
                status,
                ..TransactionBatchItemAsserts::default()
            },
            ..TransactionBatchItem::with_nonce(nonce_info)
        });
    }

    // internal helper to gather SanitizedTransaction objects for execution
    fn prepare_transactions(&self) -> (Vec<SanitizedTransaction>, Vec<TransactionCheckResult>) {
        self.transaction_batch
            .iter()
            .cloned()
            .map(|item| {
                let message = SanitizedTransaction::from_transaction_for_tests(item.transaction);
                let check_result = item.check_result.map(|tx_details| {
                    let compute_budget_limits = process_compute_budget_instructions(
                        SVMMessage::program_instructions_iter(&message),
                        &self.feature_set(),
                    );
                    let signature_count = message
                        .num_transaction_signatures()
                        .saturating_add(message.num_ed25519_signatures())
                        .saturating_add(message.num_secp256k1_signatures())
                        .saturating_add(message.num_secp256r1_signatures());

                    let compute_budget = compute_budget_limits.map(|v| {
                        v.get_compute_budget_and_limits(
                            v.loaded_accounts_bytes,
                            FeeDetails::new(
                                signature_count.saturating_mul(LAMPORTS_PER_SIGNATURE),
                                v.get_prioritization_fee(),
                            ),
                            self.feature_set()
                                .is_active(&raise_cpi_nesting_limit_to_8::id()),
                        )
                    });
                    CheckedTransactionDetails::new(tx_details.nonce, compute_budget)
                });

                (message, check_result)
            })
            .unzip()
    }

    // internal helper to gather test items for post-execution checks
    fn asserts(&self) -> Vec<TransactionBatchItemAsserts> {
        self.transaction_batch
            .iter()
            .cloned()
            .map(|item| item.asserts)
            .collect()
    }

    // internal helper to map our feature list to a FeatureSet
    fn feature_set(&self) -> FeatureSet {
        let mut feature_set = FeatureSet::all_enabled();
        for feature_id in &self.disabled_features {
            feature_set.deactivate(feature_id);
        }

        feature_set
    }
}

// NOTE `1ncomp1ete111111111111111111111111111111111` corresponds to `bpf_account_data_direct_mapping::id()`
// by hardcoding the string, we ensure when the feature is finished, it will automatically be tested
impl Default for SvmTestEntry {
    fn default() -> Self {
        Self {
            disabled_features: vec![pubkey!("1ncomp1ete111111111111111111111111111111111")],
            with_loader_v4: false,
            initial_programs: vec![],
            initial_accounts: AccountsMap::default(),
            transaction_batch: vec![],
            final_accounts: AccountsMap::default(),
        }
    }
}

// one transaction in a batch plus check results for svm and asserts for tests
#[derive(Clone, Debug)]
pub struct TransactionBatchItem {
    pub transaction: Transaction,
    pub check_result: TransactionCheckResult,
    pub asserts: TransactionBatchItemAsserts,
}

impl TransactionBatchItem {
    fn with_nonce(nonce_info: NonceInfo) -> Self {
        Self {
            check_result: Ok(CheckedTransactionDetails::new(
                Some(nonce_info),
                Ok(SVMTransactionExecutionAndFeeBudgetLimits::default()),
            )),
            ..Self::default()
        }
    }
}

impl Default for TransactionBatchItem {
    fn default() -> Self {
        Self {
            transaction: Transaction::default(),
            check_result: Ok(CheckedTransactionDetails::new(
                None,
                Ok(SVMTransactionExecutionAndFeeBudgetLimits::default()),
            )),
            asserts: TransactionBatchItemAsserts::default(),
        }
    }
}

// asserts for a given transaction in a batch
// we can automatically check whether it executed, whether it succeeded
// log items we expect to see (exect match only), and rodata
#[derive(Clone, Debug, Default)]
pub struct TransactionBatchItemAsserts {
    pub status: ExecutionStatus,
    pub logs: Vec<String>,
    pub return_data: ReturnDataAssert,
}

impl TransactionBatchItemAsserts {
    pub fn succeeded(&self) -> bool {
        self.status.succeeded()
    }

    pub fn executed(&self) -> bool {
        self.status.executed()
    }

    pub fn processed(&self) -> bool {
        self.status.processed()
    }

    pub fn discarded(&self) -> bool {
        self.status.discarded()
    }

    pub fn check_executed_transaction(&self, execution_details: &TransactionExecutionDetails) {
        assert!(self.executed());
        assert_eq!(self.succeeded(), execution_details.status.is_ok());

        if !self.logs.is_empty() {
            let actual_logs = execution_details.log_messages.as_ref().unwrap();
            for expected_log in &self.logs {
                assert!(actual_logs.contains(expected_log));
            }
        }

        if self.return_data != ReturnDataAssert::Skip {
            assert_eq!(
                self.return_data,
                execution_details.return_data.clone().into()
            );
        }
    }
}

impl From<ExecutionStatus> for TransactionBatchItemAsserts {
    fn from(status: ExecutionStatus) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }
}

// states a transaction can end in after a trip through the batch processor:
// * discarded: no-op. not even processed. a flawed transaction excluded from the entry
// * processed-failed: aka fee (and nonce) only. charged and added to an entry but not executed, would have failed invariably
// * executed-failed: failed during execution. as above, fees charged and nonce advanced
// * succeeded: what we all aspire to be in our transaction processing lifecycles
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecutionStatus {
    Discarded,
    ProcessedFailed,
    ExecutedFailed,
    #[default]
    Succeeded,
}

// note we avoid the word "failed" because it is confusing
// the batch processor uses it to mean "executed and not succeeded"
// but intuitively (and from the point of a user) it could just as likely mean "any state other than succeeded"
impl ExecutionStatus {
    pub fn succeeded(self) -> bool {
        self == Self::Succeeded
    }

    pub fn executed(self) -> bool {
        self > Self::ProcessedFailed
    }

    pub fn processed(self) -> bool {
        self != Self::Discarded
    }

    pub fn discarded(self) -> bool {
        self == Self::Discarded
    }
}

impl From<&TransactionProcessingResult> for ExecutionStatus {
    fn from(processing_result: &TransactionProcessingResult) -> Self {
        match processing_result {
            Ok(ProcessedTransaction::Executed(executed_transaction)) => {
                if executed_transaction.execution_details.status.is_ok() {
                    ExecutionStatus::Succeeded
                } else {
                    ExecutionStatus::ExecutedFailed
                }
            }
            Ok(ProcessedTransaction::FeesOnly(_)) => ExecutionStatus::ProcessedFailed,
            Err(_) => ExecutionStatus::Discarded,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReturnDataAssert {
    Some(TransactionReturnData),
    None,
    #[default]
    Skip,
}

impl From<Option<TransactionReturnData>> for ReturnDataAssert {
    fn from(option_ro_data: Option<TransactionReturnData>) -> Self {
        match option_ro_data {
            Some(ro_data) => Self::Some(ro_data),
            None => Self::None,
        }
    }
}

fn program_medley() -> Vec<SvmTestEntry> {
    let mut test_entry = SvmTestEntry::default();

    // 0: A transaction that works without any account
    {
        let program_name = "hello-solana";
        let program_id = program_address(program_name);
        test_entry.add_initial_program(program_name);

        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);
        test_entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        ));

        test_entry.transaction_batch[0]
            .asserts
            .logs
            .push("Program log: Hello, Solana!".to_string());

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);
    }

    // 1: A simple funds transfer between accounts
    {
        let program_name = "simple-transfer";
        let program_id = program_address(program_name);
        test_entry.add_initial_program(program_name);

        let fee_payer_keypair = Keypair::new();
        let sender_keypair = Keypair::new();

        let fee_payer = fee_payer_keypair.pubkey();
        let sender = sender_keypair.pubkey();
        let recipient = Pubkey::new_unique();

        let transfer_amount = 10;

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let mut sender_data = AccountSharedData::default();
        sender_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(sender, &sender_data);

        let mut recipient_data = AccountSharedData::default();
        recipient_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(recipient, &recipient_data);

        let instruction = Instruction::new_with_bytes(
            program_id,
            &u64::to_be_bytes(transfer_amount),
            vec![
                AccountMeta::new(sender, true),
                AccountMeta::new(recipient, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        );

        test_entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&fee_payer),
            &[&fee_payer_keypair, &sender_keypair],
            Hash::default(),
        ));

        test_entry.increase_expected_lamports(&recipient, transfer_amount);
        test_entry.decrease_expected_lamports(&sender, transfer_amount);
        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);
    }

    // 2: A program that utilizes a Sysvar
    {
        let program_name = "clock-sysvar";
        let program_id = program_address(program_name);
        test_entry.add_initial_program(program_name);

        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);
        test_entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        ));

        let ro_data = TransactionReturnData {
            program_id,
            data: i64::to_be_bytes(WALLCLOCK_TIME).to_vec(),
        };
        test_entry.transaction_batch[2].asserts.return_data = ReturnDataAssert::Some(ro_data);

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);
    }

    // 3: A transaction that fails
    {
        let program_id = program_address("simple-transfer");

        let fee_payer_keypair = Keypair::new();
        let sender_keypair = Keypair::new();

        let fee_payer = fee_payer_keypair.pubkey();
        let sender = sender_keypair.pubkey();
        let recipient = Pubkey::new_unique();

        let base_amount = 900_000;
        let transfer_amount = base_amount + 50;

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let mut sender_data = AccountSharedData::default();
        sender_data.set_lamports(base_amount);
        test_entry.add_initial_account(sender, &sender_data);

        let mut recipient_data = AccountSharedData::default();
        recipient_data.set_lamports(base_amount);
        test_entry.add_initial_account(recipient, &recipient_data);

        let instruction = Instruction::new_with_bytes(
            program_id,
            &u64::to_be_bytes(transfer_amount),
            vec![
                AccountMeta::new(sender, true),
                AccountMeta::new(recipient, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        );

        test_entry.push_transaction_with_status(
            Transaction::new_signed_with_payer(
                &[instruction],
                Some(&fee_payer),
                &[&fee_payer_keypair, &sender_keypair],
                Hash::default(),
            ),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.transaction_batch[3]
            .asserts
            .logs
            .push("Transfer: insufficient lamports 900000, need 900050".to_string());

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);
    }

    // 4: A transaction whose verification has already failed
    {
        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();

        test_entry.transaction_batch.push(TransactionBatchItem {
            transaction: Transaction::new_signed_with_payer(
                &[],
                Some(&fee_payer),
                &[&fee_payer_keypair],
                Hash::default(),
            ),
            check_result: Err(TransactionError::BlockhashNotFound),
            asserts: ExecutionStatus::Discarded.into(),
        });
    }

    vec![test_entry]
}

fn simple_transfer() -> Vec<SvmTestEntry> {
    let mut test_entry = SvmTestEntry::default();
    let transfer_amount = LAMPORTS_PER_SOL;

    // 0: a transfer that succeeds
    {
        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();
        let destination = Pubkey::new_unique();

        let mut source_data = AccountSharedData::default();
        let mut destination_data = AccountSharedData::default();

        source_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(source, &source_data);

        test_entry.push_transaction(system_transaction::transfer(
            &source_keypair,
            &destination,
            transfer_amount,
            Hash::default(),
        ));

        destination_data
            .checked_add_lamports(transfer_amount)
            .unwrap();
        test_entry.create_expected_account(destination, &destination_data);

        test_entry.decrease_expected_lamports(&source, transfer_amount + LAMPORTS_PER_SIGNATURE);
    }

    // 1: an executable transfer that fails
    {
        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();

        let mut source_data = AccountSharedData::default();

        source_data.set_lamports(transfer_amount - 1);
        test_entry.add_initial_account(source, &source_data);

        test_entry.push_transaction_with_status(
            system_transaction::transfer(
                &source_keypair,
                &Pubkey::new_unique(),
                transfer_amount,
                Hash::default(),
            ),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.decrease_expected_lamports(&source, LAMPORTS_PER_SIGNATURE);
    }

    // 2: a non-processable transfer that fails before loading
    {
        test_entry.transaction_batch.push(TransactionBatchItem {
            transaction: system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                transfer_amount,
                Hash::default(),
            ),
            check_result: Err(TransactionError::BlockhashNotFound),
            asserts: ExecutionStatus::Discarded.into(),
        });
    }

    // 3: a non-processable transfer that fails loading the fee-payer
    {
        test_entry.push_transaction_with_status(
            system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                transfer_amount,
                Hash::default(),
            ),
            ExecutionStatus::Discarded,
        );
    }

    // 4: a processable non-executable transfer that fails loading the program
    {
        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();

        let mut source_data = AccountSharedData::default();

        source_data.set_lamports(transfer_amount * 10);
        test_entry
            .initial_accounts
            .insert(source, source_data.clone());
        test_entry.final_accounts.insert(source, source_data);

        let mut instruction =
            system_instruction::transfer(&source, &Pubkey::new_unique(), transfer_amount);
        instruction.program_id = Pubkey::new_unique();

        test_entry.decrease_expected_lamports(&source, LAMPORTS_PER_SIGNATURE);

        test_entry.push_transaction_with_status(
            Transaction::new_signed_with_payer(
                &[instruction],
                Some(&source),
                &[&source_keypair],
                Hash::default(),
            ),
            ExecutionStatus::ProcessedFailed,
        );
    }

    vec![test_entry]
}

fn simple_nonce(fee_paying_nonce: bool) -> Vec<SvmTestEntry> {
    let mut test_entry = SvmTestEntry::default();

    let program_name = "hello-solana";
    let real_program_id = program_address(program_name);
    test_entry.add_initial_program(program_name);

    // create and return a transaction, fee payer, and nonce info
    // sets up initial account states but not final ones
    // there are four cases of fee_paying_nonce and fake_fee_payer:
    // * false/false: normal nonce account with rent minimum, normal fee payer account with 1sol
    // * true/false: normal nonce account used to pay fees with rent minimum plus 1sol
    // * false/true: normal nonce account with rent minimum, fee payer doesnt exist
    // * true/true: same account for both which does not exist
    // we also provide a side door to bring a fee-paying nonce account below rent-exemption
    let mk_nonce_transaction = |test_entry: &mut SvmTestEntry,
                                program_id,
                                fake_fee_payer: bool,
                                rent_paying_nonce: bool| {
        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();
        let nonce_pubkey = if fee_paying_nonce {
            fee_payer
        } else {
            Pubkey::new_unique()
        };

        let nonce_size = nonce::state::State::size();
        let mut nonce_balance = Rent::default().minimum_balance(nonce_size);

        if !fake_fee_payer && !fee_paying_nonce {
            let mut fee_payer_data = AccountSharedData::default();
            fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
            test_entry.add_initial_account(fee_payer, &fee_payer_data);
        } else if rent_paying_nonce {
            assert!(fee_paying_nonce);
            nonce_balance += LAMPORTS_PER_SIGNATURE;
            nonce_balance -= 1;
        } else if fee_paying_nonce {
            nonce_balance += LAMPORTS_PER_SOL;
        }

        let nonce_initial_hash = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_data =
            nonce::state::Data::new(fee_payer, nonce_initial_hash, LAMPORTS_PER_SIGNATURE);
        let nonce_account = AccountSharedData::new_data(
            nonce_balance,
            &nonce::versions::Versions::new(nonce::state::State::Initialized(nonce_data.clone())),
            &system_program::id(),
        )
        .unwrap();
        let nonce_info = NonceInfo::new(nonce_pubkey, nonce_account.clone());

        if !(fake_fee_payer && fee_paying_nonce) {
            test_entry.add_initial_account(nonce_pubkey, &nonce_account);
        }

        let instructions = vec![
            system_instruction::advance_nonce_account(&nonce_pubkey, &fee_payer),
            Instruction::new_with_bytes(program_id, &[], vec![]),
        ];

        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&fee_payer),
            &[&fee_payer_keypair],
            nonce_data.blockhash(),
        );

        (transaction, fee_payer, nonce_info)
    };

    // 0: successful nonce transaction, regardless of features
    {
        let (transaction, fee_payer, mut nonce_info) =
            mk_nonce_transaction(&mut test_entry, real_program_id, false, false);

        test_entry.push_nonce_transaction(transaction, nonce_info.clone());

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        nonce_info
            .try_advance_nonce(
                DurableNonce::from_blockhash(&LAST_BLOCKHASH),
                LAMPORTS_PER_SIGNATURE,
            )
            .unwrap();

        test_entry
            .final_accounts
            .get_mut(nonce_info.address())
            .unwrap()
            .data_as_mut_slice()
            .copy_from_slice(nonce_info.account().data());
    }

    // 1: non-executing nonce transaction (fee payer doesnt exist) regardless of features
    {
        let (transaction, _fee_payer, nonce_info) =
            mk_nonce_transaction(&mut test_entry, real_program_id, true, false);

        test_entry
            .final_accounts
            .entry(*nonce_info.address())
            .and_modify(|account| account.set_rent_epoch(0));

        test_entry.push_nonce_transaction_with_status(
            transaction,
            nonce_info,
            ExecutionStatus::Discarded,
        );
    }

    // 2: failing nonce transaction (bad system instruction) regardless of features
    {
        let (transaction, fee_payer, mut nonce_info) =
            mk_nonce_transaction(&mut test_entry, system_program::id(), false, false);

        test_entry.push_nonce_transaction_with_status(
            transaction,
            nonce_info.clone(),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        nonce_info
            .try_advance_nonce(
                DurableNonce::from_blockhash(&LAST_BLOCKHASH),
                LAMPORTS_PER_SIGNATURE,
            )
            .unwrap();

        test_entry
            .final_accounts
            .get_mut(nonce_info.address())
            .unwrap()
            .data_as_mut_slice()
            .copy_from_slice(nonce_info.account().data());
    }

    // 3: processable non-executable nonce transaction with fee-only enabled, otherwise discarded
    {
        let (transaction, fee_payer, mut nonce_info) =
            mk_nonce_transaction(&mut test_entry, Pubkey::new_unique(), false, false);

        test_entry.push_nonce_transaction_with_status(
            transaction,
            nonce_info.clone(),
            ExecutionStatus::ProcessedFailed,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        nonce_info
            .try_advance_nonce(
                DurableNonce::from_blockhash(&LAST_BLOCKHASH),
                LAMPORTS_PER_SIGNATURE,
            )
            .unwrap();

        test_entry
            .final_accounts
            .get_mut(nonce_info.address())
            .unwrap()
            .data_as_mut_slice()
            .copy_from_slice(nonce_info.account().data());

        // if the nonce account pays fees, it keeps its new rent epoch, otherwise it resets
        if !fee_paying_nonce {
            test_entry
                .final_accounts
                .get_mut(nonce_info.address())
                .unwrap()
                .set_rent_epoch(0);
        }
    }

    // 4: safety check that nonce fee-payers are required to be rent-exempt (blockhash fee-payers may be below rent-exemption)
    // if this situation is ever allowed in the future, the nonce account MUST be hidden for fee-only transactions
    // as an aside, nonce accounts closed by WithdrawNonceAccount are safe because they are ordinary executed transactions
    // we also dont care whether a non-fee nonce (or any account) pays rent because rent is charged on executed transactions
    if fee_paying_nonce {
        let (transaction, _, nonce_info) =
            mk_nonce_transaction(&mut test_entry, real_program_id, false, true);

        test_entry
            .final_accounts
            .get_mut(nonce_info.address())
            .unwrap()
            .set_rent_epoch(0);

        test_entry.push_nonce_transaction_with_status(
            transaction,
            nonce_info.clone(),
            ExecutionStatus::Discarded,
        );
    }

    // 5: rent-paying nonce fee-payers are also not charged for fee-only transactions
    if fee_paying_nonce {
        let (transaction, _, nonce_info) =
            mk_nonce_transaction(&mut test_entry, Pubkey::new_unique(), false, true);

        test_entry
            .final_accounts
            .get_mut(nonce_info.address())
            .unwrap()
            .set_rent_epoch(0);

        test_entry.push_nonce_transaction_with_status(
            transaction,
            nonce_info.clone(),
            ExecutionStatus::Discarded,
        );
    }

    vec![test_entry]
}

fn simd83_intrabatch_account_reuse() -> Vec<SvmTestEntry> {
    let mut test_entries = vec![];
    let transfer_amount = LAMPORTS_PER_SOL;
    let wallet_rent = Rent::default().minimum_balance(0);

    // batch 0: two successful transfers from the same source
    {
        let mut test_entry = SvmTestEntry::default();

        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();
        let destination1 = Pubkey::new_unique();
        let destination2 = Pubkey::new_unique();

        let mut source_data = AccountSharedData::default();
        let destination1_data = AccountSharedData::default();
        let destination2_data = AccountSharedData::default();

        source_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(source, &source_data);

        for (destination, mut destination_data) in [
            (destination1, destination1_data),
            (destination2, destination2_data),
        ] {
            test_entry.push_transaction(system_transaction::transfer(
                &source_keypair,
                &destination,
                transfer_amount,
                Hash::default(),
            ));

            destination_data
                .checked_add_lamports(transfer_amount)
                .unwrap();
            test_entry.create_expected_account(destination, &destination_data);

            test_entry
                .decrease_expected_lamports(&source, transfer_amount + LAMPORTS_PER_SIGNATURE);
        }

        test_entries.push(test_entry);
    }

    // batch 1:
    // * successful transfer, source left with rent-exempt minimum
    // * non-processable transfer due to underfunded fee-payer
    {
        let mut test_entry = SvmTestEntry::default();

        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();
        let destination = Pubkey::new_unique();

        let mut source_data = AccountSharedData::default();
        let mut destination_data = AccountSharedData::default();

        source_data.set_lamports(transfer_amount + LAMPORTS_PER_SIGNATURE + wallet_rent);
        test_entry.add_initial_account(source, &source_data);

        test_entry.push_transaction(system_transaction::transfer(
            &source_keypair,
            &destination,
            transfer_amount,
            Hash::default(),
        ));

        destination_data
            .checked_add_lamports(transfer_amount)
            .unwrap();
        test_entry.create_expected_account(destination, &destination_data);

        test_entry.decrease_expected_lamports(&source, transfer_amount + LAMPORTS_PER_SIGNATURE);

        test_entry.push_transaction_with_status(
            system_transaction::transfer(
                &source_keypair,
                &destination,
                transfer_amount,
                Hash::default(),
            ),
            ExecutionStatus::Discarded,
        );

        test_entries.push(test_entry);
    }

    // batch 2:
    // * successful transfer to a previously unfunded account
    // * successful transfer using the new account as a fee-payer in the same batch
    {
        let mut test_entry = SvmTestEntry::default();
        let first_transfer_amount = transfer_amount + LAMPORTS_PER_SIGNATURE + wallet_rent;
        let second_transfer_amount = transfer_amount;

        let grandparent_keypair = Keypair::new();
        let grandparent = grandparent_keypair.pubkey();
        let parent_keypair = Keypair::new();
        let parent = parent_keypair.pubkey();
        let child = Pubkey::new_unique();

        let mut grandparent_data = AccountSharedData::default();
        let mut parent_data = AccountSharedData::default();
        let mut child_data = AccountSharedData::default();

        grandparent_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(grandparent, &grandparent_data);

        test_entry.push_transaction(system_transaction::transfer(
            &grandparent_keypair,
            &parent,
            first_transfer_amount,
            Hash::default(),
        ));

        parent_data
            .checked_add_lamports(first_transfer_amount)
            .unwrap();
        test_entry.create_expected_account(parent, &parent_data);

        test_entry.decrease_expected_lamports(
            &grandparent,
            first_transfer_amount + LAMPORTS_PER_SIGNATURE,
        );

        test_entry.push_transaction(system_transaction::transfer(
            &parent_keypair,
            &child,
            second_transfer_amount,
            Hash::default(),
        ));

        child_data
            .checked_add_lamports(second_transfer_amount)
            .unwrap();
        test_entry.create_expected_account(child, &child_data);

        test_entry
            .decrease_expected_lamports(&parent, second_transfer_amount + LAMPORTS_PER_SIGNATURE);

        test_entries.push(test_entry);
    }

    // batch 3:
    // * non-processable transfer due to underfunded fee-payer (two signatures)
    // * successful transfer with the same fee-payer (one signature)
    {
        let mut test_entry = SvmTestEntry::default();

        let feepayer_keypair = Keypair::new();
        let feepayer = feepayer_keypair.pubkey();
        let separate_source_keypair = Keypair::new();
        let separate_source = separate_source_keypair.pubkey();
        let destination = Pubkey::new_unique();

        let mut feepayer_data = AccountSharedData::default();
        let mut separate_source_data = AccountSharedData::default();
        let mut destination_data = AccountSharedData::default();

        feepayer_data.set_lamports(1 + LAMPORTS_PER_SIGNATURE + wallet_rent);
        test_entry.add_initial_account(feepayer, &feepayer_data);

        separate_source_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(separate_source, &separate_source_data);

        test_entry.push_transaction_with_status(
            Transaction::new_signed_with_payer(
                &[system_instruction::transfer(
                    &separate_source,
                    &destination,
                    1,
                )],
                Some(&feepayer),
                &[&feepayer_keypair, &separate_source_keypair],
                Hash::default(),
            ),
            ExecutionStatus::Discarded,
        );

        test_entry.push_transaction(system_transaction::transfer(
            &feepayer_keypair,
            &destination,
            1,
            Hash::default(),
        ));

        destination_data.checked_add_lamports(1).unwrap();
        test_entry.create_expected_account(destination, &destination_data);

        test_entry.decrease_expected_lamports(&feepayer, 1 + LAMPORTS_PER_SIGNATURE);
    }

    // batch 4:
    // * processable non-executable transaction
    // * successful transfer
    // this confirms we update the AccountsMap from RollbackAccounts intrabatch
    {
        let mut test_entry = SvmTestEntry::default();

        let source_keypair = Keypair::new();
        let source = source_keypair.pubkey();
        let destination = Pubkey::new_unique();

        let mut source_data = AccountSharedData::default();
        let mut destination_data = AccountSharedData::default();

        source_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(source, &source_data);

        let mut load_program_fail_instruction =
            system_instruction::transfer(&source, &Pubkey::new_unique(), transfer_amount);
        load_program_fail_instruction.program_id = Pubkey::new_unique();

        test_entry.push_transaction_with_status(
            Transaction::new_signed_with_payer(
                &[load_program_fail_instruction],
                Some(&source),
                &[&source_keypair],
                Hash::default(),
            ),
            ExecutionStatus::ProcessedFailed,
        );

        test_entry.push_transaction(system_transaction::transfer(
            &source_keypair,
            &destination,
            transfer_amount,
            Hash::default(),
        ));

        destination_data
            .checked_add_lamports(transfer_amount)
            .unwrap();
        test_entry.create_expected_account(destination, &destination_data);

        test_entry
            .decrease_expected_lamports(&source, transfer_amount + LAMPORTS_PER_SIGNATURE * 2);

        test_entries.push(test_entry);
    }

    test_entries
}

fn simd83_nonce_reuse(fee_paying_nonce: bool) -> Vec<SvmTestEntry> {
    let mut test_entries = vec![];

    let program_name = "hello-solana";
    let program_id = program_address(program_name);

    let fee_payer_keypair = Keypair::new();
    let non_fee_nonce_keypair = Keypair::new();
    let fee_payer = fee_payer_keypair.pubkey();
    let nonce_pubkey = if fee_paying_nonce {
        fee_payer
    } else {
        non_fee_nonce_keypair.pubkey()
    };

    let nonce_size = nonce::state::State::size();
    let initial_durable = DurableNonce::from_blockhash(&Hash::new_unique());
    let initial_nonce_data =
        nonce::state::Data::new(fee_payer, initial_durable, LAMPORTS_PER_SIGNATURE);
    let initial_nonce_account = AccountSharedData::new_data(
        LAMPORTS_PER_SOL,
        &nonce::versions::Versions::new(nonce::state::State::Initialized(
            initial_nonce_data.clone(),
        )),
        &system_program::id(),
    )
    .unwrap();
    let initial_nonce_info = NonceInfo::new(nonce_pubkey, initial_nonce_account.clone());

    let advanced_durable = DurableNonce::from_blockhash(&LAST_BLOCKHASH);
    let mut advanced_nonce_info = initial_nonce_info.clone();
    advanced_nonce_info
        .try_advance_nonce(advanced_durable, LAMPORTS_PER_SIGNATURE)
        .unwrap();

    let advance_instruction = system_instruction::advance_nonce_account(&nonce_pubkey, &fee_payer);
    let withdraw_instruction = system_instruction::withdraw_nonce_account(
        &nonce_pubkey,
        &fee_payer,
        &fee_payer,
        LAMPORTS_PER_SOL,
    );

    let successful_noop_instruction = Instruction::new_with_bytes(program_id, &[], vec![]);
    let failing_noop_instruction = Instruction::new_with_bytes(system_program::id(), &[], vec![]);
    let fee_only_noop_instruction = Instruction::new_with_bytes(Pubkey::new_unique(), &[], vec![]);

    let second_transaction = Transaction::new_signed_with_payer(
        &[
            advance_instruction.clone(),
            successful_noop_instruction.clone(),
        ],
        Some(&fee_payer),
        &[&fee_payer_keypair],
        *advanced_durable.as_hash(),
    );

    let mut common_test_entry = SvmTestEntry::default();

    common_test_entry.add_initial_account(nonce_pubkey, &initial_nonce_account);

    if !fee_paying_nonce {
        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        common_test_entry.add_initial_account(fee_payer, &fee_payer_data);
    }

    common_test_entry
        .final_accounts
        .get_mut(&nonce_pubkey)
        .unwrap()
        .data_as_mut_slice()
        .copy_from_slice(advanced_nonce_info.account().data());

    common_test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

    let common_test_entry = common_test_entry;

    // batch 0: one transaction that advances the nonce twice
    {
        let mut test_entry = common_test_entry.clone();

        let transaction = Transaction::new_signed_with_payer(
            &[advance_instruction.clone(), advance_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            *initial_durable.as_hash(),
        );

        test_entry.push_nonce_transaction_with_status(
            transaction,
            initial_nonce_info.clone(),
            ExecutionStatus::ExecutedFailed,
        );

        test_entries.push(test_entry);
    }

    // batch 1:
    // * a successful nonce transaction
    // * a nonce transaction that reuses the same nonce; this transaction must be dropped
    {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[
                advance_instruction.clone(),
                successful_noop_instruction.clone(),
            ],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            *initial_durable.as_hash(),
        );

        test_entry.push_nonce_transaction(first_transaction, initial_nonce_info.clone());
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entries.push(test_entry);
    }

    // batch 2:
    // * an executable failed nonce transaction
    // * a nonce transaction that reuses the same nonce; this transaction must be dropped
    {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[advance_instruction.clone(), failing_noop_instruction],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            *initial_durable.as_hash(),
        );

        test_entry.push_nonce_transaction_with_status(
            first_transaction,
            initial_nonce_info.clone(),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entries.push(test_entry);
    }

    // batch 3:
    // * a processable non-executable nonce transaction, if fee-only transactions are enabled
    // * a nonce transaction that reuses the same nonce; this transaction must be dropped
    {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[advance_instruction.clone(), fee_only_noop_instruction],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            *initial_durable.as_hash(),
        );

        test_entry.push_nonce_transaction_with_status(
            first_transaction,
            initial_nonce_info.clone(),
            ExecutionStatus::ProcessedFailed,
        );

        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        // if the nonce account pays fees, it keeps its new rent epoch, otherwise it resets
        if !fee_paying_nonce {
            test_entry
                .final_accounts
                .get_mut(&nonce_pubkey)
                .unwrap()
                .set_rent_epoch(0);
        }

        test_entries.push(test_entry);
    }

    // batch 4:
    // * a successful blockhash transaction that also advances the nonce
    // * a nonce transaction that reuses the same nonce; this transaction must be dropped
    {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[
                successful_noop_instruction.clone(),
                advance_instruction.clone(),
            ],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        test_entry.push_nonce_transaction(first_transaction, initial_nonce_info.clone());
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entries.push(test_entry);
    }

    for test_entry in &mut test_entries {
        test_entry.add_initial_program(program_name);
    }

    // batch 5:
    // * a successful blockhash transaction that closes the nonce
    // * a nonce transaction that uses the nonce; this transaction must be dropped
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[withdraw_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entry.increase_expected_lamports(&fee_payer, LAMPORTS_PER_SOL);

        test_entry.drop_expected_account(nonce_pubkey);

        test_entries.push(test_entry);
    }

    // batch 6:
    // * a successful blockhash transaction that closes the nonce
    // * a successful blockhash transaction that funds the closed account
    // * a nonce transaction that uses the account; this transaction must be dropped
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[withdraw_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        let middle_transaction = system_transaction::transfer(
            &fee_payer_keypair,
            &nonce_pubkey,
            LAMPORTS_PER_SOL,
            Hash::default(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_transaction(middle_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        let mut new_nonce_state = AccountSharedData::default();
        new_nonce_state.set_lamports(LAMPORTS_PER_SOL);

        test_entry.update_expected_account_data(nonce_pubkey, &new_nonce_state);

        test_entries.push(test_entry);
    }

    // batch 7:
    // * a successful blockhash transaction that closes the nonce
    // * a successful blockhash transaction that reopens the account with proper nonce size
    // * a nonce transaction that uses the account; this transaction must be dropped
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[withdraw_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        let middle_transaction = system_transaction::create_account(
            &fee_payer_keypair,
            &non_fee_nonce_keypair,
            Hash::default(),
            LAMPORTS_PER_SOL,
            nonce_size as u64,
            &system_program::id(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_transaction(middle_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

        let new_nonce_state = AccountSharedData::create(
            LAMPORTS_PER_SOL,
            vec![0; nonce_size],
            system_program::id(),
            false,
            u64::MAX,
        );

        test_entry.update_expected_account_data(nonce_pubkey, &new_nonce_state);

        test_entries.push(test_entry);
    }

    // batch 8:
    // * a successful blockhash transaction that closes the nonce
    // * a successful blockhash transaction that reopens the nonce
    // * a nonce transaction that uses the nonce; this transaction must be dropped
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let first_transaction = Transaction::new_signed_with_payer(
            &[withdraw_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        let create_instructions = system_instruction::create_nonce_account(
            &fee_payer,
            &nonce_pubkey,
            &fee_payer,
            LAMPORTS_PER_SOL,
        );

        let middle_transaction = Transaction::new_signed_with_payer(
            &create_instructions,
            Some(&fee_payer),
            &[&fee_payer_keypair, &non_fee_nonce_keypair],
            Hash::default(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_transaction(middle_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

        test_entries.push(test_entry);
    }

    // batch 9:
    // * a successful blockhash noop transaction
    // * a nonce transaction that uses a spoofed nonce account; this transaction must be dropped
    // check_age would never let such a transaction through validation
    // this simulates the case where someone closes a nonce account, then reuses the address in the same batch
    // but as a non-system account that parses as an initialized nonce account
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();
        test_entry.initial_accounts.remove(&nonce_pubkey);
        test_entry.final_accounts.remove(&nonce_pubkey);

        let mut fake_nonce_account = initial_nonce_account.clone();
        fake_nonce_account.set_rent_epoch(u64::MAX);
        fake_nonce_account.set_owner(Pubkey::new_unique());
        test_entry.add_initial_account(nonce_pubkey, &fake_nonce_account);

        let first_transaction = Transaction::new_signed_with_payer(
            &[successful_noop_instruction.clone()],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        test_entries.push(test_entry);
    }

    // batch 10:
    // * a successful blockhash transaction that changes the nonce authority
    // * a nonce transaction that uses the nonce with the old authority; this transaction must be dropped
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let new_authority = Pubkey::new_unique();

        let first_transaction = Transaction::new_signed_with_payer(
            &[system_instruction::authorize_nonce_account(
                &nonce_pubkey,
                &fee_payer,
                &new_authority,
            )],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_nonce_transaction_with_status(
            second_transaction.clone(),
            advanced_nonce_info.clone(),
            ExecutionStatus::Discarded,
        );

        let final_nonce_data =
            nonce::state::Data::new(new_authority, initial_durable, LAMPORTS_PER_SIGNATURE);
        let final_nonce_account = AccountSharedData::new_data(
            LAMPORTS_PER_SOL,
            &nonce::versions::Versions::new(nonce::state::State::Initialized(final_nonce_data)),
            &system_program::id(),
        )
        .unwrap();

        test_entry.update_expected_account_data(nonce_pubkey, &final_nonce_account);

        test_entries.push(test_entry);
    }

    // batch 11:
    // * a successful blockhash transaction that changes the nonce authority
    // * a nonce transaction that uses the nonce with the new authority; this transaction succeeds
    if !fee_paying_nonce {
        let mut test_entry = common_test_entry.clone();

        let new_authority_keypair = Keypair::new();
        let new_authority = new_authority_keypair.pubkey();

        let first_transaction = Transaction::new_signed_with_payer(
            &[system_instruction::authorize_nonce_account(
                &nonce_pubkey,
                &fee_payer,
                &new_authority,
            )],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        let second_transaction = Transaction::new_signed_with_payer(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &new_authority),
                successful_noop_instruction.clone(),
            ],
            Some(&fee_payer),
            &[&fee_payer_keypair, &new_authority_keypair],
            *advanced_durable.as_hash(),
        );

        test_entry.push_transaction(first_transaction);
        test_entry.push_nonce_transaction(second_transaction.clone(), advanced_nonce_info.clone());

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

        let final_nonce_data =
            nonce::state::Data::new(new_authority, advanced_durable, LAMPORTS_PER_SIGNATURE);
        let final_nonce_account = AccountSharedData::new_data(
            LAMPORTS_PER_SOL,
            &nonce::versions::Versions::new(nonce::state::State::Initialized(final_nonce_data)),
            &system_program::id(),
        )
        .unwrap();

        test_entry.update_expected_account_data(nonce_pubkey, &final_nonce_account);

        test_entries.push(test_entry);
    }

    for test_entry in &mut test_entries {
        test_entry.add_initial_program(program_name);
    }

    test_entries
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteProgramInstruction {
    Print,
    Set,
    Dealloc,
    Realloc(usize),
}
impl WriteProgramInstruction {
    fn create_transaction(
        self,
        program_id: Pubkey,
        fee_payer: &Keypair,
        target: Pubkey,
        clamp_data_size: Option<u32>,
    ) -> Transaction {
        let (instruction_data, account_metas) = match self {
            Self::Print => (vec![0], vec![AccountMeta::new_readonly(target, false)]),
            Self::Set => (vec![1], vec![AccountMeta::new(target, false)]),
            Self::Dealloc => (
                vec![2],
                vec![
                    AccountMeta::new(target, false),
                    AccountMeta::new(solana_sdk_ids::incinerator::id(), false),
                ],
            ),
            Self::Realloc(new_size) => {
                let mut instruction_data = vec![3];
                instruction_data.extend_from_slice(&new_size.to_le_bytes());
                (instruction_data, vec![AccountMeta::new(target, false)])
            }
        };

        let mut instructions = vec![];

        if let Some(size) = clamp_data_size {
            instructions.push(ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(size));
        }

        instructions.push(Instruction::new_with_bytes(
            program_id,
            &instruction_data,
            account_metas,
        ));

        Transaction::new_signed_with_payer(
            &instructions,
            Some(&fee_payer.pubkey()),
            &[fee_payer],
            Hash::default(),
        )
    }
}

fn simd83_account_deallocate() -> Vec<SvmTestEntry> {
    let mut test_entries = vec![];

    // batch 0: sanity check, the program actually sets data
    // batch 1: removing lamports from account hides it from subsequent in-batch transactions
    for remove_lamports in [false, true] {
        let mut test_entry = SvmTestEntry::default();

        let program_name = "write-to-account";
        let program_id = program_address(program_name);
        test_entry.add_initial_program(program_name);

        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let target = Pubkey::new_unique();

        let mut target_data = AccountSharedData::create(
            Rent::default().minimum_balance(1),
            vec![0],
            program_id,
            false,
            u64::MAX,
        );
        test_entry.add_initial_account(target, &target_data);

        let set_data_transaction = WriteProgramInstruction::Set.create_transaction(
            program_id,
            &fee_payer_keypair,
            target,
            None,
        );
        test_entry.push_transaction(set_data_transaction);

        target_data.data_as_mut_slice()[0] = 100;

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);
        test_entry.update_expected_account_data(target, &target_data);

        if remove_lamports {
            let dealloc_transaction = WriteProgramInstruction::Dealloc.create_transaction(
                program_id,
                &fee_payer_keypair,
                target,
                None,
            );
            test_entry.push_transaction(dealloc_transaction);

            let print_transaction = WriteProgramInstruction::Print.create_transaction(
                program_id,
                &fee_payer_keypair,
                target,
                None,
            );
            test_entry.push_transaction(print_transaction);
            test_entry.transaction_batch[2]
                .asserts
                .logs
                .push("Program log: account size 0".to_string());

            test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

            test_entry.drop_expected_account(target);
        }

        test_entries.push(test_entry);
    }

    test_entries
}

fn simd83_fee_payer_deallocate() -> Vec<SvmTestEntry> {
    let mut test_entry = SvmTestEntry::default();

    let program_name = "hello-solana";
    let real_program_id = program_address(program_name);
    test_entry.add_initial_program(program_name);

    // 0/1: a rent-paying fee-payer goes to zero lamports on an executed transaction, the batch sees it as deallocated
    // 2/3: the same, except if fee-only transactions are enabled, it goes to zero lamports from a fee-only transaction
    for do_fee_only_transaction in [false, true] {
        let dealloc_fee_payer_keypair = Keypair::new();
        let dealloc_fee_payer = dealloc_fee_payer_keypair.pubkey();

        let mut dealloc_fee_payer_data = AccountSharedData::default();
        dealloc_fee_payer_data.set_lamports(LAMPORTS_PER_SIGNATURE);
        dealloc_fee_payer_data.set_rent_epoch(u64::MAX - 1);
        test_entry.add_initial_account(dealloc_fee_payer, &dealloc_fee_payer_data);

        let stable_fee_payer_keypair = Keypair::new();
        let stable_fee_payer = stable_fee_payer_keypair.pubkey();

        let mut stable_fee_payer_data = AccountSharedData::default();
        stable_fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(stable_fee_payer, &stable_fee_payer_data);

        // transaction which drains a fee-payer
        let instruction = Instruction::new_with_bytes(
            if do_fee_only_transaction {
                Pubkey::new_unique()
            } else {
                real_program_id
            },
            &[],
            vec![],
        );

        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&dealloc_fee_payer),
            &[&dealloc_fee_payer_keypair],
            Hash::default(),
        );

        test_entry.push_transaction_with_status(
            transaction,
            if do_fee_only_transaction {
                ExecutionStatus::ProcessedFailed
            } else {
                ExecutionStatus::Succeeded
            },
        );

        test_entry.decrease_expected_lamports(&dealloc_fee_payer, LAMPORTS_PER_SIGNATURE);

        // as noted in `account_deallocate()` we must touch the account to see if anything actually happened
        let instruction = Instruction::new_with_bytes(
            real_program_id,
            &[],
            vec![AccountMeta::new_readonly(dealloc_fee_payer, false)],
        );
        test_entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&stable_fee_payer),
            &[&stable_fee_payer_keypair],
            Hash::default(),
        ));

        test_entry.decrease_expected_lamports(&stable_fee_payer, LAMPORTS_PER_SIGNATURE);

        test_entry.drop_expected_account(dealloc_fee_payer);
    }

    // 4: a rent-paying non-nonce fee-payer goes to zero on a fee-only nonce transaction, the batch sees it as deallocated
    // we test in `simple_nonce()` that nonce fee-payers cannot as a rule be brought below rent-exemption
    {
        let dealloc_fee_payer_keypair = Keypair::new();
        let dealloc_fee_payer = dealloc_fee_payer_keypair.pubkey();

        let mut dealloc_fee_payer_data = AccountSharedData::default();
        dealloc_fee_payer_data.set_lamports(LAMPORTS_PER_SIGNATURE);
        dealloc_fee_payer_data.set_rent_epoch(u64::MAX - 1);
        test_entry.add_initial_account(dealloc_fee_payer, &dealloc_fee_payer_data);

        let stable_fee_payer_keypair = Keypair::new();
        let stable_fee_payer = stable_fee_payer_keypair.pubkey();

        let mut stable_fee_payer_data = AccountSharedData::default();
        stable_fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
        test_entry.add_initial_account(stable_fee_payer, &stable_fee_payer_data);

        let nonce_pubkey = Pubkey::new_unique();
        let initial_durable = DurableNonce::from_blockhash(&Hash::new_unique());
        let initial_nonce_data =
            nonce::state::Data::new(dealloc_fee_payer, initial_durable, LAMPORTS_PER_SIGNATURE);
        let initial_nonce_account = AccountSharedData::new_data(
            LAMPORTS_PER_SOL,
            &nonce::versions::Versions::new(nonce::state::State::Initialized(
                initial_nonce_data.clone(),
            )),
            &system_program::id(),
        )
        .unwrap();
        let initial_nonce_info = NonceInfo::new(nonce_pubkey, initial_nonce_account.clone());

        let advanced_durable = DurableNonce::from_blockhash(&LAST_BLOCKHASH);
        let mut advanced_nonce_info = initial_nonce_info.clone();
        advanced_nonce_info
            .try_advance_nonce(advanced_durable, LAMPORTS_PER_SIGNATURE)
            .unwrap();

        let advance_instruction =
            system_instruction::advance_nonce_account(&nonce_pubkey, &dealloc_fee_payer);
        let fee_only_noop_instruction =
            Instruction::new_with_bytes(Pubkey::new_unique(), &[], vec![]);

        // fee-only nonce transaction which drains a fee-payer
        let transaction = Transaction::new_signed_with_payer(
            &[advance_instruction, fee_only_noop_instruction],
            Some(&dealloc_fee_payer),
            &[&dealloc_fee_payer_keypair],
            Hash::default(),
        );
        test_entry.push_transaction_with_status(transaction, ExecutionStatus::ProcessedFailed);

        test_entry.decrease_expected_lamports(&dealloc_fee_payer, LAMPORTS_PER_SIGNATURE);

        // as noted in `account_deallocate()` we must touch the account to see if anything actually happened
        let instruction = Instruction::new_with_bytes(
            real_program_id,
            &[],
            vec![AccountMeta::new_readonly(dealloc_fee_payer, false)],
        );
        test_entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&stable_fee_payer),
            &[&stable_fee_payer_keypair],
            Hash::default(),
        ));

        test_entry.decrease_expected_lamports(&stable_fee_payer, LAMPORTS_PER_SIGNATURE);

        test_entry.drop_expected_account(dealloc_fee_payer);
    }

    vec![test_entry]
}

fn simd83_account_reallocate(formalize_loaded_transaction_data_size: bool) -> Vec<SvmTestEntry> {
    let mut test_entries = vec![];

    let program_name = "write-to-account";
    let program_id = program_address(program_name);
    let program_size = program_data_size(program_name);

    let mut common_test_entry = SvmTestEntry::default();
    common_test_entry.add_initial_program(program_name);
    if !formalize_loaded_transaction_data_size {
        common_test_entry
            .disabled_features
            .push(feature_set::formalize_loaded_transaction_data_size::id());
    }

    let fee_payer_keypair = Keypair::new();
    let fee_payer = fee_payer_keypair.pubkey();

    let mut fee_payer_data = AccountSharedData::default();
    fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
    common_test_entry.add_initial_account(fee_payer, &fee_payer_data);

    let mk_target = |size| {
        AccountSharedData::create(
            LAMPORTS_PER_SOL * 10,
            vec![0; size],
            program_id,
            false,
            u64::MAX,
        )
    };

    let target = Pubkey::new_unique();
    let target_start_size = 100;
    common_test_entry.add_initial_account(target, &mk_target(target_start_size));

    // we set a budget that is enough pre-large-realloc but not enough post-large-realloc
    // the relevant feature counts programdata size, so if enabled, we add breathing room
    // this test has nothing to do with the feature
    let size_budget = Some(if formalize_loaded_transaction_data_size {
        (program_size + MAX_PERMITTED_DATA_INCREASE) as u32
    } else {
        MAX_PERMITTED_DATA_INCREASE as u32
    });

    let print_transaction = WriteProgramInstruction::Print.create_transaction(
        program_id,
        &fee_payer_keypair,
        target,
        size_budget,
    );

    common_test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

    let common_test_entry = common_test_entry;

    // batch 0/1:
    // * successful realloc up/down
    // * change reflected in same batch
    for new_target_size in [target_start_size + 1, target_start_size - 1] {
        let mut test_entry = common_test_entry.clone();

        let realloc_transaction = WriteProgramInstruction::Realloc(new_target_size)
            .create_transaction(program_id, &fee_payer_keypair, target, None);
        test_entry.push_transaction(realloc_transaction);

        test_entry.push_transaction(print_transaction.clone());
        test_entry.transaction_batch[1]
            .asserts
            .logs
            .push(format!("Program log: account size {new_target_size}"));

        test_entry.update_expected_account_data(target, &mk_target(new_target_size));

        test_entries.push(test_entry);
    }

    // batch 2:
    // * successful large realloc up
    // * transaction is aborted based on the new transaction data size post-realloc
    {
        let mut test_entry = common_test_entry.clone();

        let new_target_size = target_start_size + MAX_PERMITTED_DATA_INCREASE;

        let realloc_transaction = WriteProgramInstruction::Realloc(new_target_size)
            .create_transaction(program_id, &fee_payer_keypair, target, None);
        test_entry.push_transaction(realloc_transaction);

        test_entry.push_transaction_with_status(
            print_transaction.clone(),
            ExecutionStatus::ProcessedFailed,
        );

        test_entry.update_expected_account_data(target, &mk_target(new_target_size));

        test_entries.push(test_entry);
    }

    test_entries
}

#[test_case(program_medley())]
#[test_case(simple_transfer())]
#[test_case(simple_nonce(false))]
#[test_case(simple_nonce(true))]
#[test_case(simd83_intrabatch_account_reuse())]
#[test_case(simd83_nonce_reuse(false))]
#[test_case(simd83_nonce_reuse(true))]
#[test_case(simd83_account_deallocate())]
#[test_case(simd83_fee_payer_deallocate())]
#[test_case(simd83_account_reallocate(false))]
#[test_case(simd83_account_reallocate(true))]
fn svm_integration(test_entries: Vec<SvmTestEntry>) {
    for test_entry in test_entries {
        let env = SvmTestEnvironment::create(test_entry);
        env.execute();
    }
}

#[test_case(true; "remove accounts executable flag check")]
#[test_case(false; "don't remove accounts executable flag check")]
fn program_cache_create_account(remove_accounts_executable_flag_checks: bool) {
    for loader_id in PROGRAM_OWNERS {
        let mut test_entry = SvmTestEntry::with_loader_v4();
        if !remove_accounts_executable_flag_checks {
            test_entry
                .disabled_features
                .push(feature_set::remove_accounts_executable_flag_checks::id());
        }

        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();

        let mut fee_payer_data = AccountSharedData::default();
        fee_payer_data.set_lamports(LAMPORTS_PER_SOL * 10);
        test_entry.add_initial_account(fee_payer, &fee_payer_data);

        let new_account_keypair = Keypair::new();
        let program_id = new_account_keypair.pubkey();

        // create an account owned by a loader
        let create_transaction = system_transaction::create_account(
            &fee_payer_keypair,
            &new_account_keypair,
            Hash::default(),
            LAMPORTS_PER_SOL,
            0,
            loader_id,
        );

        test_entry.push_transaction(create_transaction);

        test_entry
            .decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SOL + LAMPORTS_PER_SIGNATURE * 2);

        // attempt to invoke the new account
        let invoke_transaction = Transaction::new_signed_with_payer(
            &[Instruction::new_with_bytes(program_id, &[], vec![])],
            Some(&fee_payer),
            &[&fee_payer_keypair],
            Hash::default(),
        );

        // fails at load-time for executable flag if feature is disabled
        // if feature is enabled fails at execution
        let expected_status = if remove_accounts_executable_flag_checks {
            ExecutionStatus::ExecutedFailed
        } else {
            ExecutionStatus::ProcessedFailed
        };

        test_entry.push_transaction_with_status(invoke_transaction.clone(), expected_status);
        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        let mut env = SvmTestEnvironment::create(test_entry);

        // test in same entry as account creation
        env.execute();

        let mut test_entry = SvmTestEntry {
            initial_accounts: env.test_entry.final_accounts.clone(),
            final_accounts: env.test_entry.final_accounts.clone(),
            ..SvmTestEntry::default()
        };

        test_entry.push_transaction_with_status(invoke_transaction, expected_status);
        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

        // test in different entry same slot
        env.test_entry = test_entry;
        env.execute();
    }
}

#[test_case(false, false; "close::scan_only")]
#[test_case(false, true; "close::invoke")]
#[test_case(true, false; "upgrade::scan_only")]
#[test_case(true, true; "upgrade::invoke")]
fn program_cache_loaderv3_update_tombstone(upgrade_program: bool, invoke_changed_program: bool) {
    let mut test_entry = SvmTestEntry::default();

    let program_name = "hello-solana";
    let program_id = program_address(program_name);

    let fee_payer_keypair = Keypair::new();
    let fee_payer = fee_payer_keypair.pubkey();

    let mut fee_payer_data = AccountSharedData::default();
    fee_payer_data.set_lamports(LAMPORTS_PER_SOL);
    test_entry.add_initial_account(fee_payer, &fee_payer_data);

    test_entry
        .initial_programs
        .push((program_name.to_string(), DEPLOYMENT_SLOT, Some(fee_payer)));

    let buffer_address = Pubkey::new_unique();

    // upgrade or close a deployed program
    let change_instruction = if upgrade_program {
        let mut data = bincode::serialize(&UpgradeableLoaderState::Buffer {
            authority_address: Some(fee_payer),
        })
        .unwrap();
        let mut program_bytecode = load_program(program_name.to_string());
        data.append(&mut program_bytecode);

        let buffer_account = AccountSharedData::create(
            LAMPORTS_PER_SOL,
            data,
            bpf_loader_upgradeable::id(),
            true,
            u64::MAX,
        );

        test_entry.add_initial_account(buffer_address, &buffer_account);
        test_entry.drop_expected_account(buffer_address);

        loaderv3_instruction::upgrade(
            &program_id,
            &buffer_address,
            &fee_payer,
            &Pubkey::new_unique(),
        )
    } else {
        loaderv3_instruction::close_any(
            &get_program_data_address(&program_id),
            &Pubkey::new_unique(),
            Some(&fee_payer),
            Some(&program_id),
        )
    };

    test_entry.push_transaction(Transaction::new_signed_with_payer(
        &[change_instruction],
        Some(&fee_payer),
        &[&fee_payer_keypair],
        Hash::default(),
    ));

    test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

    let invoke_transaction = Transaction::new_signed_with_payer(
        &[Instruction::new_with_bytes(program_id, &[], vec![])],
        Some(&fee_payer),
        &[&fee_payer_keypair],
        Hash::default(),
    );

    // attempt to invoke the program, which must fail
    // this ensures the local program cache reflects the change of state
    // we have cases without this so we can assert the cache *before* the invoke contains the tombstone
    if invoke_changed_program {
        test_entry.push_transaction_with_status(
            invoke_transaction.clone(),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);
    }

    let mut env = SvmTestEnvironment::create(test_entry);

    // test in same entry as program change
    env.execute();
    assert!(env.is_program_blocked(&program_id));

    let mut test_entry = SvmTestEntry {
        initial_accounts: env.test_entry.final_accounts.clone(),
        final_accounts: env.test_entry.final_accounts.clone(),
        ..SvmTestEntry::default()
    };

    test_entry.push_transaction_with_status(invoke_transaction, ExecutionStatus::ExecutedFailed);

    test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

    // test in different entry same slot
    env.test_entry = test_entry;
    env.execute();
    assert!(env.is_program_blocked(&program_id));
}

#[test_case(false; "upgrade::scan_only")]
#[test_case(true; "upgrade::invoke")]
fn program_cache_loaderv3_buffer_swap(invoke_changed_program: bool) {
    let mut test_entry = SvmTestEntry::default();

    let program_name = "hello-solana";

    let fee_payer_keypair = Keypair::new();
    let fee_payer = fee_payer_keypair.pubkey();

    let mut fee_payer_data = AccountSharedData::default();
    fee_payer_data.set_lamports(LAMPORTS_PER_SOL * 10);
    test_entry.add_initial_account(fee_payer, &fee_payer_data);

    // this account will start as a buffer and then become a program
    // buffers make their way into the program cache
    // so we test that pathological address reuse is not a problem
    let target_keypair = Keypair::new();
    let target = target_keypair.pubkey();
    let programdata_address = get_program_data_address(&target);

    // we have the same buffer ready at a different address to deploy from
    let deploy_keypair = Keypair::new();
    let deploy = deploy_keypair.pubkey();

    let mut buffer_data = bincode::serialize(&UpgradeableLoaderState::Buffer {
        authority_address: Some(fee_payer),
    })
    .unwrap();
    let mut program_bytecode = load_program(program_name.to_string());
    buffer_data.append(&mut program_bytecode);

    let buffer_account = AccountSharedData::create(
        LAMPORTS_PER_SOL,
        buffer_data.clone(),
        bpf_loader_upgradeable::id(),
        true,
        u64::MAX,
    );

    test_entry.add_initial_account(target, &buffer_account);
    test_entry.add_initial_account(deploy, &buffer_account);

    let program_data = bincode::serialize(&UpgradeableLoaderState::Program {
        programdata_address,
    })
    .unwrap();
    let program_account = AccountSharedData::create(
        LAMPORTS_PER_SOL,
        program_data,
        bpf_loader_upgradeable::id(),
        true,
        u64::MAX,
    );
    test_entry.update_expected_account_data(target, &program_account);
    test_entry.drop_expected_account(deploy);

    // close the buffer
    let close_instruction =
        loaderv3_instruction::close_any(&target, &Pubkey::new_unique(), Some(&fee_payer), None);

    // reopen as a program
    #[allow(deprecated)]
    let deploy_instruction = loaderv3_instruction::deploy_with_max_program_len(
        &fee_payer,
        &target,
        &deploy,
        &fee_payer,
        LAMPORTS_PER_SOL,
        buffer_data.len(),
    )
    .unwrap();

    test_entry.push_transaction(Transaction::new_signed_with_payer(
        &[close_instruction],
        Some(&fee_payer),
        &[&fee_payer_keypair],
        Hash::default(),
    ));

    test_entry.push_transaction(Transaction::new_signed_with_payer(
        &deploy_instruction,
        Some(&fee_payer),
        &[&fee_payer_keypair, &target_keypair],
        Hash::default(),
    ));

    test_entry.decrease_expected_lamports(
        &fee_payer,
        Rent::default().minimum_balance(
            UpgradeableLoaderState::size_of_programdata_metadata() + buffer_data.len(),
        ) + LAMPORTS_PER_SIGNATURE * 3,
    );

    let invoke_transaction = Transaction::new_signed_with_payer(
        &[Instruction::new_with_bytes(target, &[], vec![])],
        Some(&fee_payer),
        &[&fee_payer_keypair],
        Hash::default(),
    );

    if invoke_changed_program {
        test_entry.push_transaction_with_status(
            invoke_transaction.clone(),
            ExecutionStatus::ExecutedFailed,
        );

        test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);
    }

    let mut env = SvmTestEnvironment::create(test_entry);

    // test in same entry as program change
    env.execute();
    assert!(env.is_program_blocked(&target));

    let mut test_entry = SvmTestEntry {
        initial_accounts: env.test_entry.final_accounts.clone(),
        final_accounts: env.test_entry.final_accounts.clone(),
        ..SvmTestEntry::default()
    };

    test_entry.push_transaction_with_status(invoke_transaction, ExecutionStatus::ExecutedFailed);

    test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE);

    // test in different entry same slot
    env.test_entry = test_entry;
    env.execute();
    assert!(env.is_program_blocked(&target));
}

#[derive(Clone, PartialEq, Eq)]
enum Inspect<'a> {
    LiveRead(&'a AccountSharedData),
    LiveWrite(&'a AccountSharedData),
    #[allow(dead_code)]
    DeadRead,
    DeadWrite,
}
impl From<Inspect<'_>> for (Option<AccountSharedData>, bool) {
    fn from(inspect: Inspect) -> Self {
        match inspect {
            Inspect::LiveRead(account) => (Some(account.clone()), false),
            Inspect::LiveWrite(account) => (Some(account.clone()), true),
            Inspect::DeadRead => (None, false),
            Inspect::DeadWrite => (None, true),
        }
    }
}

#[derive(Clone, Default)]
struct InspectedAccounts(pub HashMap<Pubkey, Vec<(Option<AccountSharedData>, bool)>>);
impl InspectedAccounts {
    fn inspect(&mut self, pubkey: Pubkey, inspect: Inspect) {
        self.0.entry(pubkey).or_default().push(inspect.into())
    }
}

#[test_case(false, false; "separate_nonce::old")]
#[test_case(false, true; "separate_nonce::simd186")]
#[test_case(true, false; "fee_paying_nonce::old")]
#[test_case(true, true; "fee_paying_nonce::simd186")]
fn svm_inspect_nonce_load_failure(
    fee_paying_nonce: bool,
    formalize_loaded_transaction_data_size: bool,
) {
    let mut test_entry = SvmTestEntry::default();
    let mut expected_inspected_accounts = InspectedAccounts::default();

    if !formalize_loaded_transaction_data_size {
        test_entry
            .disabled_features
            .push(feature_set::formalize_loaded_transaction_data_size::id());
    }

    let fee_payer_keypair = Keypair::new();
    let dummy_keypair = Keypair::new();
    let separate_nonce_keypair = Keypair::new();

    let fee_payer = fee_payer_keypair.pubkey();
    let dummy = dummy_keypair.pubkey();
    let nonce_pubkey = if fee_paying_nonce {
        fee_payer
    } else {
        separate_nonce_keypair.pubkey()
    };

    let initial_durable = DurableNonce::from_blockhash(&Hash::new_unique());
    let initial_nonce_data =
        nonce::state::Data::new(fee_payer, initial_durable, LAMPORTS_PER_SIGNATURE);
    let mut initial_nonce_account = AccountSharedData::new_data(
        LAMPORTS_PER_SOL,
        &nonce::versions::Versions::new(nonce::state::State::Initialized(
            initial_nonce_data.clone(),
        )),
        &system_program::id(),
    )
    .unwrap();
    initial_nonce_account.set_rent_epoch(u64::MAX);
    let initial_nonce_account = initial_nonce_account;
    let initial_nonce_info = NonceInfo::new(nonce_pubkey, initial_nonce_account.clone());

    let advanced_durable = DurableNonce::from_blockhash(&LAST_BLOCKHASH);
    let mut advanced_nonce_info = initial_nonce_info.clone();
    advanced_nonce_info
        .try_advance_nonce(advanced_durable, LAMPORTS_PER_SIGNATURE)
        .unwrap();

    let compute_instruction = ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(1);
    let advance_instruction = system_instruction::advance_nonce_account(&nonce_pubkey, &fee_payer);
    let fee_only_noop_instruction = Instruction::new_with_bytes(
        Pubkey::new_unique(),
        &[],
        vec![AccountMeta {
            pubkey: dummy,
            is_writable: true,
            is_signer: true,
        }],
    );

    test_entry.add_initial_account(nonce_pubkey, &initial_nonce_account);

    let mut separate_fee_payer_account = AccountSharedData::default();
    separate_fee_payer_account.set_lamports(LAMPORTS_PER_SOL);
    let separate_fee_payer_account = separate_fee_payer_account;

    let dummy_account =
        AccountSharedData::create(1, vec![0; 2], system_program::id(), false, u64::MAX);
    test_entry.add_initial_account(dummy, &dummy_account);

    // we always inspect the nonce at least once
    expected_inspected_accounts.inspect(nonce_pubkey, Inspect::LiveWrite(&initial_nonce_account));

    // if we have a fee-paying nonce, we happen to inspect it again
    // this is an unimportant implementation detail and also means these cases are trivial
    // the true test is a separate nonce, to ensure we inspect it in pre-checks
    if fee_paying_nonce {
        expected_inspected_accounts
            .inspect(nonce_pubkey, Inspect::LiveWrite(&initial_nonce_account));
    } else {
        test_entry.add_initial_account(fee_payer, &separate_fee_payer_account);
        expected_inspected_accounts
            .inspect(fee_payer, Inspect::LiveWrite(&separate_fee_payer_account));
    }

    // with simd186, transaction loading aborts when we hit the fee-payer because of TRANSACTION_ACCOUNT_BASE_SIZE
    // without simd186, transaction loading aborts on the dummy account, so it also happens to be inspected
    // the difference is immaterial to the test as long as it happens before the nonce is loaded for the transaction
    if !fee_paying_nonce && !formalize_loaded_transaction_data_size {
        expected_inspected_accounts.inspect(dummy, Inspect::LiveWrite(&dummy_account));
    }

    // by signing with the dummy account we ensure it precedes a separate nonce
    let transaction = Transaction::new_signed_with_payer(
        &[
            compute_instruction,
            advance_instruction,
            fee_only_noop_instruction,
        ],
        Some(&fee_payer),
        &[&fee_payer_keypair, &dummy_keypair],
        *initial_durable.as_hash(),
    );
    if !fee_paying_nonce {
        let sanitized = SanitizedTransaction::from_transaction_for_tests(transaction.clone());
        let dummy_index = sanitized
            .account_keys()
            .iter()
            .position(|key| *key == dummy)
            .unwrap();
        let nonce_index = sanitized
            .account_keys()
            .iter()
            .position(|key| *key == nonce_pubkey)
            .unwrap();
        assert!(dummy_index < nonce_index);
    }

    test_entry.push_nonce_transaction_with_status(
        transaction,
        initial_nonce_info.clone(),
        ExecutionStatus::ProcessedFailed,
    );

    test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);
    test_entry
        .final_accounts
        .get_mut(&nonce_pubkey)
        .unwrap()
        .data_as_mut_slice()
        .copy_from_slice(advanced_nonce_info.account().data());

    let env = SvmTestEnvironment::create(test_entry.clone());
    env.execute();

    let actual_inspected_accounts = env.mock_bank.inspected_accounts.read().unwrap().clone();
    for (expected_pubkey, expected_account) in &expected_inspected_accounts.0 {
        let actual_account = actual_inspected_accounts.get(expected_pubkey).unwrap();
        assert_eq!(
            expected_account, actual_account,
            "pubkey: {expected_pubkey}",
        );
    }
}

#[test]
fn svm_inspect_account() {
    let mut initial_test_entry = SvmTestEntry::default();
    let mut expected_inspected_accounts = InspectedAccounts::default();

    let fee_payer_keypair = Keypair::new();
    let sender_keypair = Keypair::new();

    let fee_payer = fee_payer_keypair.pubkey();
    let sender = sender_keypair.pubkey();
    let recipient = Pubkey::new_unique();

    // Setting up the accounts for the transfer

    // fee payer
    let mut fee_payer_account = AccountSharedData::default();
    fee_payer_account.set_lamports(85_000);
    fee_payer_account.set_rent_epoch(u64::MAX);
    initial_test_entry.add_initial_account(fee_payer, &fee_payer_account);
    expected_inspected_accounts.inspect(fee_payer, Inspect::LiveWrite(&fee_payer_account));

    // sender
    let mut sender_account = AccountSharedData::default();
    sender_account.set_lamports(11_000_000);
    sender_account.set_rent_epoch(u64::MAX);
    initial_test_entry.add_initial_account(sender, &sender_account);
    expected_inspected_accounts.inspect(sender, Inspect::LiveWrite(&sender_account));

    // recipient -- initially dead
    expected_inspected_accounts.inspect(recipient, Inspect::DeadWrite);

    // system program
    let system_account = AccountSharedData::create(
        5000,
        "system_program".as_bytes().to_vec(),
        native_loader::id(),
        true,
        0,
    );
    expected_inspected_accounts.inspect(system_program::id(), Inspect::LiveRead(&system_account));

    let transfer_amount = 1_000_000;
    let transaction = Transaction::new_signed_with_payer(
        &[system_instruction::transfer(
            &sender,
            &recipient,
            transfer_amount,
        )],
        Some(&fee_payer),
        &[&fee_payer_keypair, &sender_keypair],
        Hash::default(),
    );

    initial_test_entry.push_transaction(transaction);

    let mut recipient_account = AccountSharedData::default();
    recipient_account.set_lamports(transfer_amount);

    initial_test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);
    initial_test_entry.decrease_expected_lamports(&sender, transfer_amount);
    initial_test_entry.create_expected_account(recipient, &recipient_account);

    let initial_test_entry = initial_test_entry;

    // Load and execute the transaction
    let mut env = SvmTestEnvironment::create(initial_test_entry.clone());
    env.execute();

    // do another transfer; recipient should be alive now

    // fee payer
    let intermediate_fee_payer_account = initial_test_entry
        .final_accounts
        .get(&fee_payer)
        .cloned()
        .unwrap();
    expected_inspected_accounts.inspect(
        fee_payer,
        Inspect::LiveWrite(&intermediate_fee_payer_account),
    );

    // sender
    let intermediate_sender_account = initial_test_entry
        .final_accounts
        .get(&sender)
        .cloned()
        .unwrap();
    expected_inspected_accounts.inspect(sender, Inspect::LiveWrite(&intermediate_sender_account));

    // recipient -- now alive
    let intermediate_recipient_account = initial_test_entry
        .final_accounts
        .get(&recipient)
        .cloned()
        .unwrap();
    expected_inspected_accounts.inspect(
        recipient,
        Inspect::LiveWrite(&intermediate_recipient_account),
    );

    // system program
    expected_inspected_accounts.inspect(system_program::id(), Inspect::LiveRead(&system_account));

    let mut final_test_entry = SvmTestEntry {
        initial_accounts: initial_test_entry.final_accounts.clone(),
        final_accounts: initial_test_entry.final_accounts.clone(),
        ..SvmTestEntry::default()
    };

    let transfer_amount = 456;
    let transaction = Transaction::new_signed_with_payer(
        &[system_instruction::transfer(
            &sender,
            &recipient,
            transfer_amount,
        )],
        Some(&fee_payer),
        &[&fee_payer_keypair, &sender_keypair],
        Hash::default(),
    );

    final_test_entry.push_transaction(transaction);

    final_test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);
    final_test_entry.decrease_expected_lamports(&sender, transfer_amount);
    final_test_entry.increase_expected_lamports(&recipient, transfer_amount);

    // Load and execute the second transaction
    env.test_entry = final_test_entry;
    env.execute();

    // Ensure all the expected inspected accounts were inspected
    let actual_inspected_accounts = env.mock_bank.inspected_accounts.read().unwrap().clone();
    for (expected_pubkey, expected_account) in &expected_inspected_accounts.0 {
        let actual_account = actual_inspected_accounts.get(expected_pubkey).unwrap();
        assert_eq!(
            expected_account, actual_account,
            "pubkey: {expected_pubkey}",
        );
    }

    let num_expected_inspected_accounts: usize =
        expected_inspected_accounts.0.values().map(Vec::len).sum();
    let num_actual_inspected_accounts: usize =
        actual_inspected_accounts.values().map(Vec::len).sum();

    assert_eq!(
        num_expected_inspected_accounts,
        num_actual_inspected_accounts,
    );
}

// Tests for proper accumulation of metrics across loaded programs in a batch.
#[test]
fn svm_metrics_accumulation() {
    for test_entry in program_medley() {
        let env = SvmTestEnvironment::create(test_entry);

        let (transactions, check_results) = env.test_entry.prepare_transactions();

        let result = env.batch_processor.load_and_execute_sanitized_transactions(
            &env.mock_bank,
            &transactions,
            check_results,
            &env.processing_environment,
            &env.processing_config,
        );

        // jit compilation only happens on non-windows && x86_64
        #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
        {
            assert_ne!(
                result
                    .execute_timings
                    .details
                    .create_executor_jit_compile_us
                    .0,
                0
            );
        }
        assert_ne!(
            result.execute_timings.details.create_executor_load_elf_us.0,
            0
        );
        assert_ne!(
            result
                .execute_timings
                .details
                .create_executor_verify_code_us
                .0,
            0
        );
    }
}

// NOTE this could be moved to its own file in the future, but it requires a total refactor of the test runner
mod balance_collector {
    use {
        super::*,
        rand0_7::prelude::*,
        solana_program_pack::Pack,
        solana_sdk_ids::bpf_loader,
        spl_generic_token::token_2022,
        spl_token::state::{Account as TokenAccount, AccountState as TokenAccountState, Mint},
        test_case::test_case,
    };

    // this could be part of mock_bank but so far nothing but this uses it
    static SPL_TOKEN_BYTES: &[u8] =
        include_bytes!("../../program-test/src/programs/spl_token-3.5.0.so");

    const STARTING_BALANCE: u64 = LAMPORTS_PER_SOL * 100;

    // a helper for constructing a transfer instruction, agnostic over system/token
    // it also pulls double duty as a record of what the *result* of a transfer should be
    // so we can instantiate a Transfer, gen the instruction, change it to fail, change the record to amount 0
    // and then the final test confirms the pre/post balances are unchanged with no special casing
    #[derive(Debug, Default)]
    struct Transfer {
        from: Pubkey,
        to: Pubkey,
        amount: u64,
    }

    impl Transfer {
        // given a set of users, picks two randomly and does a random transfer between them
        fn new_rand(users: &[Pubkey]) -> Self {
            let mut rng = rand0_7::thread_rng();
            let [from_idx, to_idx] = (0..users.len()).choose_multiple(&mut rng, 2)[..] else {
                unreachable!()
            };
            let from = users[from_idx];
            let to = users[to_idx];
            let amount = rng.gen_range(1, STARTING_BALANCE / 100);

            Self { from, to, amount }
        }

        fn to_system_instruction(&self) -> Instruction {
            system_instruction::transfer(&self.from, &self.to, self.amount)
        }

        fn to_token_instruction(&self, fee_payer: &Pubkey) -> Instruction {
            // true tokenkeg connoisseurs will note we shouldnt have to sign the sender
            // we use a common account owner, the fee-payer, to conveniently reuse account state
            // so why do we sign? to force the sender and receiver to be in a consistent order in account keys
            // which means we can grab them by index in our final test instead of searching by key
            let mut instruction = spl_token::instruction::transfer(
                &spl_token::id(),
                &self.from,
                &self.to,
                fee_payer,
                &[],
                self.amount,
            )
            .unwrap();
            instruction.accounts[0].is_signer = true;

            instruction
        }

        fn to_instruction(&self, fee_payer: &Pubkey, use_tokens: bool) -> Instruction {
            if use_tokens {
                self.to_token_instruction(fee_payer)
            } else {
                self.to_system_instruction()
            }
        }
    }

    #[test_case(false; "native")]
    #[test_case(true; "token")]
    fn svm_collect_balances(use_tokens: bool) {
        let mut rng = rand0_7::thread_rng();

        let fee_payer_keypair = Keypair::new();
        let fake_fee_payer_keypair = Keypair::new();
        let alice_keypair = Keypair::new();
        let bob_keypair = Keypair::new();
        let charlie_keypair = Keypair::new();

        let fee_payer = fee_payer_keypair.pubkey();
        let fake_fee_payer = fake_fee_payer_keypair.pubkey();
        let mint = Pubkey::new_unique();
        let alice = alice_keypair.pubkey();
        let bob = bob_keypair.pubkey();
        let charlie = charlie_keypair.pubkey();

        let native_state = AccountSharedData::create(
            STARTING_BALANCE,
            vec![],
            system_program::id(),
            false,
            u64::MAX,
        );

        let mut mint_buf = vec![0; Mint::get_packed_len()];
        Mint {
            decimals: 9,
            is_initialized: true,
            ..Mint::default()
        }
        .pack_into_slice(&mut mint_buf);

        let mint_state =
            AccountSharedData::create(LAMPORTS_PER_SOL, mint_buf, spl_token::id(), false, u64::MAX);

        let token_account_for_tests = || TokenAccount {
            mint,
            owner: fee_payer,
            amount: STARTING_BALANCE,
            state: TokenAccountState::Initialized,
            ..TokenAccount::default()
        };

        let mut token_buf = vec![0; TokenAccount::get_packed_len()];
        token_account_for_tests().pack_into_slice(&mut token_buf);

        let token_state = AccountSharedData::create(
            LAMPORTS_PER_SOL,
            token_buf,
            spl_token::id(),
            false,
            u64::MAX,
        );

        let spl_token = AccountSharedData::create(
            LAMPORTS_PER_SOL,
            SPL_TOKEN_BYTES.to_vec(),
            bpf_loader::id(),
            true,
            u64::MAX,
        );

        for _ in 0..100 {
            let mut test_entry = SvmTestEntry::default();
            test_entry.add_initial_account(fee_payer, &native_state.clone());

            if use_tokens {
                test_entry.add_initial_account(spl_token::id(), &spl_token);
                test_entry.add_initial_account(mint, &mint_state);
                test_entry.add_initial_account(alice, &token_state);
                test_entry.add_initial_account(bob, &token_state);
                test_entry.add_initial_account(charlie, &token_state);
            } else {
                test_entry.add_initial_account(alice, &native_state);
                test_entry.add_initial_account(bob, &native_state);
                test_entry.add_initial_account(charlie, &native_state);
            }

            // test that fee-payer balances are reported correctly
            // all we need to know is whether the transaction is processed or dropped
            let mut transaction_discards = vec![];

            // every time we perform a transfer, we mutate user_balances
            // and then clone and push it into user_balance_history
            // this lets us go through every svm balance record and confirm correctness
            let mut user_balances = HashMap::new();
            user_balances.insert(alice, STARTING_BALANCE);
            user_balances.insert(bob, STARTING_BALANCE);
            user_balances.insert(charlie, STARTING_BALANCE);
            let mut user_balance_history = vec![(Transfer::default(), user_balances.clone())];

            for _ in 0..50 {
                // failures result in no balance changes (note we use a separate fee-payer)
                // we mix some in with the successes to test that we never record changes for failures
                let expected_status = match rng.gen::<f64>() {
                    n if n < 0.85 => ExecutionStatus::Succeeded,
                    n if n < 0.90 => ExecutionStatus::ExecutedFailed,
                    n if n < 0.95 => ExecutionStatus::ProcessedFailed,
                    _ => ExecutionStatus::Discarded,
                };
                transaction_discards.push(expected_status == ExecutionStatus::Discarded);

                let mut transfer = Transfer::new_rand(&[alice, bob, charlie]);
                let from_signer = vec![&alice_keypair, &bob_keypair, &charlie_keypair]
                    .into_iter()
                    .find(|k| k.pubkey() == transfer.from)
                    .unwrap();

                let instructions = match expected_status {
                    // a success results in balance changes and is a normal transaction
                    ExecutionStatus::Succeeded => {
                        user_balances
                            .entry(transfer.from)
                            .and_modify(|v| *v -= transfer.amount);
                        user_balances
                            .entry(transfer.to)
                            .and_modify(|v| *v += transfer.amount);

                        vec![transfer.to_instruction(&fee_payer, use_tokens)]
                    }
                    // transfer an unreasonable amount to fail execution
                    ExecutionStatus::ExecutedFailed => {
                        transfer.amount = u64::MAX / 2;
                        let instruction = transfer.to_instruction(&fee_payer, use_tokens);
                        transfer.amount = 0;

                        vec![instruction]
                    }
                    // use a non-existant program to fail loading
                    // token22 is very convenient because its presence ensures token bals are recorded
                    // if we had to use a random program id we would need to push a token program onto account keys
                    ExecutionStatus::ProcessedFailed => {
                        let mut instruction = transfer.to_instruction(&fee_payer, use_tokens);
                        instruction.program_id = token_2022::id();
                        transfer.amount = 0;

                        vec![instruction]
                    }
                    // use a non-existant fee-payer to trigger a discard
                    ExecutionStatus::Discarded => {
                        let mut instruction = transfer.to_instruction(&fee_payer, use_tokens);
                        if use_tokens {
                            instruction.accounts[2].pubkey = fake_fee_payer;
                        }
                        transfer.amount = 0;

                        vec![instruction]
                    }
                };

                let transaction = if expected_status.discarded() {
                    Transaction::new_signed_with_payer(
                        &instructions,
                        Some(&fake_fee_payer),
                        &[&fake_fee_payer_keypair, from_signer],
                        Hash::default(),
                    )
                } else {
                    test_entry.decrease_expected_lamports(&fee_payer, LAMPORTS_PER_SIGNATURE * 2);

                    Transaction::new_signed_with_payer(
                        &instructions,
                        Some(&fee_payer),
                        &[&fee_payer_keypair, from_signer],
                        Hash::default(),
                    )
                };

                test_entry.push_transaction_with_status(transaction, expected_status);
                user_balance_history.push((transfer, user_balances.clone()));
            }

            // this block just updates the SvmTestEntry final account states to be accurate
            // doing this instead of skipping it, we validate that user_balances is definitely correct
            // because env.execute() will assert all these states match the final bank state
            if use_tokens {
                let mut token_account = token_account_for_tests();
                let mut token_buf = vec![0; TokenAccount::get_packed_len()];

                token_account.amount = *user_balances.get(&alice).unwrap();
                token_account.pack_into_slice(&mut token_buf);
                let final_token_state = AccountSharedData::create(
                    LAMPORTS_PER_SOL,
                    token_buf.clone(),
                    spl_token::id(),
                    false,
                    u64::MAX,
                );
                test_entry.update_expected_account_data(alice, &final_token_state);

                token_account.amount = *user_balances.get(&bob).unwrap();
                token_account.pack_into_slice(&mut token_buf);
                let final_token_state = AccountSharedData::create(
                    LAMPORTS_PER_SOL,
                    token_buf.clone(),
                    spl_token::id(),
                    false,
                    u64::MAX,
                );
                test_entry.update_expected_account_data(bob, &final_token_state);

                token_account.amount = *user_balances.get(&charlie).unwrap();
                token_account.pack_into_slice(&mut token_buf);
                let final_token_state = AccountSharedData::create(
                    LAMPORTS_PER_SOL,
                    token_buf.clone(),
                    spl_token::id(),
                    false,
                    u64::MAX,
                );
                test_entry.update_expected_account_data(charlie, &final_token_state);
            } else {
                let mut alice_final_state = native_state.clone();
                alice_final_state.set_lamports(*user_balances.get(&alice).unwrap());
                test_entry.update_expected_account_data(alice, &alice_final_state);

                let mut bob_final_state = native_state.clone();
                bob_final_state.set_lamports(*user_balances.get(&bob).unwrap());
                test_entry.update_expected_account_data(bob, &bob_final_state);

                let mut charlie_final_state = native_state.clone();
                charlie_final_state.set_lamports(*user_balances.get(&charlie).unwrap());
                test_entry.update_expected_account_data(charlie, &charlie_final_state);
            }

            // turn on balance recording and run the batch
            let mut env = SvmTestEnvironment::create(test_entry);
            env.processing_config
                .recording_config
                .enable_transaction_balance_recording = true;

            let batch_output = env.execute();
            let (pre_lamport_vecs, post_lamport_vecs, pre_token_vecs, post_token_vecs) =
                batch_output.balance_collector.unwrap().into_vecs();

            // first test the fee-payer balances
            let mut running_fee_payer_balance = STARTING_BALANCE;
            for (pre_bal, post_bal, was_discarded) in pre_lamport_vecs
                .iter()
                .zip(post_lamport_vecs.clone())
                .zip(transaction_discards)
                .map(|((pres, posts), discard)| (pres[0], posts[0], discard))
            {
                // we trigger discards with a non-existent fee-payer
                if was_discarded {
                    assert_eq!(pre_bal, 0);
                    assert_eq!(post_bal, 0);
                    continue;
                }

                let expected_post_balance = running_fee_payer_balance - LAMPORTS_PER_SIGNATURE * 2;

                assert_eq!(pre_bal, running_fee_payer_balance);
                assert_eq!(post_bal, expected_post_balance);

                running_fee_payer_balance = expected_post_balance;
            }

            // thanks to execute() we know user_balances is correct
            // now we test that every step in user_balance_history matches the svm recorded balances
            // in other words, the test effectively has three balance trackers and we can test they *all* agree
            // first get the collected balances in a manner that is system/token agnostic
            let (batch_pre, batch_post) = if use_tokens {
                let pre_tupls: Vec<_> = pre_token_vecs
                    .iter()
                    .map(|bals| (bals[0].amount, bals[1].amount))
                    .collect();

                let post_tupls: Vec<_> = post_token_vecs
                    .iter()
                    .map(|bals| (bals[0].amount, bals[1].amount))
                    .collect();

                (pre_tupls, post_tupls)
            } else {
                let pre_tupls: Vec<_> = pre_lamport_vecs
                    .iter()
                    .map(|bals| (bals[1], bals[2]))
                    .collect();

                let post_tupls: Vec<_> = post_lamport_vecs
                    .iter()
                    .map(|bals| (bals[1], bals[2]))
                    .collect();

                (pre_tupls, post_tupls)
            };

            // these two asserts are trivially true. we include them just to make it clearer what these vecs are
            // for n transactions, we have n pre-balance sets and n post-balance sets from svm
            // but we have *n+1* test balance sets: we push initial state, and then push post-tx bals once per tx
            // this mismatch is not strange at all. we also only have n+1 distinct svm timesteps despite 2n records
            // pre-balances: (0 1 2 3)
            // post-balances:  (1 2 3 4)
            // this does not mean time-overlapping svm records are equal. svm only captures the two accounts used by transfer
            // whereas our test balances capture all three accounts at every timestep, so we require no pre/post separation
            assert_eq!(user_balance_history.len(), batch_pre.len() + 1);
            assert_eq!(user_balance_history.len(), batch_post.len() + 1);

            // these are the real tests
            for (i, (svm_pre_balances, svm_post_balances)) in
                batch_pre.into_iter().zip(batch_post).enumerate()
            {
                let (_, ref expected_pre_balances) = user_balance_history[i];
                let (ref transfer, ref expected_post_balances) = user_balance_history[i + 1];

                assert_eq!(
                    svm_pre_balances.0,
                    *expected_pre_balances.get(&transfer.from).unwrap()
                );
                assert_eq!(
                    svm_pre_balances.1,
                    *expected_pre_balances.get(&transfer.to).unwrap()
                );

                assert_eq!(
                    svm_post_balances.0,
                    *expected_post_balances.get(&transfer.from).unwrap()
                );
                assert_eq!(
                    svm_post_balances.1,
                    *expected_post_balances.get(&transfer.to).unwrap()
                );
            }
        }
    }
}
