use {
    super::{AdmissionPolicy, RecoverError, Shred, ShredFetchStats, view},
    agave_shred::{
        headers::AnyHeader,
        kind::{Code, Data},
        policy, recover,
        shred_variant::ShredKind,
    },
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_perf::packet::PacketRef,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
        time::{Duration, Instant},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum TurbineModeKind {
    #[default]
    Enabled = 0,
    TurbineDisabled = 1,
    TurbineAndRepairDisabled = 2,
}

impl TurbineModeKind {
    fn should_discard_packet(self, is_repair: bool) -> bool {
        match self {
            Self::Enabled => false,
            Self::TurbineDisabled => !is_repair,
            Self::TurbineAndRepairDisabled => true,
        }
    }
}

impl From<u8> for TurbineModeKind {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::TurbineDisabled,
            2 => Self::TurbineAndRepairDisabled,
            _ => Self::Enabled,
        }
    }
}

impl From<TurbineModeKind> for u8 {
    fn from(mode: TurbineModeKind) -> Self {
        mode as u8
    }
}

#[derive(Clone, Debug)]
pub struct TurbineMode(Arc<AtomicU8>);

impl TurbineMode {
    pub fn new(kind: TurbineModeKind) -> Self {
        Self(Arc::new(AtomicU8::new(kind as u8)))
    }

    pub fn get(&self) -> TurbineModeKind {
        TurbineModeKind::from(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, kind: TurbineModeKind) {
        self.0.store(kind as u8, Ordering::Relaxed);
    }
}

impl Default for TurbineMode {
    fn default() -> Self {
        Self::new(TurbineModeKind::default())
    }
}

pub struct ShredFilterContext {
    last_updated: Instant,
    root: Slot,
    max_slot: Slot,
    shred_version: u16,
    turbine_mode: Option<TurbineMode>,
    cached_turbine_mode: TurbineModeKind,
    root_bank: Arc<Bank>,
    #[cfg(test)]
    shred_limits_override: Option<(u32, u32)>,
    pub stats: ShredFetchStats,
}

impl ShredFilterContext {
    pub fn new(root_bank: Arc<Bank>, shred_version: u16) -> Self {
        Self::new_with_turbine_mode(root_bank, shred_version, None)
    }

    pub fn new_with_turbine_mode(
        root_bank: Arc<Bank>,
        shred_version: u16,
        turbine_mode: Option<TurbineMode>,
    ) -> Self {
        let root = root_bank.slot();
        let max_slot = max_shred_slot(root, root_bank.get_slots_in_epoch(root_bank.epoch()));
        let cached_turbine_mode = turbine_mode
            .as_ref()
            .map(TurbineMode::get)
            .unwrap_or_default();

        Self {
            last_updated: Instant::now(),
            root,
            max_slot,
            shred_version,
            turbine_mode,
            cached_turbine_mode,
            root_bank,
            #[cfg(test)]
            shred_limits_override: None,
            stats: ShredFetchStats::default(),
        }
    }

    pub fn maybe_update(&mut self, root_bank: Arc<Bank>) {
        if let Some(turbine_mode) = self.turbine_mode.as_ref() {
            self.cached_turbine_mode = turbine_mode.get();
        }

        if self.last_updated.elapsed().as_nanos() > root_bank.ns_per_slot {
            self.last_updated = Instant::now();
            self.root = root_bank.slot();
            self.max_slot =
                max_shred_slot(self.root, root_bank.get_slots_in_epoch(root_bank.epoch()));
            debug_assert!(self.root < self.max_slot);
            self.root_bank = root_bank;
        }
    }

    pub fn root(&self) -> Slot {
        self.root
    }

    pub fn shred_version(&self) -> u16 {
        self.shred_version
    }

