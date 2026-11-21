use {
    futures_util::{SinkExt, StreamExt},
    log::*,
    reqwest::{self, header::CONTENT_TYPE},
    serde_json::{Value, json},
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_net_utils::{SocketAddrSpace, sockets},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        client_error::{ErrorKind as ClientErrorKind, Result as ClientResult},
        config::RpcSimulateTransactionConfig,
        request::RpcError,
    },
    solana_signer::Signer,
    solana_system_transaction as system_transaction,
    solana_test_validator::TestValidator,
    solana_tpu_client_next::{
        client_builder::ClientBuilder, leader_updater::create_pinned_leader_updater,
        node_address_service::LeaderTpuCacheServiceConfig,
        websocket_node_address_service::WebsocketNodeAddressService,
    },
    solana_transaction_status::TransactionStatus,
    std::{
        sync::Arc,
        thread::sleep,
        time::{Duration, Instant},
    },
    tokio::runtime::{Builder, Runtime},
    tokio_util::sync::CancellationToken,
};

macro_rules! json_req {
    ($method: expr, $params: expr) => {{
        json!({
           "jsonrpc": "2.0",
           "id": 1,
           "method": $method,
           "params": $params,
        })
    }}
}

fn post_rpc(request: Value, rpc_url: &str) -> Value {
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(rpc_url)
        .header(CONTENT_TYPE, "application/json")
        .body(request.to_string())
        .send()
        .unwrap();
    serde_json::from_str(&response.text().unwrap()).unwrap()
}

#[test]
fn test_rpc_send_tx() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);
    let rpc_url = test_validator.rpc_url();

    let bob_pubkey = solana_pubkey::new_rand();

    let req = json_req!("getLatestBlockhash", json!([]));
    let json = post_rpc(req, &rpc_url);

    let blockhash: Hash = json["result"]["value"]["blockhash"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    info!("blockhash: {blockhash:?}");
    let tx = system_transaction::transfer(
        &alice,
        &bob_pubkey,
        Rent::default().minimum_balance(0),
        blockhash,
    );
    let serialized_encoded_tx = bs58::encode(wincode::serialize(&tx).unwrap()).into_string();

    let req = json_req!("sendTransaction", json!([serialized_encoded_tx]));
    let json: Value = post_rpc(req, &rpc_url);

    let signature = &json["result"];

    let mut confirmed_tx = false;

    let request = json_req!("getSignatureStatuses", [[signature]]);

    for _ in 0..solana_clock::DEFAULT_TICKS_PER_SLOT {
        let json = post_rpc(request.clone(), &rpc_url);

        let result: Option<TransactionStatus> =
            serde_json::from_value(json["result"]["value"][0].clone()).unwrap();
        if let Some(result) = result.as_ref()
            && result.err.is_none()
        {
            confirmed_tx = true;
            break;
        }

        sleep(Duration::from_millis(500));
    }

    assert!(confirmed_tx);

    use {
        solana_account_decoder::UiAccountEncoding,
        solana_rpc_client_api::config::RpcAccountInfoConfig,
    };
    let config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: None,
        data_slice: None,
        min_context_slot: None,
    };
    let req = json_req!(
        "getAccountInfo",
        json!([bs58::encode(bob_pubkey).into_string(), config])
    );
    let json: Value = post_rpc(req, &rpc_url);
    info!("{:?}", json["result"]["value"]);
}

#[test]
fn test_simulation_replaced_blockhash() -> ClientResult<()> {
    agave_logger::setup();

    let alice = Keypair::new();
    let validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);
    let rpc_client = RpcClient::new(validator.rpc_url());

    let bob = Keypair::new();
    let lamports = 50;

    let res = rpc_client.simulate_transaction_with_config(
        &system_transaction::transfer(&alice, &bob.pubkey(), lamports, Hash::default()),
        RpcSimulateTransactionConfig {
            replace_recent_blockhash: true,
            ..Default::default()
        },
    )?;
    assert!(
        res.value.replacement_blockhash.is_some(),
        "replaced_blockhash response is None"
    );
    let blockhash = res.value.replacement_blockhash.unwrap();
    // ensure nothing weird is going on
    assert_ne!(
        blockhash.blockhash,
        Hash::default().to_string(),
        "replaced_blockhash is default"
    );

    let res = rpc_client.simulate_transaction_with_config(
        &system_transaction::transfer(&alice, &bob.pubkey(), lamports, Hash::default()),
        RpcSimulateTransactionConfig {
            replace_recent_blockhash: false,
            ..Default::default()
        },
    )?;
    assert!(
        res.value.replacement_blockhash.is_none(),
        "replaced_blockhash is Some when nothing should be replaced"
    );

    Ok(())
}

