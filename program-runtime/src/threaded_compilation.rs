use crate::program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryType};
use std::sync::{Arc, atomic::AtomicU64};

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

impl CompilationWorker {
    pub(crate) fn new() -> Self {
        let (compile_send, compile_recv) =
            crossbeam_channel::bounded::<CompilationRequest>(COMPILE_REQUEST_CHANNEL_SIZE);
        std::thread::Builder::new()
            .name("solCompile".into())
            .spawn(move || {
                for request in compile_recv {
                    let executable = match &request.entry.program {
                        ProgramCacheEntryType::FailedVerification(_)
                        | ProgramCacheEntryType::Closed
                        | ProgramCacheEntryType::DelayVisibility
                        | ProgramCacheEntryType::Unloaded(_)
                        | ProgramCacheEntryType::Builtin(_) => continue,
                        ProgramCacheEntryType::Loaded(executable) => executable,
                    };
                    if executable.get_compiled_program().is_some() {
                        continue;
                    }

                    #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
                    {
                        use solana_svm_measure::measure::Measure;
                        use std::sync::atomic::Ordering;

                        let jit_compile_time = Measure::start("jit_compile_time");
                        if let Err(e) = executable.jit_compile() {
                            log::warn!("compiling failed even though program is valid: {e:?}");
                        }
                        let jit_compile_time = jit_compile_time.end_as_us();
                        request
                            .compile_time_us
                            .fetch_add(jit_compile_time, Ordering::Relaxed);
                    }
                }
            })
            .expect("thread spawns should succeed");
        Self {
            sender: compile_send,
        }
    }

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
