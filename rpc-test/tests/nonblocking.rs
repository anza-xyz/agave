use {
    async_trait::async_trait,
    solana_keypair::Keypair,
    solana_net_utils::sockets,
    solana_pubkey::Pubkey,
    solana_system_transaction as system_transaction,
    solana_test_validator::TestValidatorGenesis,
    solana_tpu_client_next::{
        client_builder::ClientBuilder, connection_workers_scheduler::NonblockingBroadcaster,
        leader_updater::LeaderUpdater,
    },
    std::{net::SocketAddr, sync::Arc},
    tokio::time::{sleep, Duration, Instant},
};

struct TestLeaderUpdater {
    address: SocketAddr,
}

#[async_trait]
impl LeaderUpdater for TestLeaderUpdater {
    fn next_leaders(&mut self, _lookahead_leaders: usize) -> Vec<SocketAddr> {
        vec![self.address]
    }

    async fn stop(&mut self) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_leader_updater_works() {
    let (test_validator, _) = TestValidatorGenesis::default().start_async().await;
    let tpu_address = *test_validator.tpu_quic();

    let mut leader_updater = TestLeaderUpdater {
        address: tpu_address,
    };

    let leaders = leader_updater.next_leaders(1);
    assert!(!leaders.is_empty());
    assert_eq!(leaders[0], tpu_address);

    leader_updater.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tpu_client_send_transaction() {
    let alice = Keypair::new();
    let (test_validator, _) = TestValidatorGenesis::default().start_async().await;

    let rpc_client = Arc::new(test_validator.get_async_rpc_client());
    let tpu_address = *test_validator.tpu_quic();

    let leader_updater = TestLeaderUpdater {
        address: tpu_address,
    };

    let bind_socket = sockets::bind_to_localhost_unique().unwrap();

    let (transaction_sender, client) = ClientBuilder::new(Box::new(leader_updater))
        .bind_socket(bind_socket)
        .identity(&alice)
        .leader_send_fanout(1)
        .build::<NonblockingBroadcaster>()
        .expect("Failed to build TPU client");

    let recent_blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    let tx = system_transaction::transfer(&alice, &Pubkey::new_unique(), 1000, recent_blockhash);

    let wire_tx = bincode::serialize(&tx).unwrap();
    transaction_sender
        .send_transactions_in_batch(vec![wire_tx])
        .await
        .unwrap();

    let timeout = Duration::from_secs(5);
    let now = Instant::now();
    let signatures = vec![tx.signatures[0]];
    loop {
        assert!(now.elapsed() < timeout);
        let statuses = rpc_client
            .get_signature_statuses(&signatures)
            .await
            .unwrap();
        if !statuses.value.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _ = client.shutdown().await;
}
