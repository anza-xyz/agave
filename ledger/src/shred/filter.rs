//! Relevant structures and filtering logic to be applied to ingested and recovered shreds.
//! as well as shred level feature flag detection

use {
    super::{
        DATA_SHREDS_PER_FEC_BLOCK, Error, MAX_CODE_SHREDS_PER_SLOT, MAX_DATA_SHREDS_PER_SLOT,
        Payload, ReedSolomonCache, Shred, ShredFetchStats, ShredFlags, ShredType, ShredVariant,
        layout, merkle,
    },
    crate::blockstore,
    agave_feature_set::discard_unexpected_data_complete_shreds,
    assert_matches::debug_assert_matches,
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_perf::packet::PacketRef,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};

pub struct ShredFilterContext {
    last_updated: Instant,

    // Fields for slot range filtering updated via the root bank
    root: Slot,
    max_slot: Slot,
    epoch_schedule: EpochSchedule,

    // Static from startup, filter out shreds of invalid version
    shred_version: u16,

    // Whether to discard shreds that are not end of FEC set
    // but specify DATA_COMPLETE
    discard_unexpected_data_complete_shreds_feature_slot: Option<Slot>,

    // The shred limits per slot. At the moment these are hard coded
    // In the future we could replace these with feature activation
    // slots to case on what the limits should be
    max_data_shreds_per_slot: u32,
    max_code_shreds_per_slot: u32,

    pub stats: ShredFetchStats,
}

impl ShredFilterContext {
    pub fn new(root_bank: Arc<Bank>, shred_version: u16) -> Self {
        Self::new_with_custom_shred_limits(
            root_bank,
            shred_version,
            MAX_DATA_SHREDS_PER_SLOT as u32,
            MAX_CODE_SHREDS_PER_SLOT as u32,
        )
    }

    pub fn new_with_custom_shred_limits(
        root_bank: Arc<Bank>,
        shred_version: u16,
        max_data_shreds_per_slot: u32,
        max_code_shreds_per_slot: u32,
    ) -> Self {
        let root = root_bank.slot();
        let max_slot = max_shred_slot(root, root_bank.get_slots_in_epoch(root_bank.epoch()));
        let discard_unexpected_data_complete_shreds_feature_slot = root_bank
            .feature_set
            .activated_slot(&discard_unexpected_data_complete_shreds::id());

        Self {
            last_updated: Instant::now(),
            root,
            max_slot,
            epoch_schedule: root_bank.epoch_schedule().clone(),
            shred_version,
            discard_unexpected_data_complete_shreds_feature_slot,
            max_data_shreds_per_slot,
            max_code_shreds_per_slot,
            stats: ShredFetchStats::default(),
        }
    }

    /// Periodically update filtering context based on the root bank (no more than once per slot)
    /// This is done to amortize the cost over multiple shreds, and delay in updating is completely
    /// acceptable as max shred / feature flag filtering has tolerance on the order of an epoch.
    pub fn maybe_update(&mut self, root_bank: Arc<Bank>) {
        if self.last_updated.elapsed().as_nanos() > root_bank.ns_per_slot {
            self.last_updated = Instant::now();
            self.root = root_bank.slot();
            self.max_slot =
                max_shred_slot(self.root, root_bank.get_slots_in_epoch(root_bank.epoch()));
            debug_assert!(self.root < self.max_slot);

            self.epoch_schedule = root_bank.epoch_schedule().clone();
            self.discard_unexpected_data_complete_shreds_feature_slot = root_bank
                .feature_set
                .activated_slot(&discard_unexpected_data_complete_shreds::id());

            // In the future as shred limit changes we can update self.max_*_shreds_per_slot based on root
            // bank as well
        }
    }

    pub fn maybe_submit_stats(&mut self, metric_name: &'static str, cadence: Duration) -> bool {
        self.stats.maybe_submit(metric_name, cadence)
    }

    #[must_use]
    pub fn should_discard_packet<'a, P>(&mut self, packet: P) -> bool
    where
        P: Into<PacketRef<'a>>,
    {
        let Some(shred) = layout::get_shred(packet) else {
            self.stats.index_overrun += 1;
            return true;
        };
        self.should_discard_shred(shred)
    }

