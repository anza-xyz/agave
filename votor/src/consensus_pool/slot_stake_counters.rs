#![allow(dead_code)]
// TODO(wen): remove allow(dead_code) when consensus_pool is fully integrated

use {
    crate::{
        common::{
            Stake, SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP,
            SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP, SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY,
            SAFE_TO_SKIP_THRESHOLD,
        },
        consensus_pool::stats::ConsensusPoolStats,
        event::VotorEvent,
    },
    agave_votor_messages::vote::Vote,
    solana_hash::Hash,
    std::collections::BTreeMap,
};

#[derive(Debug, Default)]
pub(crate) struct SlotStakeCounters {
    my_first_vote: Option<Vote>,
    total_stake: Stake,
    skip_total: Stake,
    notarize_total: Stake,
    notarize_entry_total: BTreeMap<Hash, Stake>,
    top_notarized_stake: Stake,
    safe_to_notar_sent: Vec<Hash>,
    safe_to_skip_sent: bool,
}

impl SlotStakeCounters {
    pub fn new(total_stake: Stake) -> Self {
        Self {
            total_stake,
            ..Default::default()
        }
    }

    pub fn add_vote(
        &mut self,
        vote: &Vote,
        entry_stake: Stake,
        is_my_own_vote: bool,
        events: &mut Vec<VotorEvent>,
        stats: &mut ConsensusPoolStats,
    ) {
        match vote {
            Vote::Skip(_) => self.skip_total = entry_stake,
            Vote::Notarize(vote) => {
                let old_entry_stake = self
                    .notarize_entry_total
                    .insert(vote.block_id, entry_stake)
                    .unwrap_or(0);
                self.notarize_total = self
                    .notarize_total
                    .saturating_sub(old_entry_stake)
                    .saturating_add(entry_stake);
                self.top_notarized_stake = self.top_notarized_stake.max(entry_stake);
            }
            _ => return, // Not interested in other vote types
        }
        if self.my_first_vote.is_none() && is_my_own_vote {
            self.my_first_vote = Some(*vote);
        }
        if self.my_first_vote.is_none() {
            // We have not voted yet, no need to check safe to notarize or skip
            return;
        }
        let slot = vote.slot();
        // Check safe to notar
        for (block_id, stake) in &self.notarize_entry_total {
            if !self.safe_to_notar_sent.contains(block_id) && self.is_safe_to_notar(block_id, stake)
            {
                events.push(VotorEvent::SafeToNotar((slot, *block_id)));
                stats.event_safe_to_notarize = stats.event_safe_to_notarize.saturating_add(1);
                self.safe_to_notar_sent.push(*block_id);
            }
        }
        // Check safe to skip
        if !self.safe_to_skip_sent && self.is_safe_to_skip() {
            events.push(VotorEvent::SafeToSkip(slot));
            self.safe_to_skip_sent = true;
            stats.event_safe_to_skip = stats.event_safe_to_skip.saturating_add(1);
        }
    }

    fn is_safe_to_notar(&self, block_id: &Hash, stake: &Stake) -> bool {
        // White paper v1.1 page 22: The event is only issued if the node voted in slot s already,
        // but not to notarize b. Moreover:
        // notar(b) >= 40% or (skip(s) + notar(b) >= 60% and notar(b) >= 20%)
        if let Some(Vote::Notarize(my_vote)) = self.my_first_vote.as_ref() {
            if &my_vote.block_id == block_id {
                return false; // I voted for the same block, no need to send NotarizeFallback
            }
        }
        let skip_ratio = self.skip_total as f64 / self.total_stake as f64;
        let notarized_ratio = *stake as f64 / self.total_stake as f64;
        trace!("safe_to_notar {block_id:?} {skip_ratio} {notarized_ratio}");
        // Check if the block fits condition (i) 40% of stake holders voted notarize
        notarized_ratio >= SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY
            // Check if the block fits condition (ii) 20% notarized, and 60% notarized or skip
            || (notarized_ratio >= SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP
                && notarized_ratio + skip_ratio >= SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP)
    }