    pub fn policy(&self, slot: Slot) -> AdmissionPolicy {
        #[cfg(test)]
        if let Some((max_data_shreds_per_slot, max_code_shreds_per_slot)) =
            self.shred_limits_override
        {
            return AdmissionPolicy {
                shred_version: self.shred_version,
                root: self.root,
                max_slot: self.max_slot,
                max_data_shreds_per_slot,
                max_code_shreds_per_slot,
            };
        }
        AdmissionPolicy {
            shred_version: self.shred_version,
            root: self.root,
            max_slot: self.max_slot,
            max_data_shreds_per_slot: self.root_bank.max_data_shreds_per_slot_for_slot(slot),
            max_code_shreds_per_slot: self.root_bank.max_code_shreds_per_slot_for_slot(slot),
        }
    }

    #[cfg(test)]
    fn set_shred_limits_for_tests(&mut self, max_data: u32, max_code: u32) {
        self.shred_limits_override = Some((max_data, max_code));
    }

    pub fn maybe_submit_stats(&mut self, metric_name: &'static str, cadence: Duration) -> bool {
        self.stats.maybe_submit(metric_name, cadence)
    }

    #[must_use]
    pub fn should_discard_packet<'a, P>(&mut self, packet: P) -> bool
    where
        P: Into<PacketRef<'a>>,
    {
        let packet = packet.into();
        let is_repair = packet.meta().repair();
        if self.cached_turbine_mode.should_discard_packet(is_repair) {
            return true;
        }
        let Some(bytes) = packet.data(..) else {
            self.stats.index_overrun += 1;
            return true;
        };
        self.should_discard_bytes(bytes, is_repair)
    }

    #[must_use]
    pub fn should_discard_shred(&mut self, shred: &[u8]) -> bool {
        self.should_discard_bytes(shred, false)
    }

    fn should_discard_bytes(&mut self, bytes: &[u8], is_repair: bool) -> bool {
        let Some((common, header)) = read_headers(bytes, is_repair) else {
            self.stats.index_overrun += 1;
            return true;
        };
        let policy = self.policy(common.slot);
        match policy::admit(&common, &header, &policy) {
            Ok(()) => {
                self.stats.record_accept(common.variant.shred_kind());
                false
            }
            Err(reason) => {
                self.stats.record_reject(&reason);
                true
            }
        }
    }
}

fn read_headers(
    bytes: &[u8],
    is_repair: bool,
) -> Option<(agave_shred::headers::CommonHeader, AnyHeader)> {
    let kind = view::peek_variant(bytes).ok()?.shred_kind();
    match (kind, is_repair) {
        (ShredKind::Data, false) => {
            let view = view::ShredView::<Data>::read_exact(bytes).ok()?;
            Some((view.common, view.header.into()))
        }
        (ShredKind::Data, true) => {
            let (view, _) = view::ShredView::<Data>::read_repair_packet(bytes).ok()?;
            Some((view.common, view.header.into()))
        }
        (ShredKind::Code, false) => {
            let view = view::ShredView::<Code>::read_exact(bytes).ok()?;
            Some((view.common, view.header.into()))
        }
        (ShredKind::Code, true) => {
            let (view, _) = view::ShredView::<Code>::read_repair_packet(bytes).ok()?;
            Some((view.common, view.header.into()))
        }
    }
}

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
    shred_slot >= epoch_schedule.get_first_slot_in_epoch(feature_epoch.saturating_add(1))
}

fn max_shred_slot(root: Slot, slots_per_epoch: Slot) -> Slot {
    const MAX_SHRED_DISTANCE_MINIMUM: Slot = 500;
    root.saturating_add(MAX_SHRED_DISTANCE_MINIMUM.max(slots_per_epoch / 2))
}

pub struct ShredRecoveryContext {
    retransmit_sender: EvictingSender<Vec<Shred>>,
    shred_filter_ctx: ShredFilterContext,
}

impl ShredRecoveryContext {
    pub fn new(
        retransmit_sender: EvictingSender<Vec<Shred>>,
        root_bank: Arc<Bank>,
        shred_version: u16,
    ) -> Self {
        let shred_filter_ctx = ShredFilterContext::new(root_bank, shred_version);
        Self {
            retransmit_sender,
            shred_filter_ctx,
        }
    }