    #[must_use]
    pub fn should_discard_shred(&mut self, shred: &[u8]) -> bool {
        match layout::get_version(shred) {
            None => {
                self.stats.index_overrun += 1;
                return true;
            }
            Some(version) => {
                if version != self.shred_version {
                    self.stats.shred_version_mismatch += 1;
                    return true;
                }
            }
        }
        let Ok(shred_variant) = layout::get_shred_variant(shred) else {
            self.stats.bad_shred_type += 1;
            return true;
        };
        let slot = match layout::get_slot(shred) {
            Some(slot) => {
                if slot > self.max_slot {
                    self.stats.slot_out_of_range += 1;
                    return true;
                }
                slot
            }
            None => {
                self.stats.slot_bad_deserialize += 1;
                return true;
            }
        };
        let Some(index) = layout::get_index(shred) else {
            self.stats.index_bad_deserialize += 1;
            return true;
        };
        let Some(fec_set_index) = layout::get_fec_set_index(shred) else {
            self.stats.fec_set_index_bad_deserialize += 1;
            return true;
        };

        match ShredType::from(shred_variant) {
            ShredType::Code => {
                if index >= self.max_code_shreds_per_slot {
                    self.stats.index_out_of_bounds += 1;
                    return true;
                }
                if slot <= self.root {
                    self.stats.slot_out_of_range += 1;
                    return true;
                }

                let Ok(erasure_config) = layout::get_erasure_config(shred) else {
                    self.stats.erasure_config_bad_deserialize += 1;
                    return true;
                };

                if !erasure_config.is_fixed() {
                    self.stats.misaligned_erasure_config += 1;
                    return true;
                }
            }
            ShredType::Data => {
                if index >= self.max_data_shreds_per_slot {
                    self.stats.index_out_of_bounds += 1;
                    return true;
                }
                let Some(parent_offset) = layout::get_parent_offset(shred) else {
                    self.stats.bad_parent_offset += 1;
                    return true;
                };
                let Some(parent) = slot.checked_sub(Slot::from(parent_offset)) else {
                    self.stats.bad_parent_offset += 1;
                    return true;
                };
                if !blockstore::verify_shred_slots(slot, parent, self.root) {
                    self.stats.slot_out_of_range += 1;
                    return true;
                }

                let Ok(shred_flags) = layout::get_flags(shred) else {
                    self.stats.shred_flags_bad_deserialize += 1;
                    return true;
                };

                let expected_data_complete_index = fec_set_index
                    .checked_add(DATA_SHREDS_PER_FEC_BLOCK as u32)
                    .and_then(|index| index.checked_sub(1));
                if shred_flags.contains(ShredFlags::DATA_COMPLETE_SHRED)
                    && (expected_data_complete_index != Some(index))
                {
                    self.stats.unexpected_data_complete_shred += 1;

                    if check_feature_activation(
                        self.discard_unexpected_data_complete_shreds_feature_slot,
                        slot,
                        &self.epoch_schedule,
                    ) {
                        return true;
                    }
                }

                if shred_flags.contains(ShredFlags::LAST_SHRED_IN_SLOT)
                    && !check_last_data_shred_index(index)
                {
                    self.stats.misaligned_last_data_index += 1;
                    return true;
                }
            }
        }

        if !check_fixed_fec_set(index, fec_set_index) {
            self.stats.misaligned_fec_set += 1;
            return true;
        }

        match shred_variant {
            ShredVariant::MerkleCode { .. } => {
                self.stats.num_shreds_merkle_code_chained =
                    self.stats.num_shreds_merkle_code_chained.saturating_add(1);
            }
            ShredVariant::MerkleData { .. } => {
                self.stats.num_shreds_merkle_data_chained =
                    self.stats.num_shreds_merkle_data_chained.saturating_add(1);
            }
        }
        false
    }
}

