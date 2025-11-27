use {
    crate::shred::{Shred, ShredType},
    solana_clock::Slot,
    std::{
        ops::AddAssign,
        time::{Duration, Instant},
    },
};

#[derive(Default, Clone, Copy)]
pub struct ProcessShredsStats {
    // Per-slot elapsed time
    pub shredding_elapsed: u64,
    pub receive_elapsed: u64,
    pub serialize_elapsed: u64,
    pub gen_data_elapsed: u64,
    pub gen_coding_elapsed: u64,
    pub sign_coding_elapsed: u64,
    pub coding_send_elapsed: u64,
    pub get_leader_schedule_elapsed: u64,
    pub coalesce_elapsed: u64,
    // The number of times entry coalescing exited because the maximum number of
    // bytes was hit.
    pub coalesce_exited_hit_max: u64,
    // The number of times entry coalescing exited because we were tightly
    // aligned to an erasure batch boundary.
    pub coalesce_exited_tightly_packed: u64,
    // The number of times entry coalescing exited because the slot ended.
    pub coalesce_exited_slot_ended: u64,
    // The number of times entry coalescing exited because the maximum coalesce
    // duration was reached.
    pub coalesce_exited_rcv_timeout: u64,
    // Histogram count of num_data_shreds obtained from serializing entries
    // counted in 5 buckets.
    num_data_shreds_hist: [usize; 5],
    // If the blockstore already has shreds for the broadcast slot.
    pub num_extant_slots: u64,
    // When looking up chained merkle root from parent slot fails.
    pub err_unknown_chained_merkle_root: u64,
    pub(crate) padding_bytes: usize,
    pub(crate) data_bytes: usize,
    num_merkle_data_shreds: usize,
    num_merkle_coding_shreds: usize,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ShredFetchStats {
    pub(super) index_overrun: usize,
    pub shred_count: usize,
    pub(super) num_shreds_merkle_code_chained: usize,
    pub(super) num_shreds_merkle_data_chained: usize,
    pub ping_count: usize,
    pub ping_err_verify_count: usize,
    pub(super) index_bad_deserialize: usize,
    pub(super) index_out_of_bounds: usize,
    pub(super) slot_bad_deserialize: usize,
    pub(super) slot_out_of_range: usize,
    pub(super) bad_shred_type: usize,
    pub(super) shred_version_mismatch: usize,
    pub(super) bad_parent_offset: usize,
    pub(super) fec_set_index_bad_deserialize: usize,
    pub(super) misaligned_fec_set: usize,
    pub(super) erasure_config_bad_deserialize: usize,
    pub(super) misaligned_erasure_config: usize,
    pub(super) shred_flags_bad_deserialize: usize,
    pub(super) misaligned_last_data_index: usize,
    pub(super) unexpected_data_complete_shred: usize,
    since: Option<Instant>,
    pub overflow_shreds: usize,
}

impl ProcessShredsStats {
    pub fn submit(
        &mut self,
        name: &'static str,
        slot: Slot,
        slot_broadcast_time: Option<Duration>,
    ) {
        let slot_broadcast_time = slot_broadcast_time
            .map(|t| t.as_micros() as i64)
            .unwrap_or(-1);
        self.num_data_shreds_hist.iter_mut().fold(0, |acc, num| {
            *num += acc;
            *num
        });
        datapoint_info!(
            name,
            ("slot", slot, i64),
            ("shredding_time", self.shredding_elapsed, i64),
            ("receive_time", self.receive_elapsed, i64),
            ("num_data_shreds", self.num_merkle_data_shreds, i64),
            ("num_coding_shreds", self.num_merkle_coding_shreds, i64),
            ("slot_broadcast_time", slot_broadcast_time, i64),
            (
                "get_leader_schedule_time",
                self.get_leader_schedule_elapsed,
                i64
            ),
            ("serialize_shreds_time", self.serialize_elapsed, i64),
            ("gen_data_time", self.gen_data_elapsed, i64),
            ("gen_coding_time", self.gen_coding_elapsed, i64),
            ("sign_coding_time", self.sign_coding_elapsed, i64),
            ("coding_send_time", self.coding_send_elapsed, i64),
            ("num_extant_slots", self.num_extant_slots, i64),
            (
                "err_unknown_chained_merkle_root",
                self.err_unknown_chained_merkle_root,
                i64
            ),
            ("padding_bytes", self.padding_bytes, i64),
            ("data_bytes", self.data_bytes, i64),
            ("num_data_shreds_07", self.num_data_shreds_hist[0], i64),
            ("num_data_shreds_15", self.num_data_shreds_hist[1], i64),
            ("num_data_shreds_31", self.num_data_shreds_hist[2], i64),
            ("num_data_shreds_63", self.num_data_shreds_hist[3], i64),
            ("num_data_shreds_64", self.num_data_shreds_hist[4], i64),
            ("coalesce_elapsed", self.coalesce_elapsed, i64),
            ("coalesce_exited_hit_max", self.coalesce_exited_hit_max, i64),
            (
                "coalesce_exited_tightly_packed",
                self.coalesce_exited_tightly_packed,
                i64
            ),
            (
                "coalesce_exited_slot_ended",
                self.coalesce_exited_slot_ended,
                i64
            ),
            (
                "coalesce_exited_rcv_timeout",
                self.coalesce_exited_rcv_timeout,
                i64
            ),
        );
        *self = Self::default();
    }

