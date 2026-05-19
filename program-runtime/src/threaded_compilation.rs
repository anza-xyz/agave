use {
    crate::program_cache_entry::ProgramCacheEntry,
    std::sync::{Arc, atomic::AtomicU64},
};

// There are ~600ms per slot. Making an assumption that each compile takes on average 1ms, that
// would mean we'd fit about 600 compiles per slot. Given that slot also involves sending and
// receiving data, executing programs, etc. giving about 1/10th of the slot time for compilation
// seems quite appropriate. Otherwise we might end up spending a lot of time compiling programs
// that have already been executed in interpreted mode.
const COMPILE_REQUEST_CHANNEL_SIZE: usize = 64;

struct CompilationRequest {
    entry: Arc<ProgramCacheEntry>,
    compile_time_us: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct CompilationWorker {
    sender: crossbeam_channel::Sender<CompilationRequest>,
}

impl Default for CompilationWorker {
    fn default() -> Self {
        let (compile_send, compile_recv) =
            crossbeam_channel::bounded::<CompilationRequest>(COMPILE_REQUEST_CHANNEL_SIZE);
        std::thread::Builder::new()
            .name("solCompile".into())
            .spawn(move || {
                for request in compile_recv {
                    request.compile_time_us.fetch_add(
                        request.entry.try_compile_loaded(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            })
            .expect("thread spawns should succeed");
        Self {
            sender: compile_send,
        }
    }
}

impl CompilationWorker {
    pub fn request_compilation(
        &self,
        entry: Arc<ProgramCacheEntry>,
        compile_time_us: Arc<AtomicU64>,
    ) {
        let _ = self.sender.try_send(CompilationRequest {
            entry,
            compile_time_us,
        });
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum CompilationMode {
    #[default]
    ThreadedJit,
    AlwaysJit,
    Never,
}

impl CompilationMode {
    pub fn get() -> Self {
        static COMPILE_MODE: std::sync::LazyLock<CompilationMode> =
            std::sync::LazyLock::new(|| {
                std::env::var("AGAVE_COMPILE_PROGRAMS")
                    .map(|v| match &*v {
                        "always" => CompilationMode::AlwaysJit,
                        "never" => CompilationMode::Never,
                        "threaded" => CompilationMode::ThreadedJit,
                        _ => CompilationMode::ThreadedJit,
                    })
                    .unwrap_or_default()
            });
        *COMPILE_MODE
    }
}
