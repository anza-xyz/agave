//! Administrative functions to manipulate the Blockstore that are not necessary
//! for normal operation
use {
    crate::blockstore::{Blockstore, Result},
    solana_clock::Slot,
    std::borrow::Cow,
};

impl Blockstore {
    /// Copy a slot from this Blockstore to `target_blockstore`
    pub fn copy_slot(
        &self,
        target_blockstore: &Self,
        slot: Slot,
        copy_block_metadata: bool,
    ) -> Result<()> {
        let shreds = self.get_data_shreds_for_slot(slot, 0)?;
        let shreds = shreds.into_iter().map(Cow::Owned);
        target_blockstore.insert_cow_shreds(shreds, None, true)?;

        if !copy_block_metadata {
            return Ok(());
        }

        let mut batch = target_blockstore.get_write_batch()?;

        if let Some(bank_hash) = self.bank_hash_cf.get(slot)? {
            target_blockstore
                .bank_hash_cf
                .put_in_batch(&mut batch, slot, &bank_hash)?;
        }
        if let Some(optimistic_slot) = self.optimistic_slots_cf.get(slot)? {
            target_blockstore.optimistic_slots_cf.put_in_batch(
                &mut batch,
                slot,
                &optimistic_slot,
            )?;
        }
        if let Some(root) = self.roots_cf.get(slot)? {
            target_blockstore
                .roots_cf
                .put_in_batch(&mut batch, slot, &root)?;
        }
        if let Some(dead) = self.dead_slots_cf.get(slot)? {
            target_blockstore
                .dead_slots_cf
                .put_in_batch(&mut batch, slot, &dead)?;
        }

        if let Some(block_height) = self.block_height_cf.get(slot)? {
            target_blockstore
                .block_height_cf
                .put_in_batch(&mut batch, slot, &block_height)?;
        }
        if let Some(block_time) = self.blocktime_cf.get(slot)? {
            target_blockstore
                .blocktime_cf
                .put_in_batch(&mut batch, slot, &block_time)?;
        }

        if let Some(rewards) = self.rewards_cf.get_slice(slot)? {
            target_blockstore
                .rewards_cf
                .put_bytes_in_batch(&mut batch, slot, &rewards);
        }

        let (entries, _, _) =
            self.get_slot_entries_with_shred_info(slot, 0, /*allow_dead_slots:*/ true)?;
        for transaction in entries.iter().flat_map(|entry| entry.transactions.iter()) {
            let signature = transaction.signatures[0];
            let index = (signature, slot);

            if let Some(transaction_meta) = self.transaction_status_cf.get_slice(index)? {
                target_blockstore.transaction_status_cf.put_bytes_in_batch(
                    &mut batch,
                    index,
                    &transaction_meta,
                );
            }
            if let Some(transaction_memo) = self.transaction_memos_cf.get_slice(index)? {
                target_blockstore.transaction_memos_cf.put_bytes_in_batch(
                    &mut batch,
                    index,
                    &transaction_memo,
                );
            }
        }
        // Intentionally skip copying address_signatures_cf; this column can
        // be recreated from transactions if really desired

        target_blockstore.write_batch(batch)
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::{blockstore::entries_to_test_shreds, get_tmp_ledger_path_auto_delete},
        solana_clock::UnixTimestamp,
        solana_entry::entry::Entry,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::v0::LoadedAddresses,
        solana_runtime::bank::RewardType,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction_context::transaction::TransactionReturnData,
        solana_transaction_status::{Reward, RewardsAndNumPartitions, TransactionStatusMeta},
    };

    fn create_entries_with_transactions() -> Vec<Entry> {
        let keypair = Keypair::new();
        let hash = Hash::default();

        let entry0 = Entry::new(&hash, 0, vec![]);

        let tx0 = system_transaction::transfer(&keypair, &keypair.pubkey(), 0, hash);
        let entry1 = Entry::new(&hash, 0, vec![tx0]);

        let tx1 = system_transaction::transfer(&keypair, &keypair.pubkey(), 1, hash);
        let tx2 = system_transaction::transfer(&keypair, &keypair.pubkey(), 2, hash);
        let entry2 = Entry::new(&hash, 0, vec![tx1, tx2]);

        vec![entry0, entry1, entry2]
    }

