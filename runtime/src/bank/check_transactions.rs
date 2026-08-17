use {
    super::{Bank, BankStatusCache},
    solana_account::ReadableAccount,
    solana_accounts_db::blockhash_queue::BlockhashQueue,
    solana_clock::Slot,
    solana_compute_budget::compute_budget::SVMTransactionExecutionBudget,
    solana_fee::calculate_fee_details,
    solana_nonce::{
        NONCED_TX_MARKER_IX_INDEX,
        state::{Data as NonceData, DurableNonce, State as NonceState},
    },
    solana_nonce_account as nonce_account,
    solana_program_runtime::execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::{
        StaticMessageWithMeta, TransactionWithMeta,
    },
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        transaction_error_metrics::TransactionErrorMetrics,
    },
    solana_svm_transaction::svm_message::{SVMMessage, SVMStaticMessage},
    solana_transaction::versioned::TransactionVersion,
    solana_transaction_error::{TransactionError, TransactionResult},
};

impl Bank {
    /// A single-transaction check function that validates nonces with strict size and
    /// authority, returning the validated nonce address, and does not check status cache.
    pub fn check_transaction_without_status_cache(
        &self,
        tx: &impl TransactionWithMeta,
        max_age: usize,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<Option<Pubkey>> {
        self.check_v1_enabled(tx)?;

        self.check_compute_budget_and_limits(tx, error_counters)?;

        let hash_queue = self.blockhash_queue.read().unwrap();
        let next_durable_nonce = hash_queue.next_durable_nonce();

        if self.check_blockhash_age(tx, max_age, &hash_queue) {
            return Ok(None);
        }

        if let Some(nonce_address) = self.check_nonce_semantics(tx, &next_durable_nonce)
            && self.check_nonce_account(tx, nonce_address, true).is_some()
        {
            return Ok(Some(nonce_address));
        }

        error_counters.blockhash_not_found += 1;
        Err(TransactionError::BlockhashNotFound)
    }

    /// The consensus check function that runs before SVM in both block-production and replay.
    pub(super) fn check_transactions_before_execution<Tx: TransactionWithMeta>(
        &self,
        txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionCheckResult> {
        self.check_transactions(txs, lock_results, max_age, false, false, error_counters)
            .0
    }

    /// External interface for our check function, hiding `strict_nonce_checks`,
    /// which is always true for non-consensus-related uses.
    pub fn check_transactions_external<Tx: TransactionWithMeta>(
        &self,
        txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        collect_processed_slots: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> (Vec<TransactionCheckResult>, Option<Vec<Option<Slot>>>) {
        self.check_transactions(
            txs,
            lock_results,
            max_age,
            collect_processed_slots,
            true,
            error_counters,
        )
    }

    // The heart of runtime transaction checking. We perform these operations, in sequence:
    // * Reject V1 transactions until the feature is active.
    //   This can be deleted after the feature is live on all clusters.
    // * Parse and validate the compute budget and limits, producing the struct for SVM,
    //   or reject the transaction if the compute budget is malformed.
    // * Check the transaction lifetime specifier, in this order:
    //   - First, check if the lifetime specifier is one of the last 151 blockhashes.
    //     If so, the transaction is valid as a normal blockhash transaction.
    //   - If not, check whether the transaction is structurally valid as a nonce transaction.
    //     This depends on no account state but does depend on ALT resolution due to write demotion.
    //     Then, load the nonce account and provisionally validate the nonce can be advanced.
    //   - If neither condition holds, reject the transaction.
    // * Check if the transaction message hash is present in the StatusCache.
    //   Reject the transaction as AlreadyProcessed if present.
    //
    // The options collect_processed_slots and strict_nonce_checks are not part of consensus.
    // We include everything in one omnibus function to have one clear implementation.
    fn check_transactions<Tx: TransactionWithMeta>(
        &self,
        txs: &[impl core::borrow::Borrow<Tx>],
        lock_results: &[TransactionResult<()>],
        max_age: usize,
        collect_processed_slots: bool,
        strict_nonce_checks: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> (Vec<TransactionCheckResult>, Option<Vec<Option<Slot>>>) {
        let check_results: Vec<TransactionCheckResult> = {
            let hash_queue = self.blockhash_queue.read().unwrap();
            let next_durable_nonce = hash_queue.next_durable_nonce();

            txs.iter()
                .zip(lock_results)
                .map(|(tx, lock_result)| {
                    let tx = tx.borrow();
                    lock_result.clone()?;

                    self.check_v1_enabled(tx)?;

                    let compute_budget_and_limits =
                        self.check_compute_budget_and_limits(tx, error_counters)?;

                    if self.check_blockhash_age(tx, max_age, &hash_queue) {
                        return Ok(CheckedTransactionDetails::new(
                            None,
                            compute_budget_and_limits,
                        ));
                    }

                    if let Some(nonce_address) = self.check_nonce_semantics(tx, &next_durable_nonce)
                        && self
                            .check_nonce_account(tx, nonce_address, strict_nonce_checks)
                            .is_some()
                    {
                        return Ok(CheckedTransactionDetails::new(
                            Some(nonce_address),
                            compute_budget_and_limits,
                        ));
                    }

                    error_counters.blockhash_not_found += 1;
                    Err(TransactionError::BlockhashNotFound)
                })
                .collect()
        };

        self.check_status_cache(txs, check_results, collect_processed_slots, error_counters)
    }

    fn check_v1_enabled(&self, tx: &impl SVMStaticMessage) -> TransactionResult<()> {
        let enable_tx_v1 = self.feature_set.snapshot().enable_tx_v1;

        if !enable_tx_v1 && tx.version() == TransactionVersion::Number(1) {
            Err(TransactionError::UnsupportedVersion)
        } else {
            Ok(())
        }
    }

    fn check_compute_budget_and_limits(
        &self,
        tx: &impl StaticMessageWithMeta,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<SVMTransactionExecutionAndFeeBudgetLimits> {
        let feature_set = &self.feature_set;
        let feature_snapshot = feature_set.snapshot();
        let fee_features = self.fee_features();
        let raise_cpi_limit = feature_snapshot.raise_cpi_nesting_limit_to_8;

        let compute_budget_and_limits = tx.transaction_configuration(feature_set).map(|config| {
            let fee_details = calculate_fee_details(
                tx,
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
                        ..SVMTransactionExecutionBudget::new_with_defaults(raise_cpi_limit)
                    },
                    loaded_accounts_data_size_limit: config.loaded_accounts_data_size_limit,
                    fee_details,
                }
            }
        });

        if compute_budget_and_limits.is_err() {
            error_counters.invalid_compute_budget += 1;
        }

        compute_budget_and_limits
    }

    fn check_blockhash_age(
        &self,
        tx: &impl SVMStaticMessage,
        max_age: usize,
        hash_queue: &BlockhashQueue,
    ) -> bool {
        let recent_blockhash = tx.recent_blockhash();
        hash_queue
            .get_hash_info_if_valid(recent_blockhash, max_age)
            .is_some()
    }

    fn check_nonce_semantics(
        &self,
        tx: &impl SVMMessage,
        next_durable_nonce: &DurableNonce,
    ) -> Option<Pubkey> {
        let nonce_is_advanceable = tx.recent_blockhash() != next_durable_nonce.as_hash();
        if !nonce_is_advanceable {
            return None;
        }

        tx.get_durable_nonce().copied()
    }

    fn check_nonce_account(
        &self,
        tx: &impl SVMStaticMessage,
        nonce_address: Pubkey,
        strict_nonce_checks: bool,
    ) -> Option<NonceData> {
        let nonce_account = self.get_account_with_fixed_root(&nonce_address)?;

        if strict_nonce_checks && nonce_account.data().len() != NonceState::size() {
            return None;
        }

        let nonce_data =
            nonce_account::verify_nonce_account(&nonce_account, tx.recent_blockhash())?;

        if strict_nonce_checks
            && !tx
                .get_ix_signers(NONCED_TX_MARKER_IX_INDEX as usize)
                .any(|signer| signer == &nonce_data.authority)
        {
            return None;
        }

        Some(nonce_data)
    }

    fn check_status_cache<Msg: StaticMessageWithMeta>(
        &self,
        messages: &[impl core::borrow::Borrow<Msg>],
        mut lock_results: Vec<TransactionCheckResult>,
        collect_processed_slots: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> (Vec<TransactionCheckResult>, Option<Vec<Option<Slot>>>) {
        // Do allocation before acquiring the lock on the status cache.
        let mut processed_slots = if collect_processed_slots {
            Some(Vec::with_capacity(messages.len()))
        } else {
            None
        };
        let rcache = self.status_cache.read().unwrap();

        for (message_ref, lock_result) in messages.iter().zip(lock_results.iter_mut()) {
            let processed_slot = if lock_result.is_ok() {
                self.get_processed_slot(message_ref.borrow(), &rcache)
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
        message: &impl StaticMessageWithMeta,
        status_cache: &BankStatusCache,
    ) -> Option<Slot> {
        let key = message.message_hash();
        let message_blockhash = message.recent_blockhash();
        status_cache
            .get_status(key, message_blockhash, &self.ancestors)
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
            AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMutWincode as _,
        },
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
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
        solana_sdk_ids::sysvar,
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

        let nonce_address = bank
            .check_nonce_semantics(&message, &bank.next_durable_nonce())
            .unwrap();
        assert_eq!(nonce_address, nonce_pubkey);

        let nonce_data = bank
            .check_nonce_account(&message, nonce_address, false)
            .unwrap();
        assert_eq!(
            nonce_data.get_lamports_per_signature(),
            STALE_LAMPORTS_PER_SIGNATURE
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
            bank.check_nonce_semantics(&message, &bank.next_durable_nonce())
                .is_none()
        );
    }

    #[test]
    fn test_check_nonce_transaction_validity_strict_nonce_checks_fail() {
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

        let nonce_address = bank
            .check_nonce_semantics(&message, &bank.next_durable_nonce())
            .unwrap();
        assert!(
            bank.check_nonce_account(&message, nonce_address, true)
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
            bank.check_nonce_semantics(&new_sanitized_message(message), &bank.next_durable_nonce())
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
        let nonce_address = bank
            .check_nonce_semantics(&message, &bank.next_durable_nonce())
            .unwrap();
        assert_eq!(nonce_address, missing_pubkey);
        assert!(
            bank.check_nonce_account(&message, nonce_address, false)
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
        let nonce_address = bank
            .check_nonce_semantics(&message, &bank.next_durable_nonce())
            .unwrap();
        assert!(
            bank.check_nonce_account(&message, nonce_address, false)
                .is_none()
        );
    }

    #[test]
    fn test_check_nonce_readonly_fail() {
        let (bank, _mint_keypair, custodian_keypair, nonce_keypair, _) =
            setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None).unwrap();
        let custodian_pubkey = custodian_keypair.pubkey();
        let nonce_pubkey = nonce_keypair.pubkey();

        // an advance-nonce instruction whose nonce account is passed as read-only
        let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
        #[allow(deprecated)]
        let nonce_instruction = Instruction::new_with_bincode(
            system_program::id(),
            &SystemInstruction::AdvanceNonceAccount,
            vec![
                AccountMeta::new_readonly(nonce_pubkey, false),
                AccountMeta::new_readonly(sysvar::recent_blockhashes::id(), false),
                AccountMeta::new_readonly(nonce_pubkey, true),
            ],
        );
        let message = new_sanitized_message(Message::new_with_blockhash(
            &[nonce_instruction],
            Some(&custodian_pubkey),
            &nonce_hash,
        ));

        assert!(
            bank.check_nonce_semantics(&message, &bank.next_durable_nonce())
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
            bank.check_nonce_semantics(&message, &bank.next_durable_nonce()),
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
        let (check_results, _) = bank.check_transactions(
            std::slice::from_ref(&tx),
            &lock_results,
            bank.max_processing_age(),
            true,
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

    fn filter_v1_transactions<Tx: TransactionWithMeta>(
        bank: &Bank,
        txs: &[Tx],
        lock_results: &[TransactionResult<()>],
    ) -> Vec<TransactionResult<()>> {
        bank.check_transactions_before_execution(
            txs,
            lock_results,
            bank.max_processing_age(),
            &mut TransactionErrorMetrics::default(),
        )
        .into_iter()
        .map(|result| result.map(|_| ()))
        .collect()
    }

    #[test]
    fn test_filter_v1_transactions_keeps_existing_errors() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let bank = Bank::new_for_tests(&genesis_config);
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

        let filtered = filter_v1_transactions(&bank, &txs, &lock_results);

        assert_eq!(filtered, lock_results);
    }

    #[test]
    fn test_filter_v1_transactions_rejects_v1_with_ok_lock_result() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let bank = Bank::new_for_tests(&genesis_config);
        let txs = vec![make_test_tx(TransactionVersion::Number(1))];
        let lock_results = vec![Ok(())];

        let filtered = filter_v1_transactions(&bank, &txs, &lock_results);

        assert_eq!(filtered, [Err(TransactionError::UnsupportedVersion)]);
    }

    #[test]
    fn test_filter_v1_transactions_keeps_v1_when_feature_enabled() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.activate_feature(&agave_feature_set::enable_tx_v1::id());
        let txs = vec![make_test_tx_with_blockhash(
            TransactionVersion::Number(1),
            bank.last_blockhash(),
        )];
        let lock_results = vec![Ok(())];

        let filtered = filter_v1_transactions(&bank, &txs, &lock_results);

        assert_eq!(filtered, [Ok(())]);
    }

    #[test]
    fn test_filter_v1_transactions_keeps_legacy_and_v0_ok() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let txs = vec![
            make_test_tx_with_blockhash(TransactionVersion::LEGACY, blockhash),
            make_test_tx_with_blockhash(TransactionVersion::Number(0), blockhash),
        ];
        let lock_results = vec![Ok(()), Ok(())];

        let filtered = filter_v1_transactions(&bank, &txs, &lock_results);

        assert_eq!(filtered, [Ok(()), Ok(())]);
    }

    #[test]
    fn test_filter_v1_transactions_mixed_results() {
        let (genesis_config, _mint_keypair) = solana_genesis_config::create_genesis_config(1);
        let bank = Bank::new_for_tests(&genesis_config);
        let blockhash = bank.last_blockhash();
        let txs = vec![
            make_test_tx_with_blockhash(TransactionVersion::LEGACY, blockhash),
            make_test_tx_with_blockhash(TransactionVersion::Number(1), blockhash),
            make_test_tx_with_blockhash(TransactionVersion::Number(0), blockhash),
            make_test_tx_with_blockhash(TransactionVersion::Number(1), blockhash),
        ];
        let lock_results = vec![
            Ok(()),
            Ok(()),
            Err(TransactionError::AccountInUse),
            Err(TransactionError::TooManyAccountLocks),
        ];

        let filtered = filter_v1_transactions(&bank, &txs, &lock_results);

        assert_eq!(
            filtered,
            [
                Ok(()),
                Err(TransactionError::UnsupportedVersion),
                Err(TransactionError::AccountInUse),
                Err(TransactionError::TooManyAccountLocks),
            ]
        );
    }
}
