//! Configuration for epochs and slots.
//!
//! Epochs mark a period of time composed of _slots_, for which a particular
//! [leader schedule][ls] is in effect. The epoch schedule determines the length
//! of epochs, and the timing of the next leader-schedule selection.
//!
//! [ls]: https://docs.solanalabs.com/consensus/leader-rotation#leader-schedule-rotation
//!
//! The epoch schedule does not change during the life of a blockchain,
//! though the length of an epoch does &mdash; during the initial launch of
//! the chain there is a "warmup" period, where epochs are short, with subsequent
//! epochs increasing in slots until they last for [`DEFAULT_SLOTS_PER_EPOCH`].

pub use crate::clock::{Epoch, Slot, DEFAULT_SLOTS_PER_EPOCH};
use solana_sdk_macro::CloneZeroed;

/// The default number of slots before an epoch starts to calculate the leader schedule.
pub const DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET: u64 = DEFAULT_SLOTS_PER_EPOCH;

/// The maximum number of slots before an epoch starts to calculate the leader schedule.
///
/// Default is an entire epoch, i.e. leader schedule for epoch X is calculated at
/// the beginning of epoch X - 1.
pub const MAX_LEADER_SCHEDULE_EPOCH_OFFSET: u64 = 3;

/// The minimum number of slots per epoch during the warmup period.
///
/// Based on `MAX_LOCKOUT_HISTORY` from `vote_program`.
pub const MINIMUM_SLOTS_PER_EPOCH: u64 = 32;

#[repr(C)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, CloneZeroed, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochSchedule {
    /// The maximum number of slots in each epoch.
    pub slots_per_epoch: u64,

    /// A number of slots before beginning of an epoch to calculate
    /// a leader schedule for that epoch.
    pub leader_schedule_slot_offset: u64,

    /// Whether epochs start short and grow.
    pub warmup: bool,

    /// The first epoch after the warmup period.
    ///
    /// Basically: `log2(slots_per_epoch) - log2(MINIMUM_SLOTS_PER_EPOCH)`.
    pub first_normal_epoch: Epoch,

    /// The first slot after the warmup period.
    ///
    /// Basically: `MINIMUM_SLOTS_PER_EPOCH * (2.pow(first_normal_epoch) - 1)`.
    pub first_normal_slot: Slot,
}

impl Default for EpochSchedule {
    fn default() -> Self {
        Self::custom(
            DEFAULT_SLOTS_PER_EPOCH,
            DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET,
            true,
        )
    }
}

impl EpochSchedule {
    pub fn new(slots_per_epoch: u64) -> Self {
        Self::custom(slots_per_epoch, slots_per_epoch, true)
    }
    pub fn without_warmup() -> Self {
        Self::custom(
            DEFAULT_SLOTS_PER_EPOCH,
            DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET,
            false,
        )
    }
    pub fn custom(slots_per_epoch: u64, leader_schedule_slot_offset: u64, warmup: bool) -> Self {
        assert!(slots_per_epoch >= MINIMUM_SLOTS_PER_EPOCH);
        let (first_normal_epoch, first_normal_slot) = if warmup {
            let next_power_of_two = slots_per_epoch.next_power_of_two();
            let log2_slots_per_epoch = next_power_of_two
                .trailing_zeros()
                .saturating_sub(MINIMUM_SLOTS_PER_EPOCH.trailing_zeros());

            (
                u64::from(log2_slots_per_epoch),
                next_power_of_two.saturating_sub(MINIMUM_SLOTS_PER_EPOCH),
            )
        } else {
            (0, 0)
        };
        EpochSchedule {
            slots_per_epoch,
            leader_schedule_slot_offset,
            warmup,
            first_normal_epoch,
            first_normal_slot,
        }
    }

    /// get the length of the given epoch (in slots)
    pub fn get_slots_in_epoch(&self, epoch: Epoch) -> u64 {
        if epoch < self.first_normal_epoch {
            2u64.saturating_pow(
                (epoch as u32).saturating_add(MINIMUM_SLOTS_PER_EPOCH.trailing_zeros()),
            )
        } else {
            self.slots_per_epoch
        }
    }

