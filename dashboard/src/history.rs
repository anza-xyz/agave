//! A flat history of what each recent slot contained.
//!
//! The slot ring in [`crate::slots`] holds whole [`SlotEntry`] records, the
//! right shape for the few hundred slots a client is sent and the wrong one
//! for a hundred thousand. This holds the same span as fixed-size rows carrying
//! only the columns the schedule page draws.

use {crate::slots::SlotEntry, serde::Serialize, solana_clock::Slot};

/// Slots kept in the packed history: a hundred thousand, about eleven hours,
/// for about eight megabytes. Allocated by the service because the server
/// answers range queries out of it before the collector exists.
pub const PACKED_SLOTS: usize = 100_000;

/// One slot, packed to the columns a schedule row draws. The leader is not
/// among them, it comes from the epoch's turn array; nor is the duration,
/// which is the gap to the previous slot with a clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackedSlot {
    /// [`crate::slots::SlotLevel`] as its discriminant.
    pub level: u8,
    /// Bit 0: a block was recorded. Bit 1: the slot's clock is known. Both needed
    /// because nought is a real reading for every count here.
    pub flags: u8,
    pub votes: u32,
    pub non_votes: u32,
    /// Compute units the block used, saturating into `u32`, seventy times the
    /// current block limit.
    pub compute: u32,
    /// Base and priority fees together, in lamports; base is `fees -
    /// priority_fees`.
    pub fees: u64,
    /// The priority half of `fees`, so the split survives into history.
    pub priority_fees: u64,
    /// Lamports paid into the jito tip accounts during this slot, as measured.
    /// What reached a distribution account and what it earned us are worked out
    /// where drawn, from rates a correction can still reach. Nought unless
    /// `HAS_TIPS` is set.
    pub tips: u64,
    /// Wall time replay's own thread spent on the slot, in microseconds and
    /// saturating into `u32`, which is over an hour. Nought unless `HAS_REPLAY`.
    pub replay_micros: u32,
    /// Wall clock of the slot's first shred, in milliseconds. Absolute rather than
    /// an offset from the window, which would have to be rebased as the window
    /// moved.
    pub time_millis: u64,
    /// Data shreds in the block, and how many had to be repaired. Nought unless
    /// `HAS_SHREDS`.
    pub shreds: u32,
    pub repaired: u32,
    /// Milliseconds from the slot's first shred to its last, and from its first
    /// shred to replay finishing. Saturating into `u32`, which is over a month.
    /// Nought unless `HAS_SHREDS` and `HAS_REPLAYED` respectively.
    pub full_millis: u32,
    pub replayed_millis: u32,
}

/// Most slots one range may carry. A row of mainnet-sized figures is about a
/// hundred and twenty bytes of JSON, so a full span is under half the frame
/// ceiling, which a test below holds it to. Twenty-five times a screenful.
pub const MAX_RANGE_SLOTS: usize = 4096;

/// One slot as it goes on the wire: a JSON array, because field names would
/// outweigh the figures. Order: level, flags, votes, non-votes, compute, fees,
/// priority fees, tips, time, replay, shreds, repaired, full, replayed. The
/// frontend mirrors it. A struct rather than a tuple, which std stops
/// deriving for at twelve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WireRow(
    pub u8,
    pub u8,
    pub u32,
    pub u32,
    pub u32,
    pub u64,
    pub u64,
    pub u64,
    pub u64,
    pub u32,
    pub u32,
    pub u32,
    pub u32,
    pub u32,
);

/// A span of the history, as it goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotRange {
    /// The slot `rows[0]` describes. Every row after it is one slot on, so the
    /// slot numbers themselves are never sent.
    pub first_slot: Slot,
    /// One entry per slot, `null` for a slot the history does not hold, whether
    /// too old, too new, or never seen.
    pub rows: Vec<Option<WireRow>>,
}

