use {
    serde::{Deserialize, Serialize},
    thread_priority::ThreadExt,
};

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

#[cfg(target_os = "linux")]
pub fn set_thread_affinity(cores: &[usize]) {
    affinity::set_thread_affinity(cores).expect("Can not set thread affinity for runtime worker");
}

#[cfg(not(target_os = "linux"))]
pub fn set_thread_affinity(_cores: &[usize]) {}

///Applies policy to the calling thread
pub fn apply_policy(
    alloc: &CoreAllocation,
    priority: u8,
    chosen_cores_mask: &std::sync::Mutex<Vec<usize>>,
) {
    std::thread::current()
        .set_priority(thread_priority::ThreadPriority::Crossplatform(
            (priority).try_into().unwrap(),
        ))
        .expect("Can not set thread priority!");

    match alloc {
        CoreAllocation::PinnedCores { min: _, max: _ } => {
            let mut lg = chosen_cores_mask
                .lock()
                .expect("Can not lock core mask mutex");
            let core = lg
                .pop()
                .expect("Not enough cores provided for pinned allocation");
            set_thread_affinity(&[core]);
        }
        CoreAllocation::DedicatedCoreSet { min: _, max: _ } => {
            let lg = chosen_cores_mask
                .lock()
                .expect("Can not lock core mask mutex");
            set_thread_affinity(&lg);
        }
        CoreAllocation::OsDefault => {}
    }
}