    fn is_safe_to_skip(&self) -> bool {
        // White paper v1.1 page 22: The event is only issued if the node voted in slot s already,
        // but not to skip s. Moreover:
        // skip(s) + Sum of all notarize - (max in notarize(b)) >= 40%
        if let Some(Vote::Notarize(_)) = self.my_first_vote.as_ref() {
            trace!(
                "safe_to_skip {} {:?} {} {} {}",
                self.my_first_vote.unwrap().slot(),
                self.my_first_vote.unwrap().block_id(),
                self.skip_total,
                self.notarize_total,
                self.top_notarized_stake
            );
            self.skip_total
                .saturating_add(self.notarize_total.saturating_sub(self.top_notarized_stake))
                as f64
                / self.total_stake as f64
                >= SAFE_TO_SKIP_THRESHOLD
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, agave_votor_messages::vote::Vote};

    #[test]
    fn test_safe_to_notar() {
        let mut counters = SlotStakeCounters::new(100);

        let mut events = vec![];
        let mut stats = ConsensusPoolStats::default();
        let slot = 2;
        // I voted for skip
        counters.add_vote(
            &Vote::new_skip_vote(slot),
            10,
            true,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_notarize, 0);

        // 40% of stake holders voted notarize
        counters.add_vote(
            &Vote::new_notarization_vote(slot, Hash::default()),
            40,
            false,
            &mut events,
            &mut stats,
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0], VotorEvent::SafeToNotar((s, block_id)) if s == slot && block_id == Hash::default())
        );
        assert_eq!(stats.event_safe_to_notarize, 1);
        events.clear();

        // Adding more notarizations does not trigger more events
        counters.add_vote(
            &Vote::new_notarization_vote(slot, Hash::default()),
            20,
            false,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_notarize, 1);

        // Reset counters
        counters = SlotStakeCounters::new(100);
        events.clear();
        stats = ConsensusPoolStats::default();

        // I voted for notarize b
        let hash_1 = Hash::new_unique();
        counters.add_vote(
            &Vote::new_notarization_vote(slot, hash_1),
            1,
            true,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_notarize, 0);

        // 25% of stake holders voted notarize b'
        let hash_2 = Hash::new_unique();
        counters.add_vote(
            &Vote::new_notarization_vote(slot, hash_2),
            25,
            false,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_notarize, 0);

        // 35% more of stake holders voted skip
        counters.add_vote(
            &Vote::new_skip_vote(slot),
            35,
            false,
            &mut events,
            &mut stats,
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0], VotorEvent::SafeToNotar((s, block_id)) if s == slot && block_id == hash_2)
        );
        assert_eq!(stats.event_safe_to_notarize, 1);
    }

    #[test]
    fn test_safe_to_skip() {
        let mut counters = SlotStakeCounters::new(100);

        let mut events = vec![];
        let mut stats = ConsensusPoolStats::default();
        let slot = 2;
        // I voted for notarize b
        counters.add_vote(
            &Vote::new_notarization_vote(slot, Hash::default()),
            10,
            true,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_skip, 0);

        // 40% of stake holders voted skip
        counters.add_vote(
            &Vote::new_skip_vote(slot),
            40,
            false,
            &mut events,
            &mut stats,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], VotorEvent::SafeToSkip(s) if s == slot));
        assert_eq!(stats.event_safe_to_skip, 1);
        events.clear();

        // Adding more skips does not trigger more events
        counters.add_vote(
            &Vote::new_skip_vote(slot),
            20,
            false,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_skip, 1);

        // Reset counters
        counters = SlotStakeCounters::new(100);
        events.clear();
        stats = ConsensusPoolStats::default();

        // I voted for notarize b, 10% of stake holders voted with me
        let hash_1 = Hash::new_unique();
        counters.add_vote(
            &Vote::new_notarization_vote(slot, hash_1),
            10,
            true,
            &mut events,
            &mut stats,
        );
        // 20% of stake holders voted a different notarization b'
        let hash_2 = Hash::new_unique();
        counters.add_vote(
            &Vote::new_notarization_vote(slot, hash_2),
            20,
            false,
            &mut events,
            &mut stats,
        );
        // 30% of stake holders voted skip
        counters.add_vote(
            &Vote::new_skip_vote(slot),
            30,
            false,
            &mut events,
            &mut stats,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], VotorEvent::SafeToSkip(s) if s == slot));
        assert_eq!(stats.event_safe_to_skip, 1);
        events.clear();

        // Adding more notarization on b does not trigger more events
        counters.add_vote(
            &Vote::new_notarization_vote(slot, hash_1),
            10,
            false,
            &mut events,
            &mut stats,
        );
        assert!(events.is_empty());
        assert_eq!(stats.event_safe_to_skip, 1);
    }
}
