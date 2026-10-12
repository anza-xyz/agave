use {
    crate::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
    solana_clock::Slot,
    solana_runtime::validated_block_finalization::ValidatedBlockFinalizationCert,
    std::sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum RpcHealthStatus {
    Ok,
    Behind { num_slots: Slot }, // Validator is behind its known validators
    Unknown,
}

pub struct RpcHealth {
    optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
    health_check_slot_distance: u64,
    override_health_check: Arc<AtomicBool>,
    #[cfg(test)]
    stub_health_status: std::sync::RwLock<Option<RpcHealthStatus>>,
}

impl RpcHealth {
    pub fn new(
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
        highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
        health_check_slot_distance: u64,
        override_health_check: Arc<AtomicBool>,
    ) -> Self {
        Self {
            optimistically_confirmed_bank,
            highest_finalized,
            health_check_slot_distance,
            override_health_check,
            #[cfg(test)]
            stub_health_status: std::sync::RwLock::new(None),
        }
    }

    fn highest_finalized_slot(&self) -> Option<Slot> {
        self.highest_finalized
            .read()
            .unwrap()
            .as_ref()
            .map(|cert| cert.block().slot)
    }

    pub fn check(&self) -> RpcHealthStatus {
        #[cfg(test)]
        {
            if let Some(stub_health_status) = *self.stub_health_status.read().unwrap() {
                return stub_health_status;
            }
        }

        if self.override_health_check.load(Ordering::Relaxed) {
            return RpcHealthStatus::Ok;
        }

        let my_latest_optimistically_confirmed_slot = self
            .optimistically_confirmed_bank
            .read()
            .unwrap()
            .bank
            .slot();

        // Votor may observe a finalization certificate before this node has replayed and rooted
        // the corresponding block.
        let Some(cluster_latest_slot) = self.highest_finalized_slot() else {
            warn!("health check: Votor has not observed a finalized slot");
            return RpcHealthStatus::Unknown;
        };

        if my_latest_optimistically_confirmed_slot
            >= cluster_latest_slot.saturating_sub(self.health_check_slot_distance)
        {
            RpcHealthStatus::Ok
        } else {
            let num_slots =
                cluster_latest_slot.saturating_sub(my_latest_optimistically_confirmed_slot);
            warn!(
                "health check: behind by {num_slots} slots: \
                 me={my_latest_optimistically_confirmed_slot}, latest \
                 cluster={cluster_latest_slot}",
            );
            RpcHealthStatus::Behind { num_slots }
        }
    }

    #[cfg(test)]
    pub(crate) fn stub(
        optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    ) -> Arc<Self> {
        Arc::new(Self::new(
            optimistically_confirmed_bank,
            Arc::default(),
            42,
            Arc::new(AtomicBool::new(false)),
        ))
    }

    #[cfg(test)]
    pub(crate) fn stub_set_health_status(&self, stub_health_status: Option<RpcHealthStatus>) {
        *self.stub_health_status.write().unwrap() = stub_health_status;
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        agave_votor_messages::{
            certificate::{CertSignature, FastFinalizeCert},
            consensus_message::Block,
        },
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_runtime::{
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
        },
    };

    #[test]
    fn test_get_health() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>> = Arc::default();
        let bank0 = bank_forks.read().unwrap().root_bank();
        assert!(bank0.slot() == 0);

        let health_check_slot_distance = 10;
        let override_health_check = Arc::new(AtomicBool::new(true));
        let health = RpcHealth::new(
            optimistically_confirmed_bank.clone(),
            highest_finalized.clone(),
            health_check_slot_distance,
            override_health_check.clone(),
        );

        // Override health check set to true - status is ok
        assert_eq!(health.check(), RpcHealthStatus::Ok);

        // Remove the override - status now unknown as no slots have been
        // optimistically confirmed yet
        override_health_check.store(false, Ordering::Relaxed);
        assert_eq!(health.check(), RpcHealthStatus::Unknown);

        // Votor's highest finalized slot determines the cluster tip.
        *highest_finalized.write().unwrap() =
            Some(ValidatedBlockFinalizationCert::from_validated_fast(
                FastFinalizeCert {
                    block: Block::new_unique(30),
                    signature: CertSignature {
                        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                        // Base2 encoding of an empty validator bitmap.
                        bitmap: vec![0, 0, 0],
                    },
                },
                &bank0,
            ));
        assert_eq!(health.check(), RpcHealthStatus::Behind { num_slots: 30 });

        let bank20 = Arc::new(Bank::new_from_parent(bank0, SlotLeader::default(), 20));
        optimistically_confirmed_bank.write().unwrap().bank = bank20.clone();
        assert_eq!(health.check(), RpcHealthStatus::Ok);

        // Being beyond the observed tip is also healthy.
        let bank31 = Arc::new(Bank::new_from_parent(bank20, SlotLeader::default(), 31));
        optimistically_confirmed_bank.write().unwrap().bank = bank31;
        assert_eq!(health.check(), RpcHealthStatus::Ok);
    }
}
