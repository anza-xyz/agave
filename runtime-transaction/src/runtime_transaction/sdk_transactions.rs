use {
    super::RuntimeTransaction,
    crate::{
        instruction_meta::InstructionMeta,
        transaction_meta::{
            CachedTransactionMeta, TransactionMeta, VersionedTransactionConfiguration,
        },
        transaction_with_meta::{StaticTransactionWithMeta, TransactionWithMeta},
    },
    solana_message::{AddressLoader, TransactionSignatureDetails},
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::{VersionedTransaction, sanitized::SanitizedVersionedTransaction},
    },
    solana_transaction_error::TransactionResult as Result,
    std::{borrow::Cow, collections::HashSet},
};

impl RuntimeTransaction<SanitizedVersionedTransaction> {
    pub fn try_from(
        sanitized_versioned_tx: SanitizedVersionedTransaction,
        message_hash: MessageHash,
    ) -> Result<Self> {
        let message_hash = match message_hash {
            MessageHash::Precomputed(hash) => hash,
            MessageHash::Compute => sanitized_versioned_tx.get_message().message.hash(),
        };
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

        let versioned_transaction_config =
            VersionedTransactionConfiguration::try_from_sanitized_versioned_message(
                sanitized_versioned_tx.get_message(),
            )?;

        Ok(Self {
            transaction: sanitized_versioned_tx,
            meta: CachedTransactionMeta {
                message_hash,
                signature_details,
                versioned_transaction_config,
                instruction_data_len,
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
        address_loader: impl AddressLoader,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> Result<Self> {
        if tx.message.instructions().len()
            > solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH
        {
            return Err(solana_transaction_error::TransactionError::SanitizeFailure);
        }

        for instr in tx.message.instructions() {
            if instr.accounts.len() > solana_transaction_context::MAX_ACCOUNTS_PER_INSTRUCTION {
                return Err(solana_transaction_error::TransactionError::SanitizeFailure);
            }
        }

        let statically_loaded_runtime_tx =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                SanitizedVersionedTransaction::try_from(tx)?,
                message_hash,
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
        let sanitized_transaction = SanitizedTransaction::try_new(
            statically_loaded_runtime_tx.transaction,
            hash,
            false,
            address_loader,
            reserved_account_keys,
        )?;

        let tx = Self {
            transaction: sanitized_transaction,
            meta: statically_loaded_runtime_tx.meta,
        };

        Ok(tx)
    }
}

impl StaticTransactionWithMeta for RuntimeTransaction<SanitizedTransaction> {
    #[inline]
    fn to_versioned_transaction(&self) -> VersionedTransaction {
        self.transaction.to_versioned_transaction()
    }

    fn serialized_size(&self) -> usize {
        wincode::serialized_size(&self.to_versioned_transaction())
            .expect("versioned transaction serialization should succeed") as usize
    }
}

impl TransactionWithMeta for RuntimeTransaction<SanitizedTransaction> {
    #[inline]
    fn as_sanitized_transaction(&self) -> Cow<'_, SanitizedTransaction> {
        Cow::Borrowed(self)
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl RuntimeTransaction<SanitizedTransaction> {
    pub fn from_transaction_for_tests(transaction: solana_transaction::Transaction) -> Self {
        let versioned_transaction = VersionedTransaction::from(transaction);
        Self::try_create(
            versioned_transaction,
            MessageHash::Compute,
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
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
    };

    fn sanitized_versioned_transaction() -> SanitizedVersionedTransaction {
        TestTransaction::new().to_sanitized_versioned_transaction()
    }

    // Simple transfer transaction for testing.
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
    fn test_advancing_transaction_type() {
        let hash = Hash::new_unique();

        let statically_loaded_transaction =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                sanitized_versioned_transaction(),
                MessageHash::Precomputed(hash),
            )
            .unwrap();

        assert_eq!(hash, *statically_loaded_transaction.message_hash());

        let dynamically_loaded_transaction = RuntimeTransaction::<SanitizedTransaction>::try_from(
            statically_loaded_transaction,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        );
        let dynamically_loaded_transaction =
            dynamically_loaded_transaction.expect("created from statically loaded tx");

        assert_eq!(hash, *dynamically_loaded_transaction.message_hash());
    }

    #[test]
    fn test_runtime_transaction_static_meta() {
        let hash = Hash::new_unique();
        let compute_unit_limit = 250_000;
        let compute_unit_price = 1_000;
        let loaded_accounts_bytes = 1_024;
        let mut test_transaction = TestTransaction::new();

        let runtime_transaction_static =
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                test_transaction
                    .add_compute_unit_limit(compute_unit_limit)
                    .add_compute_unit_price(compute_unit_price)
                    .add_loaded_accounts_bytes(loaded_accounts_bytes)
                    .to_sanitized_versioned_transaction(),
                MessageHash::Precomputed(hash),
            )
            .unwrap();

        assert_eq!(&hash, runtime_transaction_static.message_hash());

        let signature_details = &runtime_transaction_static.meta.signature_details;
        assert_eq!(1, signature_details.num_transaction_signatures());
        assert_eq!(0, signature_details.num_secp256k1_instruction_signatures());
        assert_eq!(0, signature_details.num_ed25519_instruction_signatures());

        for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
            let transaction_configuration = runtime_transaction_static
                .transaction_configuration(&feature_set)
                .unwrap();
            assert_eq!(
                compute_unit_limit,
                transaction_configuration.compute_unit_limit
            );
            assert_eq!(
                compute_unit_price,
                transaction_configuration.compute_unit_price_in_microlamports()
            );
            assert_eq!(
                loaded_accounts_bytes,
                transaction_configuration.loaded_accounts_data_size_limit
            );
        }
    }

    #[test]
    fn test_serialized_size() {
        let transaction = RuntimeTransaction::<SanitizedTransaction>::try_from(
            RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                sanitized_versioned_transaction(),
                MessageHash::Compute,
            )
            .unwrap(),
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let expected = wincode::serialized_size(&transaction.to_versioned_transaction()).unwrap();
        assert_eq!(transaction.serialized_size(), expected as usize);
        assert_eq!(
            expected,
            wincode::serialize(&transaction.to_versioned_transaction())
                .unwrap()
                .len() as u64
        );
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
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
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
            solana_message::SimpleAddressLoader::Disabled,
            &HashSet::new(),
        );
        assert_eq!(
            result.err(),
            Some(solana_transaction_error::TransactionError::SanitizeFailure)
        );
    }
}
