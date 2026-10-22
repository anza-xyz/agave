use {
    solana_clock::Epoch, solana_gossip::cluster_info::ClusterInfo, solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
};

pub(crate) struct StakedStatus {
    epoch: Epoch,
    identity: Pubkey,
    is_staked: bool,
}

impl StakedStatus {
    pub(crate) fn new(root_bank: &Bank, cluster_info: &ClusterInfo) -> Self {
        let epoch = root_bank.epoch();
        let identity = cluster_info.id();
        let is_staked = root_bank
            .epoch_staked_nodes(epoch)
            .expect("Root bank retains epoch_stakes for its own epoch")
            .get(&identity)
            .is_some_and(|stake| *stake > 0);
        Self {
            epoch,
            identity,
            is_staked,
        }
    }

    #[must_use]
    pub(super) fn is_staked(&mut self, root_bank: &Bank, cluster_info: &ClusterInfo) -> bool {
        let epoch = root_bank.epoch();
        let identity = cluster_info.id();
        if self.epoch != epoch || self.identity != identity {
            *self = Self::new(root_bank, cluster_info);
        }
        self.is_staked
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{consensus_pool_service::tests::TestContext, tests::get_cluster_info},
        solana_keypair::Keypair,
    };

    #[test]
    fn test_staked_status_refreshes_on_identity_change() {
        let mut ctx = TestContext::default();
        let root_bank = ctx.ctx.sharable_banks.root();

        let staked_cluster_info =
            get_cluster_info(ctx.validator_keypairs[1].node_keypair.insecure_clone());
        assert!(
            ctx.ctx
                .staked_status
                .is_staked(&root_bank, &staked_cluster_info)
        );
        assert_eq!(ctx.ctx.staked_status.identity, staked_cluster_info.id());

        let unstaked_cluster_info = get_cluster_info(Keypair::new());
        assert!(
            !ctx.ctx
                .staked_status
                .is_staked(&root_bank, &unstaked_cluster_info)
        );
        assert_eq!(ctx.ctx.staked_status.identity, unstaked_cluster_info.id());
    }

    #[test]
    fn test_staked_status_refreshes_on_epoch_change() {
        let ctx = TestContext::default();
        let root_bank = ctx.ctx.sharable_banks.root();
        let mut staked_status = ctx.ctx.staked_status;
        staked_status.epoch = root_bank.epoch().saturating_add(1);
        staked_status.is_staked = false;

        assert!(staked_status.is_staked(&root_bank, &ctx.ctx.cluster_info));
        assert_eq!(staked_status.epoch, root_bank.epoch());
    }
}