/// Returns true if the feature is effective for the shred slot.
/// Note: unlike normal feature flags, shred feature flags take effect 1 epoch after activation.
#[must_use]
pub fn check_feature_activation_from_bank(
    feature: &Pubkey,
    shred_slot: Slot,
    root_bank: &Bank,
) -> bool {
    check_feature_activation(
        root_bank.feature_set.activated_slot(feature),
        shred_slot,
        root_bank.epoch_schedule(),
    )
}

/// Returns true if the feature is effective for the shred slot.
/// Note: unlike normal feature flags, shred feature flags take effect 1 epoch after activation.
#[must_use]
pub fn check_feature_activation(
    feature_slot: Option<Slot>,
    shred_slot: Slot,
    epoch_schedule: &EpochSchedule,
) -> bool {
    let Some(feature_slot) = feature_slot else {
        return false;
    };

    let feature_epoch = epoch_schedule.get_epoch(feature_slot);
    let shred_epoch = epoch_schedule.get_epoch(shred_slot);
    feature_epoch < shred_epoch
}

/// The maximum shred slot we allow for ingest given a current `root` slot.
fn max_shred_slot(root: Slot, slots_per_epoch: Slot) -> Slot {
    // When running with very short epochs (e.g. for testing), we want to avoid
    // filtering out shreds that we actually need. This value was chosen empirically
    // because it's large enough to protect against observed short epoch problems
    // while being small enough to keep the overhead small on deduper, blockstore,
    // etc.
    const MAX_SHRED_DISTANCE_MINIMUM: Slot = 500;
    // Allow shreds up to 2 epochs into the future to support catching up to the tip of the cluster.
    root.saturating_add(MAX_SHRED_DISTANCE_MINIMUM.max(2 * slots_per_epoch))
}

/// Returns true if `index` and `fec_set_index` are valid under the assumption that
/// all erasure sets contain exactly `DATA_SHREDS_PER_FEC_BLOCK` data and coding shreds:
/// - `index` is between `fec_set_index` and `fec_set_index + DATA_SHREDS_PER_FEC_BLOCK`
/// - `fec_set_index` is a multiple of `DATA_SHREDS_PER_FEC_BLOCK`
fn check_fixed_fec_set(index: u32, fec_set_index: u32) -> bool {
    let Some(fec_set_end_exclusive) = fec_set_index.checked_add(DATA_SHREDS_PER_FEC_BLOCK as u32)
    else {
        return false;
    };
    index >= fec_set_index
        && index < fec_set_end_exclusive
        && fec_set_index.is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
}

/// Returns true if `index` of the last data shred is valid under the assumption that
/// all erasure sets contain exactly `DATA_SHREDS_PER_FEC_BLOCK` data and coding shreds:
/// - `index + 1` must be a multiple of `DATA_SHREDS_PER_FEC_BLOCK`
///
/// Note: this check is critical to verify that the last fec set is sufficiently sized.
/// This is checked during shred ingest with `enforce_fixed_fec_set` active.
fn check_last_data_shred_index(index: u32) -> bool {
    (index + 1).is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
}

/// Holds the context to perform filtering on shred recovery
pub struct ShredRecoveryContext {
    /// Used to perform RS erasure code recovery
    pub reed_solomon_cache: ReedSolomonCache,
    /// Sender to retransmit the recovered shreds
    retransmit_sender: EvictingSender<Vec<Payload>>,
    /// Used for filtering recovered shreds
    shred_filter_ctx: ShredFilterContext,
}

impl ShredRecoveryContext {
    pub fn new(
        reed_solomon_cache: ReedSolomonCache,
        retransmit_sender: EvictingSender<Vec<Payload>>,
        root_bank: Arc<Bank>,
        shred_version: u16,
    ) -> Self {
        let shred_filter_ctx = ShredFilterContext::new(root_bank, shred_version);
        Self {
            reed_solomon_cache,
            retransmit_sender,
            shred_filter_ctx,
        }
    }

    /// Periodically (no more than once per slot) update the context used for filtering
    /// recovered shreds. This is not latency sensitive as feature flags and slot filtering windows
    /// have an epoch order tolerance
    pub fn maybe_update(&mut self, root_bank: Arc<Bank>) {
        self.shred_filter_ctx.maybe_update(root_bank);
    }

