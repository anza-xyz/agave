use {
    super::decision_maker::{BufferedPacketsDecision, DecisionMaker},
    crate::banking_stage::packet_deserializer::PacketDeserializer,
    solana_perf::packet::BankingPacketReceiver,
    solana_runtime::bank_forks::BankForks,
    solana_unified_scheduler_pool::{BankingStageAdapter, BatchConverterCreator},
    std::sync::{Arc, RwLock},
};

pub(crate) fn unified_receiver(
    non_vote_receiver: BankingPacketReceiver,
    tpu_vote_receiver: BankingPacketReceiver,
    gossip_vote_receiver: BankingPacketReceiver,
) -> BankingPacketReceiver {
    assert!(non_vote_receiver.same_channel(&tpu_vote_receiver));
    assert!(non_vote_receiver.same_channel(&gossip_vote_receiver));
    drop((tpu_vote_receiver, gossip_vote_receiver));

    non_vote_receiver
}

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