/// Set where the slot recorded a block, as against one that has not frozen or
/// was skipped.
pub const HAS_BLOCK: u8 = 1;
/// Set where the slot's first shred was timed.
pub const HAS_CLOCK: u8 = 1 << 1;
/// Set where the slot's tips were measured. Nought is a real reading: the
/// searchers passed that leader by.
pub const HAS_TIPS: u8 = 1 << 2;
/// Set where replay's time on the slot was seen. Clear for a bank this validator
/// built, which replay never timed.
pub const HAS_REPLAY: u8 = 1 << 3;
/// Set where the blockstore reported the slot filling.
pub const HAS_SHREDS: u8 = 1 << 4;
/// Set where replay's finish was seen, and so timed from the first shred.
pub const HAS_REPLAYED: u8 = 1 << 5;

/// A fixed-size history of packed slots, direct-mapped at `slot % capacity`.
/// The slot is stored beside its row so a row from a lap ago cannot answer for
/// a current one.
pub struct SlotHistory {
    rows: Vec<(Slot, PackedSlot)>,
}

impl SlotHistory {
    /// Allocates the whole history up front. It never grows and never shrinks,
    /// so this is the only allocation it makes.
    pub fn new(capacity: usize) -> Self {
        Self {
            rows: vec![(0, PackedSlot::default()); capacity.max(1)],
        }
    }

    /// The row for `slot`, or `None` where the history has never held it or has
    /// since lapped past it.
    pub fn get(&self, slot: Slot) -> Option<&PackedSlot> {
        let (held, row) = self.rows.get(self.index(slot))?;
        (*held == slot && slot != 0).then_some(row)
    }

    /// A span of slots, oldest first. `count` is clamped rather than refused; the
    /// rows are positional, so the caller can see how many it got.
    pub fn range(&self, first_slot: Slot, count: usize) -> SlotRange {
        let count = count.min(MAX_RANGE_SLOTS);
        let rows = (0..count as u64)
            .map(|offset| {
                self.get(first_slot.saturating_add(offset)).map(|row| {
                    WireRow(
                        row.level,
                        row.flags,
                        row.votes,
                        row.non_votes,
                        row.compute,
                        row.fees,
                        row.priority_fees,
                        row.tips,
                        row.time_millis,
                        row.replay_micros,
                        row.shreds,
                        row.repaired,
                        row.full_millis,
                        row.replayed_millis,
                    )
                })
            })
            .collect();
        SlotRange { first_slot, rows }
    }

    /// What one slot contained. Called on every change to an entry, since there is
    /// no single moment at which one is finished.
    pub fn record(&mut self, entry: &SlotEntry) {
        let row = self.row(entry.slot);
        row.level = entry.level as u8;
        if let Some(block) = &entry.block {
            row.flags |= HAS_BLOCK;
            // Votes are what is left of the block once the rest is taken out. Saturating
            // because a bank whose parent has gone reports neither counter.
            row.votes = clamp(
                block
                    .transactions
                    .saturating_sub(block.non_vote_transactions),
            );
            row.non_votes = clamp(block.non_vote_transactions);
            row.compute = clamp(block.block_cost);
            row.fees = block.total_fees;
            row.priority_fees = block.priority_fees;
            if let Some(tips) = block.tips {
                row.flags |= HAS_TIPS;
                row.tips = tips;
            }
            if let Some(micros) = block.replay_micros {
                row.flags |= HAS_REPLAY;
                row.replay_micros = clamp(micros);
            }
        }
        if let Some(shreds) = &entry.shreds {
            row.flags |= HAS_SHREDS;
            row.shreds = clamp(shreds.count);
            row.repaired = clamp(shreds.repaired);
            row.full_millis = clamp(shreds.full_millis);
        }
        if let Some(millis) = entry.replayed_millis {
            row.flags |= HAS_REPLAYED;
            row.replayed_millis = clamp(millis);
        }
    }

    /// When the slot's first shred arrived, which the collector reads from the
    /// blockstore to difference the slot durations and otherwise discards.
    pub fn record_time(&mut self, slot: Slot, millis: u64) {
        let row = self.row(slot);
        row.flags |= HAS_CLOCK;
        row.time_millis = millis;
    }

    /// The row for `slot`, cleared first if it belongs to an older slot. Both
    /// writers come through here.
    fn row(&mut self, slot: Slot) -> &mut PackedSlot {
        let index = self.index(slot);
        let held = &mut self.rows[index];
        if held.0 != slot {
            *held = (slot, PackedSlot::default());
        }
        &mut held.1
    }

