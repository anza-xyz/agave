use {crate::SlotLeader, solana_clock::Slot};

/// The leader of a slot together with the leader of the slot after it, as
/// resolved from the leader schedule by whoever creates the bank for that slot.
///
/// Lives in its own module so the fields can only be set through the
/// constructors below.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SlotLeaders {
    cur_leader: SlotLeader,
    next_leader: SlotLeader,
}

impl SlotLeaders {
    /// Looks up the leaders of `slot` and `slot + 1` with `leader_at`, so the
    /// pair always matches the schedule it was read from. Returns `None` if
    /// either slot has no leader.
    pub fn try_new(
        slot: Slot,
        mut leader_at: impl FnMut(Slot) -> Option<SlotLeader>,
    ) -> Option<Self> {
        Some(Self {
            cur_leader: leader_at(slot)?,
            next_leader: leader_at(slot.saturating_add(1))?,
        })
    }

    /// Pairs leaders that were not read from a leader schedule, such as the
    /// genesis leader or a warp target past what epoch stakes cover. Prefer
    /// [`Self::try_new`].
    pub fn new_unchecked(cur_leader: SlotLeader, next_leader: SlotLeader) -> Self {
        Self {
            cur_leader,
            next_leader,
        }
    }

    /// Uses `leader` for both slots, for tests that don't care about the next
    /// leader.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(leader: SlotLeader) -> Self {
        Self::new_unchecked(leader, leader)
    }

    /// The leader of the slot.
    pub fn cur_leader(&self) -> &SlotLeader {
        &self.cur_leader
    }

    /// The leader of the slot after it.
    pub fn next_leader(&self) -> &SlotLeader {
        &self.next_leader
    }
}