    /// Submit stats at most every two seconds
    pub fn maybe_submit_stats(&mut self) {
        self.shred_filter_ctx
            .maybe_submit_stats("shred-recovery", Duration::from_secs(2));
    }

    /// Recover shreds and apply filtering to the recovered shreds
    pub fn recover<T: IntoIterator<Item = Shred>>(
        &mut self,
        shreds: T,
        recovered_shreds: &mut Vec<Payload>,
        recovered_data_shreds: &mut Vec<Shred>,
    ) -> Result<(), Error> {
        let shreds = shreds
            .into_iter()
            .map(|shred| {
                debug_assert_matches!(
                    shred.common_header().shred_variant,
                    ShredVariant::MerkleCode { .. } | ShredVariant::MerkleData { .. }
                );
                merkle::Shred::try_from(shred)
            })
            .collect::<Result<_, _>>()?;

        // With Merkle shreds, leader signs the Merkle root of the erasure batch
        // and all shreds within the same erasure batch have the same signature.
        // For recovered shreds, the (unique) signature is copied from shreds which
        // were received from turbine (or repair) and are already sig-verified.
        // The same signature also verifies for recovered shreds because when
        // reconstructing the Merkle tree for the erasure batch, we will obtain the
        // same Merkle root.
        let shreds = merkle::recover(shreds, &self.reed_solomon_cache)?;
        shreds
            .filter_map(|shred| shred.ok().map(Shred::from))
            .filter(|shred| !self.should_discard_shred(shred))
            .for_each(|shred| {
                // All shreds should be retransmitted, but because there are no
                // more missing data shreds in the erasure batch, coding shreds
                // are not stored in blockstore.
                match shred.shred_type() {
                    ShredType::Code => {
                        // Don't need Arc overhead here!
                        recovered_shreds.push(shred.into_payload());
                    }
                    ShredType::Data => {
                        // Verify that the cloning is cheap here.
                        recovered_shreds.push(shred.payload().clone());
                        recovered_data_shreds.push(shred);
                    }
                }
            });
        Ok(())
    }
    /// Send recovered shreds for retransmit
    pub fn try_retransmit_shreds(&self, recovered_shreds: Vec<Payload>) {
        if !recovered_shreds.is_empty() {
            let _ = self.retransmit_sender.try_send(recovered_shreds);
        }
    }

