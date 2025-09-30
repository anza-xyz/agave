#![allow(dead_code)]

// Just a placeholder before the real votor is upstream.

use std::sync::{atomic::AtomicBool, Arc, Condvar, Mutex};

pub struct Votor {}

impl Votor {
    pub fn wait_for_migration_or_exit(
        _exit: &Arc<AtomicBool>,
        _start: &Arc<(Mutex<bool>, Condvar)>,
    ) {
        warn!("votor::wait_for_migration_or_exit is a placeholder and does nothing");
    }
}
