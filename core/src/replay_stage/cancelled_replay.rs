//! Cancelling replay of skip-certified slots.
//!
//! Votor sends [`ReplayCancelCommand`]s, which replay processes in order at a single point of its
//! loop ([`CancelledReplaySlots::process_commands`]). So whether a slot is cancelled only changes
//! there, and only the replay thread discards cancelled banks or resumes cancelled slots.
//!
//! The only part of cancelling that is concurrent with replay is votor flagging the slot's bank
//! with [`Bank::request_replay_cancel`](solana_runtime::bank::Bank::request_replay_cancel), so
//! that replay stops executing it mid-block. Votor flags the bank and sends the slot's `Cancel`
//! command before any later `Resume` command for the slot. Hence:
//! - Processing the `Cancel` command discards any unfrozen bank votor flagged for the slot: the
//!   bank exists by the time the command is processed, since votor flags it before sending the
//!   command, and replay doesn't create a bank in the slot until it processes a later `Resume`
//!   command.
//! - Until then, `process_replay_results` doesn't mark the flagged bank dead.
//! - The flag has no effect once replay starts freezing the bank, so a bank that replay froze
//!   before or after votor flagged it is kept as is.

use {
    super::ReplayStage,
    crate::consensus::progress_map::{ForkProgress, ProgressMap},
    agave_votor::replay_signals::{ReplayCancelCommand, ReplayCancelReceiver},
    agave_votor_messages::consensus_message::{Block, BlockId},
    solana_clock::Slot,
    solana_ledger::{blockstore::Blockstore, blockstore_processor::AsyncVerificationProgress},
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::{BTreeMap, BTreeSet},
        iter,
        sync::RwLock,
        time::Instant,
    },
};

/// Replay's state for cancelling replay of skip-certified slots, which it updates from votor's
/// [`ReplayCancelCommand`]s. Only the replay thread reads or writes it.
#[derive(Debug, Default)]
pub(crate) struct CancelledReplaySlots {
    /// Slots above the root whose replay is cancelled, each mapped to when votor cancelled it.
    ///
    /// Replay doesn't create banks in these slots.
    cancelled_slots: BTreeMap<Slot, Instant>,
    /// Blocks above the root with a NotarizeFallback-or-stronger certificate that replay has no
    /// bank for. The cancelled slots on their parent chains must be replayed again (see
    /// [`Self::resume_notarized_ancestors`]).
    notarized_blocks_without_bank: BTreeSet<Block>,
}

impl CancelledReplaySlots {
    /// Returns whether replay of `slot` is cancelled.
    pub(crate) fn contains(&self, slot: Slot) -> bool {
        self.cancelled_slots.contains_key(&slot)
    }

    /// Processes votor's pending commands in order, then resumes the cancelled ancestors of
    /// NotarizeFallback-or-stronger blocks that replay has no bank for.
    pub(super) fn process_commands(
        &mut self,
        my_pubkey: &Pubkey,
        receiver: &ReplayCancelReceiver,
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        for command in receiver.try_iter() {
            match command {
                ReplayCancelCommand::Cancel { slot, cancelled_at } => {
                    let cancelled_at = *self.cancelled_slots.entry(slot).or_insert(cancelled_at);
                    Self::discard_cancelled_bank(
                        my_pubkey,
                        slot,
                        cancelled_at,
                        bank_forks,
                        progress,
                        async_verification_freelist,
                    );
                }
                ReplayCancelCommand::Resume(block) => {
                    if self.cancelled_slots.remove(&block.slot).is_some() {
                        info!(
                            "{my_pubkey}: Resuming cancelled replay of slot {} for {block:?}",
                            block.slot
                        );
                    }
                    self.notarized_blocks_without_bank.insert(block);
                }
            }
        }

        self.resume_notarized_ancestors(my_pubkey, blockstore, bank_forks);
    }

    /// Discards the bank of the cancelled `slot`. Such blocks are unlikely to be finalized, so
    /// this avoids spending replay time on them and limits how much replay time a slow block can
    /// consume.
    ///
    /// Frozen banks, leader banks, and dead banks are left alone.
    ///
    /// Note that cancellation is not a replay failure: the slot is not marked dead and its shreds
    /// are kept, so the bank can be recreated if votor resumes the slot.
    ///
    /// A discarded bank that replay started on is reported in `replay-slot-cancelled-stats`.
    fn discard_cancelled_bank(
        my_pubkey: &Pubkey,
        slot: Slot,
        cancelled_at: Instant,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        let Some(bank) = bank_forks.read().unwrap().get(slot) else {
            return;
        };
        // Dead banks don't execute anything, and keeping them prevents replaying them.
        if bank.is_frozen()
            || !bank.should_replay_from_blockstore()
            || progress.is_dead(slot).unwrap_or(false)
        {
            return;
        }

        info!(
            "{my_pubkey}: Discarding bank {slot} (bank id {:?}) whose replay was cancelled",
            bank.bank_id()
        );
        // Votor flags the bank too, unless replay inserted it into `BankForks` after votor looked
        // it up. Either way, this stops any execution `clear_slots` would otherwise wait for.
        bank.request_replay_cancel();
        // A slot cancelled before replay started on its bank has no `ForkProgress`: nothing was
        // spent on it, so there is nothing to report.
        if let Some(fork_progress) = progress.get(&slot) {
            report_cancelled_bank(slot, fork_progress, cancelled_at);
        }
        // Note that we do not want to purge transaction history: the shreds are kept, so if votor
        // resumes the slot, replay either re-executes the same block on the same parent,
        // overwriting the rows written so far with identical keys and values, or starts from an
        // `UpdateParent` offset, in which case `generate_new_bank_forks` purges the slot's
        // transaction history first.
        ReplayStage::clear_slots([slot], bank_forks, progress, async_verification_freelist);
    }