    /// If SIMD-0337 is active, apply filtering rules to recovered shreds
    pub fn should_discard_shred(&mut self, shred: &Shred) -> bool {
        if !check_feature_activation(
            self.shred_filter_ctx
                .discard_unexpected_data_complete_shreds_feature_slot,
            shred.slot(),
            &self.shred_filter_ctx.epoch_schedule,
        ) {
            // Only apply filtering rules to recovered shreds when SIMD-0337 is active
            return false;
        }

        self.shred_filter_ctx.should_discard_shred(shred.payload())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            super::{Shred, make_merkle_shreds_for_tests},
            *,
        },
        crate::{genesis_utils::create_genesis_config, shred::tests::*},
        assert_matches::assert_matches,
        itertools::Itertools,
        solana_leader_schedule::SlotLeader,
        solana_perf::packet::Packet,
        solana_runtime::bank::Bank,
        std::{
            io::{Cursor, Seek, SeekFrom, Write},
            sync::Arc,
        },
        test_case::test_case,
    };

    fn new_test_bank(slot: Slot, discard_unexpected_data_complete_shreds: bool) -> Arc<Bank> {
        let genesis_config = create_genesis_config(1).genesis_config;
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_id = agave_feature_set::discard_unexpected_data_complete_shreds::id();
        bank.deactivate_feature(&feature_id);
        if discard_unexpected_data_complete_shreds {
            bank.activate_feature(&feature_id);
        }
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        if slot == 0 {
            bank
        } else {
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), slot)
        }
    }

    #[test_case(true ; "last_in_slot")]
    #[test_case(false ; "not_last_in_slot")]
    fn test_should_discard_shred(is_last_in_slot: bool) {
        agave_logger::setup();
        let mut rng = rand::rng();
        let slot = 200;
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            is_last_in_slot,
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);

        assert_matches!(shreds[0].shred_type(), ShredType::Data);
        let parent_slot = shreds[0].parent().unwrap();
        let shred_version = shreds[0].common_header().version;

        let root_bank = new_test_bank(0, false);
        let parent_exceeded_root_bank = new_test_bank(parent_slot + 1, false);
        let slot_root_bank = new_test_bank(slot, false);
        let mut packet = Packet::default();

        {
            let shred = shreds.first().unwrap();
            assert_eq!(shred.shred_type(), ShredType::Data);
            shred.copy_to_packet(&mut packet);
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(!shred_filter_context.should_discard_packet(&packet));
        }
        {
            let mut packet = packet.clone();
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            packet.meta_mut().size = OFFSET_OF_SHRED_VARIANT;
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_overrun, 1);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX;
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_overrun, 2);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + 1;
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_overrun, 3);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX - 1;
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_overrun, 4);

            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX + 2;
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_overrun, 5);
        }
        {
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version.wrapping_add(1));
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.shred_version_mismatch, 1);
        }
        {
            let mut shred_filter_context =
                ShredFilterContext::new(parent_exceeded_root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.slot_out_of_range, 1);
        }
        {
            let parent_offset = 0u16;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor.seek(SeekFrom::Start(83)).unwrap();
                cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            }
            assert_eq!(
                layout::get_parent_offset(packet.data(..).unwrap()),
                Some(parent_offset)
            );
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.slot_out_of_range, 1);
        }
        {
            let parent_offset = u16::try_from(slot + 1).unwrap();
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor.seek(SeekFrom::Start(83)).unwrap();
                cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            }
            assert_eq!(
                layout::get_parent_offset(packet.data(..).unwrap()),
                Some(parent_offset)
            );
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.bad_parent_offset, 1);
        }
        {
            let index = u32::MAX - 10;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&index.to_le_bytes()).unwrap();
            }
            assert_eq!(layout::get_index(packet.data(..).unwrap()), Some(index));
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_out_of_bounds, 1);
        }

        {
            let shred = shreds.last().unwrap();
            assert_eq!(shred.shred_type(), ShredType::Code);
            shreds.last().unwrap().copy_to_packet(&mut packet);
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(!shred_filter_context.should_discard_packet(&packet));
        }
        {
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version.wrapping_add(1));
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.shred_version_mismatch, 1);
        }
        {
            let mut shred_filter_context =
                ShredFilterContext::new(slot_root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.slot_out_of_range, 1);
        }
        {
            let index = u32::try_from(MAX_CODE_SHREDS_PER_SLOT).unwrap();
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&index.to_le_bytes()).unwrap();
            }
            assert_eq!(layout::get_index(packet.data(..).unwrap()), Some(index));
            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.index_out_of_bounds, 1);
        }
    }

    #[test]
    fn test_should_discard_shred_fec_set_checks() {
        agave_logger::setup();
        let mut rng = rand::rng();
        let slot = 200;
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            false,    // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);

        assert_matches!(shreds[0].shred_type(), ShredType::Data);
        let shred_version = shreds[0].common_header().version;
        let root_bank = new_test_bank(0, false);

        {
            let mut packet = Packet::default();
            shreds[0].copy_to_packet(&mut packet);

            let bad_fec_set_index = 5u32;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                    .unwrap();
                cursor.write_all(&bad_fec_set_index.to_le_bytes()).unwrap();
            }

            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.misaligned_fec_set, 1);
        }

        {
            let mut packet = Packet::default();
            shreds[0].copy_to_packet(&mut packet);

            let fec_set_index = 64u32;
            let bad_index = 100u32;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                    .unwrap();
                cursor.write_all(&bad_index.to_le_bytes()).unwrap();
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                    .unwrap();
                cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
            }

            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.misaligned_fec_set, 1);
        }

        {
            let code_shred = shreds
                .iter()
                .find(|s| s.shred_type() == ShredType::Code)
                .unwrap();
            let mut packet = Packet::default();
            code_shred.copy_to_packet(&mut packet);

            let bad_num_data = 16u16;
            {
                let mut cursor = Cursor::new(packet.buffer_mut());
                cursor
                    .seek(SeekFrom::Start(OFFSET_OF_NUM_DATA as u64))
                    .unwrap();
                cursor.write_all(&bad_num_data.to_le_bytes()).unwrap();
            }

            let mut shred_filter_context =
                ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(shred_filter_context.should_discard_packet(&packet));
            assert_eq!(shred_filter_context.stats.misaligned_erasure_config, 1);
        }

        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            true,     // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();
        let shred_version = shreds[0].common_header().version;
        let data_shreds: Vec<_> = shreds
            .iter()
            .filter(|s| s.shred_type() == ShredType::Data)
            .collect();
        let last_data_shred = data_shreds.last().unwrap();
        assert!(last_data_shred.last_in_slot());
        let mut packet = Packet::default();
        last_data_shred.copy_to_packet(&mut packet);

        let bad_last_index = 30u32;
        let fec_set_index = 0u32;
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&bad_last_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
        }

        let mut shred_filter_context = ShredFilterContext::new(root_bank.clone(), shred_version);
        assert!(shred_filter_context.should_discard_packet(&packet));
        assert_eq!(shred_filter_context.stats.misaligned_last_data_index, 1);
    }

    #[test]
    fn test_should_discard_shred_with_custom_shred_limits() {
        agave_logger::setup();

        let mut rng = rand::rng();
        let root_bank = new_test_bank(0, false);
        let slot = 42;
        let shreds = make_merkle_shreds_for_tests(&mut rng, slot, 10_000, false).unwrap();

        let shred = Shred::from(shreds[0].clone());
        let index = shred.common_header().index;
        let shred_version = shred.common_header().version;
        let mut packet = Packet::default();
        shred.copy_to_packet(&mut packet);

        let mut shred_filter_context = ShredFilterContext::new_with_custom_shred_limits(
            root_bank.clone(),
            shred_version,
            index + 1,
            index + 1,
        );
        assert!(!shred_filter_context.should_discard_packet(&packet));

        let mut shred_filter_context = ShredFilterContext::new_with_custom_shred_limits(
            root_bank,
            shred_version,
            index,
            index,
        );
        assert!(shred_filter_context.should_discard_packet(&packet));
        assert_eq!(shred_filter_context.stats.index_out_of_bounds, 1);
    }

    #[test]
    fn test_data_complete_shred_index_validation() {
        agave_logger::setup();
        let mut rng = rand::rng();
        let root_bank = new_test_bank(0, true);
        let slot = root_bank.get_slots_in_epoch(root_bank.epoch());
        let shreds = make_merkle_shreds_for_tests(
            &mut rng,
            slot,
            1200 * 5, // data_size
            false,    // is_last_in_slot
        )
        .unwrap();
        let shreds: Vec<_> = shreds.into_iter().map(Shred::from).collect();

        let data_shred = shreds
            .iter()
            .find(|s| s.shred_type() == ShredType::Data)
            .unwrap();

        let shred_version = data_shred.common_header().version;

        let mut packet = Packet::default();
        data_shred.copy_to_packet(&mut packet);

        let fec_set_index = 64u32;
        let wrong_index = fec_set_index + 10;

        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&wrong_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_FLAGS as u64))
                .unwrap();
            cursor
                .write_all(&[ShredFlags::DATA_COMPLETE_SHRED.bits()])
                .unwrap();
        }

        let mut shred_filter_context = ShredFilterContext::new(root_bank.clone(), shred_version);
        assert!(shred_filter_context.should_discard_packet(&packet));
        assert_eq!(shred_filter_context.stats.unexpected_data_complete_shred, 1);

        let correct_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32 - 1;
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&correct_index.to_le_bytes()).unwrap();
        }

        let mut shred_filter_context = ShredFilterContext::new(root_bank, shred_version);
        assert!(!shred_filter_context.should_discard_packet(&packet));
        assert_eq!(shred_filter_context.stats.unexpected_data_complete_shred, 0);
    }
}
