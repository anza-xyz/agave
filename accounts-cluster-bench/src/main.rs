#![allow(clippy::arithmetic_side_effects)]
use {
    clap::{crate_description, crate_name, value_t, values_t, values_t_or_exit, App, Arg},
    log::*,
    rand::{rng, Rng},
    rayon::prelude::*,
    solana_clap_utils::{
        hidden_unless_forced, input_parsers::pubkey_of, input_validators::is_url_or_moniker,
    },
    solana_cli_config::{ConfigInput, CONFIG_FILE},
    solana_client::{
        rpc_client::SerializableTransaction, rpc_config::RpcBlockConfig,
        rpc_request::MAX_GET_CONFIRMED_BLOCKS_RANGE, transaction_executor::TransactionExecutor,
    },
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_gossip::gossip_service::discover_peers,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::{read_keypair_file, Keypair},
    solana_measure::measure::Measure,
    solana_message::Message,
    solana_net_utils::SocketAddrSpace,
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::request::TokenAccountsFilter,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::Transaction,
    solana_transaction_status::UiTransactionEncoding,
    spl_generic_token::token,
    spl_token_interface::state::Account,
    std::{
        cmp::min,
        collections::VecDeque,
        ops::Deref,
        process::exit,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Barrier, RwLock,
        },
        thread::{sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

pub const MAX_RPC_CALL_RETRIES: usize = 5;

pub fn poll_slot_height(client: &RpcClient) -> Slot {
    let mut num_retries = MAX_RPC_CALL_RETRIES;
    loop {
        let response = client.get_slot_with_commitment(CommitmentConfig::confirmed());
        if let Ok(slot) = response {
            return slot;
        } else {
            num_retries -= 1;
            warn!("get_slot_height failure: {response:?}. remaining retries {num_retries}");
        }
        if num_retries == 0 {
            panic!("failed to get_slot_height(), rpc node down?")
        }
        sleep(Duration::from_millis(100));
    }
}

pub fn poll_get_latest_blockhash(client: &RpcClient) -> Option<Hash> {
    let mut num_retries = MAX_RPC_CALL_RETRIES;
    loop {
        let response = client.get_latest_blockhash();
        if let Ok(blockhash) = response {
            return Some(blockhash);
        } else {
            num_retries -= 1;
            warn!("get_latest_blockhash failure: {response:?}. remaining retries {num_retries}");
        }
        if num_retries == 0 {
            panic!("failed to get_latest_blockhash(), rpc node down?")
        }
        sleep(Duration::from_millis(100));
    }
}

pub fn poll_get_fee_for_message(client: &RpcClient, message: &mut Message) -> (Option<u64>, Hash) {
    let mut num_retries = MAX_RPC_CALL_RETRIES;
    loop {
        let response = client.get_fee_for_message(message);

        if let Ok(fee) = response {
            return (Some(fee), message.recent_blockhash);
        } else {
            num_retries -= 1;
            warn!("get_fee_for_message failure: {response:?}. remaining retries {num_retries}");

            let blockhash = poll_get_latest_blockhash(client).expect("blockhash");
            message.recent_blockhash = blockhash;
        }
        if num_retries == 0 {
            panic!("failed to get_fee_for_message(), rpc node down?")
        }
        sleep(Duration::from_millis(100));
    }
}

fn airdrop_lamports(client: &RpcClient, id: &Keypair, desired_balance: u64) -> bool {
    let starting_balance = client.get_balance(&id.pubkey()).unwrap_or(0);
    info!("starting balance {starting_balance}");

    if starting_balance < desired_balance {
        let airdrop_amount = desired_balance - starting_balance;
        info!(
            "Airdropping {:?} lamports from {} for {}",
            airdrop_amount,
            client.url(),
            id.pubkey(),
        );

        let blockhash = client.get_latest_blockhash().unwrap();
        if let Err(err) =
            client.request_airdrop_with_blockhash(&id.pubkey(), airdrop_amount, &blockhash)
        {
            panic!(
                "Error requesting airdrop: {err:?} to addr: {0:?} amount: {airdrop_amount}",
                id.pubkey()
            );
        }

        let current_balance = client.get_balance(&id.pubkey()).unwrap_or_else(|e| {
            panic!("airdrop error {e}");
        });
        info!("current balance {current_balance}...");

        if current_balance - starting_balance != airdrop_amount {
            info!(
                "Airdrop failed? {} {} {} {}",
                id.pubkey(),
                current_balance,
                starting_balance,
                airdrop_amount,
            );
        }
    }
    true
}

struct SeedTracker {
    max_created: Arc<AtomicU64>,
    max_closed: Arc<AtomicU64>,
}

#[derive(Clone)]
struct TransactionSignatureTracker(Arc<RwLock<VecDeque<Signature>>>);

impl TransactionSignatureTracker {
    fn get_random(&self) -> Option<Signature> {
        let signatures = self.read().unwrap();
        if signatures.is_empty() {
            None
        } else {
            let random_index = rng().random_range(0..signatures.len());
            let random_signature = signatures.get(random_index);
            random_signature.cloned()
        }
    }
    fn track_transactions(&self, transactions: &[Transaction]) {
        let mut lock = self.write().unwrap();
        for signature in transactions.iter().map(Transaction::get_signature) {
            lock.push_back(*signature);
        }
    }
}

impl Deref for TransactionSignatureTracker {
    type Target = Arc<RwLock<VecDeque<Signature>>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn make_create_message(
    keypair: &Keypair,
    base_keypair: &Keypair,
    max_created_seed: Arc<AtomicU64>,
    num_instructions: usize,
    balance: u64,
    maybe_space: Option<u64>,
    mint: Option<Pubkey>,
) -> Message {
    let space = if mint.is_some() {
        Account::get_packed_len() as u64
    } else {
        maybe_space.unwrap_or_else(|| rng().random_range(0..1000))
    };

    let instructions: Vec<_> = (0..num_instructions)
        .flat_map(|_| {
            let program_id = if mint.is_some() {
                token::id()
            } else {
                system_program::id()
            };
            let seed = max_created_seed.fetch_add(1, Ordering::Relaxed).to_string();
            let to_pubkey =
                Pubkey::create_with_seed(&base_keypair.pubkey(), &seed, &program_id).unwrap();
            let mut instructions = vec![system_instruction::create_account_with_seed(
                &keypair.pubkey(),
                &to_pubkey,
                &base_keypair.pubkey(),
                &seed,
                balance,
                space,
                &program_id,
            )];
            if let Some(mint_address) = mint {
                instructions.push(
                    spl_token_interface::instruction::initialize_account(
                        &spl_token_interface::id(),
                        &to_pubkey,
                        &mint_address,
                        &base_keypair.pubkey(),
                    )
                    .unwrap(),
                );
                instructions.push(
                    spl_token_interface::instruction::approve(
                        &spl_token_interface::id(),
                        &to_pubkey,
                        &base_keypair.pubkey(),
                        &base_keypair.pubkey(),
                        &[&base_keypair.pubkey()],
                        1,
                    )
                    .unwrap(),
                );
            }

            instructions
        })
        .collect();

    Message::new(&instructions, Some(&keypair.pubkey()))
}

fn make_close_message(
    keypair: &Keypair,
    base_keypair: &Keypair,
    max_created: &AtomicU64,
    max_closed: &AtomicU64,
    num_instructions: usize,
    balance: u64,
    spl_token: bool,
) -> Message {
    let instructions: Vec<_> = (0..num_instructions)
        .filter_map(|_| {
            let program_id = if spl_token {
                token::id()
            } else {
                system_program::id()
            };
            let max_created_seed = max_created.load(Ordering::Relaxed);
            let max_closed_seed = max_closed.load(Ordering::Relaxed);
            if max_closed_seed >= max_created_seed {
                return None;
            }
            let seed = max_closed.fetch_add(1, Ordering::Relaxed).to_string();
            let address =
                Pubkey::create_with_seed(&base_keypair.pubkey(), &seed, &program_id).unwrap();
            if spl_token {
                Some(
                    spl_token_interface::instruction::close_account(
                        &spl_token_interface::id(),
                        &address,
                        &keypair.pubkey(),
                        &base_keypair.pubkey(),
                        &[],
                    )
                    .unwrap(),
                )
            } else {
                Some(system_instruction::transfer_with_seed(
                    &address,
                    &base_keypair.pubkey(),
                    seed,
                    &program_id,
                    &keypair.pubkey(),
                    balance,
                ))
            }
        })
        .collect();

    Message::new(&instructions, Some(&keypair.pubkey()))
}

#[derive(Clone, Copy, Debug)]
pub enum RpcBench {
    Version,
    Slot,
    MultipleAccounts,
    ProgramAccounts,
    TokenAccountsByOwner,
    Supply,
    TokenAccountsByDelegate,
    Block,
    Blocks,
    AccountInfo,
    Transaction,
    TransactionParsed,
    FirstAvailableBlock,
    TokenSupply,
}

#[derive(Debug)]
pub enum RpcParseError {
    InvalidOption,
}

impl FromStr for RpcBench {
    type Err = RpcParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "account-info" => Ok(RpcBench::AccountInfo),
            "block" => Ok(RpcBench::Block),
            "blocks" => Ok(RpcBench::Blocks),
            "first-available-block" => Ok(RpcBench::FirstAvailableBlock),
            "slot" => Ok(RpcBench::Slot),
            "supply" => Ok(RpcBench::Supply),
            "multiple-accounts" => Ok(RpcBench::MultipleAccounts),
            "token-accounts-by-delegate" => Ok(RpcBench::TokenAccountsByDelegate),
            "token-accounts-by-owner" => Ok(RpcBench::TokenAccountsByOwner),
            "token-supply" => Ok(RpcBench::TokenSupply),
            "transaction" => Ok(RpcBench::Transaction),
            "transaction-parsed" => Ok(RpcBench::TransactionParsed),
            "version" => Ok(RpcBench::Version),
            _ => Err(RpcParseError::InvalidOption),
        }
    }
}

fn process_get_multiple_accounts(
    max_closed: &AtomicU64,
    max_created: &AtomicU64,
    stats: &mut RpcBenchStats,
    last_error: &mut Instant,
    base_keypair_pubkey: &Pubkey,
    program_id: &Pubkey,
    client: &RpcClient,
) {
    let start = max_closed.load(Ordering::Relaxed);
    let end = max_created.load(Ordering::Relaxed);
    let mut chunk_start = start;
    let chunk_size = 10;
    while chunk_start < end {
        let chunk_end = std::cmp::min(chunk_start + chunk_size, end);

        let addresses: Vec<_> = (chunk_start..chunk_end)
            .map(|seed| {
                Pubkey::create_with_seed(base_keypair_pubkey, &seed.to_string(), program_id)
                    .unwrap()
            })
            .collect();
        chunk_start = chunk_end;
        let mut rpc_time = Measure::start("rpc-get-multiple-accounts");
        match client.get_multiple_accounts(&addresses) {
            Ok(accounts) => {
                rpc_time.stop();
                for account in accounts.into_iter().flatten() {
                    if rng().random_ratio(1, 10_000) {
                        info!(
                            "account: lamports {:?} size: {} owner: {:?}",
                            account.lamports,
                            account.data.len(),
                            account.owner
                        );
                    }
                }
                stats.total_success_time_us += rpc_time.as_us();
                stats.success += 1;
            }
            Err(e) => {
                rpc_time.stop();
                stats.total_errors_time_us += rpc_time.as_us();
                stats.errors += 1;
                if last_error.elapsed().as_secs() > 2 {
                    info!("error: {e:?}");
                    *last_error = Instant::now();
                }
                debug!("error: {e:?}");
            }
        }
    }
}

fn process_get_transaction(
    test_name: &'static str,
    transaction_signature_tracker: &TransactionSignatureTracker,
    client: &RpcClient,
    stats: &mut RpcBenchStats,
    last_error: &mut Instant,
    encoding: UiTransactionEncoding,
) {
    let Some(signature) = transaction_signature_tracker.get_random() else {
        info!("transaction: No transactions have yet been made; skipping");
        return;
    };
    let mut measure = Measure::start(test_name);
    match client.get_transaction(&signature, encoding) {
        Ok(_tx) => {
            measure.stop();
            stats.success += 1;
            stats.total_success_time_us += measure.as_us();
        }
        Err(e) => {
            measure.stop();
            stats.errors += 1;
            stats.total_errors_time_us += measure.as_us();
            if last_error.elapsed().as_secs() > 2 {
                info!("get_transaction error: {:?}", &e);
                *last_error = Instant::now();
            }
        }
    };
}

#[derive(Default)]
struct RpcBenchStats {
    errors: u64,
    success: u64,
    total_errors_time_us: u64,
    total_success_time_us: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_rpc_bench_loop(
    rpc_bench: RpcBench,
    thread: usize,
    client: &RpcClient,
    base_keypair_pubkey: &Pubkey,
    exit: &AtomicBool,
    program_id: &Pubkey,
    max_closed: &AtomicU64,
    max_created: &AtomicU64,
    slot_height: &AtomicU64,
    mint: &Option<Pubkey>,
    transaction_signature_tracker: &TransactionSignatureTracker,
) {
    let mut stats = RpcBenchStats::default();
    let mut iters = 0;
    let mut last_error = Instant::now();
    let mut last_print = Instant::now();
    fn flush_stats(
        iters: &i32,
        last_print: &mut Instant,
        rpc_bench: &RpcBench,
        stats: &mut RpcBenchStats,
        thread: &usize,
    ) {
        info!(
            "t({}) rpc({:?}) iters: {} success: {} errors: {}",
            thread, rpc_bench, iters, stats.success, stats.errors
        );
        if stats.success > 0 {
            info!(
                " t({}) rpc({:?} average success_time: {} us",
                thread,
                rpc_bench,
                stats.total_success_time_us / stats.success
            );
        }
        if stats.errors > 0 {
            info!(
                " rpc average average errors time: {} us",
                stats.total_errors_time_us / stats.errors
            );
        }
        *last_print = Instant::now();
        *stats = RpcBenchStats::default();
    }
    loop {
        if exit.load(Ordering::Relaxed) {
            flush_stats(&iters, &mut last_print, &rpc_bench, &mut stats, &thread);
            break;
        }
        match rpc_bench {
            RpcBench::AccountInfo => {
                let start: u64 = max_closed.load(Ordering::Relaxed);
                let end: u64 = max_created.load(Ordering::Relaxed);
                let seed_range = start..end;
                if seed_range.is_empty() {
                    info!("get_account_info: No accounts have yet been created; skipping");
                    continue;
                }
                let seed = rng().random_range(seed_range).to_string();
                let account_pubkey =
                    Pubkey::create_with_seed(base_keypair_pubkey, &seed, program_id).unwrap();
                let mut rpc_time = Measure::start("rpc-get-account-info");
                match client.get_account(&account_pubkey) {
                    Ok(_account) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_account_info error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::Block => {
                let slot_height = slot_height.load(Ordering::Relaxed);
                let mut rpc_time = Measure::start("rpc-get-block");
                match client.get_block_with_config(
                    slot_height,
                    RpcBlockConfig {
                        commitment: Some(CommitmentConfig::confirmed()),
                        ..Default::default()
                    },
                ) {
                    Ok(_block) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_block error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::Blocks => {
                let slot_height = slot_height.load(Ordering::Relaxed);
                let mut rpc_time = Measure::start("rpc-get-blocks");
                match client.get_blocks_with_commitment(
                    slot_height.saturating_sub(MAX_GET_CONFIRMED_BLOCKS_RANGE),
                    Some(slot_height),
                    CommitmentConfig::confirmed(),
                ) {
                    Ok(_slots) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_blocks error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::FirstAvailableBlock => {
                let mut rpc_time = Measure::start("rpc-get-first-available-block");
                match client.get_first_available_block() {
                    Ok(_slot) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_first_available_block error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::Slot => {
                let mut rpc_time = Measure::start("rpc-get-slot");
                match client.get_slot() {
                    Ok(_slot) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_slot error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::Supply => {
                let mut rpc_time = Measure::start("rpc-get-token-supply");
                match client.get_token_supply(&mint.unwrap()) {
                    Ok(_ui_token_amount) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.total_errors_time_us += rpc_time.as_us();
                        stats.errors += 1;
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get_token_supply error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::MultipleAccounts => {
                process_get_multiple_accounts(
                    max_closed,
                    max_created,
                    &mut stats,
                    &mut last_error,
                    base_keypair_pubkey,
                    program_id,
                    client,
                );
            }
            RpcBench::ProgramAccounts => {
                let mut rpc_time = Measure::start("rpc-get-program-accounts");
                match client.get_program_accounts(program_id) {
                    Ok(accounts) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                        if rng().random_ratio(1, 100) {
                            info!("accounts: {} first: {:?}", accounts.len(), accounts.first());
                        }
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.errors += 1;
                        stats.total_errors_time_us += rpc_time.as_us();
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get-program-accounts error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::TokenAccountsByDelegate => {
                let mut rpc_time = Measure::start("rpc-get-token-accounts-by-delegate");
                let filter = TokenAccountsFilter::Mint(*mint.as_ref().unwrap());
                match client.get_token_accounts_by_delegate(base_keypair_pubkey, filter) {
                    Ok(_accounts) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.errors += 1;
                        stats.total_errors_time_us += rpc_time.as_us();
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get-token-accounts-by-delegate error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::TokenAccountsByOwner => {
                let mut rpc_time = Measure::start("rpc-get-token-accounts-by-owner");
                let filter = TokenAccountsFilter::Mint(*mint.as_ref().unwrap());
                match client.get_token_accounts_by_owner(base_keypair_pubkey, filter) {
                    Ok(_accounts) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.errors += 1;
                        stats.total_errors_time_us += rpc_time.as_us();
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get-token-accounts-by-owner error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::TokenSupply => {
                let mut rpc_time = Measure::start("rpc-get-token-supply");
                match client
                    .get_token_supply_with_commitment(&mint.unwrap(), CommitmentConfig::confirmed())
                {
                    Ok(_ui_token_amount) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(e) => {
                        rpc_time.stop();
                        stats.errors += 1;
                        stats.total_errors_time_us += rpc_time.as_us();
                        if last_error.elapsed().as_secs() > 2 {
                            info!("get-token-supply error: {e:?}");
                            last_error = Instant::now();
                        }
                    }
                }
            }
            RpcBench::Transaction => {
                process_get_transaction(
                    "rpc-get-transaction-base64",
                    transaction_signature_tracker,
                    client,
                    &mut stats,
                    &mut last_error,
                    UiTransactionEncoding::Base64,
                );
            }
            RpcBench::TransactionParsed => {
                process_get_transaction(
                    "rpc-get-transaction-parsed",
                    transaction_signature_tracker,
                    client,
                    &mut stats,
                    &mut last_error,
                    UiTransactionEncoding::JsonParsed,
                );
            }
            RpcBench::Version => {
                let mut rpc_time = Measure::start("rpc-get-version");
                match client.get_version() {
                    Ok(_r) => {
                        rpc_time.stop();
                        stats.success += 1;
                        stats.total_success_time_us += rpc_time.as_us();
                    }
                    Err(_e) => {
                        rpc_time.stop();
                        stats.errors += 1;
                        stats.total_errors_time_us += rpc_time.as_us();
                    }
                }
            }
        }

        if last_print.elapsed().as_secs() > 3 {
            flush_stats(&iters, &mut last_print, &rpc_bench, &mut stats, &thread);
        }

        iters += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn make_rpc_bench_threads(
    rpc_benches: Vec<RpcBench>,
    mint: &Option<Pubkey>,
    start_bench_barrier: &Arc<Barrier>,
    exit: &Arc<AtomicBool>,
    client: &Arc<RpcClient>,
    seed_tracker: &SeedTracker,
    slot_height: &Arc<AtomicU64>,
    base_keypair_pubkey: Pubkey,
    num_rpc_bench_threads: usize,
    transaction_signature_tracker: &TransactionSignatureTracker,
) -> Vec<JoinHandle<()>> {
    let program_id = if mint.is_some() {
        token::id()
    } else {
        system_program::id()
    };
    rpc_benches
        .into_iter()
        .flat_map(|rpc_bench| {
            (0..num_rpc_bench_threads).map(move |thread| {
                let client = client.clone();
                let start_bench = start_bench_barrier.clone();
                let exit = exit.clone();
                let max_closed = seed_tracker.max_closed.clone();
                let max_created = seed_tracker.max_created.clone();
                let slot_height = slot_height.clone();
                let transaction_signature_tracker = transaction_signature_tracker.clone();
                let mint = *mint;
                Builder::new()
                    .name(format!("rpc-bench-{thread}"))
                    .spawn(move || {
                        start_bench.wait();
                        run_rpc_bench_loop(
                            rpc_bench,
                            thread,
                            &client,
                            &base_keypair_pubkey,
                            &exit,
                            &program_id,
                            &max_closed,
                            &max_created,
                            &slot_height,
                            &mint,
                            &transaction_signature_tracker,
                        )
                    })
                    .unwrap()
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_accounts_bench(
    client: Arc<RpcClient>,
    payer_keypairs: &[&Keypair],
    iterations: usize,
    maybe_space: Option<u64>,
    batch_size: usize,
    close_nth_batch: u64,
    maybe_lamports: Option<u64>,
    num_instructions: usize,
    max_accounts: Option<usize>,
    mint: Option<Pubkey>,
    reclaim_accounts: bool,
    rpc_benches: Option<Vec<RpcBench>>,
    num_rpc_bench_threads: usize,
) {
    assert!(num_instructions > 0);
    info!("Targeting {}", client.url());

    let mut latest_blockhash = Instant::now();
    let mut last_log = Instant::now();
    let mut count = 0;
    let mut blockhash = poll_get_latest_blockhash(&client).expect("blockhash");
    let slot_height = Arc::new(AtomicU64::new(poll_slot_height(&client)));
    let mut tx_sent_count = 0;
    let mut total_accounts_created = 0;
    let mut total_accounts_closed = 0;
    let mut balances: Vec<_> = payer_keypairs
        .iter()
        .map(|keypair| client.get_balance(&keypair.pubkey()).unwrap_or(0))
        .collect();
    let mut last_balance = Instant::now();

    let default_max_lamports = 1000;
    let min_balance = maybe_lamports.unwrap_or_else(|| {
        let space = maybe_space.unwrap_or(default_max_lamports);
        client
            .get_minimum_balance_for_rent_exemption(space as usize)
            .expect("min balance")
    });

    let base_keypair = Keypair::new();
    let seed_tracker = SeedTracker {
        max_created: Arc::new(AtomicU64::default()),
        max_closed: Arc::new(AtomicU64::default()),
    };
    let transaction_signature_tracker =
        TransactionSignatureTracker(Arc::new(RwLock::new(VecDeque::with_capacity(5000))));

    info!("Starting balance(s): {balances:?}");

    let executor = TransactionExecutor::new_with_rpc_client(client.clone());

    // Create and close messages both require 2 signatures, fake a 2 signature message to calculate fees
    let mut message = Message::new(
        &[
            Instruction::new_with_bytes(
                Pubkey::new_unique(),
                &[],
                vec![AccountMeta::new(Pubkey::new_unique(), true)],
            ),
            Instruction::new_with_bytes(
                Pubkey::new_unique(),
                &[],
                vec![AccountMeta::new(Pubkey::new_unique(), true)],
            ),
        ],
        None,
    );

    let exit = Arc::new(AtomicBool::new(false));
    let mut start_bench_barrier = Some(Arc::new(Barrier::new(
        // In order to unlock the benchmark threads, `wait()` must be called on each thread and then
        // once from this thread, after the first pass through the account creation loop.
        num_rpc_bench_threads + 1,
    )));
    let base_keypair_pubkey = base_keypair.pubkey();
    let rpc_bench_threads: Vec<_> = if let Some(rpc_benches) = rpc_benches {
        make_rpc_bench_threads(
            rpc_benches,
            &mint,
            start_bench_barrier.as_ref().unwrap(),
            &exit,
            &client,
            &seed_tracker,
            &slot_height,
            base_keypair_pubkey,
            num_rpc_bench_threads,
            &transaction_signature_tracker,
        )
    } else {
        Vec::new()
    };

    loop {
        if latest_blockhash.elapsed().as_millis() > 10_000 {
            blockhash = poll_get_latest_blockhash(&client).expect("blockhash");
            slot_height.store(poll_slot_height(&client), Ordering::Relaxed);
            latest_blockhash = Instant::now();
        }

        message.recent_blockhash = blockhash;
        let (fee, blockhash) = poll_get_fee_for_message(&client, &mut message);
        let fee = fee.expect("get_fee_for_message");
        let lamports = min_balance + fee;

        for (i, balance) in balances.iter_mut().enumerate() {
            if *balance < lamports || last_balance.elapsed().as_millis() > 2000 {
                if let Ok(b) = client.get_balance(&payer_keypairs[i].pubkey()) {
                    *balance = b;
                }
                last_balance = Instant::now();
                if *balance < lamports * 2 {
                    info!("Balance {balance} is less than needed: {lamports}, doing airdrop...");
                    if !airdrop_lamports(&client, payer_keypairs[i], lamports * 100_000) {
                        warn!("failed airdrop, exiting");
                        return;
                    }
                }
            }
        }

        // Create accounts
        let sigs_len = executor.num_outstanding();
        if sigs_len < batch_size {
            let num_to_create = batch_size - sigs_len;
            if num_to_create >= payer_keypairs.len() {
                info!("creating {num_to_create} new");
                let chunk_size = num_to_create / payer_keypairs.len();
                if chunk_size > 0 {
                    for (i, keypair) in payer_keypairs.iter().enumerate() {
                        let txs: Vec<_> = (0..chunk_size)
                            .into_par_iter()
                            .map(|_| {
                                let message = make_create_message(
                                    keypair,
                                    &base_keypair,
                                    seed_tracker.max_created.clone(),
                                    num_instructions,
                                    min_balance,
                                    maybe_space,
                                    mint,
                                );
                                let signers: Vec<&Keypair> = vec![keypair, &base_keypair];
                                Transaction::new(&signers, message, blockhash)
                            })
                            .collect();
                        balances[i] = balances[i].saturating_sub(lamports * txs.len() as u64);
                        info!("txs: {}", txs.len());
                        transaction_signature_tracker.track_transactions(&txs);
                        let new_ids = executor.push_transactions(txs);
                        info!("ids: {}", new_ids.len());
                        tx_sent_count += new_ids.len();
                        total_accounts_created += num_instructions * new_ids.len();
                    }
                }
            }

            if close_nth_batch > 0 {
                let num_batches_to_close =
                    total_accounts_created as u64 / (close_nth_batch * batch_size as u64);
                let expected_closed = num_batches_to_close * batch_size as u64;
                let max_closed_seed = seed_tracker.max_closed.load(Ordering::Relaxed);
                // Close every account we've created with seed between max_closed_seed..expected_closed
                if max_closed_seed < expected_closed {
                    let txs: Vec<_> = (0..expected_closed - max_closed_seed)
                        .into_par_iter()
                        .map(|_| {
                            let message = make_close_message(
                                payer_keypairs[0],
                                &base_keypair,
                                &seed_tracker.max_created,
                                &seed_tracker.max_closed,
                                1,
                                min_balance,
                                mint.is_some(),
                            );
                            let signers: Vec<&Keypair> = vec![payer_keypairs[0], &base_keypair];
                            Transaction::new(&signers, message, blockhash)
                        })
                        .collect();
                    balances[0] = balances[0].saturating_sub(fee * txs.len() as u64);
                    info!("close txs: {}", txs.len());
                    transaction_signature_tracker.track_transactions(&txs);
                    let new_ids = executor.push_transactions(txs);
                    info!("close ids: {}", new_ids.len());
                    tx_sent_count += new_ids.len();
                    total_accounts_closed += new_ids.len() as u64;
                }
            }
        } else {
            let _ = executor.drain_cleared();
        }

        if let Some(start_bench) = &start_bench_barrier {
            // As the final barrier participant, this call to `wait()` unlocks all the bench threads
            start_bench.wait();
        }
        start_bench_barrier = None;

        count += 1;
        let max_accounts_met = if let Some(max_accounts) = max_accounts {
            total_accounts_created >= max_accounts
        } else {
            false
        };
        if last_log.elapsed().as_millis() > 3000
            || (count >= iterations && iterations != 0)
            || max_accounts_met
        {
            info!(
                "total_accounts_created: {total_accounts_created} total_accounts_closed: \
                 {total_accounts_closed} tx_sent_count: {tx_sent_count} loop_count: {count} \
                 balance(s): {balances:?}"
            );
            last_log = Instant::now();
        }
        if iterations != 0 && count >= iterations {
            info!("{iterations} iterations reached");
            break;
        }
        if max_accounts_met {
            info!(
                "Max account limit of {:?} reached",
                max_accounts.unwrap_or_default()
            );
            break;
        }
        if executor.num_outstanding() >= batch_size {
            sleep(Duration::from_millis(500));
        }
    }
    executor.close();

    if reclaim_accounts {
        let executor = TransactionExecutor::new_with_rpc_client(client.clone());
        loop {
            let max_closed_seed = seed_tracker.max_closed.load(Ordering::Relaxed);
            let max_created_seed = seed_tracker.max_created.load(Ordering::Relaxed);

            if latest_blockhash.elapsed().as_millis() > 10_000 {
                blockhash = poll_get_latest_blockhash(&client).expect("blockhash");
                latest_blockhash = Instant::now();
            }
            message.recent_blockhash = blockhash;
            let (fee, blockhash) = poll_get_fee_for_message(&client, &mut message);
            let fee = fee.expect("get_fee_for_message");

            let sigs_len = executor.num_outstanding();
            if sigs_len < batch_size && max_closed_seed < max_created_seed {
                let num_to_close = min(
                    batch_size - sigs_len,
                    (max_created_seed - max_closed_seed) as usize,
                );
                if num_to_close >= payer_keypairs.len() {
                    info!("closing {num_to_close} accounts");
                    let chunk_size = num_to_close / payer_keypairs.len();
                    info!("{chunk_size:?} chunk_size");
                    if chunk_size > 0 {
                        for (i, keypair) in payer_keypairs.iter().enumerate() {
                            let txs: Vec<_> = (0..chunk_size)
                                .into_par_iter()
                                .filter_map(|_| {
                                    let message = make_close_message(
                                        keypair,
                                        &base_keypair,
                                        &seed_tracker.max_created,
                                        &seed_tracker.max_closed,
                                        num_instructions,
                                        min_balance,
                                        mint.is_some(),
                                    );
                                    if message.instructions.is_empty() {
                                        return None;
                                    }
                                    let signers: Vec<&Keypair> = vec![keypair, &base_keypair];
                                    Some(Transaction::new(&signers, message, blockhash))
                                })
                                .collect();
                            balances[i] = balances[i].saturating_sub(fee * txs.len() as u64);
                            info!("close txs: {}", txs.len());
                            transaction_signature_tracker.track_transactions(&txs);
                            let new_ids = executor.push_transactions(txs);
                            info!("close ids: {}", new_ids.len());
                            tx_sent_count += new_ids.len();
                            total_accounts_closed += (num_instructions * new_ids.len()) as u64;
                        }
                    }
                }
            } else {
                let _ = executor.drain_cleared();
            }
            count += 1;
            if last_log.elapsed().as_millis() > 3000 || max_closed_seed >= max_created_seed {
                info!(
                    "total_accounts_closed: {total_accounts_closed} tx_sent_count: \
                     {tx_sent_count} loop_count: {count} balance(s): {balances:?}"
                );
                last_log = Instant::now();
            }

            if max_closed_seed >= max_created_seed {
                break;
            }
            if executor.num_outstanding() >= batch_size {
                sleep(Duration::from_millis(500));
            }
        }
        executor.close();
    }

    exit.store(true, Ordering::Relaxed);
    for t in rpc_bench_threads {
        t.join().unwrap();
    }
}

fn main() {
    agave_logger::setup_with_default("solana=info");
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("FILEPATH")
                .takes_value(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL_OR_MONIKER")
                .takes_value(true)
                .validator(is_url_or_moniker)
                .conflicts_with("entrypoint")
                .help(
                    "URL for Solana's JSON RPC or moniker (or their first letter): [mainnet-beta, \
                     testnet, devnet, localhost]",
                ),
        )
        .arg(
            Arg::with_name("entrypoint")
                .long("entrypoint")
                .takes_value(true)
                .value_name("HOST:PORT")
                .conflicts_with("json_rpc_url")
                .help("RPC entrypoint address. Usually <ip>:8899"),
        )
        .arg(
            Arg::with_name("faucet_addr")
                .long("faucet")
                .takes_value(true)
                .value_name("HOST:PORT")
                .hidden(hidden_unless_forced())
                .help("Faucet entrypoint address. Usually <ip>:9900"),
        )
        .arg(
            Arg::with_name("space")
                .long("space")
                .takes_value(true)
                .value_name("BYTES")
                .conflicts_with("mint")
                .help("Size of accounts to create"),
        )
        .arg(
            Arg::with_name("lamports")
                .long("lamports")
                .takes_value(true)
                .value_name("LAMPORTS")
                .help("How many lamports to fund each account"),
        )
        .arg(
            Arg::with_name("identity")
                .long("identity")
                .takes_value(true)
                .multiple(true)
                .value_name("FILE")
                .help("keypair file"),
        )
        .arg(
            Arg::with_name("batch_size")
                .long("batch-size")
                .takes_value(true)
                .value_name("BYTES")
                .help("Number of transactions to send per batch"),
        )
        .arg(
            Arg::with_name("close_nth_batch")
                .long("close-frequency")
                .takes_value(true)
                .value_name("BYTES")
                .help(
                    "Every `n` batches, create a batch of close transactions for the earliest \
                     remaining batch of accounts created. Note: Should be > 1 to avoid situations \
                     where the close transactions will be submitted before the corresponding \
                     create transactions have been confirmed",
                ),
        )
        .arg(
            Arg::with_name("num_instructions")
                .long("num-instructions")
                .takes_value(true)
                .value_name("NUM_INSTRUCTIONS")
                .help("Number of accounts to create on each transaction"),
        )
        .arg(
            Arg::with_name("iterations")
                .long("iterations")
                .takes_value(true)
                .value_name("NUM_ITERATIONS")
                .help("Number of iterations to make. 0 = unlimited iterations."),
        )
        .arg(
            Arg::with_name("max_accounts")
                .long("max-accounts")
                .takes_value(true)
                .value_name("NUM_ACCOUNTS")
                .help(
                    "Halt after client has created this number of accounts. Does not count closed \
                     accounts.",
                ),
        )
        .arg(
            Arg::with_name("check_gossip")
                .long("check-gossip")
                .help("Just use entrypoint address directly"),
        )
        .arg(
            Arg::with_name("shred_version")
                .long("shred-version")
                .takes_value(true)
                .value_name("VERSION")
                .requires("check_gossip")
                .help("The shred version to use for gossip discovery"),
        )
        .arg(
            Arg::with_name("mint")
                .long("mint")
                .takes_value(true)
                .value_name("MINT_ADDRESS")
                .help("Mint address to initialize account"),
        )
        .arg(
            Arg::with_name("reclaim_accounts")
                .long("reclaim-accounts")
                .takes_value(false)
                .help("Reclaim accounts after session ends; incompatible with --iterations 0"),
        )
        .arg(
            Arg::with_name("num_rpc_bench_threads")
                .long("num-rpc-bench-threads")
                .takes_value(true)
                .value_name("NUM_THREADS")
                .help("Spawn this many RPC benching threads for each type passed by --rpc-bench"),
        )
        .arg(
            Arg::with_name("rpc_bench")
                .long("rpc-bench")
                .takes_value(true)
                .value_name("RPC_BENCH_TYPE(S)")
                .multiple(true)
                .requires_ifs(&[("supply", "mint"), ("token-accounts-by-owner", "mint")])
                .requires_ifs(&[
                    ("supply", "mint"),
                    ("token-accounts-by-owner", "mint"),
                    ("token-supply", "mint"),
                ])
                .help("Spawn a thread which calls a specific RPC method in a loop to benchmark it"),
        )
        .get_matches();

    let skip_gossip = !matches.is_present("check_gossip");
    let space = value_t!(matches, "space", u64).ok();
    let lamports = value_t!(matches, "lamports", u64).ok();
    let batch_size = value_t!(matches, "batch_size", usize).unwrap_or(4);
    let close_nth_batch = value_t!(matches, "close_nth_batch", u64).unwrap_or(0);
    let iterations = value_t!(matches, "iterations", usize).unwrap_or(10);
    let max_accounts = value_t!(matches, "max_accounts", usize).ok();
    let num_instructions = value_t!(matches, "num_instructions", usize).unwrap_or(1);
    if num_instructions == 0 || num_instructions > 500 {
        eprintln!("bad num_instructions: {num_instructions}");
        exit(1);
    }
    let rpc_benches = values_t!(matches, "rpc_bench", String)
        .map(|benches| {
            benches
                .into_iter()
                .map(|bench| RpcBench::from_str(&bench).unwrap())
                .collect()
        })
        .ok();
    let num_rpc_bench_threads = if rpc_benches.is_none() {
        0
    } else {
        value_t!(matches, "num_rpc_bench_threads", usize).unwrap_or(1)
    };

    let mint = pubkey_of(&matches, "mint");

    let payer_keypairs: Vec<_> = values_t_or_exit!(matches, "identity", String)
        .iter()
        .map(|keypair_string| {
            read_keypair_file(keypair_string)
                .unwrap_or_else(|_| panic!("bad keypair {keypair_string:?}"))
        })
        .collect();
    let mut payer_keypair_refs: Vec<&Keypair> = vec![];
    for keypair in payer_keypairs.iter() {
        payer_keypair_refs.push(keypair);
    }

    let client = if let Some(addr) = matches.value_of("entrypoint") {
        let entrypoint_addr = solana_net_utils::parse_host_port(addr).unwrap_or_else(|e| {
            eprintln!("failed to parse entrypoint address: {e}");
            exit(1)
        });
        let shred_version: Option<u16> = if !skip_gossip {
            if let Ok(version) = value_t!(matches, "shred_version", u16) {
                Some(version)
            } else {
                Some(
                    solana_net_utils::get_cluster_shred_version(&entrypoint_addr).unwrap_or_else(
                        |err| {
                            eprintln!("Failed to get shred version: {err}");
                            exit(1);
                        },
                    ),
                )
            }
        } else {
            None
        };

        let rpc_addr = if !skip_gossip {
            info!("Finding cluster entry: {entrypoint_addr:?}");
            let (gossip_nodes, _validators) = discover_peers(
                None,
                &vec![entrypoint_addr],
                None,
                Duration::from_secs(60),
                None,
                &[entrypoint_addr],
                None,
                shred_version.unwrap(),
                SocketAddrSpace::Unspecified,
            )
            .unwrap_or_else(|err| {
                eprintln!("Failed to discover {entrypoint_addr} node: {err:?}");
                exit(1);
            });

            info!("done found {} nodes", gossip_nodes.len());
            gossip_nodes[0].rpc().unwrap()
        } else {
            info!("Using {entrypoint_addr:?} as the RPC address");
            entrypoint_addr
        };

        Arc::new(RpcClient::new_socket_with_commitment(
            rpc_addr,
            CommitmentConfig::confirmed(),
        ))
    } else {
        let config = if let Some(config_file) = matches.value_of("config_file") {
            solana_cli_config::Config::load(config_file).unwrap_or_default()
        } else {
            solana_cli_config::Config::default()
        };
        let (_, json_rpc_url) = ConfigInput::compute_json_rpc_url_setting(
            matches.value_of("json_rpc_url").unwrap_or(""),
            &config.json_rpc_url,
        );
        Arc::new(RpcClient::new_with_commitment(
            json_rpc_url,
            CommitmentConfig::confirmed(),
        ))
    };

    run_accounts_bench(
        client,
        &payer_keypair_refs,
        iterations,
        space,
        batch_size,
        close_nth_batch,
        lamports,
        num_instructions,
        max_accounts,
        mint,
        matches.is_present("reclaim_accounts"),
        rpc_benches,
        num_rpc_bench_threads,
    );
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        solana_accounts_db::accounts_index::{AccountIndex, AccountSecondaryIndexes},
        solana_core::validator::ValidatorConfig,
        solana_faucet::faucet::run_local_faucet_for_tests,
        solana_local_cluster::{
            local_cluster::{ClusterConfig, LocalCluster},
            validator_configs::make_identical_validator_configs,
        },
        solana_measure::measure::Measure,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_poh_config::PohConfig,
        solana_program_pack::Pack,
        solana_test_validator::TestValidator,
        spl_token_interface::state::{Account, Mint},
    };

    fn initialize_and_add_secondary_indexes(validator_config: &mut ValidatorConfig) {
        let account_indexes = &mut validator_config.accounts_db_config.account_indexes;
        if account_indexes.is_none() {
            *account_indexes = Some(AccountSecondaryIndexes::default());
        }
        add_secondary_indexes(account_indexes.as_mut().unwrap());

        add_secondary_indexes(&mut validator_config.rpc_config.account_indexes);
    }

    fn add_secondary_indexes(indexes: &mut AccountSecondaryIndexes) {
        indexes.indexes.insert(AccountIndex::SplTokenOwner);
        indexes.indexes.insert(AccountIndex::SplTokenMint);
        indexes.indexes.insert(AccountIndex::ProgramId);
    }

    #[test]
    fn test_accounts_cluster_bench() {
        agave_logger::setup();
        let mut validator_config = ValidatorConfig::default_for_test();
        initialize_and_add_secondary_indexes(&mut validator_config);
        let num_nodes = 1;
        let mut config = ClusterConfig {
            mint_lamports: 10_000_000,
            poh_config: PohConfig::new_sleep(Duration::from_millis(50)),
            node_stakes: vec![100; num_nodes],
            validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
            ..ClusterConfig::default()
        };

        let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
        let iterations = 10;
        let maybe_space = None;
        let batch_size = 100;
        let close_nth_batch = 100;
        let maybe_lamports = None;
        let num_instructions = 2;
        let mut start = Measure::start("total accounts run");
        let rpc_addr = cluster.entry_point_info.rpc().unwrap();
        let client = Arc::new(RpcClient::new_socket_with_commitment(
            rpc_addr,
            CommitmentConfig::confirmed(),
        ));
        let mint = None;
        let reclaim_accounts = false;
        let pre_txs = client.get_transaction_count().unwrap();
        run_accounts_bench(
            client.clone(),
            &[&cluster.funding_keypair],
            iterations,
            maybe_space,
            batch_size,
            close_nth_batch,
            maybe_lamports,
            num_instructions,
            None,
            mint,
            reclaim_accounts,
            Some(vec![RpcBench::ProgramAccounts]),
            1,
        );
        let post_txs = client.get_transaction_count().unwrap();
        start.stop();
        info!("{start} pre {pre_txs} post {post_txs}");
    }

    #[test]
    fn test_halt_accounts_creation_at_max() {
        agave_logger::setup();
        let mut validator_config = ValidatorConfig::default_for_test();
        initialize_and_add_secondary_indexes(&mut validator_config);
        let num_nodes = 1;
        let mut config = ClusterConfig {
            mint_lamports: 10_000_000,
            poh_config: PohConfig::new_sleep(Duration::from_millis(50)),
            node_stakes: vec![100; num_nodes],
            validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
            ..ClusterConfig::default()
        };

        let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
        let iterations = 100;
        let maybe_space = None;
        let batch_size = 20;
        let close_nth_batch = 0;
        let maybe_lamports = None;
        let num_instructions = 2;
        let mut start = Measure::start("total accounts run");
        let rpc_addr = cluster.entry_point_info.rpc().unwrap();
        let client = Arc::new(RpcClient::new_socket_with_commitment(
            rpc_addr,
            CommitmentConfig::confirmed(),
        ));
        let mint = None;
        let reclaim_accounts = false;
        let pre_txs = client.get_transaction_count().unwrap();
        run_accounts_bench(
            client.clone(),
            &[&cluster.funding_keypair],
            iterations,
            maybe_space,
            batch_size,
            close_nth_batch,
            maybe_lamports,
            num_instructions,
            Some(90),
            mint,
            reclaim_accounts,
            Some(vec![RpcBench::ProgramAccounts]),
            1,
        );
        let post_txs = client.get_transaction_count().unwrap();
        start.stop();
        info!("{start} pre {pre_txs} post {post_txs}");
    }

    #[test]
    fn test_create_then_reclaim_spl_token_accounts() {
        agave_logger::setup();
        let mint_keypair = Keypair::new();
        let mint_pubkey = mint_keypair.pubkey();
        let faucet_addr = run_local_faucet_for_tests(
            mint_keypair,
            None, /* per_time_cap */
            0,    /* port */
        );
        let test_validator = TestValidator::with_custom_fees(
            mint_pubkey,
            1,
            Some(faucet_addr),
            SocketAddrSpace::Unspecified,
        );
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            test_validator.rpc_url(),
            CommitmentConfig::processed(),
        ));

        // Created funder
        let funder = Keypair::new();
        let latest_blockhash = rpc_client.get_latest_blockhash().unwrap();
        let signature = rpc_client
            .request_airdrop_with_blockhash(&funder.pubkey(), LAMPORTS_PER_SOL, &latest_blockhash)
            .unwrap();
        rpc_client
            .confirm_transaction_with_spinner(
                &signature,
                &latest_blockhash,
                CommitmentConfig::confirmed(),
            )
            .unwrap();

        // Create Mint
        let spl_mint_keypair = Keypair::new();
        let spl_mint_len = Mint::get_packed_len();
        let spl_mint_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(spl_mint_len)
            .unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[
                system_instruction::create_account(
                    &funder.pubkey(),
                    &spl_mint_keypair.pubkey(),
                    spl_mint_rent,
                    spl_mint_len as u64,
                    &token::id(),
                ),
                spl_token_interface::instruction::initialize_mint(
                    &spl_token_interface::id(),
                    &spl_mint_keypair.pubkey(),
                    &spl_mint_keypair.pubkey(),
                    None,
                    2,
                )
                .unwrap(),
            ],
            Some(&funder.pubkey()),
            &[&funder, &spl_mint_keypair],
            latest_blockhash,
        );
        let _sig = rpc_client
            .send_and_confirm_transaction(&transaction)
            .unwrap();

        let account_len = Account::get_packed_len();
        let minimum_balance = rpc_client
            .get_minimum_balance_for_rent_exemption(account_len)
            .unwrap();

        let iterations = 5;
        let batch_size = 100;
        let close_nth_batch = 0;
        let num_instructions = 4;
        let mut start = Measure::start("total accounts run");
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        run_accounts_bench(
            rpc_client,
            &[&keypair0, &keypair1, &keypair2],
            iterations,
            Some(account_len as u64),
            batch_size,
            close_nth_batch,
            Some(minimum_balance),
            num_instructions,
            None,
            Some(spl_mint_keypair.pubkey()),
            true,
            None,
            0,
        );
        start.stop();
        info!("{start}");
    }
}
