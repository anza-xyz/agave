use {
    crate::{
        bank::Bank,
        block_component_processor::vote_reward::{
            CalcVoteRewardUpdateVoteStatesError, calc_vote_rewards_update_vote_states,
        },
        leader_schedule_utils::leader_slot_index,
        validated_block_finalization::{
            BlockFinalizationCertError, ValidatedBlockFinalizationCert,
        },
        validated_reward_certificate::{Error as ValidatedRewardCertError, ValidatedRewardCert},
    },
    agave_transaction_view::transaction_data::TransactionData,
    agave_votor_messages::certificate::Certificate,
    crossbeam_channel::{Sender, TrySendError},
    log::*,
    smallvec::{SmallVec, smallvec},
    solana_clock::Slot,
    solana_entry::{
        block_component::{
            BlockFooterV1, BlockMarkerV1, VersionedBlockFooter, VersionedBlockHeader,
            VersionedBlockMarker, VersionedUpdateParent,
        },
        entry::EntryView,
    },
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{collections::HashSet, sync::Arc},
    thiserror::Error,
};

pub(crate) mod vote_reward;

#[derive(Debug, Error)]
pub enum BankFooterError {
    #[error("calc vote rewards updating vote states failed with \"{0}\"")]
    CalcVoteRewardUpdateVoteStates(#[from] CalcVoteRewardUpdateVoteStatesError),
}

#[derive(Debug, Error)]
pub enum BlockComponentProcessorError {
    #[error("FinalizationCertificate was invalid or failed to verify {0}")]
    InvalidFinalizationCertificate(#[from] BlockFinalizationCertError),
    #[error("Missing block footer")]
    MissingBlockFooter,
    #[error("Missing parent marker (neither a header nor an update parent was present)")]
    MissingParentMarker,
    #[error("Entry batch detected after block footer")]
    EntryBatchAfterBlockFooter,
    #[error("Alpentick must be the final block component and appear after block footer")]
    InvalidAlpentickPosition,
    #[error("Multiple block footers detected")]
    MultipleBlockFooters,
    #[error("Multiple block headers detected")]
    MultipleBlockHeaders,
    #[error(
        "Block header parent slot mismatch: header={header_parent_slot}, bank={bank_parent_slot}"
    )]
    HeaderParentSlotMismatch {
        header_parent_slot: Slot,
        bank_parent_slot: Slot,
    },
    #[error("Multiple update parents detected")]
    MultipleUpdateParents,
    #[error("Nanosecond clock out of bounds")]
    NanosecondClockOutOfBounds,
    #[error("Spurious update parent")]
    SpuriousUpdateParent,
    #[error("UpdateParent marker is only valid in the first slot of a leader window: slot {0}")]
    UpdateParentNotFirstInLeaderWindow(Slot),
    #[error(
        "UpdateParent cannot be the initial parent marker unless replay starts at UpdateParent"
    )]
    UnexpectedInitialUpdateParent,
    #[error("Abandoned bank")]
    AbandonedBank(VersionedUpdateParent),
    #[error("invalid reward certs {0}")]
    InvalidRewardCerts(#[from] ValidatedRewardCertError),
    #[error("updating bank footer failed with \"{0}\"")]
    UpdateBankFooter(#[from] BankFooterError),
}

impl BlockComponentProcessorError {
    /// Returns whether this error can come from an optimistic-parent prefix
    /// that a later usable `UpdateParent` makes obsolete.
    ///
    /// This only determines soft-dead eligibility. Replay also verifies that
    /// the failure occurred before the `UpdateParent`.
    pub fn is_update_parent_recoverable_replay_error(&self) -> bool {
        match self {
            BlockComponentProcessorError::MissingParentMarker
            | BlockComponentProcessorError::EntryBatchAfterBlockFooter
            | BlockComponentProcessorError::InvalidAlpentickPosition
            | BlockComponentProcessorError::MultipleBlockFooters
            | BlockComponentProcessorError::MultipleBlockHeaders
            | BlockComponentProcessorError::HeaderParentSlotMismatch { .. }
            | BlockComponentProcessorError::NanosecondClockOutOfBounds
            | BlockComponentProcessorError::UnexpectedInitialUpdateParent
            | BlockComponentProcessorError::SpuriousUpdateParent
            | BlockComponentProcessorError::AbandonedBank(_)
            | BlockComponentProcessorError::InvalidRewardCerts(_)
            | BlockComponentProcessorError::UpdateBankFooter(_)
            | BlockComponentProcessorError::InvalidFinalizationCertificate(_) => true,
            BlockComponentProcessorError::MissingBlockFooter
            | BlockComponentProcessorError::MultipleUpdateParents
            | BlockComponentProcessorError::UpdateParentNotFirstInLeaderWindow(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// The parent marker that established the current entry section.
enum EntryParentMarker {
    BlockHeader,
    UpdateParent,
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
/// The stage within the block we are currently in
///
/// All blocks MUST follow this exact shape
///
/// Header
/// 0 or more Entries
/// Optional UpdateParent
/// 0 or more Entries
/// Footer
/// Alpentick
///
/// Block component processing can start either from the header
/// or from the UpdateParent.
enum BlockComponentStage {
    #[default]
    /// Beginning of the block, can only accept a parent marker
    PreParentMarker,
    /// During the entries section, can accept entries or the footer
    /// If the parent marker was a block header, can also accept an UpdateParent marker
    AcceptingEntriesOrFooter { parent_marker: EntryParentMarker },
    /// After the footer, can only accept the alpentick
    AcceptingAlpentick,
    /// After the alpentick, nothing more is accepted
    Done,
}

impl BlockComponentStage {
    /// If current stage is `PreParentMarker`, transition to `AcceptingEntriesOrFooter`
    fn on_header(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => {
                *self = Self::AcceptingEntriesOrFooter {
                    parent_marker: EntryParentMarker::BlockHeader,
                };
                Ok(())
            }
            Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::BlockHeader,
            }
            | Self::AcceptingAlpentick
            | Self::Done => Err(BlockComponentProcessorError::MultipleBlockHeaders),
            Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::UpdateParent,
            } => Err(BlockComponentProcessorError::SpuriousUpdateParent),
        }
    }

    /// If current stage is `AcceptingEntriesOrFooter`, leave it unchanged.
    fn on_entry_batch(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingEntriesOrFooter { .. } => Ok(()),
            Self::AcceptingAlpentick | Self::Done => {
                Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)
            }
        }
    }

    /// If current stage is `AcceptingEntriesOrFooter`, return `AbandonedBank`
    /// If current stage is `PreParentMarker` and `allow_initial_update_parent` is specified,
    /// transition to `AcceptingEntriesOrFooter` with `EntryParentMarker::UpdateParent`
    fn on_update_parent(
        &mut self,
        update_parent: &VersionedUpdateParent,
        allow_initial_update_parent: bool,
    ) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => {
                if !allow_initial_update_parent {
                    return Err(BlockComponentProcessorError::UnexpectedInitialUpdateParent);
                }
                *self = Self::AcceptingEntriesOrFooter {
                    parent_marker: EntryParentMarker::UpdateParent,
                };
                Ok(())
            }
            Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::BlockHeader,
            } => {
                // Only an error in the sense that replay execution of this block
                // prefix is now over. Replay execution can continue after resetting
                // bank.
                Err(BlockComponentProcessorError::AbandonedBank(
                    update_parent.clone(),
                ))
            }
            Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::UpdateParent,
            } => Err(BlockComponentProcessorError::MultipleUpdateParents),
            Self::AcceptingAlpentick | BlockComponentStage::Done => {
                Err(BlockComponentProcessorError::SpuriousUpdateParent)
            }
        }
    }

    /// If the current stage is `AcceptingEntriesOrFooter`, transition to `AcceptingAlpentick`.
    fn on_footer(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingEntriesOrFooter { .. } => {
                *self = Self::AcceptingAlpentick;
                Ok(())
            }
            Self::AcceptingAlpentick | Self::Done => {
                Err(BlockComponentProcessorError::MultipleBlockFooters)
            }
        }
    }

    /// If stage is `AcceptingAlpentick`, transition to `Done`
    fn on_alpentick(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingEntriesOrFooter { .. } => {
                Err(BlockComponentProcessorError::InvalidAlpentickPosition)
            }
            Self::AcceptingAlpentick => {
                *self = Self::Done;
                Ok(())
            }
            Self::Done => Err(BlockComponentProcessorError::InvalidAlpentickPosition),
        }
    }

    /// Return `Ok(())` only if the stage is `Done`
    fn on_final(&self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::Done => Ok(()),
            Self::AcceptingAlpentick => Err(BlockComponentProcessorError::InvalidAlpentickPosition),
            Self::PreParentMarker | Self::AcceptingEntriesOrFooter { .. } => {
                Err(BlockComponentProcessorError::MissingBlockFooter)
            }
        }
    }
}

