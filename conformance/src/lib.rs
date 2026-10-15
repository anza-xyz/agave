#![cfg(feature = "agave-unstable-api")]

//! Cross-domain conformance harnesses for Agave.

#[cfg(feature = "ffi")]
pub mod block;
#[cfg(feature = "ffi")]
pub mod cost;
#[cfg(feature = "ffi")]
pub mod gossip;
#[cfg(feature = "ffi")]
pub mod shred;
#[cfg(feature = "ffi")]
pub mod txn;

/// Pins Rayon pools to one threadNo-op; required by Firedancer's conformance
/// tooling.
///
/// # Safety
///
/// Must not be called while other threads may access the process environment.
#[cfg(feature = "ffi")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_init(_log_level: i32) {
    // Safety: The caller guarantees exclusive access to the environment.
    unsafe {
        std::env::set_var("SOLANA_RAYON_THREADS", "1");
        std::env::set_var("RAYON_NUM_THREADS", "1");
    }
}

/// No-op; required by Firedancer's conformance tooling.
#[cfg(feature = "ffi")]
#[unsafe(no_mangle)]
pub extern "C" fn sol_compat_fini() {}
