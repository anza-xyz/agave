//! Logging utils.

/// Initialize a test logger with default error filter.
pub fn setup() {
    let _ = env_logger::Builder::from_env(env_logger::Env::new().default_filter_or("error"))
        .format_timestamp_nanos()
        .is_test(true)
        .try_init();
}