    /// Returns the number of elapsed slots between the start epoch and end epoch.
    pub fn calculate_elapsed_slots(&self, start_epoch: Epoch, end_epoch: Epoch) -> u64 {
        // This original code for this calculation was the following:
        //
        //  fn calculate(start_epoch: Epoch, end_epoch: Epoch, schedule: &EpochSchedule) -> u64 {
        //      (start_epoch..=end_epoch)
        //          .map(|epoch| {
        //              schedule.get_slots_in_epoch(epoch.saturating_add(1))
        //          }).sum()
        //  }
        //
        // It can be very slow if the difference between start epoch and end epoch is too big.
        // We can derive a mathematical expression to perform the calculation without a loop.
        // Let S be the start epoch, E the end epoch, N the self.first_normal_epoch, C
        // MINIMUM_SLOTS_PER_EPOCH.trailing_zeros(), and O the number of slots per epoch,
        // then we want to know:
        // elapsed_slots = 2^(S+1+C) + 2^(S+2+C) + ... + 2^(N-1+C) + O + O + ... + O
        // Let's divide the work:
        // before_first_normal = 2^(S+1+C) + 2^(S+2+C) + ... + 2^(N-1+C)
        // after_first_normal = O + O + ... + O
        // so that elapsed_slots = before_first_normal+after_first_normal
        //
        // before_first_normal is a geometric progression
        // (https://en.wikipedia.org/wiki/Geometric_progression), whose sum is a well known value.
        // before_first_normal = 2^C * (2^(S+1) + 2^(S+2) + ... + 2^(N-1))
        // before_first_normal = 2^C * (2^(S+1) * (2^(N-1-S-1+1) - 1)/(2-1))
        // before_first_normal = 2^C * (2^N - 2^(S+1))
        //
        // after_first_normal is simply a sum of terms, so we can do:
        // after_first_normal = O * ((E + 1) - N + 1)
        // after_first_normal = O * (E + 2 - N)
        //
        // [1] Note that if end_epoch is less than self.first_normal_epoch, after_first_normal is zero,
        // and end_epoch+1 would assume the value of N-1 in before_first_normal.
        //
        // [2] Likewise, if start_epoch is greater than self.first_normal_epoch, before_first_normal is
        // zero, and start_epoch+1 would assume the value of N in after_first_normal.

        let n = if end_epoch.saturating_add(1) < self.first_normal_epoch {
            // As in [1], E+1 should be N-1 here, so N = E + 2
            end_epoch.saturating_add(2)
        } else {
            // N is first_normal_epoch when end_epoch+1 is greater than first_normal_slot
            self.first_normal_epoch
        };

        // This is 2^(N)
        let two_power_of_n = 2u64.saturating_pow(n as u32);
        // This is 2^(S+1)
        let two_power_of_s_1 = 2u64.saturating_pow(start_epoch.saturating_add(1) as u32);

        // This is 2^N - 2^(S+1)
        let two_powers_sub = two_power_of_n.saturating_sub(two_power_of_s_1);
        // As C is log2(MINIMUM_SLOTS_PER_EPOCH), 2^C equals MINIMUM_SLOTS_PER_EPOCH
        // This is 2^(C) * (2^N - 2^(S+1))
        let before_first_normal = two_powers_sub.saturating_mul(MINIMUM_SLOTS_PER_EPOCH);

        let n = if self.first_normal_epoch < start_epoch.saturating_add(1) {
            // As in [2] (see my explanation), S+1 should be N here, so N = S + 1
            start_epoch.saturating_add(1)
        } else {
            // N equals first_normal_epoch when the latter is less that start_epoch +1
            self.first_normal_epoch
        };

        // This is (E + 1) - N + 1 => E + 2 - N
        let e_plus_two_minus_n = end_epoch.saturating_add(2).saturating_sub(n);
        let after_first_normal = e_plus_two_minus_n.saturating_mul(self.slots_per_epoch);

        before_first_normal.saturating_add(after_first_normal)
    }

