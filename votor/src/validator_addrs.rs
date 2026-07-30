use {
    crate::voting_service::AlpenglowPortOverride,
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        cmp,
        collections::HashSet,
        net::SocketAddr,
        sync::Arc,
        time::{Duration, Instant},
    },
};

const VALIDATORS_REFRESH_TIME: Duration = Duration::from_secs(30);

/// Struct to maintain `SockAddr` for peer validators to send them consensus msgs.
pub(crate) struct ValidatorAddrs {
    sharable_banks: SharableBanks,
    cluster_info: Arc<ClusterInfo>,
    /// Used for test purposes.
    testing_override: Option<AlpenglowPortOverride>,
    /// To support epoch rollover, stores state for 2 different epochs.
    epoch_states: [EpochState; 2],
}

impl ValidatorAddrs {
    pub(crate) fn new(
        sharable_banks: SharableBanks,
        cluster_info: Arc<ClusterInfo>,
        testing_override: Option<AlpenglowPortOverride>,
    ) -> Self {
        let epoch1 = sharable_banks.root().epoch();
        let epoch0 = epoch1.saturating_sub(1);
        let state1 = EpochState::new(
            epoch1,
            &sharable_banks,
            &cluster_info,
            testing_override.clone(),
        );
        let state0 = EpochState::new(
            epoch0,
            &sharable_banks,
            &cluster_info,
            testing_override.clone(),
        );
        Self {
            sharable_banks,
            cluster_info,
            testing_override,
            epoch_states: [state0, state1],
        }
    }

    /// Returns a list of `SockAddr` to peer validators.  Higher stake validators appear earlier.
    pub(crate) fn get_validators(&mut self, requested_slot: Slot) -> &[SocketAddr] {
        let requested_epoch = self
            .sharable_banks
            .root()
            .epoch_schedule()
            .get_epoch(requested_slot);
        if let Some(index) = self
            .epoch_states
            .iter()
            .position(|s| s.epoch == requested_epoch)
        {
            return self.epoch_states[index]
                .get_validators(&self.sharable_banks, &self.cluster_info);
        }
        assert!(requested_epoch > self.epoch_states[1].epoch);
        self.epoch_states.rotate_left(1);
        self.epoch_states[1] = EpochState::new(
            requested_epoch,
            &self.sharable_banks,
            &self.cluster_info,
            self.testing_override.clone(),
        );
        self.epoch_states[1].get_validators(&self.sharable_banks, &self.cluster_info)
    }
}

/// Per epoch state.  Stores the list of `SocketAddr`s and refreshes the list periodically.
struct EpochState {
    epoch: Epoch,
    validators: Vec<SocketAddr>,
    last_refresh: Instant,
    /// To support testing.
    override_state: Option<OverrideState>,
}

impl EpochState {
    fn new(
        epoch: Epoch,
        sharable_banks: &SharableBanks,
        cluster_info: &ClusterInfo,
        testing_override: Option<AlpenglowPortOverride>,
    ) -> Self {
        let validators = refresh(
            epoch,
            sharable_banks,
            cluster_info,
            testing_override.as_ref(),
        );
        let override_state = testing_override.map(|port| OverrideState {
            last_modified: port.last_modified(),
            port,
        });
        Self {
            epoch,
            validators,
            last_refresh: Instant::now(),
            override_state,
        }
    }

    fn get_validators(
        &mut self,
        sharable_banks: &SharableBanks,
        cluster_info: &ClusterInfo,
    ) -> &[SocketAddr] {
        if self.last_refresh.elapsed() > VALIDATORS_REFRESH_TIME
            || self
                .override_state
                .as_ref()
                .map(|s| s.should_refresh())
                .unwrap_or(false)
        {
            self.validators = refresh(
                self.epoch,
                sharable_banks,
                cluster_info,
                self.override_state.as_ref().map(|s| &s.port),
            );
            self.last_refresh = Instant::now();
            if let Some(s) = self.override_state.as_mut() {
                s.refreshed();
            }
        }
        &self.validators
    }
}

