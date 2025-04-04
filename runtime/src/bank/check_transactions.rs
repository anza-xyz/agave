use {
    super::{Bank, BankStatusCache},
    agave_feature_set::FeatureSet,
    solana_accounts_db::blockhash_queue::BlockhashQueue,
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_fee::{calculate_fee_details, FeeFeatures},
    solana_perf::perf_libs,
    solana_program_runtime::execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::{
        account::AccountSharedData,
        account_utils::StateMut,
        clock::{
            MAX_PROCESSING_AGE, MAX_TRANSACTION_FORWARDING_DELAY,
            MAX_TRANSACTION_FORWARDING_DELAY_GPU,
        },
        fee::{FeeBudgetLimits, FeeDetails},
        nonce::{
            state::{
                Data as NonceData, DurableNonce, State as NonceState, Versions as NonceVersions,
            },
            NONCED_TX_MARKER_IX_INDEX,
        },
        nonce_account,
        pubkey::Pubkey,
        transaction::{Result as TransactionResult, TransactionError},
    },
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        nonce_info::NonceInfo,
        transaction_error_metrics::TransactionErrorMetrics,
    },
    solana_svm_transaction::svm_message::SVMMessage,
};

impl Bank {
    /// Checks a batch of sanitized transactions again bank for age and status
    pub fn check_transactions_with_forwarding_delay(
        &self,
        transactions: &[impl TransactionWithMeta],
        filter: &[TransactionResult<()>],
        forward_transactions_to_leader_at_slot_offset: u64,
    ) -> Vec<TransactionCheckResult> {
        let mut error_counters = TransactionErrorMetrics::default();
        // The following code also checks if the blockhash for a transaction is too old
        // The check accounts for
        //  1. Transaction forwarding delay
        //  2. The slot at which the next leader will actually process the transaction
        // Drop the transaction if it will expire by the time the next node receives and processes it
        let api = perf_libs::api();
        let max_tx_fwd_delay = if api.is_none() {
            MAX_TRANSACTION_FORWARDING_DELAY
        } else {
            MAX_TRANSACTION_FORWARDING_DELAY_GPU
        };

        self.check_transactions(
            transactions,
            filter,
            (MAX_PROCESSING_AGE)
                .saturating_sub(max_tx_fwd_delay)
                .saturating_sub(forward_transactions_to_leader_at_slot_offset as usize),
            &mut error_counters,
        )
    }