    /// get the epoch for which the given slot should save off
    ///  information about stakers
    pub fn get_leader_schedule_epoch(&self, slot: Slot) -> Epoch {
        if slot < self.first_normal_slot {
            // until we get to normal slots, behave as if leader_schedule_slot_offset == slots_per_epoch
            self.get_epoch_and_slot_index(slot).0.saturating_add(1)
        } else {
            let new_slots_since_first_normal_slot = slot.saturating_sub(self.first_normal_slot);
            let new_first_normal_leader_schedule_slot =
                new_slots_since_first_normal_slot.saturating_add(self.leader_schedule_slot_offset);
            let new_epochs_since_first_normal_leader_schedule =
                new_first_normal_leader_schedule_slot
                    .checked_div(self.slots_per_epoch)
                    .unwrap_or(0);
            self.first_normal_epoch
                .saturating_add(new_epochs_since_first_normal_leader_schedule)
        }
    }

    /// get epoch for the given slot
    pub fn get_epoch(&self, slot: Slot) -> Epoch {
        self.get_epoch_and_slot_index(slot).0
    }

    /// get epoch and offset into the epoch for the given slot
    pub fn get_epoch_and_slot_index(&self, slot: Slot) -> (Epoch, u64) {
        if slot < self.first_normal_slot {
            let epoch = slot
                .saturating_add(MINIMUM_SLOTS_PER_EPOCH)
                .saturating_add(1)
                .next_power_of_two()
                .trailing_zeros()
                .saturating_sub(MINIMUM_SLOTS_PER_EPOCH.trailing_zeros())
                .saturating_sub(1);

            let epoch_len =
                2u64.saturating_pow(epoch.saturating_add(MINIMUM_SLOTS_PER_EPOCH.trailing_zeros()));

            (
                u64::from(epoch),
                slot.saturating_sub(epoch_len.saturating_sub(MINIMUM_SLOTS_PER_EPOCH)),
            )
        } else {
            let normal_slot_index = slot.saturating_sub(self.first_normal_slot);
            let normal_epoch_index = normal_slot_index
                .checked_div(self.slots_per_epoch)
                .unwrap_or(0);
            let epoch = self.first_normal_epoch.saturating_add(normal_epoch_index);
            let slot_index = normal_slot_index
                .checked_rem(self.slots_per_epoch)
                .unwrap_or(0);
            (epoch, slot_index)
        }
    }

    pub fn get_first_slot_in_epoch(&self, epoch: Epoch) -> Slot {
        if epoch <= self.first_normal_epoch {
            2u64.saturating_pow(epoch as u32)
                .saturating_sub(1)
                .saturating_mul(MINIMUM_SLOTS_PER_EPOCH)
        } else {
            epoch
                .saturating_sub(self.first_normal_epoch)
                .saturating_mul(self.slots_per_epoch)
                .saturating_add(self.first_normal_slot)
        }
    }

    pub fn get_last_slot_in_epoch(&self, epoch: Epoch) -> Slot {
        self.get_first_slot_in_epoch(epoch)
            .saturating_add(self.get_slots_in_epoch(epoch))
            .saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::distributions::{Distribution, Uniform},
    };

