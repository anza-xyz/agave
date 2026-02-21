use {
    crate::ledger_utils::{
        load_and_process_ledger_or_exit, open_blockstore, open_genesis_config_by,
        LoadAndProcessLedgerOutput,
    },
    base64::{engine::general_purpose::STANDARD as BASE64, Engine as _},
    clap::{value_t_or_exit, Arg, ArgMatches},
    log::*,
    serde::Serialize,
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_ledger::blockstore_options::AccessType,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_message::{v0::LoadedAddresses, AddressLoader},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        collections::BTreeMap,
        fs::File,
        io::Write,
        path::Path,
        str::FromStr,
        sync::Arc,
    },
};

const SYSVAR_IDS: &[&str] = &[
    "SysvarC1ock11111111111111111111111111111111",
    "SysvarEpochRewards1111111111111111111111111",
    "SysvarEpochSchedu1e111111111111111111111111",
    "SysvarFees111111111111111111111111111111111",
    "SysvarLastRestartS1ot1111111111111111111111",
    "SysvarRecentB1ockHashes11111111111111111111",
    "SysvarRent111111111111111111111111111111111",
    "SysvarRewards111111111111111111111111111111",
    "SysvarS1otHashes111111111111111111111111111",
    "SysvarS1otHistory11111111111111111111111111",
    "SysvarStakeHistory1111111111111111111111111",
];

#[derive(Serialize)]
struct AccountFixture {
    pubkey: String,
    lamports: u64,
    data: String,
    owner: String,
    executable: bool,
    rent_epoch: u64,
}

#[derive(Serialize)]
struct TransactionFixture {
    slot: Slot,
    blockhash: String,
    transaction: String,
    features: Vec<String>,
    accounts_before: Vec<AccountFixture>,
    accounts_after: Vec<AccountFixture>,
}

fn account_to_fixture(pubkey: &Pubkey, account: &AccountSharedData) -> AccountFixture {
    AccountFixture {
        pubkey: pubkey.to_string(),
        lamports: account.lamports(),
        data: BASE64.encode(account.data()),
        owner: account.owner().to_string(),
        executable: account.executable(),
        rent_epoch: account.rent_epoch(),
    }
}

/// Collects the full set of accounts needed to replay the transaction:
/// transaction-referenced accounts, resolved ALT addresses, ALT table accounts,
/// all sysvars, and programdata buffers for upgradeable programs.
fn collect_account_set(
    bank: &Bank,
    tx: &VersionedTransaction,
    loaded_addresses: &LoadedAddresses,
) -> BTreeMap<Pubkey, AccountSharedData> {
    let mut accounts = BTreeMap::new();
    let msg = &tx.message;

    for key in msg.static_account_keys() {
        accounts.insert(*key, bank.get_account(key).unwrap_or_default());
    }

    for addr in loaded_addresses
        .writable
        .iter()
        .chain(loaded_addresses.readonly.iter())
    {
        accounts.insert(*addr, bank.get_account(addr).unwrap_or_default());
    }

    if let Some(lookups) = msg.address_table_lookups() {
        for lookup in lookups {
            if let Some(acct) = bank.get_account(&lookup.account_key) {
                accounts.insert(lookup.account_key, acct);
            }
        }
    }

    // All sysvar accounts
    for id_str in SYSVAR_IDS {
        let pubkey = Pubkey::from_str(id_str).unwrap();
        if let Some(acct) = bank.get_account(&pubkey) {
            accounts.insert(pubkey, acct);
        }
    }

    // ProgramData buffers for any upgradeable BPF programs
    let keys_snapshot: Vec<Pubkey> = accounts.keys().cloned().collect();
    for pubkey in &keys_snapshot {
        let acct = match accounts.get(pubkey) {
            Some(a) => a.clone(),
            None => continue,
        };
        if acct.executable() && bpf_loader_upgradeable::check_id(acct.owner()) {
            if let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = acct.state()
            {
                if let Some(pd_acct) = bank.get_account(&programdata_address) {
                    accounts.insert(programdata_address, pd_acct);
                }
            }
        }
    }

    accounts
}

