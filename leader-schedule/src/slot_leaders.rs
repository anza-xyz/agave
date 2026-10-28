use crate::SlotLeader;

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
    /// Pairs the leader of a slot with the leader of the slot after it. Callers
    /// creating a bank from the leader schedule should get the pair from
    /// `LeaderScheduleCache::slot_leaders_at`.
    pub fn new(cur_leader: SlotLeader, next_leader: SlotLeader) -> Self {
        Self {
            cur_leader,
            next_leader,
        }
    }

    /// Uses `leader` for both slots, for tests that don't care about the next
    /// leader.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(leader: SlotLeader) -> Self {
        Self::new(leader, leader)
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