    pub fn maybe_update(&mut self, root_bank: Arc<Bank>) {
        self.shred_filter_ctx.maybe_update(root_bank);
    }

    pub fn maybe_submit_stats(&mut self) {
        self.shred_filter_ctx
            .maybe_submit_stats("shred-recovery", Duration::from_secs(2));
    }

    pub fn recover<T: IntoIterator<Item = Shred>>(
        &mut self,
        shreds: T,
        recovered_shreds: &mut Vec<Shred>,
        recovered_data_shreds: &mut Vec<Shred>,
    ) -> Result<(), RecoverError> {
        let mut data = Vec::new();
        let mut code = Vec::new();
        for shred in shreds {
            if self.should_discard_shred(&shred) {
                continue;
            }
            match shred.into_data() {
                Ok(shred) => data.push(shred),
                Err(shred) => {
                    if let Ok(shred) = shred.into_code() {
                        code.push(shred);
                    }
                }
            }
        }
        let recovery = recover::recover(&data, &code)?;
        for shred in recovery.code.into_iter().map(Shred::from) {
            if !self.should_discard_shred(&shred) {
                recovered_shreds.push(shred);
            }
        }
        for shred in recovery.data.into_iter().map(Shred::from) {
            if !self.should_discard_shred(&shred) {
                recovered_shreds.push(shred.clone());
                recovered_data_shreds.push(shred);
            }
        }
        Ok(())
    }

    pub fn try_retransmit_shreds(&self, recovered_shreds: Vec<Shred>) {
        if !recovered_shreds.is_empty() {
            let _ = self.retransmit_sender.try_send(recovered_shreds);
        }
    }

