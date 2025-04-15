use {crate::rolling_bit_field::RollingBitField, solana_clock::Slot};

#[derive(Debug)]
pub struct RootsTracker {
    /// Current roots where appendvecs or write cache has account data.
    /// Constructed during load from snapshots.
    /// Updated every time we add a new root or clean/shrink an append vec into irrelevancy.
    /// Range is approximately the last N slots where N is # slots per epoch.
    pub alive_roots: RollingBitField,
}

impl Default for RootsTracker {
    fn default() -> Self {
        // we expect to keep a rolling set of 400k slots around at a time
        // 4M gives us plenty of extra(?!) room to handle a width 10x what we should need.
        // cost is 4M bits of memory, which is .5MB
        Self::new(4194304)
    }
}

impl RootsTracker {
    pub fn new(max_width: u64) -> Self {
        Self {
            alive_roots: RollingBitField::new(max_width),
        }
    }

    pub fn min_alive_root(&self) -> Option<Slot> {
        self.alive_roots.min()
    }
}
