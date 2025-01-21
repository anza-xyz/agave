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
    solana_runtime::bank_forks::BankForks,
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
    let bank_forks = bank_forks.clone();
    let unified_receiver = channels.unified_receiver().clone();
    let decision_maker = DecisionMaker::new(cluster_info.id(), poh_recorder.clone());
    let transaction_recorder = poh_recorder.read().unwrap().new_recorder();

    let banking_packet_handler = Box::new(
        move |helper: &BankingStageHelper, batches: BankingPacketBatch| {
            let decision = decision_maker.make_consume_or_forward_decision();
            if matches!(decision, BufferedPacketsDecision::Forward) {
                return;
            }
            let bank = bank_forks.read().unwrap().root_bank();
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