    /// Resumes replay of every cancelled slot on the parent chain of each
    /// NotarizeFallback-or-stronger block that replay has no bank for.
    ///
    /// Such a block may be finalized, and then so is every block on its parent chain. But our
    /// pool can see the block's certificate without ever seeing the NotarizeFallback-or-stronger
    /// certificate of a cancelled ancestor. Unless we replay that ancestor again, we can't replay
    /// the block: we can't root it once it's finalized, nor build our leader window on it if it's
    /// our ParentReady parent.
    ///
    /// Walks each parent chain through the blockstore, and stops at the first block whose bank in
    /// `BankForks` matches the chain, as soon as no slot left on the chain can be cancelled, or at
    /// a missing `SlotMeta` (see [`parent_block`]). Blocks are kept until replay has their bank or
    /// the root passes them, so the walk is retried on every iteration: once repair completes a
    /// missing block, and when votor cancels an ancestor after the block's certificate.
    fn resume_notarized_ancestors(
        &mut self,
        my_pubkey: &Pubkey,
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
    ) {
        {
            let bank_forks = bank_forks.read().unwrap();
            let root = bank_forks.root();
            self.cancelled_slots.retain(|&slot, _| slot > root);
            // Replay can replay the rest of the chain from a block it has.
            self.notarized_blocks_without_bank.retain(|block| {
                block.slot > root && bank_forks.block_id(block.slot) != Some(block.block_id)
            });
        }

        let Some(&lowest_cancelled_slot) = self.cancelled_slots.keys().next() else {
            return;
        };
        for &notarized_block in &self.notarized_blocks_without_bank {
            let slots_to_resume: Vec<Slot> = {
                let chain = iter::successors(Some(notarized_block), |&block| {
                    parent_block(my_pubkey, block, notarized_block, blockstore)
                });
                chain
                    .take_while(|block| {
                        // Every slot left on the chain is at most `block.slot`, so none of them
                        // can be cancelled once we are below `lowest_cancelled_slot`.
                        block.slot >= lowest_cancelled_slot
                            // Replay can replay the rest of the chain from a block it has.
                            && bank_forks.read().unwrap().block_id(block.slot)
                                != Some(block.block_id)
                    })
                    .map(|block| block.slot)
                    .collect()
            };
            for slot in slots_to_resume {
                if self.cancelled_slots.remove(&slot).is_some() {
                    info!(
                        "{my_pubkey}: Resuming cancelled replay of slot {slot}, an ancestor of \
                         {notarized_block:?}"
                    );
                }
            }
        }
    }
}

/// Returns the parent of `block` on the chain of `notarized_block`, or `None` if the blockstore
/// doesn't have `block` yet (repair is still outstanding; the walk is retried on the next replay
/// iteration).
fn parent_block(
    my_pubkey: &Pubkey,
    block: Block,
    notarized_block: Block,
    blockstore: &Blockstore,
) -> Option<Block> {
    match blockstore.get_slot_meta_for_block_id(block.slot, block.block_id.to_hash()) {
        Ok(Some((slot_meta, _location))) => Some(Block {
            slot: slot_meta
                .parent_slot
                .expect("Full slots must have a parent"),
            block_id: BlockId::from(slot_meta.parent_block_id),
        }),
        Ok(None) => {
            trace!(
                "{my_pubkey}: Waiting for repair of {block:?} on the chain of {notarized_block:?}"
            );
            None
        }
        Err(err) => {
            warn!(
                "{my_pubkey}: Unable to read {block:?} on the chain of {notarized_block:?}: {err}"
            );
            None
        }
    }
}

/// Reports how far replay of a cancelled bank got, and how long it took, in
/// `replay-slot-cancelled-stats`.
fn report_cancelled_bank(slot: Slot, fork_progress: &ForkProgress, cancelled_at: Instant) {
    let cancel_latency_us = cancelled_at.elapsed().as_micros() as i64;
    let replay_total_elapsed_us = fork_progress
        .replay_stats
        .read()
        .unwrap()
        .started
        .elapsed()
        .as_micros() as i64;
    let (num_txs, num_entries, num_shreds) = {
        let replay_progress = fork_progress.replay_progress.read().unwrap();
        (
            replay_progress.num_txs as i64,
            replay_progress.num_entries as i64,
            replay_progress.num_shreds as i64,
        )
    };
    datapoint_info!(
        "replay-slot-cancelled-stats",
        ("slot", slot as i64, i64),
        ("replay_total_elapsed", replay_total_elapsed_us, i64),
        ("cancel_latency_us", cancel_latency_us, i64),
        ("total_transactions", num_txs, i64),
        ("total_entries", num_entries, i64),
        ("total_shreds", num_shreds, i64),
    );
}
