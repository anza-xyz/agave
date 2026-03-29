use {
    crate::bank::Bank, core::ops::Deref, solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_error::TransactionResult as Result,
};

pub enum OwnedOrBorrowed<'a, T> {
    Owned(Vec<T>),
    Borrowed(&'a [T]),
}

impl<T> Deref for OwnedOrBorrowed<'_, T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        match self {
            OwnedOrBorrowed::Owned(v) => v,
            OwnedOrBorrowed::Borrowed(v) => v,
        }
    }
}

// Represents the results of trying to lock a set of accounts
pub struct TransactionBatch<'a, 'b, Tx: SVMMessage> {
    lock_results: Vec<Result<()>>,
    bank: &'a Bank,
    sanitized_txs: OwnedOrBorrowed<'b, Tx>,
    needs_unlock: bool,
}

impl<'a, 'b, Tx: SVMMessage> TransactionBatch<'a, 'b, Tx> {
    pub fn new(
        lock_results: Vec<Result<()>>,
        bank: &'a Bank,
        sanitized_txs: OwnedOrBorrowed<'b, Tx>,
    ) -> Self {
        assert_eq!(lock_results.len(), sanitized_txs.len());
        Self {
            lock_results,
            bank,
            sanitized_txs,
            needs_unlock: true,
        }
    }

    pub fn lock_results(&self) -> &Vec<Result<()>> {
        &self.lock_results
    }

    pub fn sanitized_transactions(&self) -> &[Tx] {
        &self.sanitized_txs
    }

    pub fn bank(&self) -> &Bank {
        self.bank
    }

    pub fn set_needs_unlock(&mut self, needs_unlock: bool) {
        self.needs_unlock = needs_unlock;
    }

    pub fn needs_unlock(&self) -> bool {
        self.needs_unlock
    }

    /// For every error result, if the corresponding transaction is
    /// still locked, unlock the transaction and then record the new error.
    pub fn unlock_failures(&mut self, transaction_results: Vec<Result<()>>) {
        assert_eq!(self.lock_results.len(), transaction_results.len());
        // Shouldn't happen but if a batch was marked as not needing an unlock,
        // don't unlock failures.
        if !self.needs_unlock() {
            return;
        }

        let txs_and_results = transaction_results
            .iter()
            .enumerate()
            .inspect(|(index, result)| {
                // It's not valid to update a previously recorded lock error to
                // become an "ok" result because this could lead to serious
                // account lock violations where accounts are later unlocked
                // when they were not currently locked.
                assert!(!(result.is_ok() && self.lock_results[*index].is_err()))
            })
            .filter(|(index, result)| result.is_err() && self.lock_results[*index].is_ok())
            .map(|(index, _)| (&self.sanitized_txs[index], &self.lock_results[index]));

        // Unlock the accounts for all transactions which will be updated to an
        // lock error below.
        self.bank.unlock_accounts(txs_and_results);

        // Record all new errors by overwriting lock results. Note that it's
        // not valid to update from err -> ok and the assertion above enforces
        // that validity constraint.
        self.lock_results = transaction_results;
    }
}

// Unlock all locked accounts in destructor.
impl<Tx: SVMMessage> Drop for TransactionBatch<'_, '_, Tx> {
    fn drop(&mut self) {
        if self.needs_unlock() {
            self.set_needs_unlock(false);
            self.bank.unlock_accounts(
                self.sanitized_transactions()
                    .iter()
                    .zip(self.lock_results()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{GenesisConfigInfo, create_genesis_config_with_leader},
        solana_keypair::Keypair,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_system_transaction as system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_error::TransactionError,
        test_case::test_case,
    };

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_transaction_batch(relax_intrabatch_account_locks: bool) {
        let (bank, txs) = setup(false, relax_intrabatch_account_locks);

        // Test getting locked accounts
        let batch = bank.prepare_sanitized_batch(&txs);

        // Grab locks
        assert!(batch.lock_results().iter().all(|x| x.is_ok()));

        // Trying to grab locks again should fail
        let batch2 = bank.prepare_sanitized_batch(&txs);
        assert!(batch2.lock_results().iter().all(|x| x.is_err()));

        // Drop the first set of locks
        drop(batch);

        // Now grabbing locks should work again
        let batch2 = bank.prepare_sanitized_batch(&txs);
        assert!(batch2.lock_results().iter().all(|x| x.is_ok()));
    }

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_simulation_batch(relax_intrabatch_account_locks: bool) {
        let (bank, txs) = setup(false, relax_intrabatch_account_locks);

        // Prepare batch without locks
        let batch = bank.prepare_unlocked_batch_from_single_tx(&txs[0]);
        assert!(batch.lock_results().iter().all(|x| x.is_ok()));

        // Grab locks
        let batch2 = bank.prepare_sanitized_batch(&txs);
        assert!(batch2.lock_results().iter().all(|x| x.is_ok()));

        // Prepare another batch without locks
        let batch3 = bank.prepare_unlocked_batch_from_single_tx(&txs[0]);
        assert!(batch3.lock_results().iter().all(|x| x.is_ok()));
    }

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_unlock_failures(relax_intrabatch_account_locks: bool) {
        let (bank, txs) = setup(true, relax_intrabatch_account_locks);
        let expected_lock_results = if relax_intrabatch_account_locks {
            vec![Ok(()), Ok(()), Ok(())]
        } else {
            vec![Ok(()), Err(TransactionError::AccountInUse), Ok(())]
        };

        // Test getting locked accounts
        let mut batch = bank.prepare_sanitized_batch(&txs);
        assert_eq!(batch.lock_results, expected_lock_results,);

        let qos_results = vec![
            Ok(()),
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
        ];
        batch.unlock_failures(qos_results.clone());
        assert_eq!(batch.lock_results, qos_results);

        // Dropping the batch should unlock remaining locked transactions
        drop(batch);

        // The next batch should be able to take all the same locks as before
        let batch2 = bank.prepare_sanitized_batch(&txs);
        assert_eq!(batch2.lock_results, expected_lock_results,);
    }

    fn setup(
        insert_conflicting_tx: bool,
        relax_intrabatch_account_locks: bool,
    ) -> (Bank, Vec<RuntimeTransaction<SanitizedTransaction>>) {
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let mut bank = Bank::new_for_tests(&genesis_config);
        if !relax_intrabatch_account_locks {
            bank.deactivate_feature(&agave_feature_set::relax_intrabatch_account_locks::id());
        }

        let pubkey = solana_pubkey::new_rand();
        let keypair2 = Keypair::new();
        let pubkey2 = solana_pubkey::new_rand();

        let mut txs = vec![RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
        )];
        if insert_conflicting_tx {
            txs.push(RuntimeTransaction::from_transaction_for_tests(
                system_transaction::transfer(&mint_keypair, &pubkey2, 1, genesis_config.hash()),
            ));
        }
        txs.push(RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&keypair2, &pubkey2, 1, genesis_config.hash()),
        ));

        (bank, txs)
    }
}
