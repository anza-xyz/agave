#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::decision_maker::{BufferedPacketsDecision, DecisionMaker},
    crate::{
        banking_stage::{packet_deserializer::PacketDeserializer, BankingStage, LikeClusterInfo},
        banking_trace::Channels,
    },
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::bank_forks::BankForks,
    solana_unified_scheduler_pool::{
        BankingStageAdapter, BatchConverterCreator, DefaultSchedulerPool,
    },
    std::sync::{Arc, RwLock},
};

pub(crate) fn batch_converter_creator(
    decision_maker: DecisionMaker,
    bank_forks: Arc<RwLock<BankForks>>,
) -> BatchConverterCreator {
    Box::new(move |adapter: Arc<BankingStageAdapter>| {
        let decision_maker = decision_maker.clone();
        let bank_forks = bank_forks.clone();

        Box::new(move |batches, task_submitter| {
            let decision = decision_maker.make_consume_or_forward_decision();
            if matches!(decision, BufferedPacketsDecision::Forward) {
                return;
            }
            let bank = bank_forks.read().unwrap().root_bank();
            for batch in batches.iter() {
                // over-provision nevertheless some of packets could be invalid.
                let task_id_base = adapter.generate_task_ids(batch.len());
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

                    let task = adapter.create_new_task(transaction, index);
                    task_submitter(task);
                }
            }
        })
    })
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn ensure_banking_stage_setup(
    pool: &DefaultSchedulerPool,
    bank_forks: &Arc<RwLock<BankForks>>,
    channels: &Channels,
    cluster_info: &impl LikeClusterInfo,
    poh_recorder: &Arc<RwLock<PohRecorder>>,
) {
    if !pool.block_production_supported() {
        return;
    }

    let unified_receiver = channels.unified_receiver().clone();
    let block_producing_scheduler_handler_threads = BankingStage::num_threads() as usize;
    let decision_maker = DecisionMaker::new(cluster_info.id(), poh_recorder.clone());
    let banking_stage_monitor = Box::new(decision_maker.clone());
    let converter = batch_converter_creator(decision_maker, bank_forks.clone());

    pool.register_banking_stage(
        unified_receiver,
        block_producing_scheduler_handler_threads,
        banking_stage_monitor,
        converter,
    );
}
