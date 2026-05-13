//! When block production is disabled, we still perform the Alpenglow / PoH handoff that
//! [`BlockCreationLoop`](crate::block_creation_loop::BlockCreationLoop) normally runs at
//! startup, then drain `LeaderWindowInfo` notifications so votor does not block.
//!
//! **Warning:** If this validator can become cluster leader, disabling block production will cause
//! missed leader windows and can halt or fork the cluster. Use only for observers that will not
//! be scheduled as leader.

use {
    crate::replay_stage::Finalizer,
    agave_votor::event::LeaderWindowInfo,
    crossbeam_channel::Receiver,
    solana_entry::block_component::GenesisCertificate,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_poh::{
        poh_recorder::{GRACE_TICKS_FACTOR, MAX_GRACE_SLOTS, PohRecorder},
        record_channels::RecordReceiver,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct BlockProductionDisabledLoop {
    thread: JoinHandle<()>,
}

pub struct BlockProductionDisabledLoopConfig {
    pub exit: Arc<AtomicBool>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
    pub cluster_info: Arc<ClusterInfo>,
    pub poh_recorder: Arc<RwLock<PohRecorder>>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub record_receiver_receiver: Receiver<RecordReceiver>,
    pub leader_window_info_receiver: Receiver<LeaderWindowInfo>,
}

impl BlockProductionDisabledLoop {
    pub fn new(config: BlockProductionDisabledLoopConfig) -> Self {
        let thread = Builder::new()
            .name("solBlkProdDis".to_string())
            .spawn(move || {
                info!("block production disabled: Alpenglow handoff shim started");
                start_disabled_loop(config);
                info!("block production disabled: Alpenglow handoff shim stopped");
            })
            .unwrap();
        Self { thread }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

struct DisabledLeaderContext {
    exit: Arc<AtomicBool>,
    my_pubkey: Pubkey,
    leader_window_info_receiver: Receiver<LeaderWindowInfo>,
    record_receiver: RecordReceiver,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    blockstore: Arc<Blockstore>,
    bank_forks: Arc<RwLock<BankForks>>,
    cluster_info: Arc<ClusterInfo>,
}

fn start_disabled_loop(config: BlockProductionDisabledLoopConfig) {
    let BlockProductionDisabledLoopConfig {
        exit,
        bank_forks,
        blockstore,
        cluster_info,
        poh_recorder,
        leader_schedule_cache,
        record_receiver_receiver,
        leader_window_info_receiver,
    } = config;

    let _exit_guard = Finalizer::new(exit.clone());

    let mut my_pubkey = cluster_info.id();
    info!("{my_pubkey}: block production disabled handoff initialized");

    let record_receiver = match record_receiver_receiver.recv() {
        Ok(receiver) => receiver,
        Err(e) => {
            info!("{my_pubkey}: record receiver closed before handoff: {e:?}");
            return;
        }
    };

    let genesis_cert = bank_forks
        .read()
        .unwrap()
        .migration_status()
        .genesis_certificate()
        .expect("Migration complete, genesis certificate must exist");
    let _genesis_cert = GenesisCertificate::try_from((*genesis_cert).clone())
        .expect("Genesis certificate must be valid");

    info!("{my_pubkey}: PoH service has shut down; block production remains disabled");

    let mut ctx = DisabledLeaderContext {
        exit: exit.clone(),
        my_pubkey,
        leader_window_info_receiver,
        record_receiver,
        poh_recorder: poh_recorder.clone(),
        leader_schedule_cache,
        blockstore,
        bank_forks,
        cluster_info,
    };

    {
        let mut w_poh_recorder = poh_recorder.write().unwrap();
        w_poh_recorder.enable_alpenglow();
    }
    reset_poh_recorder_disabled(&ctx.bank_forks.read().unwrap().working_bank(), &ctx);

    while !ctx.exit.load(Ordering::Relaxed) {
        if my_pubkey != ctx.cluster_info.id() {
            let my_old_pubkey = my_pubkey;
            my_pubkey = ctx.cluster_info.id();
            ctx.my_pubkey = my_pubkey;
            warn!(
                "Identity changed from {my_old_pubkey} to {my_pubkey} while block production \
                 disabled"
            );
        }

        let Some(latest) = ctx
            .leader_window_info_receiver
            .recv_timeout(Duration::from_secs(1))
            .ok()
            .and_then(|window| {
                ctx.leader_window_info_receiver
                    .try_iter()
                    .last()
                    .or(Some(window))
            })
        else {
            continue;
        };

        warn!(
            "{my_pubkey}: --disable-block-production: ignoring leader window {}-{} (do not use \
             this mode if this validator can become leader)",
            latest.start_slot, latest.end_slot
        );
    }
}

fn reset_poh_recorder_disabled(bank: &Arc<Bank>, ctx: &DisabledLeaderContext) {
    trace!("{}: resetting poh to {}", ctx.my_pubkey, bank.slot());
    assert!(
        ctx.record_receiver.is_shutdown() && ctx.record_receiver.is_safe_to_restart(),
        "record receiver must be safe for Alpenglow reset after PoH handoff"
    );
    let next_leader_slot = ctx.leader_schedule_cache.next_leader_slot(
        &ctx.my_pubkey,
        bank.slot(),
        bank,
        Some(ctx.blockstore.as_ref()),
        GRACE_TICKS_FACTOR * MAX_GRACE_SLOTS,
    );

    ctx.poh_recorder
        .write()
        .unwrap()
        .reset(bank.clone(), next_leader_slot);
}
