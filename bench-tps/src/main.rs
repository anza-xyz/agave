#![allow(clippy::arithmetic_side_effects)]
use {
    log::*,
    solana_bench_tps::{
        bench::{do_bench_tps, max_lamports_for_prioritization},
        cli::{self, ExternalClientType},
        keypairs::get_keypairs,
        send_batch::{generate_durable_nonce_accounts, generate_keypairs},
    },
    solana_commitment_config::CommitmentConfig,
    solana_fee_calculator::FeeRateGovernor,
    solana_genesis::Base64Account,
    solana_keypair::Keypair,
    solana_rpc_client::rpc_client::RpcClient,
    solana_system_interface::program as system_program,
    solana_tps_client::{TpsClient, TpuClientNextClient},
    std::{
        collections::HashMap,
        fs::File,
        io::prelude::*,
        net::IpAddr,
        path::Path,
        process::exit,
        sync::Arc,
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// Number of signatures for all transactions in ~1 week at ~100K TPS
pub const NUM_SIGNATURES_FOR_TXS: u64 = 100_000 * 60 * 60 * 24 * 7;

#[allow(clippy::too_many_arguments)]
fn create_client(
    external_client_type: &ExternalClientType,
    json_rpc_url: &str,
    websocket_url: &str,
    bind_address: IpAddr,
    client_node_id: Option<&Keypair>,
    commitment_config: CommitmentConfig,
) -> Arc<dyn TpsClient + Send + Sync> {
    match external_client_type {
        ExternalClientType::RpcClient => Arc::new(RpcClient::new_with_commitment(
            json_rpc_url.to_string(),
            commitment_config,
        )),
        ExternalClientType::TpuClient => Arc::new(
            TpuClientNextClient::new(
                json_rpc_url,
                websocket_url,
                commitment_config,
                bind_address,
                client_node_id,
            )
            .unwrap_or_else(|err| {
                eprintln!("Could not create TpuClientNextClient {err:?}");
                exit(1);
            }),
        ),
    }
}

fn main() {
    agave_logger::setup_with_default_filter();
    solana_metrics::set_panic_hook("bench-tps", /*version:*/ None);

    let matches = cli::build_args(solana_version::version!()).get_matches();
    let cli_config = match cli::parse_args(&matches) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            exit(1);
        }
    };

    let cli::Config {
        json_rpc_url,
        websocket_url,
        id,
        tx_count,
        keypair_multiplier,
        client_ids_and_stake_file,
        write_to_client_file,
        read_from_client_file,
        target_lamports_per_signature,
        num_lamports_per_account,
        external_client_type,
        skip_tx_account_data_size,
        compute_unit_price,
        use_durable_nonce,
        instruction_padding_config,
        bind_address,
        client_node_id,
        commitment_config,
        ..
    } = &cli_config;

    let keypair_count = *tx_count * keypair_multiplier;
    if *write_to_client_file {
        info!("Generating {keypair_count} keypairs");
        let (keypairs, _) = generate_keypairs(id, keypair_count as u64);
        let num_accounts = keypairs.len() as u64;
        let max_fee = FeeRateGovernor::new(*target_lamports_per_signature, 0)
            .max_lamports_per_signature
            .saturating_add(max_lamports_for_prioritization(compute_unit_price));
        let num_lamports_per_account =
            (NUM_SIGNATURES_FOR_TXS * max_fee).div_ceil(num_accounts) + num_lamports_per_account;
        let mut accounts = HashMap::new();
        keypairs.iter().for_each(|keypair| {
            accounts.insert(
                serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap(),
                Base64Account {
                    balance: num_lamports_per_account,
                    executable: false,
                    owner: system_program::id().to_string(),
                    data: String::new(),
                },
            );
        });

        info!("Writing {client_ids_and_stake_file}");
        let serialized = serde_yaml::to_string(&accounts).unwrap();
        let path = Path::new(&client_ids_and_stake_file);
        let mut file = File::create(path).unwrap();
        file.write_all(b"---\n").unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();
        return;
    }

    let client = create_client(
        external_client_type,
        json_rpc_url,
        websocket_url,
        *bind_address,
        client_node_id.as_ref(),
        *commitment_config,
    );
    if let Some(instruction_padding_config) = instruction_padding_config {
        info!(
            "Checking for existence of instruction padding program: {}",
            instruction_padding_config.program_id
        );
        client
            .get_account(&instruction_padding_config.program_id)
            .expect(
                "Instruction padding program must be deployed to this cluster. Deploy the program \
                 using `solana program deploy \
                 ./bench-tps/tests/fixtures/spl_instruction_padding.so` and pass the resulting \
                 program id with `--instruction-padding-program-id`",
            );
    }
    let keypairs = get_keypairs(
        client.clone(),
        id,
        keypair_count,
        *num_lamports_per_account,
        client_ids_and_stake_file,
        *read_from_client_file,
        *skip_tx_account_data_size,
        instruction_padding_config.is_some(),
    );

    let nonce_keypairs = if *use_durable_nonce {
        Some(generate_durable_nonce_accounts(client.clone(), &keypairs))
    } else {
        None
    };
    do_bench_tps(client, cli_config, keypairs, nonce_keypairs);
}
