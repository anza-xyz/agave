mod events;

use geyser_shaq_queues::GeyserQueues;
use std::cell::RefCell;
use std::sync::OnceLock;

pub use events::*;

/// Global queue path for thread-local notifiers
static QUEUE_PATH: OnceLock<String> = OnceLock::new();

// Thread-local GeyserNotifier instances
thread_local! {
    static THREAD_LOCAL_NOTIFIER: RefCell<Option<GeyserNotifier>> = const { RefCell::new(None) };
}

/// Thin wrapper around Geyser V2 shared memory queue producers.
///
/// Provides helper methods for constructing and sending Geyser events
/// without repeating message construction logic at every call site.
pub struct GeyserNotifier {
    queues: GeyserQueues,
}

impl GeyserNotifier {
    /// Create a new notifier with the given queue handle.
    pub fn new(queues: GeyserQueues) -> Self {
        Self { queues }
    }

    /// Initialize the global queue path for thread-local notifiers.
    ///
    /// This should be called once at validator startup before any threads
    /// attempt to use `thread_local()`.
    ///
    /// # Arguments
    /// * `queue_path` - Base path for shared memory queues
    ///
    /// # Returns
    /// * `Ok(())` if initialization succeeds
    /// * `Err(())` if already initialized
    pub fn init_global(queue_path: String) -> std::result::Result<(), ()> {
        QUEUE_PATH.set(queue_path).map_err(|_| ())
    }

    /// Access the thread-local notifier, creating it lazily if needed.
    ///
    /// Each thread gets its own GeyserNotifier with dedicated queue producers,
    /// ensuring lock-free operation following the scheduler pattern.
    ///
    /// # Arguments
    /// * `f` - Closure that receives mutable reference to the thread's notifier
    ///
    /// # Returns
    /// * `Some(R)` if global path is initialized and notifier creation succeeds
    /// * `None` if global path not initialized or queue creation fails
    ///
    /// # Example
    /// ```ignore
    /// // At validator startup
    /// GeyserNotifier::init_global("/dev/shm/geyser-queues".to_string()).ok();
    ///
    /// // In any thread
    /// GeyserNotifier::thread_local(|notifier| {
    ///     notifier.notify_account_update(...).ok()
    /// });
    /// ```
    pub fn thread_local<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&mut GeyserNotifier) -> R,
    {
        let queue_path = QUEUE_PATH.get()?;

        THREAD_LOCAL_NOTIFIER.with(|cell| {
            let mut opt = cell.borrow_mut();

            if opt.is_none() {
                eprint!("GEYSER V2: Attempting to open queues at: {}", queue_path);
                // Try to open first (in case another thread already created them)
                let queues = match GeyserQueues::open(queue_path) {
                    Ok(q) => {
                        eprint!("GEYSER V2: Successfully opened existing queues");
                        q
                    }
                    Err(_) => {
                        eprint!("GEYSER V2: Open failed, attempting to create new queues");
                        match GeyserQueues::create(queue_path) {
                            Ok(q) => {
                                eprint!("GEYSER V2: Successfully created new queues");
                                q
                            }
                            Err(e) => {
                                eprint!("GEYSER V2: Failed to create queues: {:?}", e);
                                return None;
                            }
                        }
                    }
                };

                *opt = Some(GeyserNotifier::new(queues));
            }

            opt.as_mut().map(f)
        })
    }

    /// Get mutable reference to the underlying queues
    pub fn queues_mut(&mut self) -> &mut GeyserQueues {
        &mut self.queues
    }
}
