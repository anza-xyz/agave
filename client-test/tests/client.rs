use {
    futures_util::StreamExt,
    serde_json::{Value, json},
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_pubsub_client::{nonblocking, pubsub_client::PubsubClient},
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcBlockSubscribeFilter, RpcProgramAccountsConfig},
        response::SlotInfo,
    },
    solana_signer::Signer,
    solana_system_interface::program as system_program,
    solana_system_transaction as system_transaction,
    solana_test_validator::TestValidator,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        net::TcpListener,
        thread::{self, JoinHandle, sleep},
        time::{Duration, Instant},
    },
};

// Client protocol tests use a peer fixture. Agave no longer serves subscriptions.
fn subscription_peer(
    method: &'static str,
    params: Value,
    results: Vec<Value>,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let thread = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let request: Value =
            serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["method"], format!("{method}Subscribe"));
        assert_eq!(request["params"], params);
        socket
            .send(tungstenite::Message::Text(
                json!({"jsonrpc":"2.0", "id":request["id"], "result":42})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        for result in results {
            socket.send(tungstenite::Message::Text(json!({"jsonrpc":"2.0", "method":format!("{method}Notification"), "params":{"subscription":42,"result":result}}).to_string().into())).unwrap();
        }
        socket.close(None).unwrap();
    });
    (url, thread)
}

fn account_result() -> Value {
    json!({"context":{"slot":1},"value":{"owner":system_program::id().to_string(),"lamports":100,"data":"","executable":false,"rentEpoch":u64::MAX,"space":0}})
}

#[test]
fn test_rpc_client() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);

    let bob_pubkey = solana_pubkey::new_rand();

    let client = RpcClient::new(test_validator.rpc_url());

    assert_eq!(
        client.get_version().unwrap().solana_core,
        solana_version::semver!()
    );

    assert!(client.get_account(&bob_pubkey).is_err());

    assert_eq!(client.get_balance(&bob_pubkey).unwrap(), 0);

    let original_alice_balance = client.get_balance(&alice.pubkey()).unwrap();

    let blockhash = client.get_latest_blockhash().unwrap();

    let tx = system_transaction::transfer(&alice, &bob_pubkey, 20 * LAMPORTS_PER_SOL, blockhash);
    let tx = VersionedTransaction::from(tx);
    let fee = client.get_fee_for_versioned_message(&tx.message).unwrap();
    let signature = client.send_transaction(&tx).unwrap();

    let mut confirmed_tx = false;

    let now = Instant::now();
    while now.elapsed().as_secs() <= 20 {
        let response = client
            .confirm_transaction_with_commitment(&signature, CommitmentConfig::processed())
            .unwrap();

        if response.value {
            confirmed_tx = true;
            break;
        }

        sleep(Duration::from_millis(500));
    }

    assert!(confirmed_tx);

    assert_eq!(
        client
            .get_balance_with_commitment(&bob_pubkey, CommitmentConfig::processed())
            .unwrap()
            .value,
        20 * LAMPORTS_PER_SOL
    );
    assert_eq!(
        client
            .get_balance_with_commitment(&alice.pubkey(), CommitmentConfig::processed())
            .unwrap()
            .value,
        original_alice_balance - 20 * LAMPORTS_PER_SOL - fee
    );
}

#[test]
fn test_account_subscription() {
    let pubkey = Pubkey::new_unique();
    let config = RpcAccountInfoConfig {
        commitment: Some(CommitmentConfig::finalized()),
        ..Default::default()
    };
    let expected = account_result();
    let (url, server) = subscription_peer(
        "account",
        json!([pubkey.to_string(), config]),
        vec![expected.clone()],
    );
    let (mut client, receiver) =
        PubsubClient::account_subscribe(url, &pubkey, Some(config)).unwrap();
    assert_eq!(
        serde_json::to_value(receiver.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap(),
        expected
    );
    client.shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn test_block_subscription() {
    let expected = json!({"context":{"slot":12}, "value":{"slot":12,"block":{"blockhash":"hash","previousBlockhash":"parent","parentSlot":11,"signatures":["signature"]},"err":null}});
    let (url, server) = subscription_peer("block", json!(["all", null]), vec![expected.clone()]);
    let (mut client, receiver) =
        PubsubClient::block_subscribe(url, RpcBlockSubscribeFilter::All, None).unwrap();
    let actual = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(actual.context.slot, 12);
    assert_eq!(actual.value.slot, 12);
    let block = actual.value.block.unwrap();
    assert_eq!(block.parent_slot, 11);
    assert_eq!(block.blockhash, "hash");
    assert_eq!(block.signatures.unwrap(), vec!["signature"]);
    assert!(actual.value.err.is_none());
    client.shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn test_program_subscription() {
    let pubkey = Pubkey::new_unique();
    let account = account_result();
    let expected = json!({"context":account["context"],"value":{"pubkey":pubkey.to_string(),"account":account["value"]}});
    let config = RpcProgramAccountsConfig::default();
    let (url, server) = subscription_peer(
        "program",
        json!([system_program::id().to_string(), config]),
        vec![expected.clone()],
    );
    let (mut client, receiver) =
        PubsubClient::program_subscribe(url, &system_program::id(), Some(config)).unwrap();
    assert_eq!(
        serde_json::to_value(receiver.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap(),
        expected
    );
    client.shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn test_root_subscription() {
    let (url, server) = subscription_peer("root", Value::Null, vec![json!(1), json!(2)]);
    let (mut client, receiver) = PubsubClient::root_subscribe(url).unwrap();
    for slot in [1, 2] {
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), slot);
    }
    client.shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn test_slot_subscription() {
    let slots = [
        SlotInfo {
            slot: 1,
            parent: 0,
            root: 0,
        },
        SlotInfo {
            slot: 2,
            parent: 1,
            root: 1,
        },
    ];
    let (url, server) = subscription_peer(
        "slot",
        json!([]),
        slots
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect(),
    );
    let (mut client, receiver) = PubsubClient::slot_subscribe(url).unwrap();
    for slot in slots {
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), slot);
    }
    client.shutdown().unwrap();
    server.join().unwrap();
}

#[tokio::test]
async fn test_slot_subscription_async() {
    let slots = [
        SlotInfo {
            slot: 1,
            parent: 0,
            root: 0,
        },
        SlotInfo {
            slot: 2,
            parent: 1,
            root: 1,
        },
    ];
    let (url, server) = subscription_peer(
        "slot",
        json!([]),
        slots
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect(),
    );
    let client = nonblocking::pubsub_client::PubsubClient::new(url)
        .await
        .unwrap();
    let (mut notifications, unsubscribe) = client.slot_subscribe().await.unwrap();
    for slot in slots {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), notifications.next())
                .await
                .unwrap(),
            Some(slot)
        );
    }
    unsubscribe().await;
    drop(notifications);
    client.shutdown().await.unwrap();
    server.join().unwrap();
}
