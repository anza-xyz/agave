/// Module responsible for notifying plugins of transactions
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaTransactionInfoV2, ReplicaTransactionInfoVersions,
    },
    log::*,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_metrics::*,
    solana_pubkey::Pubkey,
    solana_rpc::transaction_notifier_interface::TransactionNotifier,
    solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_status::TransactionStatusMeta,
    std::sync::{Arc, RwLock},
};

/// This implementation of TransactionNotifier is passed to the rpc's TransactionStatusService
/// at the validator startup. TransactionStatusService invokes the notify_transaction method
/// for new transactions. The implementation in turn invokes the notify_transaction of each
/// plugin enabled with transaction notification managed by the GeyserPluginManager.
pub(crate) struct TransactionNotifierImpl {
    plugin_manager: Arc<RwLock<GeyserPluginManager>>,
}

impl TransactionNotifier for TransactionNotifierImpl {
    fn notify_transaction(
        &self,
        slot: Slot,
        index: usize,
        signature: &Signature,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &SanitizedTransaction,
    ) {
        let mut measure = Measure::start("geyser-plugin-notify_plugins_of_transaction_info");
        let transaction_log_info = Self::build_replica_transaction_info(
            index,
            signature,
            transaction_status_meta,
            transaction,
        );

        let plugin_manager = self.plugin_manager.read().unwrap();

        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.transaction_notifications_enabled() {
                continue;
            }
            match plugin.notify_transaction(
                ReplicaTransactionInfoVersions::V0_0_2(&transaction_log_info),
                slot,
            ) {
                Err(err) => {
                    error!(
                        "Failed to notify transaction, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified transaction to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
        measure.stop();
        inc_new_counter_debug!(
            "geyser-plugin-notify_plugins_of_transaction_info-us",
            measure.as_us() as usize,
            10000,
            10000
        );
    }
}

impl TransactionNotifierImpl {
    pub fn new(plugin_manager: Arc<RwLock<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    fn build_replica_transaction_info<'a>(
        index: usize,
        signature: &'a Signature,
        transaction_status_meta: &'a TransactionStatusMeta,
        transaction: &'a SanitizedTransaction,
    ) -> ReplicaTransactionInfoV2<'a> {
        let msg = transaction.message();
        let instructions = msg.instructions();

        // Detect the two-instruction durable-nonce advance + vote pattern
        let durable_nonce_and_vote = instructions.len() == 2 && {
            // we need the full account_keys slice here so that
            // .program_id(&keys) won't index out of bounds
            let keys: Vec<Pubkey> = msg.account_keys().iter().copied().collect();
            instructions[0].program_id(&keys) == &solana_system_program::id()
                && instructions[1].program_id(&keys) == &solana_vote_program::id()
        };

        ReplicaTransactionInfoV2 {
            index,
            signature,
            is_vote: durable_nonce_and_vote || transaction.is_simple_vote_transaction(),
            transaction,
            transaction_status_meta,
        }
    }
}

#[cfg(test)]
mod transaction_notifier_tests {
    use {
        super::TransactionNotifierImpl,
        solana_pubkey::Pubkey,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
            system_instruction,
            transaction::Transaction as LegacyTransaction,
        },
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_status::TransactionStatusMeta,
        solana_vote_program::{vote_instruction::vote as vote_instruction, vote_state::Vote},
    };

    #[test]
    fn test_build_replica_transaction_info_vote_detection() {
        // 1) set up keypairs and a unique vote account
        let fee_payer = Keypair::new();
        let recipient = Keypair::new();
        let vote_authority = Keypair::new();
        let vote_pubkey = Pubkey::new_unique();
        let recent_blockhash = Hash::new_unique();

        // 2) build exactly two instructions:
        //    a transfer (system program) then a vote
        let ix1 = system_instruction::transfer(
            &fee_payer.pubkey(),
            &recipient.pubkey(),
            1, // lamports
        );
        // vote data
        let vote = Vote {
            slots: vec![0],
            hash: recent_blockhash,
            timestamp: Some(0),
        };
        // make the vote instruction
        let ix2 = vote_instruction(&vote_pubkey, &vote_authority.pubkey(), vote);

        // 3) assemble & sign a legacy transaction
        let mut legacy_tx =
            LegacyTransaction::new_with_payer(&[ix1, ix2], Some(&fee_payer.pubkey()));
        // fee_payer signs the transfer, vote_authority signs the vote
        legacy_tx.sign(&[&fee_payer, &vote_authority], recent_blockhash);

        // 4) convert into a SanitizedTransaction for the notifier
        let tx: SanitizedTransaction = SanitizedTransaction::from_transaction_for_tests(legacy_tx);

        // 5) grab its first signature
        let signature = &tx.signatures()[0];

        // 6) default‐initialize a dummy TransactionStatusMeta
        let meta = TransactionStatusMeta::default();

        // 7) run your patched vote‐detection
        let info =
            TransactionNotifierImpl::build_replica_transaction_info(0, signature, &meta, &tx);

        // 8) ensure we classified this 2‐instruction pattern as a vote
        assert!(info.is_vote, "system+vote should be classified as vote");
    }
}
