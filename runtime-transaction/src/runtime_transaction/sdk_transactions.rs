use {
    super::{ComputeBudgetInstructionDetails, RuntimeTransaction},
    crate::{
        instruction_meta::InstructionMeta,
        transaction_config_source::{TransactionConfigSource, TransactionConfigValues},
        transaction_meta::{StaticMeta, TransactionMeta},
        transaction_with_meta::TransactionWithMeta,
    },
    solana_message::{AddressLoader, TransactionSignatureDetails, VersionedMessage},
    solana_program_entrypoint::HEAP_LENGTH,
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        simple_vote_transaction_checker::is_simple_vote_transaction,
        versioned::{VersionedTransaction, sanitized::SanitizedVersionedTransaction},
    },
    solana_transaction_error::TransactionResult as Result,
    std::{borrow::Cow, collections::HashSet},
};

impl RuntimeTransaction<SanitizedVersionedTransaction> {
    pub fn try_from(
        sanitized_versioned_tx: SanitizedVersionedTransaction,
        message_hash: MessageHash,
        is_simple_vote_tx: Option<bool>,
    ) -> Result<Self> {
        let message_hash = match message_hash {
            MessageHash::Precomputed(hash) => hash,
            MessageHash::Compute => sanitized_versioned_tx.get_message().message.hash(),
        };
        let is_simple_vote_tx = is_simple_vote_tx
            .unwrap_or_else(|| is_simple_vote_transaction(&sanitized_versioned_tx));

        let InstructionMeta {
            precompile_signature_details,
            instruction_data_len,
        } = InstructionMeta::try_new(
            sanitized_versioned_tx
                .get_message()
                .program_instructions_iter()
                .map(|(program_id, ix)| (program_id, SVMInstruction::from(ix))),
        )?;
        let signature_details = TransactionSignatureDetails::new(
            u64::from(
                sanitized_versioned_tx
                    .get_message()
                    .message
                    .header()
                    .num_required_signatures,
            ),
            precompile_signature_details.num_secp256k1_instruction_signatures,
            precompile_signature_details.num_ed25519_instruction_signatures,
            precompile_signature_details.num_secp256r1_instruction_signatures,
        );
        let transaction_config_source =
            if let VersionedMessage::V1(msg) = &sanitized_versioned_tx.get_message().message {
                // NOTE: sanitized v1::message must have default or sanitized `config` values.
                TransactionConfigSource::V1(TransactionConfigValues {
                    priority_fee_lamports: msg.config.priority_fee.unwrap_or(0),
                    compute_unit_limit: msg.config.compute_unit_limit.unwrap_or(0),
                    loaded_accounts_data_size_limit: msg
                        .config
                        .loaded_accounts_data_size_limit
                        .unwrap_or(0),
                    requested_heap_size: msg.config.heap_size.unwrap_or(HEAP_LENGTH as u32),
                })
            } else {
                TransactionConfigSource::LegacyAndV0(ComputeBudgetInstructionDetails::try_from(
                    sanitized_versioned_tx
                        .get_message()
                        .program_instructions_iter()
                        .map(|(program_id, ix)| (program_id, SVMInstruction::from(ix))),
                )?)
            };

        Ok(Self {
            transaction: sanitized_versioned_tx,
            meta: TransactionMeta {
                message_hash,
                is_simple_vote_transaction: is_simple_vote_tx,
                signature_details,
                instruction_data_len,
                transaction_config_source,
            },
        })
    }
}

impl RuntimeTransaction<SanitizedTransaction> {
    /// Create a new `RuntimeTransaction<SanitizedTransaction>` from an
    /// unsanitized `VersionedTransaction`.
    pub fn try_create(
        tx: VersionedTransaction,
        message_hash: MessageHash,
        is_simple_vote_tx: Option<bool>,
        address_loader: impl AddressLoader,
        reserved_account_keys: &HashSet<Pubkey>,
        enable_instruction_accounts_limit: bool,
    ) -> Result<Self> {
        if tx.message.instructions().len()
            > solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH
        {
            return Err(solana_transaction_error::TransactionError::SanitizeFailure);
        }

        if enable_instruction_accounts_limit {
            for instr in tx.message.instructions() {
                if instr.accounts.len() > solana_transaction_context::MAX_ACCOUNTS_PER_INSTRUCTION {
                    return Err(solana_transaction_error::TransactionError::SanitizeFailure);
                }
            }
        }

        let statically_loaded_runtime_tx =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                SanitizedVersionedTransaction::try_from(tx)?,
                message_hash,
                is_simple_vote_tx,
            )?;
        Self::try_from(
            statically_loaded_runtime_tx,
            address_loader,
            reserved_account_keys,
        )
    }

    /// Create a new `RuntimeTransaction<SanitizedTransaction>` from a
    /// `RuntimeTransaction<SanitizedVersionedTransaction>` that already has
    /// static metadata loaded.
    pub fn try_from(
        statically_loaded_runtime_tx: RuntimeTransaction<SanitizedVersionedTransaction>,
        address_loader: impl AddressLoader,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> Result<Self> {
        let hash = *statically_loaded_runtime_tx.message_hash();
        let is_simple_vote_tx = statically_loaded_runtime_tx.is_simple_vote_transaction();
        let sanitized_transaction = SanitizedTransaction::try_new(
            statically_loaded_runtime_tx.transaction,
            hash,
            is_simple_vote_tx,
            address_loader,
            reserved_account_keys,
        )?;

        let mut tx = Self {
            transaction: sanitized_transaction,
            meta: statically_loaded_runtime_tx.meta,
        };
        tx.load_dynamic_metadata()?;

        Ok(tx)
    }

    fn load_dynamic_metadata(&mut self) -> Result<()> {
        Ok(())
    }
}