    fn populate_slot_with_metadata(blockstore: &Blockstore, slot: Slot, entries: &[Entry]) {
        let shreds = entries_to_test_shreds(entries, slot, slot.saturating_sub(1), true, 0);
        blockstore.insert_shreds(shreds, None, true).unwrap();

        let hash = Hash::from([(slot % 256) as u8; 32]);

        blockstore
            .insert_optimistic_slot(slot, &hash, (slot * 100) as UnixTimestamp)
            .unwrap();
        blockstore.insert_bank_hash(slot, hash, true);
        blockstore.set_block_height(slot, slot).unwrap();
        blockstore
            .set_block_time(slot, (slot * 100) as UnixTimestamp)
            .unwrap();

        let rewards = (0..2)
            .map(|i| Reward {
                pubkey: solana_pubkey::new_rand().to_string(),
                lamports: (slot + i) as i64,
                post_balance: slot + i,
                reward_type: Some(RewardType::Fee),
                commission: None,
                commission_bps: None,
            })
            .collect();
        let rewards = RewardsAndNumPartitions {
            rewards,
            num_partitions: Some(slot),
        };
        blockstore.write_rewards(slot, rewards).unwrap();

        for (index, transaction) in entries
            .iter()
            .flat_map(|entry| entry.transactions.iter())
            .enumerate()
        {
            let signature = transaction.signatures[0];

            let status_meta = TransactionStatusMeta {
                status: Ok(()),
                fee: slot,
                pre_balances: vec![slot + 1],
                post_balances: vec![slot + 2],
                inner_instructions: Some(vec![]),
                log_messages: Some(vec![]),
                pre_token_balances: Some(vec![]),
                post_token_balances: Some(vec![]),
                rewards: Some(vec![]),
                loaded_addresses: LoadedAddresses::default(),
                return_data: Some(TransactionReturnData::default()),
                compute_units_consumed: Some(slot + 3),
                cost_units: Some(slot + 4),
            };
            blockstore
                .write_transaction_status(slot, signature, std::iter::empty(), status_meta, index)
                .unwrap();
            blockstore
                .write_transaction_memos(&signature, slot, format!("{signature}"))
                .unwrap();
        }
    }

    #[test]
    fn test_blockstore_copy() {
        let source_ledger_path = get_tmp_ledger_path_auto_delete!();
        let source_blockstore = Blockstore::open(source_ledger_path.path()).unwrap();

        let target_ledger_path = get_tmp_ledger_path_auto_delete!();
        let target_blockstore = Blockstore::open(target_ledger_path.path()).unwrap();

        let entries = create_entries_with_transactions();

        // Basic copy without block metadata
        let slot = 10;
        let allow_dead_slots = false;
        let copy_block_metadata = false;
        populate_slot_with_metadata(&source_blockstore, slot, &entries);
        source_blockstore
            .copy_slot(&target_blockstore, slot, copy_block_metadata)
            .unwrap();

        assert_eq!(
            source_blockstore
                .get_slot_entries_with_shred_info(slot, 0, allow_dead_slots)
                .unwrap(),
            target_blockstore
                .get_slot_entries_with_shred_info(slot, 0, allow_dead_slots)
                .unwrap()
        );

        // Copy dead slot without block metadata
        let slot = 20;
        let allow_dead_slots = true;
        let copy_block_metadata = false;
        populate_slot_with_metadata(&source_blockstore, slot, &entries);
        source_blockstore.set_dead_slot(slot).unwrap();
        source_blockstore
            .copy_slot(&target_blockstore, slot, copy_block_metadata)
            .unwrap();

        assert!(!target_blockstore.is_dead(slot));
        assert_eq!(
            source_blockstore
                .get_slot_entries_with_shred_info(slot, 0, allow_dead_slots)
                .unwrap(),
            target_blockstore
                .get_slot_entries_with_shred_info(slot, 0, allow_dead_slots)
                .unwrap()
        );

        // Copy slot with block metadata
        let slot = 30;
        let copy_block_metadata = true;
        populate_slot_with_metadata(&source_blockstore, slot, &entries);
        source_blockstore.set_roots([slot].iter()).unwrap();
        source_blockstore
            .copy_slot(&target_blockstore, slot, copy_block_metadata)
            .unwrap();

        assert_eq!(
            source_blockstore.get_bank_hash(slot).unwrap(),
            target_blockstore.get_bank_hash(slot).unwrap()
        );
        assert_eq!(
            source_blockstore.get_optimistic_slot(slot).unwrap(),
            target_blockstore.get_optimistic_slot(slot).unwrap()
        );
        assert!(target_blockstore.is_root(slot));
        let require_previous_blockhash = false;
        assert_eq!(
            source_blockstore
                .get_rooted_block(slot, require_previous_blockhash)
                .unwrap(),
            target_blockstore
                .get_rooted_block(slot, require_previous_blockhash)
                .unwrap()
        );
    }
}