#[derive(Default)]
pub struct BlockComponentProcessor {
    stage: BlockComponentStage,
}

impl BlockComponentProcessor {
    pub fn on_final(&self) -> Result<(), BlockComponentProcessorError> {
        self.stage.on_final()
    }

    /// Process an entry batch.
    ///
    /// Validates that a parent marker (header or update parent) has been processed
    /// before any entry batches. The terminal Alpenglow tick is the only entry
    /// batch allowed after the block footer.
    pub fn on_entry_batch<D: TransactionData>(
        &mut self,
        entries: &[EntryView<D>],
        is_final_component: bool,
    ) -> Result<(), BlockComponentProcessorError> {
        // The alpentick must be the final block component.
        // It is fine for other ticks to be present in the block, they will be rejected
        // for `TooManyTicks` in `verify_ticks()`
        let is_alpentick = is_final_component
            && matches!(entries, [entry] if entry.is_tick() && entry.num_hashes == 1);

        if is_alpentick {
            self.stage.on_alpentick()
        } else {
            self.stage.on_entry_batch()
        }
    }

    /// Process an Alpenglow block marker.
    pub fn on_marker(
        &mut self,
        bank: Arc<Bank>,
        parent_bank: Arc<Bank>,
        shred_version: u16,
        marker: VersionedBlockMarker,
        allow_initial_update_parent: bool,
        finalization_cert_sender: Option<&Sender<SmallVec<[Certificate; 2]>>>,
    ) -> Result<(), BlockComponentProcessorError> {
        let slot = bank.slot();
        let VersionedBlockMarker::V1(marker) = marker;

        let fast_leader_handover_active =
            bank.feature_set.snapshot().alpenglow_fast_leader_handover;

        match marker {
            BlockMarkerV1::BlockHeader(header) => {
                self.on_header(header.inner(), bank.parent_slot())
            }
            BlockMarkerV1::BlockFooter(footer) => self.on_footer(
                bank,
                parent_bank,
                shred_version,
                footer.into_inner(),
                finalization_cert_sender,
            ),

            BlockMarkerV1::UpdateParent(update_parent) => {
                if fast_leader_handover_active {
                    self.on_update_parent(slot, update_parent.inner(), allow_initial_update_parent)
                } else {
                    Err(BlockComponentProcessorError::SpuriousUpdateParent)
                }
            }
        }
    }