    #[test]
    fn test_epoch_schedule() {
        // one week of slots at 8 ticks/slot, 10 ticks/sec is
        // (1 * 7 * 24 * 4500u64).next_power_of_two();

        // test values between MINIMUM_SLOT_LEN and MINIMUM_SLOT_LEN * 16, should cover a good mix
        for slots_per_epoch in MINIMUM_SLOTS_PER_EPOCH..=MINIMUM_SLOTS_PER_EPOCH * 16 {
            let epoch_schedule = EpochSchedule::custom(slots_per_epoch, slots_per_epoch / 2, true);

            assert_eq!(epoch_schedule.get_first_slot_in_epoch(0), 0);
            assert_eq!(
                epoch_schedule.get_last_slot_in_epoch(0),
                MINIMUM_SLOTS_PER_EPOCH - 1
            );

            let mut last_leader_schedule = 0;
            let mut last_epoch = 0;
            let mut last_slots_in_epoch = MINIMUM_SLOTS_PER_EPOCH;
            for slot in 0..(2 * slots_per_epoch) {
                // verify that leader_schedule_epoch is continuous over the warmup
                // and into the first normal epoch

                let leader_schedule = epoch_schedule.get_leader_schedule_epoch(slot);
                if leader_schedule != last_leader_schedule {
                    assert_eq!(leader_schedule, last_leader_schedule + 1);
                    last_leader_schedule = leader_schedule;
                }

                let (epoch, offset) = epoch_schedule.get_epoch_and_slot_index(slot);

                //  verify that epoch increases continuously
                if epoch != last_epoch {
                    assert_eq!(epoch, last_epoch + 1);
                    last_epoch = epoch;
                    assert_eq!(epoch_schedule.get_first_slot_in_epoch(epoch), slot);
                    assert_eq!(epoch_schedule.get_last_slot_in_epoch(epoch - 1), slot - 1);

                    // verify that slots in an epoch double continuously
                    //   until they reach slots_per_epoch

                    let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
                    if slots_in_epoch != last_slots_in_epoch && slots_in_epoch != slots_per_epoch {
                        assert_eq!(slots_in_epoch, last_slots_in_epoch * 2);
                    }
                    last_slots_in_epoch = slots_in_epoch;
                }
                // verify that the slot offset is less than slots_in_epoch
                assert!(offset < last_slots_in_epoch);
            }

            // assert that these changed  ;)
            assert!(last_leader_schedule != 0); // t
            assert!(last_epoch != 0);
            // assert that we got to "normal" mode
            assert!(last_slots_in_epoch == slots_per_epoch);
        }
    }

    #[test]
    fn test_clone() {
        let epoch_schedule = EpochSchedule {
            slots_per_epoch: 1,
            leader_schedule_slot_offset: 2,
            warmup: true,
            first_normal_epoch: 4,
            first_normal_slot: 5,
        };
        #[allow(clippy::clone_on_copy)]
        let cloned_epoch_schedule = epoch_schedule.clone();
        assert_eq!(cloned_epoch_schedule, epoch_schedule);
    }

    fn check_elapsed_epochs(start: Epoch, end: Epoch, schedule: &EpochSchedule) -> u64 {
        (start..=end)
            .map(|epoch| schedule.get_slots_in_epoch(epoch.saturating_add(1)))
            .sum()
    }

    #[test]
    fn test_calculate_elapsed_slots_sanity() {
        let epoch_schedule = EpochSchedule {
            slots_per_epoch: 5,
            leader_schedule_slot_offset: 2,
            warmup: true,
            first_normal_epoch: 10,
            first_normal_slot: 5,
        };

        let cases = vec![
            (0, 5),
            (1, 8),
            (1, 9),
            (1, 10),
            (10, 15),
            (12, 20),
            (2, 30),
            (1, 1),
            (10, 10),
            (20, 20),
            (0, 0),
            (50, 20),
            (20, 0),
            (20, 10),
            (20, 5),
            (10, 5),
            (8, 3),
        ];

        for item in &cases {
            assert_eq!(
                check_elapsed_epochs(item.0, item.1, &epoch_schedule),
                epoch_schedule.calculate_elapsed_slots(item.0, item.1)
            );
        }
    }

    #[test]
    #[cfg(not(target_os = "solana"))]
    fn test_calculate_elapsed_slots_fuzzy() {
        let mut rng = rand::thread_rng();
        let slots_per_epoch_dist = Uniform::from(1..=20);
        let first_normal_epoch_dist = Uniform::from(1..=60);

        let epoch_schedule = EpochSchedule {
            slots_per_epoch: slots_per_epoch_dist.sample(&mut rng),
            leader_schedule_slot_offset: 2,
            warmup: true,
            first_normal_epoch: first_normal_epoch_dist.sample(&mut rng),
            first_normal_slot: 5,
        };

        let start_epoch_dist = Uniform::from(0..=125);
        for _ in 0..5000 {
            let start_epoch = start_epoch_dist.sample(&mut rng);
            let end_epoch_dist = Uniform::from(start_epoch..=125);
            let end_epoch = end_epoch_dist.sample(&mut rng);
            assert_eq!(
                check_elapsed_epochs(start_epoch, end_epoch, &epoch_schedule),
                epoch_schedule.calculate_elapsed_slots(start_epoch, end_epoch)
            );
        }
    }
}
