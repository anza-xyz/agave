//! Module responsible for updating the staked key map.
//! Adapted from jito-replayer code.

use {
    crate::load_balancer::LoadBalancer,
    log::warn,
    solana_client::client_error,
    solana_sdk::pubkey::Pubkey,
    solana_streamer::streamer::StakedNodes,
    std::{
        collections::HashMap,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const PK_TO_STAKE_REFRESH_DURATION: Duration = Duration::from_secs(5);

pub struct StakeUpdater {
    thread_hdl: JoinHandle<()>,
}

impl StakeUpdater {
    pub fn new(
        exit: Arc<AtomicBool>,
        rpc_load_balancer: Arc<LoadBalancer>,
        shared_staked_nodes: Arc<RwLock<StakedNodes>>,
        staked_nodes_overrides: HashMap<Pubkey, u64>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("stkUpdtr".to_string())
            .spawn(move || {
                let mut last_stakes = Instant::now();
                while !exit.load(Ordering::Relaxed) {
                    let mut stake_map = Arc::new(HashMap::new());
                    match Self::try_refresh_pk_to_stake(
                        &mut last_stakes,
                        &mut stake_map,
                        &rpc_load_balancer,
                    ) {
                        Ok(true) => {
                            let shared =
                                StakedNodes::new(stake_map, staked_nodes_overrides.clone());
                            *shared_staked_nodes.write().unwrap() = shared;
                        }
                        Err(err) => {
                            warn!("Failed to refresh pk to stake map! Error: {:?}", err);
                            sleep(PK_TO_STAKE_REFRESH_DURATION);
                        }
                        _ => {}
                    }
                }
            })
            .unwrap();

        Self { thread_hdl }
    }

    fn try_refresh_pk_to_stake(
        last_stakes: &mut Instant,
        pubkey_stake_map: &mut Arc<HashMap<Pubkey, u64>>,
        rpc_load_balancer: &Arc<LoadBalancer>,
    ) -> client_error::Result<bool> {
        if last_stakes.elapsed() > PK_TO_STAKE_REFRESH_DURATION {
            let client = rpc_load_balancer.rpc_client();
            let vote_accounts = client.get_vote_accounts()?;

            *pubkey_stake_map = Arc::new(
                vote_accounts
                    .current
                    .iter()
                    .chain(vote_accounts.delinquent.iter())
                    .filter_map(|vote_account| {
                        Some((
                            Pubkey::from_str(&vote_account.node_pubkey).ok()?,
                            vote_account.activated_stake,
                        ))
                    })
                    .collect(),
            );

            *last_stakes = Instant::now();
            Ok(true)
        } else {
            sleep(Duration::from_secs(1));
            Ok(false)
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