    fn on_footer(
        &mut self,
        bank: Arc<Bank>,
        parent_bank: Arc<Bank>,
        shred_version: u16,
        footer: VersionedBlockFooter,
        finalization_cert_sender: Option<&Sender<SmallVec<[Certificate; 2]>>>,
    ) -> Result<(), BlockComponentProcessorError> {
        self.stage.on_footer()?;

        let VersionedBlockFooter::V1(footer) = footer;

        Self::enforce_nanosecond_clock_bounds(&bank, &parent_bank, &footer)?;

        let BlockFooterV1 {
            bank_hash,
            block_producer_time_nanos,
            block_user_agent: _,
            block_final_cert,
            skip_reward_cert,
            notar_reward_cert,
        } = footer;

        let reward_cert = ValidatedRewardCert::try_new(
            &bank,
            shred_version,
            &skip_reward_cert,
            &notar_reward_cert,
        )?;
        let block_producer_time_nanos =
            Self::block_producer_time_nanos_as_i64(block_producer_time_nanos)?;
        let final_cert = block_final_cert
            .map(|final_cert| {
                ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank, shred_version)
                    .map_err(BlockComponentProcessorError::InvalidFinalizationCertificate)
            })
            .transpose()?;

        let (footer_input, pool_input) = match final_cert {
            None => (None, None),
            Some(cert) => {
                let (signers, finalize_cert, notarize_cert) = cert.into_parts();
                let final_slot = finalize_cert.cert_type.slot();
                (
                    Some((signers, final_slot)),
                    Some((finalize_cert, notarize_cert)),
                )
            }
        };

        Self::update_bank_with_footer_fields(
            &bank,
            block_producer_time_nanos,
            Some(bank_hash),
            reward_cert,
            footer_input
                .as_ref()
                .map(|(validators, slot)| (validators, *slot)),
        )?;

        // Send finalization cert(s) to consensus pool
        if let Some((finalize_cert, notarize_cert)) = pool_input
            && let Some(sender) = finalization_cert_sender
        {
            let channel_name = "finalization_cert_sender";
            let certs = match notarize_cert {
                None => smallvec![finalize_cert],
                Some(c) => smallvec![finalize_cert, c],
            };
            match sender.try_send(certs) {
                Ok(()) => (),
                Err(TrySendError::Full(_)) => {
                    warn!("channel \"{channel_name}\" is full, dropping msg")
                }
                Err(TrySendError::Disconnected(_)) => {
                    warn!("channel \"{channel_name}\" disconnected")
                }
            }
        }