    pub fn should_discard_shred(&mut self, shred: &Shred) -> bool {
        let policy = self.shred_filter_ctx.policy(shred.slot());
        match policy::admit(shred.common(), shred.header(), &policy) {
            Ok(()) => false,
            Err(reason) => {
                self.shred_filter_ctx.stats.record_reject(&reason);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::create_genesis_config,
            shred::{
                DATA_SHREDS_PER_FEC_BLOCK, MAX_CODE_SHREDS_PER_SLOT, MAX_DATA_SHREDS_PER_SLOT,
                ProcessShredsStats, ShredFlags, ShredType, Shredder,
            },
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_perf::packet::{Packet, PacketFlags},
        solana_runtime::{bank::Bank, slot_params::slot_time_feature_gates},
        std::{
            io::{Cursor, Seek, SeekFrom, Write},
            sync::Arc,
            time::Duration,
        },
        test_case::test_case,
    };

    const OFFSET_OF_SHRED_INDEX: usize = 64 + 1 + 8;
    const OFFSET_OF_FEC_SET_INDEX: usize = OFFSET_OF_SHRED_INDEX + 4 + 2;
    const OFFSET_OF_PARENT_OFFSET: usize = 83;
    const OFFSET_OF_SHRED_FLAGS: usize = 85;
    const OFFSET_OF_NUM_DATA: usize = 83;

    fn make_shreds(slot: Slot, is_last_in_slot: bool) -> Vec<Shred> {
        let keypair = Keypair::new();
        let shredder = Shredder::new(slot, slot - 1, 0, 42).unwrap();
        let data: Vec<u8> = (0..1200 * 5).map(|i| i as u8).collect();
        shredder
            .make_shreds_from_data_slice(
                &keypair,
                &data,
                is_last_in_slot,
                Hash::default(),
                64,
                &mut ProcessShredsStats::default(),
            )
            .unwrap()
    }

    fn copy_to_packet(shred: &Shred, packet: &mut Packet) {
        let bytes = shred.bytes();
        packet.buffer_mut()[..bytes.len()].copy_from_slice(bytes);
        packet.meta_mut().size = bytes.len();
    }

    fn new_test_bank(slot: Slot) -> Arc<Bank> {
        let genesis_config = create_genesis_config(1).genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);
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
        let slot = 200;
        let shreds = make_shreds(slot, is_last_in_slot);
        assert_eq!(shreds.iter().map(Shred::fec_set_index).dedup().count(), 1);

        assert_matches!(shreds[0].kind(), ShredType::Data);
        let parent_slot = shreds[0].parent_slot().unwrap();
        let shred_version = shreds[0].version();

        let root_bank = new_test_bank(0);
        let parent_exceeded_root_bank = new_test_bank(parent_slot + 1);
        let slot_root_bank = new_test_bank(slot);
        let mut packet = Packet::default();

        {
            let shred = shreds.first().unwrap();
            copy_to_packet(shred, &mut packet);
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(!ctx.should_discard_packet(&packet));
        }
        {
            let mut packet = packet.clone();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            packet.meta_mut().size = OFFSET_OF_SHRED_INDEX;
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.index_overrun, 1);
        }
        {
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version.wrapping_add(1));
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.shred_version_mismatch, 1);
        }
        {
            let mut ctx = ShredFilterContext::new(parent_exceeded_root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.bad_parent_offset, 1);
        }
        {
            let parent_offset = 0u16;
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_PARENT_OFFSET as u64))
                .unwrap();
            cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.bad_parent_offset, 1);
        }
        {
            let parent_offset = u16::try_from(slot + 1).unwrap();
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_PARENT_OFFSET as u64))
                .unwrap();
            cursor.write_all(&parent_offset.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.bad_parent_offset, 1);
        }
        {
            let index = u32::MAX - 10;
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&index.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.index_out_of_bounds, 1);
        }
        {
            let shred = shreds.last().unwrap();
            assert_eq!(shred.kind(), ShredType::Code);
            copy_to_packet(shred, &mut packet);
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(!ctx.should_discard_packet(&packet));
        }
        {
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version.wrapping_add(1));
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.shred_version_mismatch, 1);
        }
        {
            let mut ctx = ShredFilterContext::new(slot_root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.slot_out_of_range, 1);
        }
        {
            let index = u32::try_from(MAX_CODE_SHREDS_PER_SLOT).unwrap();
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&index.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.index_out_of_bounds, 1);
        }
    }

    #[test]
    fn test_recovery_shred_limits() {
        agave_logger::setup();
        let slot = 200;
        let coding_shreds: Vec<_> = make_shreds(slot, false)
            .into_iter()
            .filter(|shred| shred.kind() == ShredType::Code)
            .collect();
        let shred_version = coding_shreds[0].version();
        let max_code_shreds_per_slot = coding_shreds[0].index();
        let (dummy_retransmit_sender, _) = EvictingSender::new_bounded(0);
        let mut ctx =
            ShredRecoveryContext::new(dummy_retransmit_sender, new_test_bank(0), shred_version);
        ctx.shred_filter_ctx
            .set_shred_limits_for_tests(MAX_DATA_SHREDS_PER_SLOT as u32, max_code_shreds_per_slot);
        let mut recovered_shreds = Vec::new();
        let mut recovered_data_shreds = Vec::new();
        assert_matches!(
            ctx.recover(
                coding_shreds,
                &mut recovered_shreds,
                &mut recovered_data_shreds,
            ),
            Err(RecoverError::NoShreds)
        );
        assert!(recovered_shreds.is_empty());
        assert!(recovered_data_shreds.is_empty());
    }

    #[test]
    fn test_should_discard_packet_with_turbine_mode() {
        agave_logger::setup();
        let root_bank = new_test_bank(0);
        let shred = make_shreds(42, false)[0].clone();
        let shred_version = shred.version();
        let turbine_mode = TurbineMode::new(TurbineModeKind::TurbineDisabled);
        let mut packet = Packet::default();
        copy_to_packet(&shred, &mut packet);

        let mut ctx = ShredFilterContext::new_with_turbine_mode(
            root_bank.clone(),
            shred_version,
            Some(turbine_mode.clone()),
        );
        assert!(ctx.should_discard_packet(&packet));

        packet.meta_mut().flags.insert(PacketFlags::REPAIR);
        packet.buffer_mut()[shred.bytes().len()..][..4].copy_from_slice(&7u32.to_le_bytes());
        packet.meta_mut().size = shred.bytes().len() + 4;
        assert!(!ctx.should_discard_packet(&packet));

        turbine_mode.set(TurbineModeKind::TurbineAndRepairDisabled);
        ctx.last_updated = Instant::now() - Duration::from_secs(1);
        ctx.maybe_update(root_bank.clone());
        assert!(ctx.should_discard_packet(&packet));

        packet.meta_mut().flags.remove(PacketFlags::REPAIR);
        packet.meta_mut().size = shred.bytes().len();
        turbine_mode.set(TurbineModeKind::Enabled);
        ctx.last_updated = Instant::now() - Duration::from_secs(1);
        ctx.maybe_update(root_bank.clone());
        assert!(!ctx.should_discard_packet(&packet));
    }

    #[test]
    fn test_should_discard_shred_fec_set_checks() {
        agave_logger::setup();
        let slot = 200;
        let shreds = make_shreds(slot, false);
        let shred_version = shreds[0].version();
        let root_bank = new_test_bank(0);

        {
            let mut packet = Packet::default();
            copy_to_packet(&shreds[0], &mut packet);
            let bad_fec_set_index = 5u32;
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&bad_fec_set_index.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.misaligned_fec_set, 1);
        }
        {
            let mut packet = Packet::default();
            copy_to_packet(&shreds[0], &mut packet);
            let fec_set_index = 64u32;
            let bad_index = 100u32;
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&bad_index.to_le_bytes()).unwrap();
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_FEC_SET_INDEX as u64))
                .unwrap();
            cursor.write_all(&fec_set_index.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.misaligned_fec_set, 1);
        }
        {
            let code_shred = shreds.iter().find(|s| s.kind() == ShredType::Code).unwrap();
            let mut packet = Packet::default();
            copy_to_packet(code_shred, &mut packet);
            let bad_num_data = 16u16;
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_NUM_DATA as u64))
                .unwrap();
            cursor.write_all(&bad_num_data.to_le_bytes()).unwrap();
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            assert!(ctx.should_discard_packet(&packet));
            assert_eq!(ctx.stats.misaligned_erasure_config, 1);
        }

        let shreds = make_shreds(slot, true);
        let shred_version = shreds[0].version();
        let last_data_shred = shreds
            .iter()
            .filter(|s| s.kind() == ShredType::Data)
            .last()
            .unwrap();
        assert!(last_data_shred.last_in_slot());
        let mut packet = Packet::default();
        copy_to_packet(last_data_shred, &mut packet);

        let fec_set_index = 1u32;
        let bad_last_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32 - 1;
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
        let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
        assert!(ctx.should_discard_packet(&packet));
        assert_eq!(ctx.stats.misaligned_last_data_index, 1);
    }

    #[test]
    fn test_should_discard_shred_with_custom_shred_limits() {
        agave_logger::setup();
        let shreds = make_shreds(200, false);
        let shred_version = shreds[0].version();
        let root_bank = new_test_bank(0);

        for shred_type in [ShredType::Data, ShredType::Code] {
            let shred = shreds.iter().find(|s| s.kind() == shred_type).unwrap();
            let index = shred.index();
            let (max_data, max_code) = (
                MAX_DATA_SHREDS_PER_SLOT as u32,
                MAX_CODE_SHREDS_PER_SLOT as u32,
            );
            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            match shred_type {
                ShredType::Data => ctx.set_shred_limits_for_tests(index + 1, max_code),
                ShredType::Code => ctx.set_shred_limits_for_tests(max_data, index + 1),
            }
            assert!(!ctx.should_discard_shred(shred.bytes()));

            let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
            match shred_type {
                ShredType::Data => ctx.set_shred_limits_for_tests(index, max_code),
                ShredType::Code => ctx.set_shred_limits_for_tests(max_data, index),
            }
            assert!(ctx.should_discard_shred(shred.bytes()));
            assert_eq!(ctx.stats.index_out_of_bounds, 1);
        }
    }

    #[test]
    fn test_shred_limit_for_slot_times() {
        for (feature_id, params) in slot_time_feature_gates() {
            let genesis_config = create_genesis_config(1).genesis_config;
            let mut root_bank = Bank::new_for_tests(&genesis_config);
            for (id, _) in slot_time_feature_gates() {
                root_bank.deactivate_feature(&id);
            }
            root_bank.activate_feature(&feature_id);
            let (root_bank, _) = root_bank.wrap_with_bank_forks_for_tests();
            let effective_slot = root_bank
                .epoch_schedule()
                .get_first_slot_in_epoch(root_bank.epoch() + 1);
            let ctx = ShredFilterContext::new(root_bank, 0);
            let before = ctx.policy(effective_slot.saturating_sub(1));
            assert_eq!(
                before.max_data_shreds_per_slot,
                MAX_DATA_SHREDS_PER_SLOT as u32
            );
            let after = ctx.policy(effective_slot);
            assert_eq!(
                after.max_data_shreds_per_slot,
                params.max_data_shreds_per_slot()
            );
            assert_eq!(
                after.max_code_shreds_per_slot,
                params.max_code_shreds_per_slot()
            );
        }
    }

    #[test]
    fn test_data_complete_shred_index_validation() {
        agave_logger::setup();
        let root_bank = new_test_bank(0);
        let slot = root_bank.get_slots_in_epoch(root_bank.epoch());
        let shreds = make_shreds(slot, false);
        let data_shred = shreds.iter().find(|s| s.kind() == ShredType::Data).unwrap();
        let shred_version = data_shred.version();

        let mut packet = Packet::default();
        copy_to_packet(data_shred, &mut packet);

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
                .write_all(&[ShredFlags::DATA_COMPLETE_SHRED])
                .unwrap();
        }
        let mut ctx = ShredFilterContext::new(root_bank.clone(), shred_version);
        assert!(ctx.should_discard_packet(&packet));
        assert_eq!(ctx.stats.unexpected_data_complete_shred, 1);

        let correct_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32 - 1;
        {
            let mut cursor = Cursor::new(packet.buffer_mut());
            cursor
                .seek(SeekFrom::Start(OFFSET_OF_SHRED_INDEX as u64))
                .unwrap();
            cursor.write_all(&correct_index.to_le_bytes()).unwrap();
        }
        let mut ctx = ShredFilterContext::new(root_bank, shred_version);
        assert!(!ctx.should_discard_packet(&packet));
        assert_eq!(ctx.stats.unexpected_data_complete_shred, 0);
    }

    #[test_case(EpochSchedule::custom(100, 100, false), None, 100 => false ; "inactive feature")]
    #[test_case(EpochSchedule::custom(100, 100, false), Some(0), 99 => false ; "genesis activation, end of epoch 0")]
    #[test_case(EpochSchedule::custom(100, 100, false), Some(0), 100 => true ; "genesis activation, start of epoch 1")]
    #[test_case(EpochSchedule::custom(100, 100, false), Some(100), 199 => false ; "boundary activation, end of activation epoch")]
    #[test_case(EpochSchedule::custom(100, 100, false), Some(100), 200 => true ; "boundary activation, start of next epoch")]
    #[test_case(EpochSchedule::custom(100, 100, true), Some(32), 95 => false ; "warmup, end of epoch 1")]
    #[test_case(EpochSchedule::custom(100, 100, true), Some(32), 96 => true ; "warmup, start of first full epoch")]
    fn test_feature_activation_slot_formula(
        epoch_schedule: EpochSchedule,
        feature_slot: Option<Slot>,
        shred_slot: Slot,
    ) -> bool {
        check_feature_activation(feature_slot, shred_slot, &epoch_schedule)
    }
}
