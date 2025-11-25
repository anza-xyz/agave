#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
use {log::warn, std::env};
//TODO remove this hack when rayon fixes itself

// reduce the number of threads each pool is allowed to half the cpu core count, to avoid rayon
// hogging cpu
static MAX_RAYON_THREADS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    env::var("SOLANA_RAYON_THREADS")
        .ok()
        .and_then(|num_threads| {
            warn!(
                "Use of SOLANA_RAYON_THREADS has been deprecated and will be removed soon. Use \
                 the individual agave-validator CLI flags to configure threadpool sizes"
            );
            num_threads.parse().ok()
        })
        .unwrap_or_else(|| num_cpus::get() / 2)
        .max(1)
});

pub fn get_thread_count() -> usize {
    *MAX_RAYON_THREADS
}

#[deprecated(
    since = "3.0.0",
    note = "The solana-rayon-threadlimit crate will be removed, use num_cpus::get() or something \
            similar instead"
)]
pub fn get_max_thread_count() -> usize {
    #[allow(deprecated)]
    get_thread_count().saturating_mul(2)
}
