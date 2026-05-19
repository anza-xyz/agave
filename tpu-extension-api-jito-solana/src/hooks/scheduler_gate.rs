use {
    agave_tpu_extension_api::SchedulerGate,
    std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Pauses the packet scheduler while a bundle is executing.
///
/// `BundleStage` raises the flag before acquiring locks and lowers it after
/// releasing them; the scheduler spins until the flag is clear.
pub struct BundleSchedulerGate(Arc<AtomicBool>);

impl BundleSchedulerGate {
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
}

impl SchedulerGate for BundleSchedulerGate {
    #[inline(always)]
    fn should_yield(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yield_flag_starts_false() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = BundleSchedulerGate::new(Arc::clone(&flag));
        assert!(!gate.should_yield());
        flag.store(true, Ordering::Release);
        assert!(gate.should_yield());
    }
}
