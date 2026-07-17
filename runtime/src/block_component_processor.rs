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
    agave_votor_messages::{
        certificate::{CertSignature, CertificateType, GenesisCert},
        consensus_message::Block,
        migration::MigrationStatus,
        sig_verified_messages::SigVerifiedBatch,
        unverified_vote_message::UnverifiedCertificate,
    },
    crossbeam_channel::Sender,
    log::*,
    solana_clock::Slot,
    solana_entry::{
        block_component::{
            BlockFooterV1, BlockMarkerV1, GenesisCertBlockMarker, VersionedBlockFooter,
            VersionedBlockHeader, VersionedBlockMarker, VersionedUpdateParent,
        },
        entry::Entry,
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
    #[error("BlockComponent detected pre-migration")]
    BlockComponentPreMigration,
    #[error("GenesisCertificate marker detected when GenesisCertificate is already populated")]
    GenesisCertificateAlreadyPopulated,
    #[error("GenesisCertificate marker detected when the cluster has Alpenglow enabled at slot 0")]
    GenesisCertificateInAlpenglowCluster,
    #[error("GenesisCertificate marker detected on a block which is not a child of genesis")]
    GenesisCertificateOnNonChild,
    #[error("GenesisCertificate was invalid and failed to verify")]
    GenesisCertificateFailedVerification,
    #[error("GenesisCertificate marker must immediately follow the block header")]
    GenesisCertificateOutOfOrder,
    #[error("FinalizationCertificate was invalid or failed to verify {0}")]
    InvalidFinalizationCertificate(#[from] BlockFinalizationCertError),
    #[error("Missing block footer")]
    MissingBlockFooter,
    #[error("Missing genesis certificate marker")]
    MissingGenesisCertificateMarker,
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
            | BlockComponentProcessorError::GenesisCertificateOutOfOrder
            | BlockComponentProcessorError::AbandonedBank(_)
            | BlockComponentProcessorError::InvalidRewardCerts(_)
            | BlockComponentProcessorError::UpdateBankFooter(_)
            | BlockComponentProcessorError::InvalidFinalizationCertificate(_) => true,
            BlockComponentProcessorError::BlockComponentPreMigration
            | BlockComponentProcessorError::GenesisCertificateAlreadyPopulated
            | BlockComponentProcessorError::GenesisCertificateInAlpenglowCluster
            | BlockComponentProcessorError::GenesisCertificateOnNonChild
            | BlockComponentProcessorError::GenesisCertificateFailedVerification
            | BlockComponentProcessorError::MissingBlockFooter
            | BlockComponentProcessorError::MissingGenesisCertificateMarker
            | BlockComponentProcessorError::MultipleUpdateParents
            | BlockComponentProcessorError::SpuriousUpdateParent
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
/// Optional Genesis marker - this is the only valid position for a genesis marker
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
    /// Immediately after the header, can accept genesis marker, entries or footer
    AcceptingGenesisOrEntries,
    /// During the entries section, can accept entries or the footer
    /// If the parent marker was a block header, can also accept an UpdateParent marker
    AcceptingEntriesOrFooter { parent_marker: EntryParentMarker },
    /// After the footer, can only accept the alpentick
    AcceptingAlpentick,
    /// After the alpentick, nothing more is accepted
    Done,
}

impl BlockComponentStage {
    /// If current stage is `PreParentMarker`, transition to `AcceptingGenesisOrEntries`
    fn on_header(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => {
                *self = Self::AcceptingGenesisOrEntries;
                Ok(())
            }
            Self::AcceptingGenesisOrEntries
            | Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::BlockHeader,
            }
            | Self::AcceptingAlpentick
            | Self::Done => Err(BlockComponentProcessorError::MultipleBlockHeaders),
            Self::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::UpdateParent,
            } => Err(BlockComponentProcessorError::SpuriousUpdateParent),
        }
    }

    /// If current stage is `AcceptingGenesisOrEntries`, transition to `AcceptingEntriesOrFooter`
    fn on_genesis_certificate(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingGenesisOrEntries => {
                *self = Self::AcceptingEntriesOrFooter {
                    parent_marker: EntryParentMarker::BlockHeader,
                };
                Ok(())
            }
            Self::AcceptingEntriesOrFooter { .. } | Self::AcceptingAlpentick | Self::Done => {
                Err(BlockComponentProcessorError::GenesisCertificateOutOfOrder)
            }
        }
    }

    /// If current stage is `AcceptingGenesisOrEntries` or `AcceptingEntriesOrFooter`, transition to
    /// `AcceptingEntriesOrFooter`
    fn on_entry_batch(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingGenesisOrEntries => {
                *self = Self::AcceptingEntriesOrFooter {
                    parent_marker: EntryParentMarker::BlockHeader,
                };
                Ok(())
            }
            Self::AcceptingEntriesOrFooter { .. } => Ok(()),
            Self::AcceptingAlpentick | Self::Done => {
                Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)
            }
        }
    }

    /// If current stage is `AcceptingGenesisOrEntries` or `AcceptingEntriesOrFooter`, return `AbandonedBank`
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
            Self::AcceptingGenesisOrEntries
            | Self::AcceptingEntriesOrFooter {
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

    /// If the current stage is `AcceptingGenesisOrEntries`, `AcceptingEntriesOrFooter`
    /// transition to `AcceptingAlpentick`
    fn on_footer(&mut self) -> Result<(), BlockComponentProcessorError> {
        match self {
            Self::PreParentMarker => Err(BlockComponentProcessorError::MissingParentMarker),
            Self::AcceptingGenesisOrEntries | Self::AcceptingEntriesOrFooter { .. } => {
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
            Self::AcceptingGenesisOrEntries => {
                Err(BlockComponentProcessorError::InvalidAlpentickPosition)
            }
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
            Self::PreParentMarker
            | Self::AcceptingGenesisOrEntries
            | Self::AcceptingEntriesOrFooter { .. } => {
                Err(BlockComponentProcessorError::MissingBlockFooter)
            }
        }
    }
}

#[derive(Default)]
pub struct BlockComponentProcessor {
    stage: BlockComponentStage,
    has_genesis_certificate_marker: bool,
}

impl BlockComponentProcessor {
    pub fn on_final(
        &self,
        migration_status: &MigrationStatus,
        slot: Slot,
        parent_slot: Slot,
    ) -> Result<(), BlockComponentProcessorError> {
        // Only require block markers (header/footer) for slots where they should be present
        if !migration_status.should_allow_block_markers(slot) {
            return Ok(());
        }

        if Self::requires_genesis_certificate_marker(migration_status, parent_slot)
            && !self.has_genesis_certificate_marker
        {
            return Err(BlockComponentProcessorError::MissingGenesisCertificateMarker);
        }

        self.stage.on_final()
    }

    /// Check if `parent_slot` is the alpenglow genesis block for use in enforcing
    /// that the block has a genesis block marker
    ///
    /// Note: We have an exemption for Dev clusters that have alpenglow active at slot 0,
    /// as these clusters do not need a genesis block marker
    fn requires_genesis_certificate_marker(
        migration_status: &MigrationStatus,
        parent_slot: Slot,
    ) -> bool {
        migration_status
            .genesis_block()
            .is_some_and(|genesis_block| {
                genesis_block.slot != 0 && parent_slot == genesis_block.slot
            })
    }

    /// Process an entry batch.
    ///
    /// Validates that a parent marker (header or update parent) has been processed
    /// before any entry batches. The terminal Alpenglow tick is the only entry
    /// batch allowed after the block footer.
    pub fn on_entry_batch(
        &mut self,
        migration_status: &MigrationStatus,
        slot: Slot,
        entries: &[Entry],
        is_final_component: bool,
    ) -> Result<(), BlockComponentProcessorError> {
        if !migration_status.should_allow_block_markers(slot) {
            return Ok(());
        }

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

    /// Process a block marker:
    /// - Pre migration, no block markers are allowed
    /// - During the migration only header and genesis certificate are allowed:
    ///     - This is in case our node was slow in observing the completion of the migration
    ///     - By seeing the first alpenglow block, we can advance the migration phase
    /// - Once the migration is complete all markers are allowed
    pub fn on_marker(
        &mut self,
        bank: Arc<Bank>,
        parent_bank: Arc<Bank>,
        shred_version: u16,
        marker: VersionedBlockMarker,
        allow_initial_update_parent: bool,
        finalization_cert_sender: Option<&Sender<SigVerifiedBatch>>,
        migration_status: &MigrationStatus,
    ) -> Result<(), BlockComponentProcessorError> {
        let slot = bank.slot();
        let VersionedBlockMarker::V1(marker) = marker;

        let markers_fully_enabled = migration_status.should_allow_block_markers(slot);
        let in_migration = migration_status.is_in_migration();
        let fast_leader_handover_active =
            bank.feature_set.snapshot().alpenglow_fast_leader_handover;

        match marker {
            // Header and genesis cert can be processed either:
            // - once migration is fully enabled, or
            // - while we're still in the migration phase (to let us advance it)
            BlockMarkerV1::BlockHeader(header) if markers_fully_enabled || in_migration => {
                self.on_header(header.inner(), bank.parent_slot())
            }
            BlockMarkerV1::GenesisCertificate(genesis_cert_block_marker)
                if markers_fully_enabled || in_migration =>
            {
                self.on_genesis_cert_block_marker(
                    bank,
                    shred_version,
                    genesis_cert_block_marker.into_inner(),
                    migration_status,
                )
            }

            // Everything else is only valid once migration is complete
            BlockMarkerV1::BlockFooter(footer) if markers_fully_enabled => self.on_footer(
                bank,
                parent_bank,
                shred_version,
                footer.into_inner(),
                finalization_cert_sender,
            ),

            BlockMarkerV1::UpdateParent(update_parent) if markers_fully_enabled => {
                if fast_leader_handover_active {
                    self.on_update_parent(slot, update_parent.inner(), allow_initial_update_parent)
                } else {
                    Err(BlockComponentProcessorError::SpuriousUpdateParent)
                }
            }

            // Any other combination means we saw a marker too early
            _ => Err(BlockComponentProcessorError::BlockComponentPreMigration),
        }
    }

    /// Processes the genesis block marker with full verification
    pub fn on_genesis_cert_block_marker(
        &mut self,
        bank: Arc<Bank>,
        shred_version: u16,
        genesis_block_marker: GenesisCertBlockMarker,
        migration_status: &MigrationStatus,
    ) -> Result<(), BlockComponentProcessorError> {
        self.stage.on_genesis_certificate()?;
        self.process_unvalidated_genesis_cert_block_marker(
            bank,
            genesis_block_marker,
            migration_status,
            Some(shred_version),
        )?;
        Ok(())
    }

    /// Processes a locally produced genesis certificate marker without verification
    pub fn on_genesis_cert_block_marker_leader(
        &mut self,
        bank: Arc<Bank>,
        genesis_block_marker: GenesisCertBlockMarker,
        migration_status: &MigrationStatus,
    ) -> Result<(), BlockComponentProcessorError> {
        self.process_unvalidated_genesis_cert_block_marker(
            bank,
            genesis_block_marker,
            migration_status,
            None,
        )?;
        Ok(())
    }

    /// Performs verification if `shred_version` is specified
    fn process_unvalidated_genesis_cert_block_marker(
        &mut self,
        bank: Arc<Bank>,
        genesis_block_marker: GenesisCertBlockMarker,
        migration_status: &MigrationStatus,
        shred_version: Option<u16>,
    ) -> Result<(), BlockComponentProcessorError> {
        // Genesis Certificate is only allowed for direct child of genesis
        if bank.parent_slot() == 0 {
            return Err(BlockComponentProcessorError::GenesisCertificateInAlpenglowCluster);
        }

        let parent_block_id = bank
            .parent_block_id()
            .expect("Block id is populated for all slots > 0");
        if (bank.parent_slot(), parent_block_id)
            != (genesis_block_marker.slot, genesis_block_marker.block_id)
        {
            return Err(BlockComponentProcessorError::GenesisCertificateOnNonChild);
        }

        if bank.get_alpenglow_genesis_certificate().is_some() {
            return Err(BlockComponentProcessorError::GenesisCertificateAlreadyPopulated);
        }

        let genesis_cert = GenesisCert {
            block: Block {
                slot: genesis_block_marker.slot,
                block_id: genesis_block_marker.block_id,
            },
            signature: CertSignature {
                signature: genesis_block_marker.bls_signature,
                bitmap: genesis_block_marker.bitmap,
            },
        };
        if let Some(shred_version) = shred_version {
            Self::verify_genesis_certificate(&bank, &genesis_cert, shred_version)?;
        }

        bank.set_alpenglow_genesis_certificate(&genesis_cert);
        bank.set_hashes_per_tick(None);
        self.has_genesis_certificate_marker = true;

        if migration_status.is_alpenglow_enabled() {
            // We participated in the migration, nothing to do
            return Ok(());
        }

        // We missed the migration however we ingested the first alpenglow block.
        // This is either a result of startup replay, or in some weird cases steady state replay after a network partition.
        // Either way we ingest the genesis block details moving us to `ReadyToEnable`.
        // Since this is a direct child of genesis, and we are replaying, we know we have frozen the genesis block.
        // Then `load_frozen_forks` or `replay_stage` will take care of the rest.
        warn!(
            "{}: Alpenglow genesis marker processed during replay of {}. Transitioning Alpenglow \
             to ReadyToEnable",
            migration_status.my_pubkey(),
            bank.slot()
        );
        migration_status.set_genesis_block(genesis_cert.block);
        migration_status.set_genesis_certificate(Arc::new(genesis_cert));
        assert!(migration_status.is_ready_to_enable());

        Ok(())
    }

    fn verify_genesis_certificate(
        bank: &Bank,
        cert: &GenesisCert,
        shred_version: u16,
    ) -> Result<(), BlockComponentProcessorError> {
        let cert_slot = cert.block.slot;
        let unverified_cert = UnverifiedCertificate {
            cert_type: CertificateType::Genesis(cert.block),
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap.clone(),
            shred_version,
        };
        bank.verify_certificate(unverified_cert).map_err(|_| {
            warn!(
                "Failed to verify genesis certificate for slot {cert_slot} in bank slot {}",
                bank.slot()
            );
            BlockComponentProcessorError::GenesisCertificateFailedVerification
        })?;

        Ok(())
    }

    fn on_footer(
        &mut self,
        bank: Arc<Bank>,
        parent_bank: Arc<Bank>,
        shred_version: u16,
        footer: VersionedBlockFooter,
        finalization_cert_sender: Option<&Sender<SigVerifiedBatch>>,
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
            if let Some(notarize_cert) = notarize_cert {
                let cert = SigVerifiedBatch::Certificates(vec![notarize_cert]);
                // TODO blocking send.
                let _ = sender
                    .send(cert)
                    .inspect_err(|_| info!("ConsensusMessage sender disconnected"));
            }
            let cert = SigVerifiedBatch::Certificates(vec![finalize_cert]);
            // TODO blocking send.
            let _ = sender
                .send(cert)
                .inspect_err(|_| info!("ConsensusMessage sender disconnected"));
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
        // Get parent time from the nanosecond clock account, or from the Tower-based
        // clock for the first Alpenglow block.
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
            genesis_utils::{activate_all_features_alpenglow, create_genesis_config},
        },
        rand::Rng,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_clock::DEFAULT_MS_PER_SLOT,
        solana_entry::{
            block_component::{
                BlockFooterV1, BlockHeaderV1, UpdateParentV1, VersionedUpdateParent,
            },
            entry::Entry,
        },
        solana_hash::Hash,
        std::{
            assert_matches,
            sync::{Arc, RwLock},
        },
    };

    const DEFAULT_NS_PER_SLOT: u64 = DEFAULT_MS_PER_SLOT * 1_000_000;

    fn create_test_bank() -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let genesis_config_info = create_genesis_config(10_000);
        Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config)
    }

    fn create_test_bank_alpenglow() -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let mut genesis_config_info = create_genesis_config(10_000);
        activate_all_features_alpenglow(&mut genesis_config_info.genesis_config);
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

    fn test_genesis_cert_marker() -> GenesisCertBlockMarker {
        GenesisCertBlockMarker {
            slot: 0,
            block_id: Hash::default(),
            bls_signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        }
    }

    fn post_migration_status_with_genesis_slot(genesis_slot: Slot) -> MigrationStatus {
        let migration_status = MigrationStatus::default();
        let migration_slot = migration_status.record_feature_activation(0);
        assert!(genesis_slot < migration_slot);

        let genesis_block = Block {
            slot: genesis_slot,
            block_id: Hash::default(),
        };
        migration_status.set_genesis_block(genesis_block);
        let cert = Arc::new(GenesisCert {
            block: genesis_block,
            signature: CertSignature {
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            },
        });
        migration_status.set_genesis_certificate(cert);
        migration_status.enable_alpenglow_during_startup();

        migration_status
    }

    fn processor_after_header() -> BlockComponentProcessor {
        BlockComponentProcessor {
            stage: BlockComponentStage::AcceptingGenesisOrEntries,
            ..BlockComponentProcessor::default()
        }
    }

    fn processor_after_footer() -> BlockComponentProcessor {
        BlockComponentProcessor {
            stage: BlockComponentStage::AcceptingAlpentick,
            ..BlockComponentProcessor::default()
        }
    }

    fn processor_done() -> BlockComponentProcessor {
        BlockComponentProcessor {
            stage: BlockComponentStage::Done,
            ..BlockComponentProcessor::default()
        }
    }

    fn alpentick(num_hashes: u64) -> [Entry; 1] {
        [Entry::new(&Hash::default(), num_hashes, vec![])]
    }

    #[test]
    fn test_missing_header_error_on_entry_batch() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();

        // Try to process entry batch without header - should fail
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::MissingParentMarker)
        ));
    }

    #[test]
    fn test_missing_header_error_on_genesis_certificate() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let marker =
            VersionedBlockMarker::from_genesis_cert_block_marker(test_genesis_cert_marker());

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        let result = processor.on_marker(
            bank,
            parent,
            shred_version,
            marker,
            false,
            None,
            &migration_status,
        );
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::MissingParentMarker)
        ));
    }

    #[test]
    fn test_genesis_certificate_after_entry_batch_errors() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });
        processor
            .on_marker(
                bank.clone(),
                parent.clone(),
                shred_version,
                header,
                false,
                None,
                &migration_status,
            )
            .unwrap();
        processor
            .on_entry_batch(&migration_status, bank.slot(), &[], false)
            .unwrap();

        let marker =
            VersionedBlockMarker::from_genesis_cert_block_marker(test_genesis_cert_marker());
        let result = processor.on_marker(
            bank,
            parent,
            shred_version,
            marker,
            false,
            None,
            &migration_status,
        );
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::GenesisCertificateOutOfOrder)
        ));
    }

    #[test]
    fn test_genesis_certificate_immediately_after_header_passes_order_check() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });
        processor
            .on_marker(
                bank.clone(),
                parent.clone(),
                shred_version,
                header,
                false,
                None,
                &migration_status,
            )
            .unwrap();

        let marker =
            VersionedBlockMarker::from_genesis_cert_block_marker(test_genesis_cert_marker());
        let result = processor.on_marker(
            bank,
            parent,
            shred_version,
            marker,
            false,
            None,
            &migration_status,
        );
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::GenesisCertificateInAlpenglowCluster)
        ));
    }

    #[test]
    fn test_missing_footer_error_on_slot_full() {
        let migration_status = MigrationStatus::post_migration_status();
        let processor = processor_after_header();

        // Try to mark slot as full without footer - should fail
        let result = processor.on_final(&migration_status, 1, 0);
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::MissingBlockFooter)
        ));
    }

    #[test]
    fn test_first_alpenglow_block_requires_genesis_certificate_marker() {
        let migration_status = post_migration_status_with_genesis_slot(1);
        let processor = processor_after_footer();

        let result = processor.on_final(&migration_status, 2, 1);
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::MissingGenesisCertificateMarker)
        ));
    }

    #[test]
    fn test_first_alpenglow_block_with_genesis_certificate_marker_succeeds() {
        let migration_status = post_migration_status_with_genesis_slot(1);
        let (genesis_bank, bank_forks) = create_test_bank();
        let parent = create_child_bank(&bank_forks, &genesis_bank, 1);
        let parent_block_id = Hash::new_unique();
        parent.set_block_id(Some(parent_block_id));
        let bank = create_child_bank(&bank_forks, &parent, 2);
        let genesis_marker = GenesisCertBlockMarker {
            slot: parent.slot(),
            block_id: parent_block_id,
            bls_signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        };
        let mut processor = processor_after_header();

        processor
            .on_genesis_cert_block_marker_leader(bank, genesis_marker, &migration_status)
            .unwrap();
        processor.stage = BlockComponentStage::Done;
        assert!(processor.on_final(&migration_status, 2, 1).is_ok());
    }

    #[test]
    fn test_first_alpenglow_block_genesis_slot_zero_skips_genesis_certificate_marker_check() {
        let migration_status = MigrationStatus::post_migration_status();
        let processor = processor_done();

        assert!(processor.on_final(&migration_status, 1, 0).is_ok());
    }

    #[test]
    fn test_multiple_headers_error() {
        let mut processor = BlockComponentProcessor::default();
        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });

        // First header should succeed
        assert!(processor.on_header(&header, 0).is_ok());

        // Second header should fail
        let result = processor.on_header(&header, 0);
        assert!(matches!(
            result,
            Err(BlockComponentProcessorError::MultipleBlockHeaders)
        ));
    }

    #[test]
    fn test_multiple_footers_error() {
        let mut processor = processor_after_header();

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 400_000_000; // parent + 400ms

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        // First footer should succeed
        processor
            .on_footer(
                bank.clone(),
                parent.clone(),
                shred_version,
                footer.clone(),
                None,
            )
            .unwrap();

        // Second footer should fail
        let err = processor
            .on_footer(bank, parent, shred_version, footer, None)
            .unwrap_err();
        assert!(matches!(
            err,
            BlockComponentProcessorError::MultipleBlockFooters
        ));
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
    fn test_on_header_sets_flag() {
        let mut processor = BlockComponentProcessor::default();
        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });

        processor.on_header(&header, 0).unwrap();
        assert_eq!(
            processor.stage,
            BlockComponentStage::AcceptingGenesisOrEntries
        );
    }

    #[test]
    fn test_on_header_parent_slot_mismatch_error() {
        let mut processor = BlockComponentProcessor::default();
        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 2,
            parent_block_id: Hash::default(),
        });

        assert!(matches!(
            processor.on_header(&header, 0),
            Err(BlockComponentProcessorError::HeaderParentSlotMismatch {
                header_parent_slot: 2,
                bank_parent_slot: 0,
            })
        ));
    }

    #[test]
    fn test_on_marker_processes_header() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let marker = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        processor
            .on_marker(
                bank,
                parent,
                shred_version,
                marker,
                false,
                None,
                &migration_status,
            )
            .unwrap();
        assert_eq!(
            processor.stage,
            BlockComponentStage::AcceptingGenesisOrEntries
        );
    }

    #[test]
    fn test_on_marker_rejects_header_parent_slot_mismatch() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let marker = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 7, // mismatches bank.parent_slot() below (0)
            parent_block_id: Hash::default(),
        });

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        assert!(matches!(
            processor.on_marker(
                bank,
                parent,
                shred_version,
                marker,
                false,
                None,
                &migration_status
            ),
            Err(BlockComponentProcessorError::HeaderParentSlotMismatch {
                header_parent_slot: 7,
                bank_parent_slot: 0,
            })
        ));
    }

    #[test]
    fn test_on_marker_processes_footer() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = processor_after_header();

        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 300_000_000; // parent + 300ms
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        let marker = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        processor
            .on_marker(
                bank.clone(),
                parent,
                shred_version,
                marker,
                false,
                None,
                &migration_status,
            )
            .unwrap();
        assert_eq!(processor.stage, BlockComponentStage::AcceptingAlpentick);

        // Verify clock sysvar was updated
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);
    }

    #[test]
    fn test_complete_workflow_success() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 100_000_000; // parent + 100ms
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        // Process header
        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });
        processor.on_header(&header, bank.parent_slot()).unwrap();

        // Process some entry batches (not full yet)
        processor
            .on_entry_batch(&migration_status, 1, &[], false)
            .unwrap();

        // Process footer with valid timestamp
        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        processor
            .on_footer(bank.clone(), parent.clone(), shred_version, footer, None)
            .unwrap();

        // Verify clock sysvar was updated
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);

        // Entry batch after footer should fail because the footer is terminal.
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert_matches!(
            result,
            Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)
        );
    }

    #[test]
    fn test_alpentick_position_validation() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = processor_after_footer();
        let good_alpentick = alpentick(1);

        processor
            .on_entry_batch(&migration_status, 1, &good_alpentick, true)
            .unwrap();
        assert_matches!(
            processor.on_entry_batch(&migration_status, 1, &good_alpentick, true),
            Err(BlockComponentProcessorError::InvalidAlpentickPosition)
        );

        let mut processor = BlockComponentProcessor::default();
        assert_matches!(
            processor.on_entry_batch(&migration_status, 1, &good_alpentick, true),
            Err(BlockComponentProcessorError::MissingParentMarker)
        );

        let mut processor = processor_after_footer();
        let bad_alpentick = alpentick(2);
        assert_matches!(
            processor.on_entry_batch(&migration_status, 1, &bad_alpentick, true),
            Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)
        );

        let migration_status = MigrationStatus::default();
        let mut processor = BlockComponentProcessor::default();
        processor
            .on_entry_batch(&migration_status, 1, &good_alpentick, true)
            .unwrap();
    }

    #[test]
    fn test_block_marker_detected_pre_migration() {
        let migration_status = MigrationStatus::default();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Try to process a block header marker pre-migration - should fail
        let marker = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });

        let err = processor
            .on_marker(
                bank,
                parent,
                shred_version,
                marker,
                false,
                None,
                &migration_status,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            BlockComponentProcessorError::BlockComponentPreMigration
        ));
    }

    #[test]
    fn test_footer_and_update_parent_rejected_pre_migration() {
        let migration_status = MigrationStatus::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_marker = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: (parent_time_nanos + 500_000_000) as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        let mut processor = BlockComponentProcessor::default();
        assert!(matches!(
            processor
                .on_marker(
                    bank.clone(),
                    parent.clone(),
                    shred_version,
                    footer_marker,
                    false,
                    None,
                    &migration_status
                )
                .unwrap_err(),
            BlockComponentProcessorError::BlockComponentPreMigration
        ));

        let update_parent_marker = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        let mut processor = BlockComponentProcessor::default();
        assert!(matches!(
            processor
                .on_marker(
                    bank,
                    parent,
                    shred_version,
                    update_parent_marker,
                    false,
                    None,
                    &migration_status
                )
                .unwrap_err(),
            BlockComponentProcessorError::BlockComponentPreMigration
        ));
    }

    #[test]
    fn test_entry_batch_pre_migration_succeeds() {
        let migration_status = MigrationStatus::default();
        let mut processor = BlockComponentProcessor::default();

        // Processing entry batches pre-migration (without markers) should succeed
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert!(result.is_ok());

        // Even with slot full
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_complete_workflow_post_migration() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Process header marker
        let header_marker = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });
        processor
            .on_marker(
                bank.clone(),
                parent.clone(),
                shred_version,
                header_marker,
                false,
                None,
                &migration_status,
            )
            .unwrap();

        // Process entry batches
        processor
            .on_entry_batch(&migration_status, 1, &[], false)
            .unwrap();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 500_000_000; // parent + 500ms
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        // Process footer marker
        let footer_marker = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        processor
            .on_marker(
                bank.clone(),
                parent,
                shred_version,
                footer_marker,
                false,
                None,
                &migration_status,
            )
            .unwrap();

        // Verify clock sysvar was updated
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);

        // Entry batch after footer should fail because the footer is terminal.
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert_matches!(
            result,
            Err(BlockComponentProcessorError::EntryBatchAfterBlockFooter)
        );
    }

    #[test]
    fn test_footer_without_header_errors() {
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 1_000_000_000,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        // Try to process footer without header - should fail
        let err = processor
            .on_footer(bank, parent, shred_version, footer, None)
            .unwrap_err();
        assert!(matches!(
            err,
            BlockComponentProcessorError::MissingParentMarker
        ));
    }

    #[test]
    fn test_marker_with_footer_at_slot_full() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = processor_after_header();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 1);
        let shred_version = rand::rng().random();

        // Calculate valid timestamp based on parent's time
        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer_time_nanos = parent_time_nanos + 600_000_000; // parent + 600ms
        let expected_time_secs = footer_time_nanos / 1_000_000_000;

        // Process footer marker
        let footer_marker = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: footer_time_nanos as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        // Should succeed - footer is processed
        processor
            .on_marker(
                bank.clone(),
                parent,
                shred_version,
                footer_marker,
                false,
                None,
                &migration_status,
            )
            .unwrap();
        assert_eq!(processor.stage, BlockComponentStage::AcceptingAlpentick);

        // Verify clock sysvar was updated
        assert_eq!(bank.clock().unix_timestamp, expected_time_secs);
    }

    #[test]
    fn test_entry_batch_with_header_not_full_succeeds() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = processor_after_header();

        // Process entry batch with header but not full - should succeed even without footer
        let result = processor.on_entry_batch(&migration_status, 1, &[], false);
        assert!(result.is_ok());
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

    // Helper function to test clock bounds enforcement
    fn test_clock_bounds_helper(
        slot_gap: u64,
        timestamp_fn: impl FnOnce(i64, i64, i64) -> i64,
        should_pass: bool,
    ) {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        let (parent, bank_forks) = create_test_bank_alpenglow();
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
    fn test_clock_bounds_at_minimum() {
        test_clock_bounds_helper(1, |_, lower, _| lower, true);
    }

    #[test]
    fn test_clock_bounds_at_maximum() {
        test_clock_bounds_helper(1, |_, _, upper| upper, true);
    }

    #[test]
    fn test_clock_bounds_below_minimum() {
        test_clock_bounds_helper(1, |_, lower, _| lower - 1, false);
    }

    #[test]
    fn test_clock_bounds_above_maximum() {
        test_clock_bounds_helper(1, |_, _, upper| upper + 1, false);
    }

    #[test]
    fn test_clock_bounds_multi_slot_gap() {
        // For 5 slots: upper_bound = parent_time + 2 * 5 * 400ms = parent_time + 4000ms
        // Use 2 seconds which is within bounds
        test_clock_bounds_helper(5, |_, lower, _| lower + 2_000_000_000, true);
    }

    #[test]
    fn test_clock_bounds_multi_slot_gap_exceeds() {
        // Exceed by 1 second beyond the upper bound
        test_clock_bounds_helper(5, |_, _, upper| upper + 1_000_000_000, false);
    }

    #[test]
    fn test_clock_bounds_timestamp_equals_parent() {
        // Timestamp equal to parent time (should fail, must be strictly greater)
        test_clock_bounds_helper(1, |parent_time, _, _| parent_time, false);
    }

    #[test]
    fn test_clock_bounds_without_parent_nanosecond_clock_rejects_out_of_bounds() {
        let mut processor = processor_after_header();
        let shred_version = rand::rng().random();

        let (parent, bank_forks) = create_test_bank_alpenglow();
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

        let (parent, bank_forks) = create_test_bank_alpenglow();
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

    #[test]
    fn test_initial_up_reject() {
        let mut processor = BlockComponentProcessor::default();
        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        assert!(matches!(
            processor.on_update_parent(4, &update_parent, false),
            Err(BlockComponentProcessorError::UnexpectedInitialUpdateParent)
        ));
        assert_eq!(processor.stage, BlockComponentStage::PreParentMarker);
    }

    #[test]
    fn test_update_parent_rejects_non_first_leader_window_slot() {
        let mut processor = BlockComponentProcessor::default();
        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        assert!(matches!(
            processor.on_update_parent(5, &update_parent, true),
            Err(BlockComponentProcessorError::UpdateParentNotFirstInLeaderWindow(5))
        ));
        assert_eq!(processor.stage, BlockComponentStage::PreParentMarker);
    }

    #[test]
    fn test_initial_up_ok() {
        let mut processor = BlockComponentProcessor::default();
        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        processor.on_update_parent(4, &update_parent, true).unwrap();
        assert_eq!(
            processor.stage,
            BlockComponentStage::AcceptingEntriesOrFooter {
                parent_marker: EntryParentMarker::UpdateParent,
            }
        );
    }

    #[test]
    fn test_update_parent_after_header_abandoned_bank() {
        let mut processor = BlockComponentProcessor::default();
        processor
            .on_header(
                &VersionedBlockHeader::V1(BlockHeaderV1 {
                    parent_slot: 0,
                    parent_block_id: Hash::default(),
                }),
                0,
            )
            .unwrap();

        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        assert!(matches!(
            processor.on_update_parent(4, &update_parent, false),
            Err(BlockComponentProcessorError::AbandonedBank(_))
        ));
    }

    #[test]
    fn test_update_parent_after_footer_error() {
        let mut processor = processor_after_footer();
        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        assert_matches!(
            processor.on_update_parent(4, &update_parent, false),
            Err(BlockComponentProcessorError::SpuriousUpdateParent)
        );
    }

    #[test]
    fn test_multiple_update_parents_error() {
        let mut processor = BlockComponentProcessor::default();
        let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
            new_parent_slot: 0,
            new_parent_block_id: Hash::default(),
        });

        // First should succeed
        processor.on_update_parent(4, &update_parent, true).unwrap();

        // Second should fail
        assert_matches!(
            processor.on_update_parent(4, &update_parent, true),
            Err(BlockComponentProcessorError::MultipleUpdateParents)
        );
    }

    #[test]
    fn test_header_after_update_parent_error() {
        let mut processor = BlockComponentProcessor::default();
        processor
            .on_update_parent(
                4,
                &VersionedUpdateParent::V1(UpdateParentV1 {
                    new_parent_slot: 0,
                    new_parent_block_id: Hash::default(),
                }),
                true,
            )
            .unwrap();

        let header = VersionedBlockHeader::V1(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });

        assert!(matches!(
            processor.on_header(&header, 0),
            Err(BlockComponentProcessorError::SpuriousUpdateParent)
        ));
    }

    #[test]
    fn test_workflow_with_update_parent() {
        let migration_status = MigrationStatus::post_migration_status();
        let mut processor = BlockComponentProcessor::default();
        let (parent, bank_forks) = create_test_bank();
        let bank = create_child_bank(&bank_forks, &parent, 4);
        let slot = bank.slot();
        let shred_version = rand::rng().random();

        processor
            .on_update_parent(
                slot,
                &VersionedUpdateParent::V1(UpdateParentV1 {
                    new_parent_slot: 0,
                    new_parent_block_id: Hash::default(),
                }),
                true,
            )
            .unwrap();

        processor
            .on_entry_batch(&migration_status, slot, &[], false)
            .unwrap();

        let parent_time_nanos = parent.clock().unix_timestamp.saturating_mul(1_000_000_000);
        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: (parent_time_nanos + 100_000_000) as u64,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        processor
            .on_footer(bank, parent, shred_version, footer, None)
            .unwrap();

        let good_alpentick = alpentick(1);
        processor
            .on_entry_batch(&migration_status, slot, &good_alpentick, true)
            .unwrap();

        processor.on_final(&migration_status, slot, 0).unwrap();
    }
}