        Ok(())
    }

    fn on_header(
        &mut self,
        header: &VersionedBlockHeader,
        bank_parent_slot: Slot,
    ) -> Result<(), BlockComponentProcessorError> {
        self.stage.on_header()?;

        let VersionedBlockHeader::V1(header) = header;
        if header.parent_slot != bank_parent_slot {
            return Err(BlockComponentProcessorError::HeaderParentSlotMismatch {
                header_parent_slot: header.parent_slot,
                bank_parent_slot,
            });
        }
        Ok(())
    }

    fn on_update_parent(
        &mut self,
        slot: Slot,
        update_parent: &VersionedUpdateParent,
        allow_initial_update_parent: bool,
    ) -> Result<(), BlockComponentProcessorError> {
        if leader_slot_index(slot) != 0 {
            return Err(BlockComponentProcessorError::UpdateParentNotFirstInLeaderWindow(slot));
        }

        self.stage
            .on_update_parent(update_parent, allow_initial_update_parent)
    }

    fn enforce_nanosecond_clock_bounds(
        bank: &Bank,
        parent_bank: &Bank,
        footer: &BlockFooterV1,
    ) -> Result<(), BlockComponentProcessorError> {
        // Fall back to the Clock sysvar until the parent has populated its nanosecond clock.
        let parent_time_nanos = parent_bank
            .get_nanosecond_clock()
            .unwrap_or_else(|| bank.clock().unix_timestamp.saturating_mul(1_000_000_000));

        let parent_slot = parent_bank.slot();
        let current_time_nanos =
            Self::block_producer_time_nanos_as_i64(footer.block_producer_time_nanos)?;
        let current_slot = bank.slot();
        let elapsed_slot_duration_nanos =
            bank.slot_range_duration_nanos(parent_slot.saturating_add(1), current_slot);

        let (lower_bound_nanos, upper_bound_nanos) =
            Self::nanosecond_time_bounds(parent_time_nanos, elapsed_slot_duration_nanos);

        let is_valid =
            lower_bound_nanos <= current_time_nanos && current_time_nanos <= upper_bound_nanos;

        match is_valid {
            true => Ok(()),
            false => Err(BlockComponentProcessorError::NanosecondClockOutOfBounds),
        }
    }

    /// Converts a footer timestamp to the signed nanosecond representation used
    /// by bank clock state.
    ///
    /// The `block_producer_time_nanos` parameter comes from wire-format footer
    /// data and is rejected if it cannot be represented as `i64`; wrapping it
    /// would make an extreme future timestamp look negative.
    fn block_producer_time_nanos_as_i64(
        block_producer_time_nanos: u64,
    ) -> Result<i64, BlockComponentProcessorError> {
        i64::try_from(block_producer_time_nanos)
            .map_err(|_| BlockComponentProcessorError::NanosecondClockOutOfBounds)
    }

    /// Given a parent time and elapsed slot duration, calculates inclusive
    /// block producer timestamp bounds.
    ///
    /// `parent_time_nanos` describes the parent bank's nanosecond clock.
    /// `elapsed_slot_duration_nanos` is the summed duration for all skipped
    /// and working slots after the parent. The returned `(lower_bound,
    /// upper_bound)` accepts timestamps where
    /// `lower_bound <= working_bank_time <= upper_bound`.
    ///
    /// Refer to
    /// https://github.com/solana-foundation/solana-improvement-documents/pull/363
    /// for details on the bounds calculation.
    pub fn nanosecond_time_bounds(
        parent_time_nanos: i64,
        elapsed_slot_duration_nanos: u128,
    ) -> (i64, i64) {
        let min_working_bank_time = parent_time_nanos.saturating_add(1);
        let max_working_bank_time_offset = elapsed_slot_duration_nanos
            .saturating_mul(2)
            .min(i64::MAX as u128) as i64;
        let max_working_bank_time = parent_time_nanos.saturating_add(max_working_bank_time_offset);

        (min_working_bank_time, max_working_bank_time)
    }

    pub fn update_bank_with_footer_fields(
        bank: &Bank,
        block_producer_time_nanos: i64,
        bank_hash: Option<Hash>,
        reward_cert: Option<ValidatedRewardCert>,
        final_cert_input: Option<(&HashSet<Pubkey>, Slot)>,
    ) -> Result<(), BankFooterError> {
        bank.update_clock_from_footer(block_producer_time_nanos);
        calc_vote_rewards_update_vote_states(
            bank,
            reward_cert,
            final_cert_input,
            block_producer_time_nanos,
        )?;

        if let Some(hash) = bank_hash {
            // Record expected bank hash from footer for later verification when the bank is frozen.
            bank.set_expected_bank_hash(hash);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
            genesis_utils::create_genesis_config,
        },
        agave_votor_messages::consensus_message::BlockId,
        bytes::Bytes,
        rand::Rng,
        solana_clock::DEFAULT_MS_PER_SLOT,
        solana_entry::block_component::{
            BlockFooterV1, BlockHeaderV1, UpdateParentV1, VersionedUpdateParent,
        },
        solana_hash::Hash,
        solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
        std::sync::{Arc, RwLock},
        test_case::test_case,
    };

    const DEFAULT_NS_PER_SLOT: u64 = DEFAULT_MS_PER_SLOT * 1_000_000;

    fn create_test_bank() -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let genesis_config_info = create_genesis_config(10_000);
        Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config)
    }

    fn create_child_bank(
        bank_forks: &RwLock<BankForks>,
        parent: &Arc<Bank>,
        slot: u64,
    ) -> Arc<Bank> {
        Bank::new_from_parent_with_bank_forks(
            bank_forks,
            parent.clone(),
            SlotLeader::new_unique(),
            slot,
        )
    }

    fn processor_after_header() -> BlockComponentProcessor {
        BlockComponentProcessor {
            stage: BlockComponentStage::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::BlockHeader,
            },
        }
    }

    #[test]
    fn test_on_footer_sets_timestamp() {
        let mut processor = processor_after_header();

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 200_000_000; // parent + 200ms
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        processor
            .on_footer(bank.clone(), parent, shred_version, footer, None)
            .unwrap();

        assert_eq!(processor.stage, BlockComponentStage::AcceptingAlpentick);

        // Verify clock sysvar was updated with correct timestamp (nanos converted to seconds)
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);
    }

    #[test]
    fn test_footer_sets_epoch_start_timestamp_on_epoch_change() {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        // Create genesis bank
        let genesis_config_info = create_genesis_config(10_000);
        let (genesis_bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config);

        // Get epoch schedule to find first slot of next epoch
        let epoch_schedule = genesis_bank.epoch_schedule();
        let first_slot_in_epoch_1 = epoch_schedule.get_first_slot_in_epoch(1);

        // Create parent bank at last slot of epoch 0
        let mut parent = genesis_bank.clone();
        for slot in 1..first_slot_in_epoch_1 {
            parent = create_child_bank(&bank_forks, &parent, slot);
        }

        // Create bank at first slot of epoch 1
        let bank = create_child_bank(&bank_forks, &parent, first_slot_in_epoch_1);

        // Verify we're in epoch 1
        assert_eq!(bank.epoch(), 1);

        // Calculate valid timestamp based on parent's time
        let parent_slot = parent.slot();
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let current_slot = bank.slot();
        let elapsed_slot_duration_nanos =
            bank.slot_range_duration_nanos(parent_slot.saturating_add(1), current_slot);

        // Use a timestamp in the middle of the valid range
        let (lower_bound, upper_bound) = BlockComponentProcessor::nanosecond_time_bounds(
            parent_time_nanos,
            elapsed_slot_duration_nanos,
        );
        let footer_time_nanos = (lower_bound + upper_bound) / 2;
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        processor
            .on_footer(bank.clone(), parent, shred_version, footer, None)
            .unwrap();

        // Verify clock sysvar was updated
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);

        // Verify epoch_start_timestamp was set correctly for the new epoch
        assert_eq!(bank.clock().epoch_start_timestamp, expected_time_secs);
    }

    // Test clock bounds enforcement
    #[test_case(1, |_, lower, _| lower, true; "at_minimum")]
    #[test_case(1, |_, _, upper| upper, true; "at_maximum")]
    #[test_case(1, |_, lower, _| lower - 1, false; "below_minimum")]
    #[test_case(1, |_, _, upper| upper + 1, false; "above_maximum")]
    // For 5 slots: upper_bound = parent_time + 2 * 5 * 400ms = parent_time + 4000ms
    // Use 2 seconds which is within bounds
    #[test_case(5, |_, lower, _| lower + 2_000_000_000, true; "multi_slot_gap")]
    // Exceed by 1 second beyond the upper bound
    #[test_case(5, |_, _, upper| upper + 1_000_000_000, false; "multi_slot_gap_exceeds")]
    // Timestamp equal to parent time (should fail, must be strictly greater)
    #[test_case(1, |parent_time, _, _| parent_time, false; "timestamp_equals_parent")]
    fn test_clock_bounds(
        slot_gap: u64,
        timestamp_fn: impl FnOnce(i64, i64, i64) -> i64,
        should_pass: bool,
    ) {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        let (parent, bank_forks) = create_test_bank();
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);

        // Set up clock on parent so validation doesn't skip bounds checking
        parent.update_clock_from_footer(parent_time_nanos);

        let bank: Arc<Bank> = create_child_bank(&bank_forks, &parent, slot_gap);
        let elapsed_slot_duration_nanos = bank.slot_range_duration_nanos(1, slot_gap);

        let (lower_bound, upper_bound) = BlockComponentProcessor::nanosecond_time_bounds(
            parent_time_nanos,
            elapsed_slot_duration_nanos,
        );

        let footer_time_nanos = timestamp_fn(parent_time_nanos, lower_bound, upper_bound);

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        let result = processor.on_footer(bank, parent, shred_version, footer, None);
        if should_pass {
            result.unwrap();
        } else {
            assert!(matches!(
                result.unwrap_err(),
                BlockComponentProcessorError::NanosecondClockOutOfBounds
            ));
        }
    }

    #[test]
    fn test_clock_bounds_without_parent_nanosecond_clock_rejects_out_of_bounds() {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        let (parent, bank_forks) = create_test_bank();
        assert_eq!(parent.get_nanosecond_clock(), None);

        let bank = create_child_bank(&bank_forks, &parent, 1);
        let parent_time_nanos = bank.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let elapsed_slot_duration_nanos =
            bank.slot_range_duration_nanos(parent.slot().saturating_add(1), bank.slot());
        let (_, upper_bound) = BlockComponentProcessor::nanosecond_time_bounds(
            parent_time_nanos,
            elapsed_slot_duration_nanos,
        );

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: u64::try_from(upper_bound.saturating_add(1)).unwrap(),
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        assert!(matches!(
            processor
                .on_footer(bank, parent, shred_version, footer, None)
                .unwrap_err(),
            BlockComponentProcessorError::NanosecondClockOutOfBounds
        ));
    }

    #[test]
    fn test_clock_bounds_rejects_timestamp_above_i64() {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        let (parent, bank_forks) = create_test_bank();
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        parent.update_clock_from_footer(parent_time_nanos);
        let bank = create_child_bank(&bank_forks, &parent, 1);

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: u64::MAX,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        assert!(matches!(
            processor
                .on_footer(bank, parent, shred_version, footer, None)
                .unwrap_err(),
            BlockComponentProcessorError::NanosecondClockOutOfBounds
        ));
    }

    // Helper function to test nanosecond_time_bounds calculation
    fn test_nanosecond_time_bounds_helper(
        parent_time_nanos: i64,
        elapsed_slot_duration_nanos: u128,
        expected_lower: i64,
        expected_upper: i64,
    ) {
        let (lower, upper) = BlockComponentProcessor::nanosecond_time_bounds(
            parent_time_nanos,
            elapsed_slot_duration_nanos,
        );

        assert_eq!(lower, expected_lower);
        assert_eq!(upper, expected_upper);
    }

    #[test]
    fn test_nanosecond_time_bounds_calculation() {
        // Test the nanosecond_time_bounds function directly
        // diff_slots = 15 - 10 = 5
        // lower = parent_time + 1
        // upper = parent_time + 2 * 5 * 400_000_000 = parent_time + 4_000_000_000
        let parent_slot = 10;
        let parent_time = 1_000_000_000_000; // 1000 seconds in nanos
        let working_slot = 15;
        let slot_delta = working_slot - parent_slot;
        test_nanosecond_time_bounds_helper(
            parent_time,
            u128::from(slot_delta).saturating_mul(u128::from(DEFAULT_NS_PER_SLOT)),
            parent_time + 1,
            parent_time + (2 * DEFAULT_NS_PER_SLOT * slot_delta) as i64,
        );
    }

    #[test]
    fn test_nanosecond_time_bounds_same_slot() {
        // Test with same slot (diff = 0)
        // diff_slots = 0
        // lower = parent_time + 1
        // upper = parent_time + 2 * 0 * 400_000_000 = parent_time
        // Note: In this case, lower > upper, so no timestamp would be valid
        // This is expected since we shouldn't have the same slot for parent and working bank
        let parent_time = 1_000_000_000_000;
        test_nanosecond_time_bounds_helper(parent_time, 0, parent_time + 1, parent_time);
    }

    #[test]
    fn test_nanosecond_time_bounds_saturates_upper_bound() {
        let parent_time = i64::MAX - 5;
        let (lower, upper) =
            BlockComponentProcessor::nanosecond_time_bounds(parent_time, u128::MAX);

        assert_eq!(lower, parent_time + 1);
        assert_eq!(upper, i64::MAX);
    }

    /// Each case runs a component sequence against a fresh processor — one
    /// `on_marker` / `on_entry_batch` call per component — then `on_final`,
    /// as on a full slot.
    #[test]
    fn test_processor_component_sequences() {
        use BlockComponentProcessorError as E;

        type Step = Box<
            dyn FnOnce(&mut BlockComponentProcessor) -> Result<(), BlockComponentProcessorError>,
        >;

        let (parent, bank_forks) = create_test_bank();
        // First slot of a leader window so UpdateParent passes the window check
        let slot = NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot;
        let bank = create_child_bank(&bank_forks, &parent, slot);
        assert_eq!(leader_slot_index(slot), 0);
        // A bank whose slot is not the first in its leader window, for the
        // UpdateParent window check
        let bank_not_window_start = create_child_bank(&bank_forks, &parent, slot + 1);
        assert_ne!(leader_slot_index(slot + 1), 0);
        let shred_version: u16 = rand::rng().random();

        // A timestamp inside the footer clock bounds for every case below
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = u64::try_from(parent_time_nanos + 400_000_000).unwrap();

        // One step = one direct `on_marker` call
        let marker_step = {
            let (bank, parent) = (bank.clone(), parent.clone());
            move |marker: VersionedBlockMarker, allow_initial_update_parent: bool| -> Step {
                let (bank, parent) = (bank.clone(), parent.clone());
                Box::new(move |processor| {
                    processor.on_marker(
                        bank,
                        parent,
                        shred_version,
                        marker,
                        allow_initial_update_parent,
                        None,
                    )
                })
            }
        };
        // One step = one direct `on_entry_batch` call
        let batch_step = move |entries: Vec<EntryView<Bytes>>, is_final: bool| -> Step {
            Box::new(move |processor| processor.on_entry_batch(&entries, is_final))
        };

        // Step builders: real wire types, fresh per case
        let header_with_parent_slot = |parent_slot: Slot| {
            marker_step(
                VersionedBlockMarker::from_block_header(BlockHeaderV1 {
                    parent_slot,
                    parent_block_id: Hash::default(),
                }),
                false,
            )
        };
        let header = || header_with_parent_slot(0);
        let update_parent = |allow_initial_update_parent: bool| {
            marker_step(
                VersionedBlockMarker::from_update_parent(UpdateParentV1 {
                    new_parent_slot: 0,
                    new_parent_block_id: BlockId::default(),
                }),
                allow_initial_update_parent,
            )
        };
        // Same UpdateParent marker, sent to a bank whose slot is not the
        // first in its leader window
        let update_parent_not_window_start = {
            let (bank, parent) = (bank_not_window_start.clone(), parent.clone());
            move |allow_initial_update_parent: bool| -> Step {
                let (bank, parent) = (bank.clone(), parent.clone());
                Box::new(move |processor| {
                    processor.on_marker(
                        bank,
                        parent,
                        shred_version,
                        VersionedBlockMarker::from_update_parent(UpdateParentV1 {
                            new_parent_slot: 0,
                            new_parent_block_id: BlockId::default(),
                        }),
                        allow_initial_update_parent,
                        None,
                    )
                })
            }
        };
        let footer = || {
            marker_step(
                VersionedBlockMarker::from_block_footer(BlockFooterV1 {
                    bank_hash: Hash::new_unique(),
                    block_producer_time_nanos: footer_time_nanos,
                    block_user_agent: vec![],
                    block_final_cert: None,
                    skip_reward_cert: None,
                    notar_reward_cert: None,
                }),
                false,
            )
        };
        let entries = || {
            batch_step(
                vec![EntryView {
                    num_hashes: 2,
                    hash: Hash::default(),
                    transactions: vec![],
                }],
                false,
            )
        };
        // Final batch with num_hashes != 1: never classified as the alpentick,
        // even as the last component of a full slot
        let final_entries = || {
            batch_step(
                vec![EntryView {
                    num_hashes: 2,
                    hash: Hash::default(),
                    transactions: vec![],
                }],
                true,
            )
        };
        // Single tick with num_hashes == 1: classified as the alpentick only
        // when it is the final component of a full slot
        let tick = |is_final: bool| {
            batch_step(
                vec![EntryView {
                    num_hashes: 1,
                    hash: Hash::default(),
                    transactions: vec![],
                }],
                is_final,
            )
        };

        let abandoned = || {
            E::AbandonedBank(VersionedUpdateParent::V1(UpdateParentV1 {
                new_parent_slot: 0,
                new_parent_block_id: BlockId::default(),
            }))
        };

        #[rustfmt::skip]
        let cases: Vec<(Vec<Step>, Result<(), E>)> = vec![
            // Valid block
            (vec![header(), footer(), tick(true)], Ok(())),
            (vec![header(), entries(), entries(), footer(), tick(true)], Ok(())),
            (vec![update_parent(true), entries(), footer(), tick(true)], Ok(())),
            (vec![update_parent(true), footer(), tick(true)], Ok(())),
            // Alpentick-shaped batch mid-block is a plain entry batch, not the alpentick
            (vec![header(), tick(false), footer(), tick(true)], Ok(())),
            (vec![update_parent(true), tick(false), footer(), tick(true)], Ok(())),

            // Empty block
            (vec![], Err(E::MissingBlockFooter)),

            // MissingParentMarker
            (vec![entries()], Err(E::MissingParentMarker)),
            (vec![footer()], Err(E::MissingParentMarker)),
            (vec![tick(true)], Err(E::MissingParentMarker)),
            (vec![tick(false)], Err(E::MissingParentMarker)),

            // From-shred-zero replay must not accept an initial UpdateParent
            (vec![update_parent(false)], Err(E::UnexpectedInitialUpdateParent)),

            // Duplicate / misplaced parent markers
            (vec![header(), header()], Err(E::MultipleBlockHeaders)),
            (vec![header(), entries(), header()], Err(E::MultipleBlockHeaders)),
            (vec![header(), footer(), header()], Err(E::MultipleBlockHeaders)),
            (vec![update_parent(true), header()], Err(E::SpuriousUpdateParent)),
            (vec![update_parent(true), update_parent(true)], Err(E::MultipleUpdateParents)),
            (vec![update_parent(true), update_parent(false)], Err(E::MultipleUpdateParents)),

            // Header parent slot must match the bank parent slot
            (vec![header_with_parent_slot(3)], Err(E::HeaderParentSlotMismatch { header_parent_slot: 3, bank_parent_slot: 0 })),

            // UpdateParent is only valid in the first slot of a leader window,
            // whatever the flag
            (vec![update_parent_not_window_start(true)], Err(E::UpdateParentNotFirstInLeaderWindow(5))),
            (vec![update_parent_not_window_start(false)], Err(E::UpdateParentNotFirstInLeaderWindow(5))),

            // Mid-block UpdateParent: controlled abort (fast leader handover)
            // the flag only matters as the first component
            (vec![header(), update_parent(false)], Err(abandoned())),
            (vec![header(), update_parent(true)], Err(abandoned())),
            (vec![header(), entries(), update_parent(false)], Err(abandoned())),
            (vec![header(), footer(), update_parent(false)], Err(E::SpuriousUpdateParent)),
            (vec![header(), footer(), update_parent(true)], Err(E::SpuriousUpdateParent)),

            // Alpentick (tick with is_final) only directly after the footer
            (vec![header(), tick(true)], Err(E::InvalidAlpentickPosition)),
            (vec![header(), entries(), tick(true)], Err(E::InvalidAlpentickPosition)),
            (vec![update_parent(true), tick(true)], Err(E::InvalidAlpentickPosition)),

            // Footer is terminal
            (vec![header(), footer(), entries(), tick(true)], Err(E::EntryBatchAfterBlockFooter)),
            (vec![header(), footer(), footer()], Err(E::MultipleBlockFooters)),
            // Alpentick-shaped but non-final after the footer: plain entry batch
            (vec![header(), footer(), tick(false), tick(true)], Err(E::EntryBatchAfterBlockFooter)),
            // Final but not alpentick-shaped after the footer: plain entry batch
            (vec![header(), footer(), final_entries()], Err(E::EntryBatchAfterBlockFooter)),

            // Nothing after the alpentick
            (vec![header(), footer(), tick(true), header()], Err(E::MultipleBlockHeaders)),
            (vec![header(), footer(), tick(true), entries()], Err(E::EntryBatchAfterBlockFooter)),
            (vec![header(), footer(), tick(true), tick(false)], Err(E::EntryBatchAfterBlockFooter)),
            // A second alpentick after the real one
            (vec![header(), footer(), tick(true), tick(true)], Err(E::InvalidAlpentickPosition)),
            (vec![header(), footer(), tick(true), footer()], Err(E::MultipleBlockFooters)),
            (vec![header(), footer(), tick(true), update_parent(false)], Err(E::SpuriousUpdateParent)),

            // Missing tail on a full slot
            (vec![header()], Err(E::MissingBlockFooter)),
            (vec![header(), entries()], Err(E::MissingBlockFooter)),
            (vec![update_parent(true), entries()], Err(E::MissingBlockFooter)),
            // Footer but no alpentick
            (vec![header(), footer()], Err(E::InvalidAlpentickPosition)),
        ];

        for (case_index, (steps, expected)) in cases.into_iter().enumerate() {
            let mut processor = BlockComponentProcessor::default();
            let result = steps
                .into_iter()
                .try_for_each(|step| step(&mut processor))
                .and_then(|()| processor.on_final());

            match (result, expected) {
                (Ok(()), Ok(())) => (),
                (Err(err), Err(expected_err)) => {
                    assert_eq!(
                        std::mem::discriminant(&err),
                        std::mem::discriminant(&expected_err),
                        "case {case_index}: got {err:?}, expected {expected_err:?}"
                    );
                }
                (result, expected) => {
                    panic!("case {case_index}: got {result:?}, expected {expected:?}");
                }
            }
        }
    }

    /// Every stage x event cell: `Ok(next)` asserts the transition, `Err(e)`
    /// asserts the error and that the stage is unchanged.
    #[test]
    fn test_stage_transitions() {
        type Event =
            Box<dyn Fn(&mut BlockComponentStage) -> Result<(), BlockComponentProcessorError>>;

        let update_parent_marker = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: BlockId::default(),
        });
        let abandoned =
            || BlockComponentProcessorError::AbandonedBank(update_parent_marker.clone());

        // Events: one direct stage method call each
        let on_header = || -> Event { Box::new(|stage| stage.on_header()) };
        let on_entry_batch = || -> Event { Box::new(|stage| stage.on_entry_batch()) };
        let on_update_parent = |allow_initial_update_parent: bool| -> Event {
            let marker = update_parent_marker.clone();
            Box::new(move |stage| stage.on_update_parent(&marker, allow_initial_update_parent))
        };
        let on_footer = || -> Event { Box::new(|stage| stage.on_footer()) };
        let on_alpentick = || -> Event { Box::new(|stage| stage.on_alpentick()) };
        let on_final = || -> Event { Box::new(|stage| stage.on_final()) };

        let pre_parent_marker = BlockComponentStage::PreParentMarker;
        let entries_after_header = BlockComponentStage::AcceptingEntriesOrFooter {
            parent_marker: EntryParentMarker::BlockHeader,
        };
        let entries_after_update_parent = BlockComponentStage::AcceptingEntriesOrFooter {
            parent_marker: EntryParentMarker::UpdateParent,
        };
        let accepting_alpentick = BlockComponentStage::AcceptingAlpentick;
        let done = BlockComponentStage::Done;

        #[rustfmt::skip]
        let cases: Vec<(&BlockComponentStage, Event, Result<BlockComponentStage, BlockComponentProcessorError>)> = vec![
            // - PreParentMarker: only a parent marker
            (&pre_parent_marker, on_header(), Ok(entries_after_header.clone())),
            (&pre_parent_marker, on_entry_batch(), Err(BlockComponentProcessorError::MissingParentMarker)),
            (&pre_parent_marker, on_update_parent(false), Err(BlockComponentProcessorError::UnexpectedInitialUpdateParent)),
            (&pre_parent_marker, on_update_parent(true), Ok(entries_after_update_parent.clone())),
            (&pre_parent_marker, on_footer(), Err(BlockComponentProcessorError::MissingParentMarker)),
            (&pre_parent_marker, on_alpentick(), Err(BlockComponentProcessorError::MissingParentMarker)),

            // - AcceptingEntriesOrFooter { BlockHeader }
            (&entries_after_header, on_header(), Err(BlockComponentProcessorError::MultipleBlockHeaders)),
            (&entries_after_header, on_entry_batch(), Ok(entries_after_header.clone())),
            (&entries_after_header, on_update_parent(false), Err(abandoned())),
            (&entries_after_header, on_update_parent(true), Err(abandoned())),
            (&entries_after_header, on_footer(), Ok(accepting_alpentick.clone())),
            (&entries_after_header, on_alpentick(), Err(BlockComponentProcessorError::InvalidAlpentickPosition)),

            // - AcceptingEntriesOrFooter { UpdateParent }
            // a header here is reported as a spurious UpdateParent, not a second header
            (&entries_after_update_parent, on_header(), Err(BlockComponentProcessorError::SpuriousUpdateParent)),
            (&entries_after_update_parent, on_entry_batch(), Ok(entries_after_update_parent.clone())),
            (&entries_after_update_parent, on_update_parent(false), Err(BlockComponentProcessorError::MultipleUpdateParents)),
            (&entries_after_update_parent, on_update_parent(true), Err(BlockComponentProcessorError::MultipleUpdateParents)),
            (&entries_after_update_parent, on_footer(), Ok(accepting_alpentick.clone())),
            (&entries_after_update_parent, on_alpentick(), Err(BlockComponentProcessorError::InvalidAlpentickPosition)),

            // - AcceptingAlpentick: only the alpentick
            (&accepting_alpentick, on_header(), Err(BlockComponentProcessorError::MultipleBlockHeaders)),
            (&accepting_alpentick, on_entry_batch(), Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)),
            (&accepting_alpentick, on_update_parent(false), Err(BlockComponentProcessorError::SpuriousUpdateParent)),
            (&accepting_alpentick, on_update_parent(true), Err(BlockComponentProcessorError::SpuriousUpdateParent)),
            (&accepting_alpentick, on_footer(), Err(BlockComponentProcessorError::MultipleBlockFooters)),
            (&accepting_alpentick, on_alpentick(), Ok(done.clone())),

            // - Done: nothing more
            // MultipleBlockHeaders even if the block started with an UpdateParent
            (&done, on_header(), Err(BlockComponentProcessorError::MultipleBlockHeaders)),
            (&done, on_entry_batch(), Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)),
            (&done, on_update_parent(false), Err(BlockComponentProcessorError::SpuriousUpdateParent)),
            (&done, on_update_parent(true), Err(BlockComponentProcessorError::SpuriousUpdateParent)),
            (&done, on_footer(), Err(BlockComponentProcessorError::MultipleBlockFooters)),
            (&done, on_alpentick(), Err(BlockComponentProcessorError::InvalidAlpentickPosition)),

            // - on_final: Ok only from Done
            (&pre_parent_marker, on_final(), Err(BlockComponentProcessorError::MissingBlockFooter)),
            (&entries_after_header, on_final(), Err(BlockComponentProcessorError::MissingBlockFooter)),
            (&entries_after_update_parent, on_final(), Err(BlockComponentProcessorError::MissingBlockFooter)),
            (&accepting_alpentick, on_final(), Err(BlockComponentProcessorError::InvalidAlpentickPosition)),
            (&done, on_final(), Ok(done.clone())),
        ];

        for (case_index, (start, event, expected)) in cases.into_iter().enumerate() {
            let mut stage = start.clone();
            let result = event(&mut stage);
            match (result, expected) {
                (Ok(()), Ok(next)) => {
                    assert_eq!(stage, next, "case {case_index}: from {start:?}");
                }
                (Err(err), Err(expected_err)) => {
                    assert_eq!(
                        std::mem::discriminant(&err),
                        std::mem::discriminant(&expected_err),
                        "case {case_index}: from {start:?}, got {err:?}, expected {expected_err:?}"
                    );
                    assert_eq!(&stage, start, "case {case_index}: stage changed on error");
                }
                (result, expected) => {
                    panic!(
                        "case {case_index}: from {start:?}, got {result:?}, expected {expected:?}"
                    );
                }
            }
        }
    }
}