    /// `slot % capacity` without a bare remainder: the workspace denies
    /// `arithmetic_side_effects`.
    fn index(&self, slot: Slot) -> usize {
        let capacity = self.rows.len() as u64;
        usize::try_from(slot.checked_rem(capacity).unwrap_or(0)).unwrap_or(0)
    }
}

/// Into `u32`, clamped: "at least this much" reads better than a wrapped small
/// number.
fn clamp(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::slots::{BlockDetail, ShredArrival, SlotLevel},
    };

    fn entry(slot: Slot) -> SlotEntry {
        SlotEntry {
            slot,
            level: SlotLevel::Rooted,
            mine: false,
            block: None,
            duration_nanos: None,
            time_millis: None,
            shreds: None,
            replayed_millis: None,
        }
    }

    fn with_block(slot: Slot, transactions: u64, non_vote: u64) -> SlotEntry {
        SlotEntry {
            block: Some(BlockDetail {
                transactions,
                non_vote_transactions: non_vote,
                failed_transactions: 0,
                entries: 0,
                block_cost: 41_827_311,
                block_cost_limit: 60_000_000,
                account_cost_limit: 12_000_000,
                total_fees: 104_600_000,
                priority_fees: 0,
                tips: None,
                replay_micros: None,
            }),
            ..entry(slot)
        }
    }

    #[test]
    fn test_a_slot_reads_back_what_was_recorded_for_it() {
        let mut history = SlotHistory::new(64);
        history.record(&with_block(900, 9_500, 8_752));
        history.record_time(900, 1_756_000_000_123);

        let row = history.get(900).expect("recorded");
        assert_eq!(row.votes, 748);
        assert_eq!(row.non_votes, 8_752);
        assert_eq!(row.compute, 41_827_311);
        assert_eq!(row.fees, 104_600_000);
        assert_eq!(row.time_millis, 1_756_000_000_123);
        assert_eq!(row.flags, HAS_BLOCK | HAS_CLOCK);
    }

    #[test]
    fn test_the_two_writers_do_not_clear_each_other() {
        // The block arrives when the bank freezes and the clock when the
        // blockstore is walked, on different ticks and in either order.
        for clock_first in [true, false] {
            let mut history = SlotHistory::new(64);
            if clock_first {
                history.record_time(900, 42);
                history.record(&with_block(900, 10, 4));
            } else {
                history.record(&with_block(900, 10, 4));
                history.record_time(900, 42);
            }
            let row = history.get(900).expect("recorded");
            assert_eq!(row.time_millis, 42, "clock_first={clock_first}");
            assert_eq!(row.non_votes, 4, "clock_first={clock_first}");
            assert_eq!(row.flags, HAS_BLOCK | HAS_CLOCK);
        }
    }

    #[test]
    fn test_a_slot_a_lap_ago_is_not_mistaken_for_this_one() {
        // The bug this shape invites: two slots in the same row.
        let mut history = SlotHistory::new(64);
        history.record(&with_block(10, 5_000, 4_000));
        history.record(&with_block(74, 9, 4));

        assert!(history.get(10).is_none());
        assert_eq!(history.get(74).expect("recorded").non_votes, 4);
    }

    #[test]
    fn test_a_lapped_row_keeps_nothing_of_the_slot_it_held() {
        // Cleared rather than overwritten field by field: the new slot may have
        // no block yet, and the old one's counts must not answer for it.
        let mut history = SlotHistory::new(64);
        history.record(&with_block(10, 5_000, 4_000));
        history.record_time(10, 99);
        history.record(&entry(74));

        let row = history.get(74).expect("recorded");
        assert_eq!(row.flags, 0);
        assert_eq!(row.non_votes, 0);
        assert_eq!(row.time_millis, 0);
    }

    #[test]
    fn test_a_range_is_positional_and_holds_a_gap_open() {
        // The slot numbers are never sent, so a missing slot takes its place in the
        // list.
        let mut history = SlotHistory::new(64);
        history.record(&with_block(10, 10, 4));
        history.record(&with_block(12, 20, 9));

        let range = history.range(10, 3);
        assert_eq!(range.first_slot, 10);
        assert_eq!(range.rows.len(), 3);
        assert!(range.rows[0].is_some());
        assert!(range.rows[1].is_none(), "slot 11 was never recorded");
        assert_eq!(range.rows[2].expect("slot 12").3, 9);
    }

    #[test]
    fn test_a_range_past_the_ceiling_is_clamped_rather_than_refused() {
        // A client that guesses slightly wrong gets what fits.
        let history = SlotHistory::new(64);
        let range = history.range(10, MAX_RANGE_SLOTS.saturating_add(1_000));
        assert_eq!(range.rows.len(), MAX_RANGE_SLOTS);
    }

    #[test]
    fn test_a_range_off_the_end_of_the_history_is_all_holes() {
        // Not an error. Scrolling past what has been retained is ordinary, and
        // the page draws rows with no figures for it.
        let history = SlotHistory::new(64);
        let range = history.range(900, 4);
        assert_eq!(range.rows.len(), 4);
        assert!(range.rows.iter().all(Option::is_none));
    }

    #[test]
    fn test_a_range_carries_the_columns_in_the_order_the_frontend_reads_them() {
        // The one place the wire order is pinned.
        let mut history = SlotHistory::new(64);
        history.record(&with_block(10, 9_500, 8_752));
        history.record_time(10, 1_756_000_000_123);

        let WireRow(
            level,
            flags,
            votes,
            non_votes,
            compute,
            fees,
            priority,
            tips,
            time,
            replay,
            shreds,
            repaired,
            full,
            replayed,
        ) = history.range(10, 1).rows[0].expect("recorded");
        assert_eq!(level, SlotLevel::Rooted as u8);
        assert_eq!(flags, HAS_BLOCK | HAS_CLOCK);
        assert_eq!(votes, 748);
        assert_eq!(non_votes, 8_752);
        assert_eq!(compute, 41_827_311);
        assert_eq!(fees, 104_600_000);
        assert_eq!(priority, 0);
        // Unmeasured, so the flag is clear and the column reads nothing. The
        // fixture leaves tips out precisely so this stays the default case.
        assert_eq!(tips, 0);
        assert_eq!(flags & HAS_TIPS, 0);
        assert_eq!(time, 1_756_000_000_123);
        assert_eq!(replay, 0);
        assert_eq!(flags & HAS_REPLAY, 0);
        assert_eq!((shreds, repaired, full, replayed), (0, 0, 0, 0));
        assert_eq!(flags & (HAS_SHREDS | HAS_REPLAYED), 0);
    }

    #[test]
    fn test_shred_arrival_and_replay_end_travel_with_their_flags() {
        // Both live on the entry rather than the block: a slot fills before it
        // freezes, and a dead one fills without freezing at all.
        let mut history = SlotHistory::new(64);
        let mut filled = entry(50);
        filled.shreds = Some(ShredArrival {
            count: 928,
            repaired: 63,
            full_millis: 921,
        });
        history.record(&filled);
        let mut replayed = with_block(51, 100, 10);
        replayed.shreds = Some(ShredArrival {
            count: 1_203,
            repaired: 0,
            full_millis: 341,
        });
        replayed.replayed_millis = Some(393);
        history.record(&replayed);

        let dead = history.get(50).expect("recorded");
        assert_eq!(dead.flags, HAS_SHREDS);
        assert_eq!(
            (dead.shreds, dead.repaired, dead.full_millis),
            (928, 63, 921)
        );
        assert_eq!(dead.replayed_millis, 0);

        let live = history.get(51).expect("recorded");
        assert_eq!(live.flags, HAS_BLOCK | HAS_SHREDS | HAS_REPLAYED);
        assert_eq!(live.full_millis, 341);
        assert_eq!(live.replayed_millis, 393);
    }

    #[test]
    fn test_a_full_range_of_mainnet_sized_rows_fits_the_message_ceiling() {
        // Every figure as large as a real slot's gets: fees in the thousands of
        // SOL, a thirteen-digit clock, a compute figure at the row's clamp. The
        // worst case the types allow does not fit and never did; this is the
        // case the page will meet.
        let mut history = SlotHistory::new(MAX_RANGE_SLOTS);
        for slot in 0..MAX_RANGE_SLOTS as Slot {
            let mut big = with_block(slot, 99_999, 99_999);
            if let Some(block) = big.block.as_mut() {
                block.block_cost = u64::from(u32::MAX);
                block.total_fees = 9_999_999_999_999;
                block.priority_fees = 9_999_999_999_999;
                block.tips = Some(9_999_999_999_999);
                block.replay_micros = Some(9_999_999);
            }
            big.shreds = Some(ShredArrival {
                count: 99_999,
                repaired: 99_999,
                full_millis: 999_999,
            });
            big.replayed_millis = Some(999_999);
            history.record(&big);
            history.record_time(slot, 1_999_999_999_999);
        }

        let encoded = crate::proto::encode("slot", "range", &history.range(0, MAX_RANGE_SLOTS));
        assert!(
            encoded.len() < crate::proto::MAX_MESSAGE / 2,
            "a full range is {} bytes against a {} byte ceiling",
            encoded.len(),
            crate::proto::MAX_MESSAGE
        );
    }

    #[test]
    fn test_replay_time_travels_with_its_flag() {
        let mut history = SlotHistory::new(64);
        let mut replayed = with_block(40, 100, 10);
        if let Some(block) = replayed.block.as_mut() {
            block.replay_micros = Some(47_200);
        }
        history.record(&replayed);
        history.record(&with_block(41, 100, 10));

        let read = history.get(40).expect("recorded");
        assert_eq!(read.flags & HAS_REPLAY, HAS_REPLAY);
        assert_eq!(read.replay_micros, 47_200);
        let built = history.get(41).expect("recorded");
        assert_eq!(built.flags & HAS_REPLAY, 0);
    }

    #[test]
    fn test_tips_of_nought_are_not_tips_that_were_never_read() {
        // The reason for a third flag bit: a turn the searchers passed by and a turn
        // never measured must not draw the same.
        let mut history = SlotHistory::new(64);

        let mut measured = with_block(20, 100, 10);
        if let Some(block) = measured.block.as_mut() {
            block.tips = Some(0);
        }
        history.record(&measured);
        history.record(&with_block(21, 100, 10));

        let read = history.get(20).expect("recorded");
        assert_eq!(read.flags & HAS_TIPS, HAS_TIPS);
        assert_eq!(read.tips, 0);

        let unread = history.get(21).expect("recorded");
        assert_eq!(unread.flags & HAS_TIPS, 0);
        assert_eq!(unread.tips, 0);
    }

    #[test]
    fn test_the_two_kinds_of_fee_are_kept_apart() {
        // Base is the subtraction, so the pair has to survive the trip.
        let mut history = SlotHistory::new(64);
        let mut block = with_block(30, 100, 10);
        if let Some(detail) = block.block.as_mut() {
            detail.total_fees = 104_600_000;
            detail.priority_fees = 99_000_000;
        }
        history.record(&block);

        let read = history.get(30).expect("recorded");
        assert_eq!(read.fees, 104_600_000);
        assert_eq!(read.priority_fees, 99_000_000);
        assert_eq!(read.fees.saturating_sub(read.priority_fees), 5_600_000);
    }

    #[test]
    fn test_a_slot_never_recorded_reads_as_absent() {
        let history = SlotHistory::new(64);
        assert!(history.get(900).is_none());
    }

    #[test]
    fn test_a_block_that_landed_empty_is_not_a_block_that_was_never_seen() {
        // Both are nought in every count, which is why the flag exists.
        let mut history = SlotHistory::new(64);
        history.record(&with_block(900, 0, 0));
        history.record(&entry(901));

        assert_eq!(
            history.get(900).expect("recorded").flags & HAS_BLOCK,
            HAS_BLOCK
        );
        assert_eq!(history.get(901).expect("recorded").flags & HAS_BLOCK, 0);
    }

    #[test]
    fn test_a_count_past_the_row_is_clamped_rather_than_wrapped() {
        let mut history = SlotHistory::new(64);
        history.record(&with_block(900, u64::MAX, 0));
        assert_eq!(history.get(900).expect("recorded").votes, u32::MAX);
    }
}
