use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub enum CoreAllocation {
    ///Use OS default allocation (i.e. do not alter core affinity)
    #[default]
    OsDefault,
    ///Pin each thread to a core in given range. Number of cores should be >= number of threads
    PinnedCores { min: usize, max: usize },
    ///Pin the threads to a set of cores
    DedicatedCoreSet { min: usize, max: usize },
}

impl CoreAllocation {
    /// Converts into a vector of core IDs. OsDefault is converted to empty vector.
    pub fn as_core_mask_vector(&self) -> Vec<usize> {
        match *self {
            CoreAllocation::PinnedCores { min, max } => (min..max).collect(),
            CoreAllocation::DedicatedCoreSet { min, max } => (min..max).collect(),
            CoreAllocation::OsDefault => vec![],
        }
    }
}

///Applies policy to the calling thread
pub fn apply_policy(alloc: &CoreAllocation, priority: u32) {}
