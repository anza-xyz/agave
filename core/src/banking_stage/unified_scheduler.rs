//! This module contains any integration code to put the unified scheduler subsystem into the
//! banking stage, starting from `BankingPacketBatch` ingestion from the sig verify stage and to
//! `Task` submission to the unified scheduler.
//!
//! These preprocessing for task creation can be multi-threaded trivially. At the same time, the
//! maximum cpu core utilization needs to be constrained among this processing and the actual task
//! handling of unified scheduler. Thus, it's desired to share a single thread pool for the two
//! kinds of work. Towards that end, the integration was implemented as a callback-style, which is
//! invoked (via `select!` on `banking_packet_receiver`) at each of unified scheduler handler
//! threads.
//!
//! Aside from some limited abstraction leakage to make `select!` work at the
//! solana-unified-scheduler-pool crate, almost all of these preprocessing are intentionally
//! encapsulated into this module, at the cost of dynamic dispatch per each BankingPacketBatch to
//! retain the unified scheduler agnostic scheduler over scheduling mode (block verification vs
//! block production) as much as possible.
//!
//! Lastly, what the callback closure in this module does is roughly as follows:
//! 1. Translate the raw packet bytes into some structs
//! 2. Do various sanitization on them
//! 3. Calculate priorities
//! 4. Convert them to tasks with the help of provided BankingStageHelper (remember that pubkey
//!    lookup for UsageQueue is also performed here; thus multi-threaded and off-loaded from the
//!    scheduler thread)
//! 5. Submit the tasks.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        packet_deserializer::PacketDeserializer,
        LikeClusterInfo,
    },
    crate::banking_trace::Channels,
    agave_banking_stage_ingress_types::BankingPacketBatch,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::{bank_forks::BankForks, root_bank_cache::RootBankCache},
    solana_unified_scheduler_pool::{BankingStageHelper, DefaultSchedulerPool},
    std::sync::{Arc, RwLock},
};

#[allow(dead_code)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn ensure_banking_stage_setup(
    pool: &DefaultSchedulerPool,
    bank_forks: &Arc<RwLock<BankForks>>,
    channels: &Channels,
    cluster_info: &impl LikeClusterInfo,
    poh_recorder: &Arc<RwLock<PohRecorder>>,
) {
    let mut root_bank_cache = RootBankCache::new(bank_forks.clone());
    let unified_receiver = channels.unified_receiver().clone();
    let mut decision_maker = DecisionMaker::new(cluster_info.id(), poh_recorder.clone());
    let transaction_recorder = poh_recorder.read().unwrap().new_recorder();

    let banking_packet_handler = Box::new(
        move |helper: &BankingStageHelper, batches: BankingPacketBatch| {
            let decision = decision_maker.make_consume_or_forward_decision();
            if matches!(decision, BufferedPacketsDecision::Forward) {
                return;
            }
            let bank = root_bank_cache.root_bank();
            for batch in batches.iter() {
                // over-provision nevertheless some of packets could be invalid.
                let task_id_base = helper.generate_task_ids(batch.len());
                let packets = PacketDeserializer::deserialize_packets_with_indexes(batch);

                for (packet, packet_index) in packets {
                    let Some((transaction, _deactivation_slot)) = packet
                        .build_sanitized_transaction(
                            bank.vote_only_bank(),
                            &bank,
                            bank.get_reserved_account_keys(),
                        )
                    else {
                        continue;
                    };

                    let index = task_id_base + packet_index;

                    let task = helper.create_new_task(transaction, index);
                    helper.send_new_task(task);
                }
            }
        },
    );

    pool.register_banking_stage(
        unified_receiver,
        banking_packet_handler,
        transaction_recorder,
    );
}
