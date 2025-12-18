use {
    log::*,
    std::{env, path::PathBuf},
};

pub fn locate_perf_libs() -> Option<PathBuf> {
    let exe = env::current_exe().expect("Unable to get executable path");
    let perf_libs = exe.parent().unwrap().join("perf-libs");
    if perf_libs.is_dir() {
        info!("perf-libs found at {perf_libs:?}");
        return Some(perf_libs);
    }
    warn!("{perf_libs:?} does not exist");
    None
}

pub fn append_to_ld_library_path(mut ld_library_path: String) {
    if let Ok(env_value) = env::var("LD_LIBRARY_PATH") {
        ld_library_path.push(':');
        ld_library_path.push_str(&env_value);
    }
    info!("setting ld_library_path to: {ld_library_path:?}");
    // Safety: caller (init_poh) is executed before any spawned threads might access the environment
    unsafe { env::set_var("LD_LIBRARY_PATH", ld_library_path) }
}