    pub(crate) fn record_num_data_shreds(&mut self, num_data_shreds: usize) {
        let index = usize::BITS - num_data_shreds.leading_zeros();
        let index = index.saturating_sub(3) as usize;
        let index = index.min(self.num_data_shreds_hist.len() - 1);
        self.num_data_shreds_hist[index] += 1;
    }

    #[inline]
    pub fn record_shred(&mut self, shred: &Shred) {
        let num_shreds = match shred.shred_type() {
            ShredType::Code => &mut self.num_merkle_coding_shreds,
            ShredType::Data => &mut self.num_merkle_data_shreds,
        };
        *num_shreds += 1;
    }
}

impl ShredFetchStats {
    pub fn maybe_submit(&mut self, name: &'static str, cadence: Duration) -> bool {
        let elapsed = self.since.as_ref().map(Instant::elapsed);
        if elapsed.unwrap_or(Duration::MAX) < cadence {
            return false;
        }
        datapoint_info!(
            name,
            ("index_overrun", self.index_overrun, i64),
            ("shred_count", self.shred_count, i64),
            (
                "num_shreds_merkle_code_chained",
                self.num_shreds_merkle_code_chained,
                i64
            ),
            (
                "num_shreds_merkle_data_chained",
                self.num_shreds_merkle_data_chained,
                i64
            ),
            ("ping_count", self.ping_count, i64),
            ("ping_err_verify_count", self.ping_err_verify_count, i64),
            ("slot_bad_deserialize", self.slot_bad_deserialize, i64),
            ("index_bad_deserialize", self.index_bad_deserialize, i64),
            ("index_out_of_bounds", self.index_out_of_bounds, i64),
            ("slot_out_of_range", self.slot_out_of_range, i64),
            ("bad_shred_type", self.bad_shred_type, i64),
            ("shred_version_mismatch", self.shred_version_mismatch, i64),
            ("bad_parent_offset", self.bad_parent_offset, i64),
            (
                "fec_set_index_bad_deserialize",
                self.fec_set_index_bad_deserialize,
                i64
            ),
            ("misaligned_fec_set_size", self.misaligned_fec_set, i64),
            (
                "erasure_config_bad_deserialize",
                self.erasure_config_bad_deserialize,
                i64
            ),
            (
                "misaligned_erasure_config",
                self.misaligned_erasure_config,
                i64
            ),
            (
                "shred_flags_bad_deserialize",
                self.shred_flags_bad_deserialize,
                i64
            ),
            (
                "misaligned_last_data_index",
                self.misaligned_last_data_index,
                i64
            ),
            ("overflow_shreds", self.overflow_shreds, i64),
            (
                "unexpected_data_complete_shred",
                self.unexpected_data_complete_shred,
                i64
            )
        );
        *self = Self {
            since: Some(Instant::now()),
            ..Self::default()
        };

        true
    }
}

impl AddAssign<ProcessShredsStats> for ProcessShredsStats {
    fn add_assign(&mut self, rhs: Self) {
        let Self {
            shredding_elapsed,
            receive_elapsed,
            serialize_elapsed,
            gen_data_elapsed,
            gen_coding_elapsed,
            sign_coding_elapsed,
            coding_send_elapsed,
            get_leader_schedule_elapsed,
            coalesce_elapsed,
            coalesce_exited_hit_max,
            coalesce_exited_tightly_packed,
            coalesce_exited_slot_ended,
            coalesce_exited_rcv_timeout,
            num_data_shreds_hist,
            num_extant_slots,
            err_unknown_chained_merkle_root,
            padding_bytes,
            data_bytes,
            num_merkle_data_shreds,
            num_merkle_coding_shreds,
        } = rhs;
        self.shredding_elapsed += shredding_elapsed;
        self.receive_elapsed += receive_elapsed;
        self.serialize_elapsed += serialize_elapsed;
        self.gen_data_elapsed += gen_data_elapsed;
        self.gen_coding_elapsed += gen_coding_elapsed;
        self.sign_coding_elapsed += sign_coding_elapsed;
        self.coding_send_elapsed += coding_send_elapsed;
        self.get_leader_schedule_elapsed += get_leader_schedule_elapsed;
        self.coalesce_elapsed += coalesce_elapsed;
        self.coalesce_exited_hit_max += coalesce_exited_hit_max;
        self.coalesce_exited_tightly_packed += coalesce_exited_tightly_packed;
        self.coalesce_exited_slot_ended += coalesce_exited_slot_ended;
        self.coalesce_exited_rcv_timeout += coalesce_exited_rcv_timeout;
        self.num_extant_slots += num_extant_slots;
        self.err_unknown_chained_merkle_root += err_unknown_chained_merkle_root;
        self.padding_bytes += padding_bytes;
        self.data_bytes += data_bytes;
        self.num_merkle_data_shreds += num_merkle_data_shreds;
        self.num_merkle_coding_shreds += num_merkle_coding_shreds;
        for (i, bucket) in self.num_data_shreds_hist.iter_mut().enumerate() {
            *bucket += num_data_shreds_hist[i];
        }
    }
}