#[test]
fn test_rpc_invalid_requests() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);
    let rpc_url = test_validator.rpc_url();

    let bob_pubkey = solana_pubkey::new_rand();

    // test invalid get_balance request
    let req = json_req!("getBalance", json!(["invalid9999"]));
    let json = post_rpc(req, &rpc_url);

    let the_error = json["error"]["message"].as_str().unwrap();
    assert_eq!(the_error, "Invalid param: Invalid");

    // test invalid get_account_info request
    let req = json_req!("getAccountInfo", json!(["invalid9999"]));
    let json = post_rpc(req, &rpc_url);

    let the_error = json["error"]["message"].as_str().unwrap();
    assert_eq!(the_error, "Invalid param: Invalid");

    // test invalid get_account_info request
    let req = json_req!("getAccountInfo", json!([bob_pubkey.to_string()]));
    let json = post_rpc(req, &rpc_url);

    let the_value = &json["result"]["value"];
    assert!(the_value.is_null());
}

#[test]
fn test_run_tpu_send_transaction() {
    let mint_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();
    let test_validator =
        TestValidator::start_with_config(mint_pubkey, None, SocketAddrSpace::Unspecified);
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        test_validator.rpc_url(),
        CommitmentConfig::processed(),
    ));

    let recent_blockhash = rpc_client.get_latest_blockhash().unwrap();
    let tx =
        system_transaction::transfer(&mint_keypair, &Pubkey::new_unique(), 42, recent_blockhash);

    // Send transaction using tpu-client-next
    let rt = Runtime::new().unwrap();
    let bind_socket = sockets::bind_to_localhost_unique().unwrap();
    let tpu_address = *test_validator.tpu_quic();

    let leader_updater = create_pinned_leader_updater(tpu_address);

    let (transaction_sender, _client) = rt.block_on(async {
        ClientBuilder::new(leader_updater)
            .bind_socket(bind_socket)
            .identity(&mint_keypair)
            .build()
            .expect("Failed to build TPU client")
    });

    let tx_bytes = wincode::serialize(&tx).unwrap();
    rt.block_on(async {
        let _ = transaction_sender.send_transaction(tx_bytes).await;
    });

    let timeout = Duration::from_secs(5);
    let now = Instant::now();
    let signatures = vec![tx.signatures[0]];
    loop {
        assert!(now.elapsed() < timeout);
        let statuses = rpc_client.get_signature_statuses(&signatures).unwrap();
        if !statuses.value.is_empty() {
            return;
        }
    }
}

#[test]
fn test_node_address_service_slot_updates() {
    agave_logger::setup();

    let test_validator =
        TestValidator::start_with_config(Pubkey::new_unique(), None, SocketAddrSpace::Unspecified);

    let rt = Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();
    rt.block_on(async {
        let rpc_client = Arc::new(test_validator.get_async_rpc_client());
        let mut slot = rpc_client.get_slot().await.unwrap();
        // Stand in for an external subscription provider while HTTP requests still
        // go to the local validator.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "slotsUpdatesSubscribe");
            socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"jsonrpc":"2.0","id":request["id"],"result":42}).to_string().into())).await.unwrap();
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        slot = slot.checked_add(1).unwrap();
                        socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"jsonrpc":"2.0","method":"slotsUpdatesNotification","params":{"subscription":42,"result":{"type":"completed","slot":slot,"timestamp":0}}}).to_string().into())).await.unwrap();
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                let request: Value = serde_json::from_str(&text).unwrap();
                                assert_eq!(request["method"], "slotsUpdatesUnsubscribe");
                                assert_eq!(request["params"], json!([42]));
                                socket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"jsonrpc":"2.0","id":request["id"],"result":true}).to_string().into())).await.unwrap();
                            }
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                            other => panic!("unexpected WebSocket message: {other:?}"),
                        }
                    }
                }
            }
        });
        let cancel = CancellationToken::new();
        let (provider, service) = WebsocketNodeAddressService::run(
            rpc_client,
            url,
            LeaderTpuCacheServiceConfig::default(),
            cancel.clone(),
        )
        .await
        .expect("WebsocketNodeAddressService should start");

        let start_slot = provider.estimated_current_slot();
        let timeout = Duration::from_secs(5);
        let now = Instant::now();
        loop {
            assert!(
                now.elapsed() < timeout,
                "estimated slot did not advance within {timeout:?}"
            );
            if provider.estimated_current_slot() != start_slot {
                break;
            }
            tokio::time::sleep(Duration::from_millis(solana_clock::DEFAULT_MS_PER_SLOT)).await;
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), service.shutdown()).await.unwrap().expect("clean shutdown");
        peer.await.unwrap();
    });
}

#[test]
fn deserialize_rpc_error() -> ClientResult<()> {
    agave_logger::setup();

    let alice = Keypair::new();
    let validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);
    let rpc_client = RpcClient::new(validator.rpc_url());

    let bob = Keypair::new();
    let lamports = 50;
    let blockhash = rpc_client.get_latest_blockhash()?;
    let mut tx = system_transaction::transfer(&alice, &bob.pubkey(), lamports, blockhash);

    // This will cause an error
    tx.signatures.clear();

    let err = rpc_client.send_transaction(&tx);
    let err = err.unwrap_err();

    match err.kind() {
        ClientErrorKind::RpcError(RpcError::RpcRequestError { .. }) => {
            // This is what used to happen
            panic!()
        }
        ClientErrorKind::RpcError(RpcError::RpcResponseError { .. }) => Ok(()),
        _ => {
            panic!()
        }
    }
}
