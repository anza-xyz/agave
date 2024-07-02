use {
    solana_metrics::datapoint_trace,
    solana_program_runtime::{loaded_programs::LoadProgramMetrics, timings::ExecuteDetailsTimings},
    solana_sdk::saturating_add_assign,
};

pub fn submit_datapoint(metrics: LoadProgramMetrics, timings: &mut ExecuteDetailsTimings) {
    saturating_add_assign!(
        timings.create_executor_register_syscalls_us,
        metrics.register_syscalls_us
    );
    saturating_add_assign!(timings.create_executor_load_elf_us, metrics.load_elf_us);
    saturating_add_assign!(
        timings.create_executor_verify_code_us,
        metrics.verify_code_us
    );
    saturating_add_assign!(
        timings.create_executor_jit_compile_us,
        metrics.jit_compile_us
    );
    datapoint_trace!(
        "create_executor_trace",
        ("program_id", metrics.program_id, String),
        ("register_syscalls_us", metrics.register_syscalls_us, i64),
        ("load_elf_us", metrics.load_elf_us, i64),
        ("verify_code_us", metrics.verify_code_us, i64),
        ("jit_compile_us", metrics.jit_compile_us, i64),
    );
}
