use {
    super::{Bank, BankStatusCache},
    agave_feature_set::FeatureSet,
    solana_account::ReadableAccount,
    solana_accounts_db::blockhash_queue::BlockhashQueue,
    solana_clock::{MAX_TRANSACTION_FORWARDING_DELAY, Slot},
    solana_compute_budget::compute_budget::SVMTransactionExecutionBudget,
    solana_fee::calculate_fee_details,
    solana_nonce::{
        NONCED_TX_MARKER_IX_INDEX,
        state::{Data as NonceData, DurableNonce, State as NonceState},
    },
    solana_nonce_account as nonce_account,
    solana_program_runtime::execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        transaction_error_metrics::TransactionErrorMetrics,
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction::versioned::TransactionVersion,
    solana_transaction_error::{TransactionError, TransactionResult},
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
        let max_tx_fwd_delay = MAX_TRANSACTION_FORWARDING_DELAY;

        self.check_transactions(
            transactions,
            filter,
            self.max_processing_age()
                .saturating_sub(max_tx_fwd_delay)
                .saturating_sub(forward_transactions_to_leader_at_slot_offset as usize),
            false,
            &mut error_counters,
        )
    }

    pub fn check_transactions<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        strict_nonce_size_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        self.check_transactions_with_processed_slots(
            sanitized_txs,
            lock_results,
            max_age,
            false,
            strict_nonce_size_check,
            error_counters,
        )
        .0
    }

    /// Checks a sanitized transaction against the bank for age,
    /// without checking the status cache. This is a leader-only
    /// function and must not be used in replay without a feature gate.
    pub fn check_transaction_without_status_cache(
        &self,
        tx: &impl SVMMessage,
        max_age: usize,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<Option<Pubkey>> {
        let feature_set: &FeatureSet = &self.feature_set;
        let feature_snapshot = feature_set.snapshot();
        let enable_tx_v1 = feature_snapshot.enable_tx_v1;

        if !enable_tx_v1 && tx.version() == TransactionVersion::Number(1) {
            return Err(TransactionError::UnsupportedVersion);
        }

        let hash_queue = self.blockhash_queue.read().unwrap();
        let last_blockhash = hash_queue.last_hash();
        let next_durable_nonce = DurableNonce::from_blockhash(&last_blockhash);

        self.check_transaction_age(
            tx,
            max_age,
            &next_durable_nonce,
            &hash_queue,
            error_counters,
            true, // strict_nonce_size_check
            true, // strict_nonce_authority_check
        )
    }

    pub fn check_transactions_with_processed_slots<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        collect_processed_slots: bool,
        strict_nonce_size_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> (Vec<TransactionCheckResult>, Option<Vec<Option<Slot>>>) {
        let lock_results = self.filter_v1_transactions(sanitized_txs, lock_results);

        let lock_results = self.check_age_and_compute_budget_limits(
            sanitized_txs,
            lock_results,
            max_age,
            strict_nonce_size_check,
            error_counters,
        );
        self.check_status_cache(
            sanitized_txs,
            lock_results,
            collect_processed_slots,
            error_counters,
        )
    }

    fn filter_v1_transactions<'a, Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &'a [impl core::borrow::Borrow<Tx>],
        lock_results: &'a [TransactionResult<()>],
    ) -> impl Iterator<Item = TransactionResult<()>> + 'a {
        let enable_tx_v1 = self.feature_set.snapshot().enable_tx_v1;
        // Discard v1 transactions until feature gate is activated.
        sanitized_txs
            .iter()
            .zip(lock_results)
            .map(move |(tx, lock_result)| match lock_result {
                Err(err) => Err(err.clone()),
                Ok(())
                    if !enable_tx_v1 && tx.borrow().version() == TransactionVersion::Number(1) =>
                {
                    Err(TransactionError::UnsupportedVersion)
                }
                Ok(()) => Ok(()),
            })
    }

    fn check_age_and_compute_budget_limits<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: impl IntoIterator<Item = TransactionResult<()>>,
        max_age: usize,
        strict_nonce_size_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        let hash_queue = self.blockhash_queue.read().unwrap();
        let last_blockhash = hash_queue.last_hash();
        let next_durable_nonce = DurableNonce::from_blockhash(&last_blockhash);

        let feature_set: &FeatureSet = &self.feature_set;
        let feature_snapshot = feature_set.snapshot();
        let fee_features = self.fee_features();

        let raise_cpi_limit = feature_snapshot.raise_cpi_nesting_limit_to_8;

        sanitized_txs
            .iter()
            .zip(lock_results)
            .map(|(tx, lock_res)| match lock_res {
                Ok(()) => {
                    let compute_budget_and_limits = tx
                        .borrow()
                        .transaction_configuration(feature_set)
                        .map(|config| {
                            let fee_details = calculate_fee_details(
                                tx.borrow(),
                                self.fee_structure.lamports_per_signature,
                                config.priority_fee_lamports,
                                fee_features,
                            );
                            if let Some(compute_budget) = self.compute_budget {
                                // This block of code is only necessary to retain legacy behavior of the code.
                                // It should be removed along with the change to favor transaction's compute budget limits
                                // over configured compute budget in Bank.
                                compute_budget.get_compute_budget_and_limits(
                                    config.loaded_accounts_data_size_limit,
                                    fee_details,
                                )
                            } else {
                                SVMTransactionExecutionAndFeeBudgetLimits {
                                    budget: SVMTransactionExecutionBudget {
                                        compute_unit_limit: u64::from(config.compute_unit_limit),
                                        heap_size: config.updated_heap_bytes,
                                        ..SVMTransactionExecutionBudget::new_with_defaults(
                                            raise_cpi_limit,
                                        )
                                    },
                                    loaded_accounts_data_size_limit: config
                                        .loaded_accounts_data_size_limit,
                                    fee_details,
                                }
                            }
                        })
                        .inspect_err(|_err| {
                            error_counters.invalid_compute_budget += 1;
                        })?;

                    let nonce_address = self.check_transaction_age(
                        tx.borrow(),
                        max_age,
                        &next_durable_nonce,
                        &hash_queue,
                        error_counters,
                        strict_nonce_size_check,
                        false,
                    )?;

                    Ok(CheckedTransactionDetails::new(
                        nonce_address,
                        compute_budget_and_limits,
                    ))
                }
                Err(e) => Err(e),
            })
            .collect()
    }

    fn check_transaction_age(
        &self,
        tx: &impl SVMMessage,
        max_age: usize,
        next_durable_nonce: &DurableNonce,
        hash_queue: &BlockhashQueue,
        error_counters: &mut TransactionErrorMetrics,
        strict_nonce_size_check: bool,
        strict_nonce_authority_check: bool,
    ) -> TransactionResult<Option<Pubkey>> {
        let recent_blockhash = tx.recent_blockhash();
        if hash_queue
            .get_hash_info_if_valid(recent_blockhash, max_age)
            .is_some()
        {
            Ok(None)
        } else if let Some((nonce_address, _)) = self.check_nonce_transaction_validity(
            tx,
            next_durable_nonce,
            strict_nonce_size_check,
            strict_nonce_authority_check,
        ) {
            Ok(Some(nonce_address))
        } else {
            error_counters.blockhash_not_found += 1;
            Err(TransactionError::BlockhashNotFound)
        }
    }

    pub(super) fn check_nonce_transaction_validity(
        &self,
        message: &impl SVMMessage,
        next_durable_nonce: &DurableNonce,
        strict_nonce_size_check: bool,
        strict_nonce_authority_check: bool,
    ) -> Option<(Pubkey, u64)> {
        let nonce_is_advanceable = message.recent_blockhash() != next_durable_nonce.as_hash();
        if !nonce_is_advanceable {
            return None;
        }

        let (nonce_address, nonce_data) =
            self.load_message_nonce_data(message, strict_nonce_size_check)?;

        if strict_nonce_authority_check
            && !message
                .get_ix_signers(NONCED_TX_MARKER_IX_INDEX as usize)
                .any(|signer| signer == &nonce_data.authority)
        {
            return None;
        }

        let previous_lamports_per_signature = nonce_data.get_lamports_per_signature();

        Some((nonce_address, previous_lamports_per_signature))
    }

    pub(super) fn load_message_nonce_data(
        &self,
        message: &impl SVMMessage,
        strict_nonce_size_check: bool,
    ) -> Option<(Pubkey, NonceData)> {
        let nonce_address = message.get_durable_nonce()?;
        let nonce_account = self.get_account_with_fixed_root(nonce_address)?;
        if strict_nonce_size_check && nonce_account.data().len() != NonceState::size() {
            return None;
        }
        let nonce_data =
            nonce_account::verify_nonce_account(&nonce_account, message.recent_blockhash())?;

        Some((*nonce_address, nonce_data))
    }

    fn check_status_cache<Tx: TransactionWithMeta>(
        &self,
        sanitized_txs: &[impl core::borrow::Borrow<Tx>],
        mut lock_results: Vec<TransactionCheckResult>,
        collect_processed_slots: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> (Vec<TransactionCheckResult>, Option<Vec<Option<Slot>>>) {
        // Do allocation before acquiring the lock on the status cache.
        let mut processed_slots = if collect_processed_slots {
            Some(Vec::with_capacity(sanitized_txs.len()))
        } else {
            None
        };
        let rcache = self.status_cache.read().unwrap();

        for (sanitized_tx_ref, lock_result) in sanitized_txs.iter().zip(lock_results.iter_mut()) {
            let processed_slot = if lock_result.is_ok() {
                self.get_processed_slot(sanitized_tx_ref.borrow(), &rcache)
            } else {
                None
            };

            if processed_slot.is_some() {
                error_counters.already_processed += 1;
                *lock_result = Err(TransactionError::AlreadyProcessed);
            }

            if let Some(processed_slots) = processed_slots.as_mut() {
                processed_slots.push(processed_slot)
            }
        }

        (lock_results, processed_slots)
    }

    fn get_processed_slot(
        &self,
        sanitized_tx: &impl TransactionWithMeta,
        status_cache: &BankStatusCache,
    ) -> Option<Slot> {
        let key = sanitized_tx.message_hash();
        let transaction_blockhash = sanitized_tx.recent_blockhash();
        status_cache
            .get_status(key, transaction_blockhash, &self.ancestors)
            .map(|status| status.0)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::{
            ReservedAccountKeys,
            tests::{
                get_nonce_blockhash, get_nonce_data_from_account, new_sanitized_message,
                setup_nonce_with_bank,
            },
        },
        solana_account::{
            AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMut,
        },
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            Message, MessageHeader, SanitizedMessage, SanitizedVersionedMessage,
            SimpleAddressLoader, VersionedMessage,
            compiled_instruction::CompiledInstruction,
            v0::{self, LoadedAddresses, MessageAddressTableLookup},
            v1,
        },
        solana_nonce::{state::State as NonceState, versions::Versions as NonceVersions},
        solana_runtime_transaction::{
            runtime_transaction::RuntimeTransaction, transaction_meta::TransactionMeta,
        },
        solana_signer::Signer,
        solana_svm_transaction::svm_message::SVMStaticMessage,
        solana_system_interface::{
            instruction::{self as system_instruction, SystemInstruction},
            program as system_program,
        },
        solana_transaction::{
            sanitized::{MessageHash, SanitizedTransaction},
            versioned::VersionedTransaction,
        },
        std::collections::HashSet,
    };

    #[test]
    fn test_check_nonce_transaction_validity_ok() {
        const STALE_LAMPORTS_PER_SIGNATURE: u64 = 42;
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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

        assert_eq!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                false,
                false
            ),
            Some((nonce_pubkey, STALE_LAMPORTS_PER_SIGNATURE)),
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_not_nonce_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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
        assert!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                false,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_strict_nonce_size_check_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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

        let nonce_account = bank.get_account(&nonce_pubkey).unwrap();
        let mut resized_nonce_account = AccountSharedData::new(
            nonce_account.lamports(),
            NonceState::size() + 1,
            nonce_account.owner(),
        );
        resized_nonce_account.data_as_mut_slice()[..nonce_account.data().len()]
            .copy_from_slice(nonce_account.data());
        bank.store_account(&nonce_pubkey, &resized_nonce_account);

        assert!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                true,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_missing_ix_pubkey_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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
        assert!(
            bank.check_nonce_transaction_validity(
                &new_sanitized_message(message),
                &bank.next_durable_nonce(),
                false,
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_nonce_acc_does_not_exist_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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
        assert!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                false,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_bad_tx_hash_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
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
        assert!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                false,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_nonce_is_alt() {
        let nonce_authority = Pubkey::new_unique();
        let (bank, _mint_keypair, _custodian_keypair, nonce_keypair, _) = setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            Some(nonce_authority),
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

        assert_eq!(
            bank.check_nonce_transaction_validity(
                &message,
                &bank.next_durable_nonce(),
                false,
                false
            ),
            None,
        );
    }

    fn make_test_tx(version: TransactionVersion) -> impl TransactionWithMeta {
        make_test_tx_with_blockhash(version, Hash::new_unique())
    }

    fn make_test_tx_with_blockhash(
        version: TransactionVersion,
        recent_blockhash: Hash,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1);

        let message = match version {
            TransactionVersion::LEGACY => VersionedMessage::Legacy(Message::new_with_blockhash(
                &[ix],
                Some(&payer.pubkey()),
                &recent_blockhash,
            )),
            TransactionVersion::Number(0) => VersionedMessage::V0(
                v0::Message::try_compile(&payer.pubkey(), &[ix], &[], recent_blockhash).unwrap(),
            ),
            TransactionVersion::Number(1) => VersionedMessage::V1(
                v1::Message::try_compile(&payer.pubkey(), &[ix], recent_blockhash).unwrap(),
            ),
            TransactionVersion::Number(other) => {
                panic!("unsupported test transaction version: {other}")
            }
        };

        let tx = VersionedTransaction::try_new(message, &[&payer]).unwrap();
        // Note: enabled loader is needed to create v0 runtime-transaction
        let address_loader =
            solana_message::SimpleAddressLoader::Enabled(solana_message::v0::LoadedAddresses {
                writable: vec![],
                readonly: vec![],
            });
        let rt = RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            None,
            address_loader,
            &ReservedAccountKeys::empty_key_set(),
        );
        rt.unwrap()
    }

    #[test]
    fn test_check_transaction_without_status_cache_allows_already_processed() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let bank = Bank::new_for_tests(&genesis_config);
        let tx = make_test_tx_with_blockhash(TransactionVersion::LEGACY, bank.last_blockhash());

        bank.status_cache.write().unwrap().insert(
            tx.recent_blockhash(),
            tx.message_hash(),
            bank.slot(),
            Ok(()),
        );

        let lock_results = [Ok(())];
        let mut error_counters = TransactionErrorMetrics::default();
        let check_results = bank.check_transactions(
            std::slice::from_ref(&tx),
            &lock_results,
            bank.max_processing_age(),
            true,
            &mut error_counters,
        );
        assert!(matches!(
            check_results.as_slice(),
            [Err(TransactionError::AlreadyProcessed)]
        ));

        let mut error_counters = TransactionErrorMetrics::default();
        let check_result = bank.check_transaction_without_status_cache(
            &tx,
            bank.max_processing_age(),
            &mut error_counters,
        );
        assert_eq!(check_result, Ok(None));
    }

    #[test]
    fn test_filter_v1_transactions_keeps_existing_errors() {
        let txs = vec![
            make_test_tx(TransactionVersion::LEGACY),
            make_test_tx(TransactionVersion::Number(0)),
            make_test_tx(TransactionVersion::Number(1)),
        ];
        let lock_results = vec![
            Err(TransactionError::AccountInUse),
            Err(TransactionError::TooManyAccountLocks),
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
        ];

        let filtered = Bank::default_for_tests().filter_v1_transactions(&txs, &lock_results);

        assert!(filtered.eq(lock_results.iter().cloned()));
    }

    #[test]
    fn test_filter_v1_transactions_rejects_v1_with_ok_lock_result() {
        let txs = vec![make_test_tx(TransactionVersion::Number(1))];
        let lock_results = vec![Ok(())];

        let filtered = Bank::default_for_tests().filter_v1_transactions(&txs, &lock_results);

        assert!(filtered.eq([Err(TransactionError::UnsupportedVersion)]));
    }

    #[test]
    fn test_filter_v1_transactions_keeps_v1_when_feature_enabled() {
        let txs = vec![make_test_tx(TransactionVersion::Number(1))];
        let lock_results = vec![Ok(())];
        let mut bank = Bank::default_for_tests();
        bank.activate_feature(&agave_feature_set::enable_tx_v1::id());

        let filtered = bank.filter_v1_transactions(&txs, &lock_results);

        assert!(filtered.eq([Ok(())]));
    }

    #[test]
    fn test_filter_v1_transactions_keeps_legacy_and_v0_ok() {
        let txs = vec![
            make_test_tx(TransactionVersion::LEGACY),
            make_test_tx(TransactionVersion::Number(0)),
        ];
        let lock_results = vec![Ok(()), Ok(())];

        let filtered = Bank::default_for_tests().filter_v1_transactions(&txs, &lock_results);

        assert!(filtered.eq([Ok(()), Ok(())]));
    }

    #[test]
    fn test_filter_v1_transactions_mixed_results() {
        let txs = vec![
            make_test_tx(TransactionVersion::LEGACY),
            make_test_tx(TransactionVersion::Number(1)),
            make_test_tx(TransactionVersion::Number(0)),
            make_test_tx(TransactionVersion::Number(1)),
        ];
        let lock_results = vec![
            Ok(()),
            Ok(()),
            Err(TransactionError::AccountInUse),
            Err(TransactionError::TooManyAccountLocks),
        ];

        let filtered = Bank::default_for_tests().filter_v1_transactions(&txs, &lock_results);

        assert!(filtered.eq([
            Ok(()),
            Err(TransactionError::UnsupportedVersion),
            Err(TransactionError::AccountInUse),
            Err(TransactionError::TooManyAccountLocks),
        ]));
    }
}