/// Returns a refreshed list of `SockAddr`s to peer validators.  Higher staked validators appear earlier.
fn refresh(
    epoch: Epoch,
    sharable_banks: &SharableBanks,
    cluster_info: &ClusterInfo,
    testing_override: Option<&AlpenglowPortOverride>,
) -> Vec<SocketAddr> {
    let root_bank = sharable_banks.root();
    let staked_nodes = match root_bank.epoch_staked_nodes(epoch) {
        Some(r) => r,
        None => {
            let working_bank = sharable_banks.working();
            let Some(res) = working_bank.epoch_staked_nodes(epoch) else {
                return vec![];
            };
            res
        }
    };
    let override_map = testing_override.as_ref().map(|o| o.get_override_map());

    struct Node {
        stake: u64,
        socket: SocketAddr,
    }

    let mut nodes = staked_nodes
        .iter()
        .filter(|(pubkey, _)| pubkey != &&cluster_info.id())
        .filter_map(|(pubkey, stake)| {
            cluster_info.lookup_contact_info(pubkey, |node| {
                node.alpenglow().map(|socket| {
                    let socket = override_map
                        .as_ref()
                        .map(|m| m.get(pubkey).cloned().unwrap_or(socket))
                        .unwrap_or(socket);
                    Node {
                        stake: *stake,
                        socket,
                    }
                })
            })?
        })
        .collect::<Vec<_>>();
    nodes.sort_unstable_by_key(|a| cmp::Reverse(a.stake));
    let mut sockets = HashSet::with_capacity(nodes.len());
    nodes.retain(|node| sockets.insert(node.socket));
    nodes.into_iter().map(|n| n.socket).collect()
}

struct OverrideState {
    last_modified: Instant,
    port: AlpenglowPortOverride,
}

impl OverrideState {
    #[must_use]
    fn should_refresh(&self) -> bool {
        self.port.last_modified() != self.last_modified
    }

    fn refreshed(&mut self) {
        self.last_modified = self.port.last_modified();
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::tests::get_cluster_info,
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
    };

    fn socket(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn setup(sockets: &[SocketAddr]) -> (SharableBanks, Arc<ClusterInfo>, Vec<Pubkey>) {
        let validator_keypairs = (0..sockets.len())
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            100_000_000_000,
            &validator_keypairs,
            vec![10_000_000_000; validator_keypairs.len()],
        );
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let cluster_info = get_cluster_info(Keypair::new());
        let validator_pubkeys = validator_keypairs
            .iter()
            .zip(sockets)
            .map(|(keypairs, socket)| {
                let pubkey = keypairs.node_keypair.pubkey();
                let mut contact_info = ContactInfo::new_localhost(&pubkey, 0);
                contact_info.set_alpenglow(*socket).unwrap();
                cluster_info.insert_info(contact_info);
                pubkey
            })
            .collect();
        (sharable_banks, cluster_info, validator_pubkeys)
    }

    fn epochs(validator_addrs: &ValidatorAddrs) -> [Epoch; 2] {
        std::array::from_fn(|index| validator_addrs.epoch_states[index].epoch)
    }

    #[test]
    fn test_epoch_states_roll_forward() {
        let (sharable_banks, cluster_info, _) = setup(&[socket(10_001)]);
        let epoch_schedule = sharable_banks.root().epoch_schedule().clone();
        let mut validator_addrs = ValidatorAddrs::new(sharable_banks, cluster_info, None);
        assert_eq!(epochs(&validator_addrs), [0, 0]);

        validator_addrs.get_validators(epoch_schedule.get_first_slot_in_epoch(1));
        assert_eq!(epochs(&validator_addrs), [0, 1]);

        validator_addrs.get_validators(epoch_schedule.get_first_slot_in_epoch(0));
        assert_eq!(epochs(&validator_addrs), [0, 1]);

        validator_addrs.get_validators(epoch_schedule.get_first_slot_in_epoch(2));
        assert_eq!(epochs(&validator_addrs), [1, 2]);
    }
}