fn snapshot_accounts(
    bank: &Bank,
    account_set: &BTreeMap<Pubkey, AccountSharedData>,
) -> Vec<AccountFixture> {
    account_set
        .keys()
        .map(|pubkey| {
            let acct = bank.get_account(pubkey).unwrap_or_default();
            account_to_fixture(pubkey, &acct)
        })
        .collect()
}

fn resolve_lookup_addresses(bank: &Bank, tx: &VersionedTransaction) -> LoadedAddresses {
    match tx.message.address_table_lookups() {
        Some(lookups) if !lookups.is_empty() => {
            let lookups_owned = lookups.to_vec();
            match bank.load_addresses(&lookups_owned) {
                Ok(loaded) => loaded,
                Err(e) => {
                    warn!("Failed to resolve address table lookups: {e:?}");
                    LoadedAddresses::default()
                }
            }
        }
        _ => LoadedAddresses::default(),
    }
}

pub fn transaction_fixture(ledger_path: &Path, arg_matches: &ArgMatches<'_>) {
    let signature_str = arg_matches
        .value_of("transaction_signature")
        .expect("transaction-signature is required");
    let signature = Signature::from_str(signature_str).unwrap_or_else(|e| {
        eprintln!("Error: invalid transaction signature: {e}");
        std::process::exit(1);
    });

    let target_slot = value_t_or_exit!(arg_matches, "slot", Slot);

    let output_path = arg_matches
        .value_of("output")
        .unwrap_or("txn_fixture.json");

    let genesis_config = open_genesis_config_by(ledger_path, arg_matches);
    let mut process_options = crate::args::parse_process_options(ledger_path, arg_matches);

    let blockstore = Arc::new(open_blockstore(
        ledger_path,
        arg_matches,
        AccessType::ReadOnly,
    ));

    // Verify transaction exists in the target slot
    info!("Verifying transaction {signature} exists in slot {target_slot}...");
    let slot_entries: Vec<Entry> = blockstore
        .get_slot_entries(target_slot, 0)
        .unwrap_or_else(|e| {
            eprintln!("Error reading slot entries: {e}");
            std::process::exit(1);
        });
    let target_tx = slot_entries
        .iter()
        .flat_map(|entry| &entry.transactions)
        .find(|tx| !tx.signatures.is_empty() && tx.signatures[0] == signature)
        .cloned()
        .unwrap_or_else(|| {
            eprintln!("Error: transaction {signature} not found in slot {target_slot}");
            std::process::exit(1);
        });
    info!("Transaction found in slot {target_slot}");

    // Find the parent slot
    let slot_meta = blockstore
        .meta(target_slot)
        .unwrap_or_else(|e| {
            eprintln!("Error reading slot meta: {e}");
            std::process::exit(1);
        })
        .unwrap_or_else(|| {
            eprintln!("Error: slot {target_slot} not found in blockstore");
            std::process::exit(1);
        });
    let parent_slot = slot_meta.parent_slot.unwrap_or_else(|| {
        eprintln!("Error: slot {target_slot} has no parent (orphan slot)");
        std::process::exit(1);
    });
    info!("Parent slot: {parent_slot}");

    // Replay to parent slot
    process_options.halt_at_slot = Some(parent_slot);
    info!("Replaying ledger to parent slot {parent_slot}...");
    let LoadAndProcessLedgerOutput {
        bank_forks,
        accounts_background_service,
        ..
    } = load_and_process_ledger_or_exit(
        arg_matches,
        &genesis_config,
        Arc::clone(&blockstore),
        process_options,
        None,
    );

    let parent_bank = bank_forks
        .read()
        .unwrap()
        .get(parent_slot)
        .unwrap_or_else(|| {
            eprintln!("Error: parent slot {parent_slot} bank not available after replay");
            std::process::exit(1);
        });
    info!("Parent bank loaded at slot {parent_slot}");

    // Create child bank for the target slot
    info!("Creating child bank for slot {target_slot}...");
    let child_bank = Bank::new_from_parent(parent_bank, &Pubkey::default(), target_slot);

    // Process entries in the target slot up to (but not including) the target transaction
    info!("Processing entries in slot {target_slot} up to target transaction...");
    let no_scheduler = BankWithScheduler::no_scheduler_available();
    let mut found_target = false;

    for entry in &slot_entries {
        if entry.is_tick() {
            child_bank.register_tick(&entry.hash, &no_scheduler);
            continue;
        }

        let target_pos = entry
            .transactions
            .iter()
            .position(|tx| !tx.signatures.is_empty() && tx.signatures[0] == signature);

        if let Some(idx) = target_pos {
            // Process only transactions preceding the target in this entry
            if idx > 0 {
                let preceding: Vec<VersionedTransaction> =
                    entry.transactions[..idx].to_vec();
                if let Err(e) = child_bank.try_process_entry_transactions(preceding) {
                    eprintln!("Warning: failed to process preceding transactions: {e}");
                }
            }
            found_target = true;
            break;
        } else {
            let txs: Vec<VersionedTransaction> = entry.transactions.clone();
            if let Err(e) = child_bank.try_process_entry_transactions(txs) {
                eprintln!("Warning: failed to process entry batch: {e}");
            }
        }
    }

    if !found_target {
        eprintln!("Error: target transaction not found while processing entries");
        std::process::exit(1);
    }

    // Resolve address lookup tables and collect the full account set
    let loaded_addresses = resolve_lookup_addresses(&child_bank, &target_tx);

    // Capture BEFORE state
    info!("Capturing before state...");
    let account_set = collect_account_set(&child_bank, &target_tx, &loaded_addresses);
    let accounts_before: Vec<AccountFixture> = account_set
        .iter()
        .map(|(pk, acct)| account_to_fixture(pk, acct))
        .collect();

    // Execute the target transaction
    info!("Executing target transaction...");
    match child_bank.try_process_entry_transactions(vec![target_tx.clone()]) {
        Ok(results) => {
            for (i, r) in results.iter().enumerate() {
                match r {
                    Ok(()) => info!("Transaction {i} executed successfully"),
                    Err(e) => warn!("Transaction {i} execution error: {e}"),
                }
            }
        }
        Err(e) => {
            eprintln!("Error processing target transaction: {e}");
            std::process::exit(1);
        }
    }

    // Capture AFTER state
    info!("Capturing after state...");
    let accounts_after = snapshot_accounts(&child_bank, &account_set);

    // Serialize the fixture
    let blockhash = target_tx.message.recent_blockhash().to_string();
    let tx_bytes = bincode::serialize(&target_tx).unwrap_or_else(|e| {
        eprintln!("Error serializing transaction: {e}");
        std::process::exit(1);
    });

    let mut active_features: Vec<String> = child_bank
        .feature_set
        .active()
        .keys()
        .map(|k| k.to_string())
        .collect();
    active_features.sort();

    let fixture = TransactionFixture {
        slot: target_slot,
        blockhash,
        transaction: BASE64.encode(&tx_bytes),
        features: active_features,
        accounts_before,
        accounts_after,
    };

    let json = serde_json::to_string_pretty(&fixture).unwrap();
    let mut f = File::create(output_path).unwrap_or_else(|e| {
        eprintln!("Error creating output file: {e}");
        std::process::exit(1);
    });
    f.write_all(json.as_bytes()).unwrap_or_else(|e| {
        eprintln!("Error writing output file: {e}");
        std::process::exit(1);
    });

    // Clean shutdown
    drop(child_bank);
    drop(bank_forks);
    accounts_background_service.join().unwrap();

    println!("Fixture written to {output_path}");
    println!(
        "  Slot: {target_slot}, Accounts: {}",
        fixture.accounts_before.len()
    );
}

pub fn transaction_fixture_args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("transaction_signature")
            .long("transaction-signature")
            .value_name("SIGNATURE")
            .takes_value(true)
            .required(true)
            .help("Base58-encoded signature of the target transaction"),
        Arg::with_name("slot")
            .long("slot")
            .value_name("SLOT")
            .takes_value(true)
            .required(true)
            .validator(|v| {
                v.parse::<u64>()
                    .map(|_| ())
                    .map_err(|e| format!("{e}"))
            })
            .help("Slot containing the target transaction"),
        Arg::with_name("output")
            .long("output")
            .short("o")
            .value_name("PATH")
            .takes_value(true)
            .default_value("txn_fixture.json")
            .help("Output path for the JSON fixture file"),
    ]
}
