//! A module to define FinalizedSlot
use {solana_clock::Slot, std::cmp::Ordering};

/// A slot can be finalized if the node has a fast finalization cert for it (fast finalization) or
/// it has a notar and a finalization cert for it (slow finalization).  We prefer fast finalization
/// certs above slow finalization for the same slot.  This enum helps in comparing two different
/// finalized slots while preferring fast finalization over slow for the same slot.
#[derive(Debug, PartialEq, Eq)]
pub enum FinalizedSlot {
    /// The slot was slow finalized.
    Slow(Slot),
    /// The slot was fast finalized.
    Fast(Slot),
}

impl FinalizedSlot {
    /// Returns the actual slot.
    pub fn slot(&self) -> Slot {
        match self {
            Self::Slow(slot) | Self::Fast(slot) => *slot,
        }
    }
}

impl Ord for FinalizedSlot {
    /// Overloading the cmp operators to prefer fast finalization over slow for the same slot.
    fn cmp(&self, other: &Self) -> Ordering {
        match self.slot().cmp(&other.slot()) {
            Ordering::Equal => match (self, other) {
                (Self::Fast(_), Self::Slow(_)) => Ordering::Greater,
                (Self::Slow(_), Self::Fast(_)) => Ordering::Less,
                _ => Ordering::Equal,
            },
            ordering => ordering,
        }
    }
}

impl PartialOrd for FinalizedSlot {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn validation() {
        assert!(FinalizedSlot::Slow(5) < FinalizedSlot::Slow(6));
        assert!(FinalizedSlot::Slow(5) == FinalizedSlot::Slow(5));
        assert!(FinalizedSlot::Slow(5) > FinalizedSlot::Slow(4));

        assert!(FinalizedSlot::Fast(5) < FinalizedSlot::Fast(6));
        assert!(FinalizedSlot::Fast(5) == FinalizedSlot::Fast(5));
        assert!(FinalizedSlot::Fast(5) > FinalizedSlot::Fast(4));

        assert!(FinalizedSlot::Slow(5) < FinalizedSlot::Fast(6));
        assert!(FinalizedSlot::Slow(5) < FinalizedSlot::Fast(5));
        assert!(FinalizedSlot::Slow(5) > FinalizedSlot::Fast(4));

        assert!(FinalizedSlot::Fast(5) < FinalizedSlot::Slow(6));
        assert!(FinalizedSlot::Fast(5) > FinalizedSlot::Slow(5));
        assert!(FinalizedSlot::Fast(5) > FinalizedSlot::Slow(4));
    }
}
