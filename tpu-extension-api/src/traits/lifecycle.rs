use std::thread;

/// Shutdown contract for a long-running pipeline stage.
///
/// The two-phase protocol — signal then wait — lets the caller abort all
/// stages concurrently before blocking on any join:
///
/// ```text
/// for stage in stages.iter().rev() { stage.abort(); }   // signal all
/// for stage in stages.into_iter().rev() { stage.join()?; } // wait all
/// ```
///
/// `abort` must be idempotent: `Tpu::join` calls it before `join`, and each
/// stage's own `join` may also call it defensively.
pub trait LifecycleStage: Send + 'static {
    /// Send the shutdown signal. Non-blocking; must be idempotent.
    fn abort(&self);
    /// Block until the stage thread exits. Consumes the handle.
    fn join(self: Box<Self>) -> thread::Result<()>;
}

/// A custom pipeline stage owned by the `Tpu` and managed alongside core stages.
///
/// Implement this on each stage struct the fork spawns (block-engine relay,
/// bundle sigverify, bundle executor, …). The `Tpu` calls `abort` in reverse
/// push order and `join` in the same order, giving intake stages a chance to
/// drain before processing stages are stopped.
pub trait TpuStage: LifecycleStage {}