    pub fn check_transactions<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        let lock_results = self.check_age_and_compute_budget_limits(
            sanitized_txs,
            lock_results,
            max_age,
            error_counters,
        );
        self.check_status_cache(sanitized_txs, lock_results, error_counters)
    }

    fn check_age_and_compute_budget_limits<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        let hash_queue = self.blockhash_queue.read().unwrap();
        let last_blockhash = hash_queue.last_hash();
        let next_durable_nonce = DurableNonce::from_blockhash(&last_blockhash);
        // safe so long as the BlockhashQueue is consistent
        let next_lamports_per_signature = hash_queue
            .get_lamports_per_signature(&last_blockhash)
            .unwrap();

        let feature_set: &FeatureSet = &self.feature_set;
        let fee_features = FeeFeatures::from(feature_set);

        sanitized_txs
            .iter()
            .zip(lock_results)
            .map(|(tx, lock_res)| match lock_res {
                Ok(()) => {
                    let compute_budget_and_limits = process_compute_budget_instructions(
                        tx.borrow().program_instructions_iter(),
                        feature_set,
                    )
                    .map(|limit| {
                        let fee_budget = FeeBudgetLimits::from(limit);
                        let fee_details = calculate_fee_details(
                            tx.borrow(),
                            false,
                            self.fee_structure.lamports_per_signature,
                            fee_budget.prioritization_fee,
                            fee_features,
                        );
                        if let Some(compute_budget) = self.compute_budget {
                            // This block of code is only necessary to retain legacy behavior of the code.
                            // It should be removed along with the change to favor transaction's compute budget limits
                            // over configured compute budget in Bank.
                            compute_budget.get_compute_budget_and_limits(
                                fee_budget.loaded_accounts_data_size_limit,
                                fee_details,
                            )
                        } else {
                            limit.get_compute_budget_and_limits(
                                fee_budget.loaded_accounts_data_size_limit,
                                fee_details,
                            )
                        }
                    });
                    self.check_transaction_age(
                        tx.borrow(),
                        max_age,
                        &next_durable_nonce,
                        &hash_queue,
                        next_lamports_per_signature,
                        error_counters,
                        compute_budget_and_limits,
                    )
                }
                Err(e) => Err(e.clone()),
            })
            .collect()
    }

    fn checked_transactions_details_with_test_override(
        nonce: Option<NonceInfo>,
        lamports_per_signature: u64,
        compute_budget_and_limits: Result<
            SVMTransactionExecutionAndFeeBudgetLimits,
            TransactionError,
        >,
    ) -> CheckedTransactionDetails {
        let compute_budget_and_limits = if lamports_per_signature == 0 {
            // This is done to support legacy tests. The tests should be updated, and check
            // for 0 lamports_per_signature should be removed from the code.
            compute_budget_and_limits.map(|v| SVMTransactionExecutionAndFeeBudgetLimits {
                budget: v.budget,
                loaded_accounts_data_size_limit: v.loaded_accounts_data_size_limit,
                fee_details: FeeDetails::default(),
            })
        } else {
            compute_budget_and_limits
        };
        CheckedTransactionDetails::new(nonce, compute_budget_and_limits)
    }

    fn check_transaction_age(
        &self,
        tx: &impl SVMMessage,
        max_age: usize,
        next_durable_nonce: &DurableNonce,
        hash_queue: &BlockhashQueue,
        next_lamports_per_signature: u64,
        error_counters: &mut TransactionErrorMetrics,
        compute_budget: Result<SVMTransactionExecutionAndFeeBudgetLimits, TransactionError>,
    ) -> TransactionCheckResult {
        let recent_blockhash = tx.recent_blockhash();
        if let Some(hash_info) = hash_queue.get_hash_info_if_valid(recent_blockhash, max_age) {
            Ok(Self::checked_transactions_details_with_test_override(
                None,
                hash_info.lamports_per_signature(),
                compute_budget,
            ))
        } else if let Some((nonce, previous_lamports_per_signature)) = self
            .check_load_and_advance_message_nonce_account(
                tx,
                next_durable_nonce,
                next_lamports_per_signature,
            )
        {
            Ok(Self::checked_transactions_details_with_test_override(
                Some(nonce),
                previous_lamports_per_signature,
                compute_budget,
            ))
        } else {
            error_counters.blockhash_not_found += 1;
            Err(TransactionError::BlockhashNotFound)
        }
    }

    pub(super) fn check_load_and_advance_message_nonce_account(
        &self,
        message: &impl SVMMessage,
        next_durable_nonce: &DurableNonce,
        next_lamports_per_signature: u64,
    ) -> Option<(NonceInfo, u64)> {
        let nonce_is_advanceable = message.recent_blockhash() != next_durable_nonce.as_hash();
        if !nonce_is_advanceable {
            return None;
        }

        let (nonce_address, mut nonce_account, nonce_data) =
            self.load_message_nonce_account(message)?;

        let previous_lamports_per_signature = nonce_data.get_lamports_per_signature();
        let next_nonce_state = NonceState::new_initialized(
            &nonce_data.authority,
            *next_durable_nonce,
            next_lamports_per_signature,
        );
        nonce_account
            .set_state(&NonceVersions::new(next_nonce_state))
            .ok()?;

        Some((
            NonceInfo::new(nonce_address, nonce_account),
            previous_lamports_per_signature,
        ))
    }

    pub(super) fn load_message_nonce_account(
        &self,
        message: &impl SVMMessage,
    ) -> Option<(Pubkey, AccountSharedData, NonceData)> {
        let require_static_nonce_account = self
            .feature_set
            .is_active(&agave_feature_set::require_static_nonce_account::id());
        let nonce_address = message.get_durable_nonce(require_static_nonce_account)?;
        let nonce_account = self.get_account_with_fixed_root(nonce_address)?;
        let nonce_data =
            nonce_account::verify_nonce_account(&nonce_account, message.recent_blockhash())?;

        let nonce_is_authorized = message
            .get_ix_signers(NONCED_TX_MARKER_IX_INDEX as usize)
            .any(|signer| signer == &nonce_data.authority);
        if !nonce_is_authorized {
            return None;
        }

        Some((*nonce_address, nonce_account, nonce_data))
    }

    fn check_status_cache<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: Vec<TransactionCheckResult>,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        // Do allocation before acquiring the lock on the status cache.
        let mut check_results = Vec::with_capacity(sanitized_txs.len());
        let rcache = self.status_cache.read().unwrap();

        check_results.extend(sanitized_txs.iter().zip(lock_results).map(
            |(sanitized_tx, lock_result)| {
                let sanitized_tx = sanitized_tx.borrow();
                if lock_result.is_ok()
                    && self.is_transaction_already_processed(sanitized_tx, &rcache)
                {
                    error_counters.already_processed += 1;
                    return Err(TransactionError::AlreadyProcessed);
                }

                lock_result
            },
        ));
        check_results
    }

    fn is_transaction_already_processed(
        &self,
        sanitized_tx: &impl TransactionWithMeta,
        status_cache: &BankStatusCache,
    ) -> bool {
        let key = sanitized_tx.message_hash();
        let transaction_blockhash = sanitized_tx.recent_blockhash();
        status_cache
            .get_status(key, transaction_blockhash, &self.ancestors)
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::tests::{
            get_nonce_blockhash, get_nonce_data_from_account, new_sanitized_message,
            setup_nonce_with_bank,
        },
        solana_sdk::{
            hash::Hash,
            instruction::CompiledInstruction,
            message::{
                v0::{self, LoadedAddresses, MessageAddressTableLookup},
                Message, MessageHeader, SanitizedMessage, SanitizedVersionedMessage,
                SimpleAddressLoader, VersionedMessage,
            },
            signature::Keypair,
            signer::Signer,
            system_instruction::{self, SystemInstruction},
            system_program,
        },
        std::collections::HashSet,
        test_case::test_case,
    };

    #[test]
    fn test_check_and_load_message_nonce_account_ok() {
        const STALE_LAMPORTS_PER_SIGNATURE: u64 = 42;
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();

        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        let message = new_sanitized_message(Message::new_with_blockhash(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
                system_instruction::transfer(&custodian_pubkey, &nonce_pubkey, 100_000),
            ],
            Some(&custodian_pubkey),
            &nonce_hash,
        ));

        // set a spurious lamports_per_signature value
        let mut nonce_account = bank.get_account(&nonce_pubkey).unwrap();
        let nonce_data = get_nonce_data_from_account(&nonce_account).unwrap();
        nonce_account
            .set_state(&NonceVersions::new(NonceState::new_initialized(
                &nonce_data.authority,
                nonce_data.durable_nonce,
                STALE_LAMPORTS_PER_SIGNATURE,
            )))
            .unwrap();
        bank.store_account(&nonce_pubkey, &nonce_account);

        let nonce_account = bank.get_account(&nonce_pubkey).unwrap();
        let (_, next_lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        let mut expected_nonce_info = NonceInfo::new(nonce_pubkey, nonce_account);
        expected_nonce_info
            .try_advance_nonce(bank.next_durable_nonce(), next_lamports_per_signature)
            .unwrap();

        // we now expect to:
        // * advance the nonce account to the current durable nonce value
        // * set the blockhash queue's last blockhash's lamports_per_signature value in the nonce data
        // * retrieve the previous lamports_per_signature value set on the nonce data for transaction fee checks
        assert_eq!(
            bank.check_load_and_advance_message_nonce_account(
                &message,
                &bank.next_durable_nonce(),
                next_lamports_per_signature
            ),
            Some((expected_nonce_info, STALE_LAMPORTS_PER_SIGNATURE)),
        );
    }

    #[test]
    fn test_check_and_load_message_nonce_account_not_nonce_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();

        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        let message = new_sanitized_message(Message::new_with_blockhash(
            &[
                system_instruction::transfer(&custodian_pubkey, &nonce_pubkey, 100_000),
                system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            ],
            Some(&custodian_pubkey),
            &nonce_hash,
        ));
        let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        assert!(bank
            .check_load_and_advance_message_nonce_account(
                &message,
                &bank.next_durable_nonce(),
                lamports_per_signature
            )
            .is_none());
    }

    #[test]
    fn test_check_and_load_message_nonce_account_missing_ix_pubkey_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();

        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        let mut message = Message::new_with_blockhash(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
                system_instruction::transfer(&custodian_pubkey, &nonce_pubkey, 100_000),
            ],
            Some(&custodian_pubkey),
            &nonce_hash,
        );
        message.instructions[0].accounts.clear();
        let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        assert!(bank
            .check_load_and_advance_message_nonce_account(
                &new_sanitized_message(message),
                &bank.next_durable_nonce(),
                lamports_per_signature,
            )
            .is_none());
    }

    #[test]
    fn test_check_and_load_message_nonce_account_nonce_acc_does_not_exist_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();
        let missing_keypair = Keypair::new();
        let missing_pubkey = missing_keypair.pubkey();

        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        let message = new_sanitized_message(Message::new_with_blockhash(
            &[
                system_instruction::advance_nonce_account(&missing_pubkey, &nonce_pubkey),
                system_instruction::transfer(&custodian_pubkey, &nonce_pubkey, 100_000),
            ],
            Some(&custodian_pubkey),
            &nonce_hash,
        ));
        let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        assert!(bank
            .check_load_and_advance_message_nonce_account(
                &message,
                &bank.next_durable_nonce(),
                lamports_per_signature
            )
            .is_none());
    }

    #[test]
    fn test_check_and_load_message_nonce_account_bad_tx_hash_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();

        let message = new_sanitized_message(Message::new_with_blockhash(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
                system_instruction::transfer(&custodian_pubkey, &nonce_pubkey, 100_000),
            ],
            Some(&custodian_pubkey),
            &Hash::default(),
        ));
        let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        assert!(bank
            .check_load_and_advance_message_nonce_account(
                &message,
                &bank.next_durable_nonce(),
                lamports_per_signature,
            )
            .is_none());
    }

    #[test_case(true; "test_check_and_load_message_nonce_account_nonce_is_alt_disallowed")]
    #[test_case(false; "test_check_and_load_message_nonce_account_nonce_is_alt_allowed")]
    fn test_check_and_load_message_nonce_account_nonce_is_alt(require_static_nonce_account: bool) {
        let feature_set = if require_static_nonce_account {
            FeatureSet::all_enabled()
        } else {
            FeatureSet::default()
        };
        let nonce_authority = Pubkey::new_unique();
        let (bank, _mint_keypair, _custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            Some(nonce_authority),
            feature_set,
        )
        .unwrap();

        let nonce_pubkey = nonce_keypair.pubkey();
        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        let loaded_addresses = LoadedAddresses {
            writable: vec![nonce_pubkey],
            readonly: vec![],
        };

        let message = SanitizedMessage::try_new(
            SanitizedVersionedMessage::try_new(VersionedMessage::V0(v0::Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                account_keys: vec![nonce_authority, system_program::id()],
                recent_blockhash: nonce_hash,
                instructions: vec![CompiledInstruction::new(
                    1, // index of system program
                    &SystemInstruction::AdvanceNonceAccount,
                    vec![
                        2, // index of alt nonce account
                        0, // index of nonce_authority
                    ],
                )],
                address_table_lookups: vec![MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: (0..loaded_addresses.writable.len())
                        .map(|x| x as u8)
                        .collect(),
                    readonly_indexes: (0..loaded_addresses.readonly.len())
                        .map(|x| (loaded_addresses.writable.len() + x) as u8)
                        .collect(),
                }],
            }))
            .unwrap(),
            SimpleAddressLoader::Enabled(loaded_addresses),
            &HashSet::new(),
        )
        .unwrap();

        let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
        assert_eq!(
            bank.check_load_and_advance_message_nonce_account(
                &message,
                &bank.next_durable_nonce(),
                lamports_per_signature
            )
            .is_none(),
            require_static_nonce_account,
        );
    }
}
