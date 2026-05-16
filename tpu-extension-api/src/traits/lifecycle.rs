use std::thread;

// abort() must be idempotent; separate from join() to allow concurrent draining.
pub trait LifecycleStage: Send + 'static {
    fn abort(&self);
    fn join(self: Box<Self>) -> thread::Result<()>;
}

/// A custom pipeline stage that runs alongside the TPU.
/// Out of scope for the initial wiring: bundle transaction injection (bypassing sigverify)
/// is addressed in a separate RFC.
pub trait TpuStage: LifecycleStage {}