impl TransactionWithMeta for RuntimeTransaction<SanitizedTransaction> {
    #[inline]
    fn as_sanitized_transaction(&self) -> Cow<'_, SanitizedTransaction> {
        Cow::Borrowed(self)
    }

    #[inline]
    fn to_versioned_transaction(&self) -> VersionedTransaction {
        self.transaction.to_versioned_transaction()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl RuntimeTransaction<SanitizedTransaction> {
    pub fn from_transaction_for_tests(transaction: solana_transaction::Transaction) -> Self {
        let versioned_transaction = VersionedTransaction::from(transaction);
        let enable_instruction_accounts_limit = true;
        Self::try_create(
            versioned_transaction,
            MessageHash::Compute,
            None,
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
            enable_instruction_accounts_limit,
        )
        .expect("failed to create RuntimeTransaction from Transaction")
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::FeatureSet,
        agave_reserved_account_keys::ReservedAccountKeys,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_message::{
            Message, MessageHeader, SimpleAddressLoader, VersionedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        solana_vote_interface::{self as vote, state::Vote},
    };

    fn vote_sanitized_versioned_transaction() -> SanitizedVersionedTransaction {
        let bank_hash = Hash::new_unique();
        let block_hash = Hash::new_unique();
        let vote_keypair = Keypair::new();
        let node_keypair = Keypair::new();
        let auth_keypair = Keypair::new();
        let votes = Vote::new(vec![1, 2, 3], bank_hash);
        let vote_ix =
            vote::instruction::vote(&vote_keypair.pubkey(), &auth_keypair.pubkey(), votes);
        let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));
        vote_tx.partial_sign(&[&node_keypair], block_hash);
        vote_tx.partial_sign(&[&auth_keypair], block_hash);

        SanitizedVersionedTransaction::try_from(VersionedTransaction::from(vote_tx)).unwrap()
    }

    fn non_vote_sanitized_versioned_transaction() -> SanitizedVersionedTransaction {
        TestTransaction::new().to_sanitized_versioned_transaction()
    }

    // Simple transfer transaction for testing, it does not support vote instruction
    // because simple vote transaction will not request limits
    struct TestTransaction {
        from_keypair: Keypair,
        hash: Hash,
        instructions: Vec<Instruction>,
    }

    impl TestTransaction {
        fn new() -> Self {
            let from_keypair = Keypair::new();
            let instructions = vec![system_instruction::transfer(
                &from_keypair.pubkey(),
                &solana_pubkey::new_rand(),
                1,
            )];
            TestTransaction {
                from_keypair,
                hash: Hash::new_unique(),
                instructions,
            }
        }

        fn add_compute_unit_limit(&mut self, val: u32) -> &mut TestTransaction {
            self.instructions
                .push(ComputeBudgetInstruction::set_compute_unit_limit(val));
            self
        }

        fn add_compute_unit_price(&mut self, val: u64) -> &mut TestTransaction {
            self.instructions
                .push(ComputeBudgetInstruction::set_compute_unit_price(val));
            self
        }

        fn add_loaded_accounts_bytes(&mut self, val: u32) -> &mut TestTransaction {
            self.instructions
                .push(ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(val));
            self
        }

        fn to_sanitized_versioned_transaction(&self) -> SanitizedVersionedTransaction {
            let message = Message::new(&self.instructions, Some(&self.from_keypair.pubkey()));
            let tx = Transaction::new(&[&self.from_keypair], message, self.hash);
            SanitizedVersionedTransaction::try_from(VersionedTransaction::from(tx)).unwrap()
        }
    }

    #[test]
    fn test_runtime_transaction_is_vote_meta() {
        fn get_is_simple_vote(
            svt: SanitizedVersionedTransaction,
            is_simple_vote: Option<bool>,
        ) -> bool {
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                svt,
                MessageHash::Compute,
                is_simple_vote,
            )
            .unwrap()
            .meta
            .is_simple_vote_transaction
        }

        assert!(!get_is_simple_vote(
            non_vote_sanitized_versioned_transaction(),
            None
        ));

        assert!(get_is_simple_vote(
            non_vote_sanitized_versioned_transaction(),
            Some(true), // override
        ));

        assert!(get_is_simple_vote(
            vote_sanitized_versioned_transaction(),
            None
        ));

        assert!(!get_is_simple_vote(
            vote_sanitized_versioned_transaction(),
            Some(false), // override
        ));
    }

    #[test]
    fn test_advancing_transaction_type() {
        let hash = Hash::new_unique();

        let statically_loaded_transaction =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                non_vote_sanitized_versioned_transaction(),
                MessageHash::Precomputed(hash),
                None,
            )
            .unwrap();

        assert_eq!(hash, *statically_loaded_transaction.message_hash());
        assert!(!statically_loaded_transaction.is_simple_vote_transaction());

        let dynamically_loaded_transaction = RuntimeTransaction::<SanitizedTransaction>::try_from(
            statically_loaded_transaction,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        );
        let dynamically_loaded_transaction =
            dynamically_loaded_transaction.expect("created from statically loaded tx");

        assert_eq!(hash, *dynamically_loaded_transaction.message_hash());
        assert!(!dynamically_loaded_transaction.is_simple_vote_transaction());
    }

    #[test]
    fn test_runtime_transaction_static_meta() {
        let hash = Hash::new_unique();
        let compute_unit_limit = 250_000;
        let compute_unit_price = 1_000;
        let priority_fee_lamports = compute_unit_limit * compute_unit_price / 1_000_000;
        let loaded_accounts_bytes = 1_024;
        let mut test_transaction = TestTransaction::new();

        let runtime_transaction_static =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                test_transaction
                    .add_compute_unit_limit(compute_unit_limit)
                    .add_compute_unit_price(compute_unit_price.into())
                    .add_loaded_accounts_bytes(loaded_accounts_bytes)
                    .to_sanitized_versioned_transaction(),
                MessageHash::Precomputed(hash),
                None,
            )
            .unwrap();

        assert_eq!(&hash, runtime_transaction_static.message_hash());
        assert!(!runtime_transaction_static.is_simple_vote_transaction());

        let signature_details = &runtime_transaction_static.meta.signature_details;
        assert_eq!(1, signature_details.num_transaction_signatures());
        assert_eq!(0, signature_details.num_secp256k1_instruction_signatures());
        assert_eq!(0, signature_details.num_ed25519_instruction_signatures());

        for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
            let fee_budget_limits = runtime_transaction_static
                .transaction_config_source()
                .sanitize_and_convert_to_fee_budget_limits(&feature_set)
                .unwrap();
            assert_eq!(
                compute_unit_limit as u64,
                fee_budget_limits.compute_unit_limit
            );
            assert_eq!(
                priority_fee_lamports as u64,
                fee_budget_limits.prioritization_fee
            );
            assert_eq!(
                loaded_accounts_bytes,
                fee_budget_limits.loaded_accounts_data_size_limit.get()
            );
        }
    }

    /// Helper to create a V1 `SanitizedVersionedTransaction` with the given config
    /// and a single no-op instruction.
    fn v1_sanitized_versioned_transaction(
        config: solana_message::v1::TransactionConfig,
    ) -> SanitizedVersionedTransaction {
        let payer = Pubkey::new_unique();
        let program_id = Pubkey::new_unique();
        let message = solana_message::v1::Message::new(
            MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            config,
            Hash::new_unique(),
            vec![payer, program_id],
            vec![CompiledInstruction::new_from_raw_parts(1, vec![], vec![0])],
        );
        let tx = VersionedTransaction {
            signatures: vec![Signature::default(); 1],
            message: VersionedMessage::V1(message),
        };

        SanitizedVersionedTransaction::try_from(tx).unwrap()
    }

    #[test]
    fn test_v1_transaction_config_extraction() {
        use {
            crate::transaction_config_source::TransactionConfigSource,
            solana_message::v1::TransactionConfig,
        };

        // Construct our test tx.
        let config = TransactionConfig::empty()
            .with_priority_fee(42)
            .with_compute_unit_limit(300_000)
            .with_loaded_accounts_data_size_limit(2048)
            .with_heap_size(65536);
        let tx = v1_sanitized_versioned_transaction(config);
        let tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
            tx,
            MessageHash::Compute,
            None,
        )
        .unwrap();

        // Assert - Config matches expectations.
        match tx.transaction_config_source() {
            TransactionConfigSource::V1(values) => {
                assert_eq!(values.priority_fee_lamports, 42);
                assert_eq!(values.compute_unit_limit, 300_000);
                assert_eq!(values.loaded_accounts_data_size_limit, 2048);
                assert_eq!(values.requested_heap_size, 65536);
            }
            _ => unreachable!(),
        }

        // Assert - Compute budget limits matches expectations.
        let feature_set = FeatureSet::all_enabled();
        let (limits, updated_heap_bytes) = tx
            .transaction_config_source()
            .sanitize_and_convert_to_fee_budget_limits_and_requested_heap_size(&feature_set)
            .unwrap();
        assert_eq!(limits.compute_unit_limit, 300_000);
        assert_eq!(limits.prioritization_fee, 42 * 300_000 / 1_000_000);
        assert_eq!(limits.loaded_accounts_data_size_limit.get(), 2048);
        assert_eq!(updated_heap_bytes, 65536);
    }

    #[test]
    fn test_v1_default_config_values() {
        use {
            crate::transaction_config_source::TransactionConfigSource,
            solana_message::v1::TransactionConfig,
        };

        let tx = v1_sanitized_versioned_transaction(TransactionConfig::empty());
        let tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
            tx,
            MessageHash::Compute,
            None,
        )
        .unwrap();

        // Assert - Config matches expectations.
        match tx.transaction_config_source() {
            TransactionConfigSource::V1(values) => {
                assert_eq!(values.priority_fee_lamports, 0);
                assert_eq!(values.compute_unit_limit, 0);
                assert_eq!(values.loaded_accounts_data_size_limit, 0);
                assert_eq!(
                    values.requested_heap_size,
                    solana_program_entrypoint::HEAP_LENGTH as u32
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_v1_ignores_compute_budget_instructions() {
        use {
            crate::transaction_config_source::TransactionConfigSource,
            solana_message::v1::TransactionConfig,
        };

        // Build a V1 message with config AND a ComputeBudget instruction that
        // specifies different values. The header config should win.
        let payer = Pubkey::new_unique();
        let cb_ix = ComputeBudgetInstruction::set_compute_unit_limit(999_999);
        let config = TransactionConfig::empty()
            .with_priority_fee(100)
            .with_compute_unit_limit(500_000)
            .with_loaded_accounts_data_size_limit(4096)
            .with_heap_size(32768);
        let message = solana_message::v1::Message::new(
            MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            config,
            Hash::new_unique(),
            vec![payer, cb_ix.program_id],
            vec![CompiledInstruction::new_from_raw_parts(
                1,
                cb_ix.data,
                vec![0],
            )],
        );
        let tx = VersionedTransaction {
            signatures: vec![Signature::default(); 1],
            message: VersionedMessage::V1(message),
        };
        let tx = SanitizedVersionedTransaction::try_from(tx).unwrap();
        let tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
            tx,
            MessageHash::Compute,
            None,
        )
        .unwrap();

        // Assert - Compute budget instruction ignored, limit is 500,000 not 999,999.
        match tx.transaction_config_source() {
            TransactionConfigSource::V1(values) => {
                assert_eq!(values.compute_unit_limit, 500_000);
                assert_eq!(values.priority_fee_lamports, 100);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_simd_406_instruction_accounts_limit() {
        let account_keys = vec![Pubkey::new_unique(); 3];
        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        };
        let mut accounts: Vec<u8> = vec![0; 254];
        // Exactly 255 accounts must pass sanitization
        accounts.push(1);
        let instr = CompiledInstruction::new_from_raw_parts(2, Vec::new(), accounts.clone());
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); 1],
            message: VersionedMessage::Legacy(solana_message::Message {
                header,
                account_keys: account_keys.clone(),
                recent_blockhash: Hash::default(),
                instructions: vec![instr],
            }),
        };
        let result = RuntimeTransaction::<SanitizedTransaction>::try_create(
            transaction,
            MessageHash::Compute,
            None,
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
            true,
        );
        assert!(result.is_ok());

        // 256 accounts must fail
        accounts.push(2);
        let instr = CompiledInstruction::new_from_raw_parts(2, Vec::new(), accounts.clone());
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); 1],
            message: VersionedMessage::Legacy(solana_message::Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions: vec![instr],
            }),
        };
        let result = RuntimeTransaction::<SanitizedTransaction>::try_create(
            transaction,
            MessageHash::Compute,
            None,
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
            true,
        );
        assert_eq!(
            result.err(),
            Some(solana_transaction_error::TransactionError::SanitizeFailure)
        );
    }
}
