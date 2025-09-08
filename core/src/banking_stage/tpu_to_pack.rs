//! Service to send transaction packets to the external scheduler.
//!

use {
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    agave_scheduler_bindings::TpuToPackMessage,
    rts_alloc::Allocator,
    std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::JoinHandle,
    },
};

pub fn spawn(
    exit: Arc<AtomicBool>,
    non_vote_receiver: BankingPacketReceiver,
    allocator_path: PathBuf,
    allocator_worker_id: u32,
    queue_path: PathBuf,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("solTpu2Pack".to_string())
        .spawn(move || {
            // Setup allocator and queue
            if let Some((allocator, producer)) =
                setup(allocator_path, allocator_worker_id, queue_path)
            {
                tpu_to_pack(exit, non_vote_receiver, allocator, producer);
            }
        })
        .unwrap()
}

fn tpu_to_pack(
    exit: Arc<AtomicBool>,
    _non_vote_receiver: BankingPacketReceiver,
    _allocator: Allocator,
    _producer: shaq::Producer<TpuToPackMessage>,
) {
    while exit.load(Ordering::Relaxed) {}
}

fn setup(
    allocator_path: impl AsRef<Path>,
    allocator_worker_id: u32,
    queue_path: impl AsRef<Path>,
) -> Option<(Allocator, shaq::Producer<TpuToPackMessage>)> {
    let allocator = Allocator::join(allocator_path, allocator_worker_id)
        .map_err(|err| {
            error!("Failed to join allocator: {err:?}");
        })
        .ok()?;

    let producer = shaq::Producer::join(queue_path)
        .map_err(|err| {
            error!("Failed to join queue: {err:?}");
        })
        .ok()?;

    Some((allocator, producer))
}
