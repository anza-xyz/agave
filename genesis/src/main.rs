//! A command-line executable for generating the chain's genesis config.
#![allow(clippy::arithmetic_side_effects)]

use {
    agave_feature_set::FEATURE_NAMES,
    base64::{prelude::BASE64_STANDARD, Engine},
    clap::{crate_description, crate_name, value_t, value_t_or_exit, App, Arg, ArgMatches},
    itertools::Itertools,
    solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
    solana_bls_signatures::Pubkey as BLSPubkey,
    solana_clap_utils::{
        input_parsers::{
            bls_pubkeys_of, cluster_type_of, pubkey_of, pubkeys_of,
            unix_timestamp_from_rfc3339_datetime,
        },
        input_validators::{
            is_pubkey, is_pubkey_or_keypair, is_rfc3339_datetime, is_slot, is_url_or_moniker,
            is_valid_percentage, normalize_to_url_if_moniker,
        },
    },
    solana_clock as clock,
    solana_cluster_type::ClusterType,
    solana_commitment_config::CommitmentConfig,
    solana_entry::poh::compute_hashes_per_tick,
    solana_epoch_schedule::EpochSchedule,
    solana_feature_gate_interface as feature,
    solana_fee_calculator::FeeRateGovernor,
    solana_genesis::{
        genesis_accounts::add_genesis_stake_accounts, Base64Account, StakedValidatorAccountInfo,
        ValidatorAccountsFile,
    },
    solana_genesis_config::GenesisConfig,
    solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    solana_inflation::Inflation,
    solana_keypair::{read_keypair_file, Keypair},
    solana_ledger::{blockstore::create_new_ledger, blockstore_options::LedgerColumnOptions},
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::request::MAX_MULTIPLE_ACCOUNTS,
    solana_runtime::{
        genesis_utils::{
            add_genesis_epoch_rewards_account, add_genesis_stake_config_account,
            bls_pubkey_to_compressed_bytes,
        },
        stake_utils,
    },
    solana_sdk_ids::system_program,
    solana_signer::Signer,
    solana_stake_interface::state::StakeStateV2,
    solana_vote_program::vote_state::{self, VoteStateV4},
    std::{
        collections::HashMap,
        error,
        fs::File,
        io::{self, Read},
        path::PathBuf,
        process,
        slice::Iter,
        str::FromStr,
        time::Duration,
    },
};

pub enum AccountFileFormat {
    Pubkey,
    Keypair,
}

fn pubkey_from_str(key_str: &str) -> Result<Pubkey, Box<dyn error::Error>> {
    Pubkey::from_str(key_str).or_else(|_| {
        let bytes: Vec<u8> = serde_json::from_str(key_str)?;
        let keypair =
            Keypair::try_from(bytes.as_ref()).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(keypair.pubkey())
    })
}

pub fn load_genesis_accounts(file: &str, genesis_config: &mut GenesisConfig) -> io::Result<u64> {
    let mut lamports = 0;
    let accounts_file = File::open(file)?;

    let genesis_accounts: HashMap<String, Base64Account> =
        serde_yaml::from_reader(accounts_file)
            .map_err(|err| io::Error::other(format!("{err:?}")))?;

    for (key, account_details) in genesis_accounts {
        let pubkey = pubkey_from_str(key.as_str())
            .map_err(|err| io::Error::other(format!("Invalid pubkey/keypair {key}: {err:?}")))?;

        let owner_program_id = Pubkey::from_str(account_details.owner.as_str()).map_err(|err| {
            io::Error::other(format!(
                "Invalid owner: {}: {:?}",
                account_details.owner, err
            ))
        })?;

        let mut account = AccountSharedData::new(account_details.balance, 0, &owner_program_id);
        if account_details.data != "~" {
            account.set_data_from_slice(
                &BASE64_STANDARD
                    .decode(account_details.data.as_str())
                    .map_err(|err| {
                        io::Error::other(format!(
                            "Invalid account data: {}: {:?}",
                            account_details.data, err
                        ))
                    })?,
            );
        }
        account.set_executable(account_details.executable);
        lamports += account.lamports();
        genesis_config.add_account(pubkey, account);
    }

    Ok(lamports)
}

pub fn load_validator_accounts(
    file: &str,
    commission: u8,
    rent: &Rent,
    genesis_config: &mut GenesisConfig,
) -> io::Result<()> {
    let accounts_file = File::open(file)?;
    let validator_genesis_accounts: Vec<StakedValidatorAccountInfo> =
        serde_yaml::from_reader::<_, ValidatorAccountsFile>(accounts_file)
            .map_err(|err| io::Error::other(format!("{err:?}")))?
            .validator_accounts;

    for account_details in validator_genesis_accounts {
        let pubkeys = [
            pubkey_from_str(account_details.identity_account.as_str()).map_err(|err| {
                io::Error::other(format!(
                    "Invalid pubkey/keypair {}: {:?}",
                    account_details.identity_account, err
                ))
            })?,
            pubkey_from_str(account_details.vote_account.as_str()).map_err(|err| {
                io::Error::other(format!(
                    "Invalid pubkey/keypair {}: {:?}",
                    account_details.vote_account, err
                ))
            })?,
            pubkey_from_str(account_details.stake_account.as_str()).map_err(|err| {
                io::Error::other(format!(
                    "Invalid pubkey/keypair {}: {:?}",
                    account_details.stake_account, err
                ))
            })?,
        ];
        let bls_pubkeys: Vec<BLSPubkey> = account_details.bls_pubkey.map_or(Ok(vec![]), |s| {
            BLSPubkey::from_str(&s)
                .map(|pk| vec![pk])
                .map_err(|err| io::Error::other(format!("Invalid BLS pubkey: {err}")))
        })?;

        add_validator_accounts(
            genesis_config,
            &mut pubkeys.iter(),
            &mut bls_pubkeys.iter(),
            account_details.balance_lamports,
            account_details.stake_lamports,
            commission,
            rent,
            None,
        )?;
    }

    Ok(())
}

fn check_rpc_genesis_hash(
    cluster_type: &ClusterType,
    rpc_client: &RpcClient,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(genesis_hash) = cluster_type.get_genesis_hash() {
        let rpc_genesis_hash = rpc_client.get_genesis_hash()?;
        if rpc_genesis_hash != genesis_hash {
            return Err(format!(
                "The genesis hash for the specified cluster {cluster_type:?} does not match the \
                 genesis hash reported by the specified RPC. Cluster genesis hash: \
                 {genesis_hash}, RPC reported genesis hash: {rpc_genesis_hash}"
            )
            .into());
        }
    }
    Ok(())
}

fn features_to_deactivate_for_cluster(
    cluster_type: &ClusterType,
    matches: &ArgMatches<'_>,
) -> Result<Vec<Pubkey>, Box<dyn error::Error>> {
    let mut features_to_deactivate = pubkeys_of(matches, "deactivate_feature").unwrap_or_default();
    if cluster_type == &ClusterType::Development {
        return Ok(features_to_deactivate);
    }

    // if we're here, the cluster type must be one of "mainnet-beta", "testnet", or "devnet"
    assert!(matches!(
        cluster_type,
        ClusterType::MainnetBeta | ClusterType::Testnet | ClusterType::Devnet
    ));
    let json_rpc_url = normalize_to_url_if_moniker(
        matches
            .value_of("json_rpc_url")
            .unwrap_or(matches.value_of("cluster_type").unwrap()),
    );
    let rpc_client = RpcClient::new_with_commitment(json_rpc_url, CommitmentConfig::confirmed());
    check_rpc_genesis_hash(cluster_type, &rpc_client)?;
    for feature_ids in FEATURE_NAMES
        .keys()
        .cloned()
        .collect::<Vec<Pubkey>>()
        .chunks(MAX_MULTIPLE_ACCOUNTS)
    {
        rpc_client
            .get_multiple_accounts(feature_ids)
            .map_err(|err| format!("Failed to fetch: {err}"))?
            .into_iter()
            .zip(feature_ids)
            .for_each(|(maybe_account, feature_id)| {
                if maybe_account
                    .as_ref()
                    .and_then(feature::from_account)
                    .and_then(|feature| feature.activated_at)
                    .is_none()
                {
                    features_to_deactivate.push(*feature_id);
                }
            });
    }
    Ok(features_to_deactivate)
}

fn add_validator_accounts(
    genesis_config: &mut GenesisConfig,
    pubkeys_iter: &mut Iter<Pubkey>,
    bls_pubkeys_iter: &mut Iter<BLSPubkey>,
    lamports: u64,
    stake_lamports: u64,
    commission: u8,
    rent: &Rent,
    authorized_pubkey: Option<&Pubkey>,
) -> io::Result<()> {
    rent_exempt_check(
        stake_lamports,
        rent.minimum_balance(StakeStateV2::size_of()),
    )?;

    loop {
        let Some(identity_pubkey) = pubkeys_iter.next() else {
            break;
        };
        let vote_pubkey = pubkeys_iter.next().unwrap();
        let stake_pubkey = pubkeys_iter.next().unwrap();

        genesis_config.add_account(
            *identity_pubkey,
            AccountSharedData::new(lamports, 0, &system_program::id()),
        );

        let bls_pubkey_compressed_bytes =
            bls_pubkeys_iter.next().map(bls_pubkey_to_compressed_bytes);
        let vote_account = vote_state::create_v4_account_with_authorized(
            identity_pubkey,
            identity_pubkey,
            identity_pubkey,
            bls_pubkey_compressed_bytes,
            u16::from(commission) * 100,
            rent.minimum_balance(VoteStateV4::size_of()).max(1),
        );

        genesis_config.add_account(
            *stake_pubkey,
            stake_utils::create_stake_account(
                authorized_pubkey.unwrap_or(identity_pubkey),
                vote_pubkey,
                &vote_account,
                rent,
                stake_lamports,
            ),
        );
        genesis_config.add_account(*vote_pubkey, vote_account);
    }
    Ok(())
}

fn rent_exempt_check(stake_lamports: u64, exempt: u64) -> io::Result<()> {
    if stake_lamports < exempt {
        Err(io::Error::other(format!(
            "error: insufficient validator stake lamports: {stake_lamports} for rent exemption, \
             requires {exempt}"
        )))
    } else {
        Ok(())
    }
}

#[allow(clippy::cognitive_complexity)]
fn main() -> Result<(), Box<dyn error::Error>> {
    let default_faucet_pubkey = solana_cli_config::Config::default().keypair_path;
    let fee_rate_governor = FeeRateGovernor::default();
    let (
        default_target_lamports_per_signature,
        default_target_signatures_per_slot,
        default_fee_burn_percentage,
    ) = {
        (
            &fee_rate_governor.target_lamports_per_signature.to_string(),
            &fee_rate_governor.target_signatures_per_slot.to_string(),
            &fee_rate_governor.burn_percent.to_string(),
        )
    };

    let rent = Rent::default();
    let (
        default_lamports_per_byte_year,
        default_rent_exemption_threshold,
        default_rent_burn_percentage,
    ) = {
        (
            &rent.lamports_per_byte_year.to_string(),
            &rent.exemption_threshold.to_string(),
            &rent.burn_percent.to_string(),
        )
    };

    // vote account
    let default_bootstrap_validator_lamports = &(500 * LAMPORTS_PER_SOL)
        .max(rent.minimum_balance(VoteStateV4::size_of()))
        .to_string();
    // stake account
    let default_bootstrap_validator_stake_lamports = &(LAMPORTS_PER_SOL / 2)
        .max(rent.minimum_balance(StakeStateV2::size_of()))
        .to_string();

    let default_target_tick_duration = PohConfig::default().target_tick_duration;
    let default_ticks_per_slot = &clock::DEFAULT_TICKS_PER_SLOT.to_string();
    let default_cluster_type = "mainnet-beta";
    let default_genesis_archive_unpacked_size = MAX_GENESIS_ARCHIVE_UNPACKED_SIZE.to_string();

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::with_name("creation_time")
                .long("creation-time")
                .value_name("RFC3339 DATE TIME")
                .validator(is_rfc3339_datetime)
                .takes_value(true)
                .help(
                    "Time when the bootstrap validator will start the cluster [default: current \
                     system time]",
                ),
        )
        .arg(
            Arg::with_name("bootstrap_validator")
                .short("b")
                .long("bootstrap-validator")
                .value_name("IDENTITY_PUBKEY VOTE_PUBKEY STAKE_PUBKEY")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .number_of_values(3)
                .multiple(true)
                .required(true)
                .help("The bootstrap validator's identity, vote and stake pubkeys"),
        )
        .arg(
            Arg::with_name("bootstrap_validator_bls_pubkey")
                .long("bootstrap-validator-bls-pubkey")
                .value_name("BLS_PUBKEY")
                .multiple(true)
                .takes_value(true)
                .required(false)
                .help("The bootstrap validator's bls pubkey"),
        )
        .arg(
            Arg::with_name("ledger_path")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .help("Use directory as persistent ledger location"),
        )
        .arg(
            Arg::with_name("faucet_lamports")
                .short("t")
                .long("faucet-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .help("Number of lamports to assign to the faucet"),
        )
        .arg(
            Arg::with_name("faucet_pubkey")
                .short("m")
                .long("faucet-pubkey")
                .value_name("PUBKEY")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .requires("faucet_lamports")
                .default_value(&default_faucet_pubkey)
                .help("Path to file containing the faucet's pubkey"),
        )
        .arg(
            Arg::with_name("bootstrap_stake_authorized_pubkey")
                .long("bootstrap-stake-authorized-pubkey")
                .value_name("BOOTSTRAP STAKE AUTHORIZED PUBKEY")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .help(
                    "Path to file containing the pubkey authorized to manage the bootstrap \
                     validator's stake [default: --bootstrap-validator IDENTITY_PUBKEY]",
                ),
        )
        .arg(
            Arg::with_name("bootstrap_validator_lamports")
                .long("bootstrap-validator-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_bootstrap_validator_lamports)
                .help("Number of lamports to assign to the bootstrap validator"),
        )
        .arg(
            Arg::with_name("bootstrap_validator_stake_lamports")
                .long("bootstrap-validator-stake-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_bootstrap_validator_stake_lamports)
                .help("Number of lamports to assign to the bootstrap validator's stake account"),
        )
        .arg(
            Arg::with_name("target_lamports_per_signature")
                .long("target-lamports-per-signature")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_target_lamports_per_signature)
                .help(
                    "The cost in lamports that the cluster will charge for signature verification \
                     when the cluster is operating at target-signatures-per-slot",
                ),
        )
        .arg(
            Arg::with_name("lamports_per_byte_year")
                .long("lamports-per-byte-year")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_lamports_per_byte_year)
                .help(
                    "The cost in lamports that the cluster will charge per byte per year for \
                     accounts with data",
                ),
        )
        .arg(
            Arg::with_name("rent_exemption_threshold")
                .long("rent-exemption-threshold")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_rent_exemption_threshold)
                .help(
                    "amount of time (in years) the balance has to include rent for to qualify as \
                     rent exempted account",
                ),
        )
        .arg(
            Arg::with_name("rent_burn_percentage")
                .long("rent-burn-percentage")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_rent_burn_percentage)
                .help("percentage of collected rent to burn")
                .validator(is_valid_percentage),
        )
        .arg(
            Arg::with_name("fee_burn_percentage")
                .long("fee-burn-percentage")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_fee_burn_percentage)
                .help("percentage of collected fee to burn")
                .validator(is_valid_percentage),
        )
        .arg(
            Arg::with_name("vote_commission_percentage")
                .long("vote-commission-percentage")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value("100")
                .help("percentage of vote commission")
                .validator(is_valid_percentage),
        )
        .arg(
            Arg::with_name("target_signatures_per_slot")
                .long("target-signatures-per-slot")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_target_signatures_per_slot)
                .help(
                    "Used to estimate the desired processing capacity of the cluster. When the \
                     latest slot processes fewer/greater signatures than this value, the \
                     lamports-per-signature fee will decrease/increase for the next slot. A value \
                     of 0 disables signature-based fee adjustments",
                ),
        )
        .arg(
            Arg::with_name("target_tick_duration")
                .long("target-tick-duration")
                .value_name("MILLIS")
                .takes_value(true)
                .help("The target tick rate of the cluster in milliseconds"),
        )
        .arg(
            Arg::with_name("hashes_per_tick")
                .long("hashes-per-tick")
                .value_name("NUM_HASHES|\"auto\"|\"sleep\"")
                .takes_value(true)
                .default_value("auto")
                .help(
                    "How many PoH hashes to roll before emitting the next tick. If \"auto\", \
                     determine based on --target-tick-duration and the hash rate of this \
                     computer. If \"sleep\", for development sleep for --target-tick-duration \
                     instead of hashing",
                ),
        )
        .arg(
            Arg::with_name("ticks_per_slot")
                .long("ticks-per-slot")
                .value_name("TICKS")
                .takes_value(true)
                .default_value(default_ticks_per_slot)
                .help("The number of ticks in a slot"),
        )
        .arg(
            Arg::with_name("slots_per_epoch")
                .long("slots-per-epoch")
                .value_name("SLOTS")
                .validator(is_slot)
                .takes_value(true)
                .help("The number of slots in an epoch"),
        )
        .arg(
            Arg::with_name("enable_warmup_epochs")
                .long("enable-warmup-epochs")
                .help(
                    "When enabled epochs start short and will grow. Useful for warming up stake \
                     quickly during development",
                ),
        )
        .arg(
            Arg::with_name("primordial_accounts_file")
                .long("primordial-accounts-file")
                .value_name("FILENAME")
                .takes_value(true)
                .multiple(true)
                .help("The location of pubkey for primordial accounts and balance"),
        )
        .arg(
            Arg::with_name("validator_accounts_file")
                .long("validator-accounts-file")
                .value_name("FILENAME")
                .takes_value(true)
                .multiple(true)
                .help(
                    "The location of a file containing a list of identity, vote, and stake \
                     pubkeys and balances for validator accounts to bake into genesis",
                ),
        )
        .arg(
            Arg::with_name("cluster_type")
                .long("cluster-type")
                .possible_values(&ClusterType::STRINGS)
                .takes_value(true)
                .default_value(default_cluster_type)
                .help("Selects the features that will be enabled for the cluster"),
        )
        .arg(
            Arg::with_name("deactivate_feature")
                .long("deactivate-feature")
                .takes_value(true)
                .value_name("FEATURE_PUBKEY")
                .validator(is_pubkey)
                .multiple(true)
                .help(
                    "Deactivate this feature in genesis. Compatible with --cluster-type \
                     development",
                ),
        )
        .arg(
            Arg::with_name("max_genesis_archive_unpacked_size")
                .long("max-genesis-archive-unpacked-size")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(&default_genesis_archive_unpacked_size)
                .help("maximum total uncompressed file size of created genesis archive"),
        )
        .arg(
            Arg::with_name("bpf_program")
                .long("bpf-program")
                .value_name("ADDRESS LOADER SBF_PROGRAM.SO")
                .takes_value(true)
                .number_of_values(3)
                .multiple(true)
                .help("Install a SBF program at the given address"),
        )
        .arg(
            Arg::with_name("upgradeable_program")
                .long("upgradeable-program")
                .value_name("ADDRESS UPGRADEABLE_LOADER SBF_PROGRAM.SO UPGRADE_AUTHORITY")
                .takes_value(true)
                .number_of_values(4)
                .multiple(true)
                .help(
                    "Install an upgradeable SBF program at the given address with the given \
                     upgrade authority (or \"none\")",
                ),
        )
        .arg(
            Arg::with_name("inflation")
                .required(false)
                .long("inflation")
                .takes_value(true)
                .possible_values(&["pico", "full", "none"])
                .help("Selects inflation"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL_OR_MONIKER")
                .takes_value(true)
                .global(true)
                .validator(is_url_or_moniker)
                .help(
                    "URL for Solana's JSON RPC or moniker (or their first letter): [mainnet-beta, \
                     testnet, devnet, localhost]. Used for cloning feature sets",
                ),
        )
        .arg(
            Arg::with_name("alpenglow")
                .long("alpenglow")
                .help("Whether we use Alpenglow consensus."),
        )
        .get_matches();

    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());

    let rent = Rent {
        lamports_per_byte_year: value_t_or_exit!(matches, "lamports_per_byte_year", u64),
        exemption_threshold: value_t_or_exit!(matches, "rent_exemption_threshold", f64),
        burn_percent: value_t_or_exit!(matches, "rent_burn_percentage", u8),
    };

    let bootstrap_validator_pubkeys = pubkeys_of(&matches, "bootstrap_validator").unwrap();
    assert_eq!(bootstrap_validator_pubkeys.len() % 3, 0);

    let bootstrap_validator_bls_pubkeys =
        bls_pubkeys_of(&matches, "bootstrap_validator_bls_pubkey");
    if let Some(bls_pubkeys) = &bootstrap_validator_bls_pubkeys {
        assert_eq!(
            bls_pubkeys.len() * 3,
            bootstrap_validator_pubkeys.len(),
            "Number of BLS pubkeys must match the number of bootstrap validator identities"
        );
    }

    // Ensure there are no duplicated pubkeys in the --bootstrap-validator list
    {
        let mut v = bootstrap_validator_pubkeys.clone();
        v.sort();
        v.dedup();
        if v.len() != bootstrap_validator_pubkeys.len() {
            eprintln!("Error: --bootstrap-validator pubkeys cannot be duplicated");
            process::exit(1);
        }
    }

    let bootstrap_validator_lamports =
        value_t_or_exit!(matches, "bootstrap_validator_lamports", u64);

    let bootstrap_validator_stake_lamports =
        value_t_or_exit!(matches, "bootstrap_validator_stake_lamports", u64);

    let bootstrap_stake_authorized_pubkey =
        pubkey_of(&matches, "bootstrap_stake_authorized_pubkey");
    let faucet_lamports = value_t!(matches, "faucet_lamports", u64).unwrap_or(0);
    let faucet_pubkey = pubkey_of(&matches, "faucet_pubkey");

    let ticks_per_slot = value_t_or_exit!(matches, "ticks_per_slot", u64);

    let mut fee_rate_governor = FeeRateGovernor::new(
        value_t_or_exit!(matches, "target_lamports_per_signature", u64),
        value_t_or_exit!(matches, "target_signatures_per_slot", u64),
    );
    fee_rate_governor.burn_percent = value_t_or_exit!(matches, "fee_burn_percentage", u8);

    let mut poh_config = PohConfig {
        target_tick_duration: if matches.is_present("target_tick_duration") {
            Duration::from_micros(value_t_or_exit!(matches, "target_tick_duration", u64))
        } else {
            default_target_tick_duration
        },
        ..PohConfig::default()
    };

    let cluster_type = cluster_type_of(&matches, "cluster_type").unwrap();

    // Get the features to deactivate if provided
    let features_to_deactivate = features_to_deactivate_for_cluster(&cluster_type, &matches)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

    match matches.value_of("hashes_per_tick").unwrap() {
        "auto" => match cluster_type {
            ClusterType::Development => {
                let hashes_per_tick =
                    compute_hashes_per_tick(poh_config.target_tick_duration, 1_000_000);
                poh_config.hashes_per_tick = Some(hashes_per_tick / 2); // use 50% of peak ability
            }
            ClusterType::Devnet | ClusterType::Testnet | ClusterType::MainnetBeta => {
                poh_config.hashes_per_tick = Some(clock::DEFAULT_HASHES_PER_TICK);
            }
        },
        "sleep" => {
            poh_config.hashes_per_tick = None;
        }
        _ => {
            poh_config.hashes_per_tick = Some(value_t_or_exit!(matches, "hashes_per_tick", u64));
        }
    }

    let slots_per_epoch = if matches.value_of("slots_per_epoch").is_some() {
        value_t_or_exit!(matches, "slots_per_epoch", u64)
    } else {
        match cluster_type {
            ClusterType::Development => clock::DEFAULT_DEV_SLOTS_PER_EPOCH,
            ClusterType::Devnet | ClusterType::Testnet | ClusterType::MainnetBeta => {
                clock::DEFAULT_SLOTS_PER_EPOCH
            }
        }
    };
    let epoch_schedule = EpochSchedule::custom(
        slots_per_epoch,
        slots_per_epoch,
        matches.is_present("enable_warmup_epochs"),
    );

    let mut genesis_config = GenesisConfig {
        ticks_per_slot,
        poh_config,
        fee_rate_governor,
        rent,
        epoch_schedule,
        cluster_type,
        ..GenesisConfig::default()
    };

    if let Ok(raw_inflation) = value_t!(matches, "inflation", String) {
        let inflation = match raw_inflation.as_str() {
            "pico" => Inflation::pico(),
            "full" => Inflation::full(),
            "none" => Inflation::new_disabled(),
            _ => unreachable!(),
        };
        genesis_config.inflation = inflation;
    }

    let commission = value_t_or_exit!(matches, "vote_commission_percentage", u8);
    let rent = genesis_config.rent.clone();

    let is_alpenglow = matches.is_present("alpenglow");

    add_validator_accounts(
        &mut genesis_config,
        &mut bootstrap_validator_pubkeys.iter(),
        &mut bootstrap_validator_bls_pubkeys.unwrap_or_default().iter(),
        bootstrap_validator_lamports,
        bootstrap_validator_stake_lamports,
        commission,
        &rent,
        bootstrap_stake_authorized_pubkey.as_ref(),
    )?;

    if let Some(creation_time) = unix_timestamp_from_rfc3339_datetime(&matches, "creation_time") {
        genesis_config.creation_time = creation_time;
    }

    if let Some(faucet_pubkey) = faucet_pubkey {
        genesis_config.add_account(
            faucet_pubkey,
            AccountSharedData::new(faucet_lamports, 0, &system_program::id()),
        );
    }

    add_genesis_stake_config_account(&mut genesis_config);
    add_genesis_epoch_rewards_account(&mut genesis_config);

    if is_alpenglow {
        solana_runtime::genesis_utils::activate_all_features_alpenglow(&mut genesis_config);
    } else {
        solana_runtime::genesis_utils::activate_all_features(&mut genesis_config);
    }

    if !features_to_deactivate.is_empty() {
        solana_runtime::genesis_utils::deactivate_features(
            &mut genesis_config,
            &features_to_deactivate,
        );
    }

    if let Some(files) = matches.values_of("primordial_accounts_file") {
        for file in files {
            load_genesis_accounts(file, &mut genesis_config)?;
        }
    }

    if let Some(files) = matches.values_of("validator_accounts_file") {
        for file in files {
            load_validator_accounts(file, commission, &rent, &mut genesis_config)?;
        }
    }

    let max_genesis_archive_unpacked_size =
        value_t_or_exit!(matches, "max_genesis_archive_unpacked_size", u64);

    let issued_lamports = genesis_config
        .accounts
        .values()
        .map(|account| account.lamports)
        .sum::<u64>();

    add_genesis_stake_accounts(&mut genesis_config, issued_lamports - faucet_lamports);

    let parse_address = |address: &str, input_type: &str| {
        address.parse::<Pubkey>().unwrap_or_else(|err| {
            eprintln!("Error: invalid {input_type} {address}: {err}");
            process::exit(1);
        })
    };

    let parse_program_data = |program: &str| {
        let mut program_data = vec![];
        File::open(program)
            .and_then(|mut file| file.read_to_end(&mut program_data))
            .unwrap_or_else(|err| {
                eprintln!("Error: failed to read {program}: {err}");
                process::exit(1);
            });
        program_data
    };

    if let Some(values) = matches.values_of("bpf_program") {
        for (address, loader, program) in values.tuples() {
            let address = parse_address(address, "address");
            let loader = parse_address(loader, "loader");
            let program_data = parse_program_data(program);
            genesis_config.add_account(
                address,
                AccountSharedData::from(Account {
                    lamports: genesis_config.rent.minimum_balance(program_data.len()),
                    data: program_data,
                    executable: true,
                    owner: loader,
                    rent_epoch: 0,
                }),
            );
        }
    }

    if let Some(values) = matches.values_of("upgradeable_program") {
        for (address, loader, program, upgrade_authority) in values.tuples() {
            let address = parse_address(address, "address");
            let loader = parse_address(loader, "loader");
            let program_data_elf = parse_program_data(program);
            let upgrade_authority_address = if upgrade_authority == "none" {
                Pubkey::default()
            } else {
                upgrade_authority.parse::<Pubkey>().unwrap_or_else(|_| {
                    read_keypair_file(upgrade_authority)
                        .map(|keypair| keypair.pubkey())
                        .unwrap_or_else(|err| {
                            eprintln!(
                                "Error: invalid upgrade_authority {upgrade_authority}: {err}"
                            );
                            process::exit(1);
                        })
                })
            };

            let (programdata_address, _) =
                Pubkey::find_program_address(&[address.as_ref()], &loader);
            let mut program_data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
            program_data.extend_from_slice(&program_data_elf);
            genesis_config.add_account(
                programdata_address,
                AccountSharedData::from(Account {
                    lamports: genesis_config.rent.minimum_balance(program_data.len()),
                    data: program_data,
                    owner: loader,
                    executable: false,
                    rent_epoch: 0,
                }),
            );

            let program_data = bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();
            genesis_config.add_account(
                address,
                AccountSharedData::from(Account {
                    lamports: genesis_config.rent.minimum_balance(program_data.len()),
                    data: program_data,
                    owner: loader,
                    executable: true,
                    rent_epoch: 0,
                }),
            );
        }
    }

    agave_logger::setup();
    create_new_ledger(
        &ledger_path,
        &genesis_config,
        max_genesis_archive_unpacked_size,
        LedgerColumnOptions::default(),
    )?;

    println!("{genesis_config}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_bls_signatures::keypair::Keypair as BLSKeypair,
        solana_borsh::v1 as borsh1,
        solana_genesis_config::GenesisConfig,
        solana_stake_interface as stake,
        std::{collections::HashMap, fs::remove_file, io::Write, path::Path},
        test_case::test_case,
    };

    #[test]
    fn test_append_primordial_accounts_to_genesis() {
        // Test invalid file returns error
        assert!(load_genesis_accounts("unknownfile", &mut GenesisConfig::default()).is_err());

        let mut genesis_config = GenesisConfig::default();

        let mut genesis_accounts = HashMap::new();
        genesis_accounts.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 2,
                executable: false,
                data: String::from("aGVsbG8="),
            },
        );
        genesis_accounts.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 1,
                executable: true,
                data: String::from("aGVsbG8gd29ybGQ="),
            },
        );
        genesis_accounts.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 3,
                executable: true,
                data: String::from("bWUgaGVsbG8gdG8gd29ybGQ="),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(b"---\n").unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("test_append_primordial_accounts_to_genesis.yml");
        // Test valid file returns ok

        remove_file(path).unwrap();

        {
            // Test all accounts were added
            assert_eq!(genesis_config.accounts.len(), genesis_accounts.len());

            // Test account data matches
            for (pubkey_str, b64_account) in genesis_accounts.iter() {
                let pubkey = pubkey_str.parse().unwrap();
                assert_eq!(
                    b64_account.owner,
                    genesis_config.accounts[&pubkey].owner.to_string()
                );

                assert_eq!(
                    b64_account.balance,
                    genesis_config.accounts[&pubkey].lamports
                );

                assert_eq!(
                    b64_account.executable,
                    genesis_config.accounts[&pubkey].executable
                );

                assert_eq!(
                    b64_account.data,
                    BASE64_STANDARD.encode(&genesis_config.accounts[&pubkey].data)
                );
            }
        }

        // Test more accounts can be appended
        let mut genesis_accounts1 = HashMap::new();
        genesis_accounts1.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 6,
                executable: true,
                data: String::from("eW91IGFyZQ=="),
            },
        );
        genesis_accounts1.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 5,
                executable: false,
                data: String::from("bWV0YSBzdHJpbmc="),
            },
        );
        genesis_accounts1.insert(
            solana_pubkey::new_rand().to_string(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 10,
                executable: false,
                data: String::from("YmFzZTY0IHN0cmluZw=="),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts1).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(b"---\n").unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("test_append_primordial_accounts_to_genesis.yml");

        remove_file(path).unwrap();

        // Test total number of accounts is correct
        assert_eq!(
            genesis_config.accounts.len(),
            genesis_accounts.len() + genesis_accounts1.len()
        );

        // Test old accounts are still there
        for (pubkey_str, b64_account) in genesis_accounts.iter() {
            let pubkey = &pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[pubkey].lamports,
            );
        }

        // Test new account data matches
        for (pubkey_str, b64_account) in genesis_accounts1.iter() {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.owner,
                genesis_config.accounts[&pubkey].owner.to_string()
            );

            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                b64_account.executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                b64_account.data,
                BASE64_STANDARD.encode(&genesis_config.accounts[&pubkey].data),
            );
        }

        // Test accounts from keypairs can be appended
        let account_keypairs: Vec<_> = (0..3).map(|_| Keypair::new()).collect();
        let mut genesis_accounts2 = HashMap::new();
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[0].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 20,
                executable: true,
                data: String::from("Y2F0IGRvZw=="),
            },
        );
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[1].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 15,
                executable: false,
                data: String::from("bW9ua2V5IGVsZXBoYW50"),
            },
        );
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[2].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: solana_pubkey::new_rand().to_string(),
                balance: 30,
                executable: true,
                data: String::from("Y29tYSBtb2Nh"),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts2).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(b"---\n").unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("genesis");

        remove_file(path).unwrap();

        // Test total number of accounts is correct
        assert_eq!(
            genesis_config.accounts.len(),
            genesis_accounts.len() + genesis_accounts1.len() + genesis_accounts2.len()
        );

        // Test old accounts are still there
        for (pubkey_str, b64_account) in genesis_accounts {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );
        }

        // Test new account data matches
        for (pubkey_str, b64_account) in genesis_accounts1 {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.owner,
                genesis_config.accounts[&pubkey].owner.to_string(),
            );

            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                b64_account.executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                b64_account.data,
                BASE64_STANDARD.encode(&genesis_config.accounts[&pubkey].data),
            );
        }

        // Test account data for keypairs matches
        account_keypairs.iter().for_each(|keypair| {
            let keypair_str = serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap();
            let pubkey = keypair.pubkey();
            assert_eq!(
                genesis_accounts2[&keypair_str].owner,
                genesis_config.accounts[&pubkey].owner.to_string(),
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].data,
                BASE64_STANDARD.encode(&genesis_config.accounts[&pubkey].data),
            );
        });
    }

    #[test]
    fn test_genesis_account_struct_compatibility() {
        #[rustfmt::skip]
        let yaml_string_pubkey =
            "---\
             \n98frSc8R8toHoS3tQ1xWSvHCvGEADRM9hAm5qmUKjSDX:\
             \n  balance: 4\
             \n  owner: Gw6S9CPzR8jHku1QQMdiqcmUKjC2dhJ3gzagWduA6PGw\
             \n  data:\
             \n  executable: true\
             \n88frSc8R8toHoS3tQ1xWSvHCvGEADRM9hAm5qmUKjSDX:\
             \n  balance: 3\
             \n  owner: Gw7S9CPzR8jHku1QQMdiqcmUKjC2dhJ3gzagWduA6PGw\
             \n  data: ~\
             \n  executable: true\
             \n6s36rsNPDfRSvzwek7Ly3mQu9jUMwgqBhjePZMV6Acp4:\
             \n  balance: 2\
             \n  owner: DBC5d45LUHTCrq42ZmCdzc8A8ufwTaiYsL9pZY7KU6TR\
             \n  data: aGVsbG8=\
             \n  executable: false\
             \n8Y98svZv5sPHhQiPqZvqA5Z5djQ8hieodscvb61RskMJ:\
             \n  balance: 1\
             \n  owner: DSknYr8cPucRbx2VyssZ7Yx3iiRqNGD38VqVahkUvgV1\
             \n  data: aGVsbG8gd29ybGQ=\
             \n  executable: true";

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let path = tmpfile.path();
        let mut file = File::create(path).unwrap();
        file.write_all(yaml_string_pubkey.as_bytes()).unwrap();

        let mut genesis_config = GenesisConfig::default();
        load_genesis_accounts(path.to_str().unwrap(), &mut genesis_config).expect("genesis");
        remove_file(path).unwrap();

        assert_eq!(genesis_config.accounts.len(), 4);

        #[rustfmt::skip]
        let yaml_string_keypair =
            "---\
             \n\"[17,12,234,59,35,246,168,6,64,36,169,164,219,96,253,79,238,202,164,160,195,89,9,\
             96,179,117,255,239,32,64,124,66,233,130,19,107,172,54,86,32,119,148,4,39,199,40,122,\
             230,249,47,150,168,163,159,83,233,97,18,25,238,103,25,253,108]\":\
             \n  balance: 20\
             \n  owner: 9ZfsP6Um1KU8d5gNzTsEbSJxanKYp5EPF36qUu4FJqgp\
             \n  data: Y2F0IGRvZw==\
             \n  executable: true\
             \n\"[36,246,244,43,37,214,110,50,134,148,148,8,205,82,233,67,223,245,122,5,149,232,\
             213,125,244,182,26,29,56,224,70,45,42,163,71,62,222,33,229,54,73,136,53,174,128,103,\
             247,235,222,27,219,129,180,77,225,174,220,74,201,123,97,155,159,234]\":\
             \n  balance: 15\
             \n  owner: F9dmtjJPi8vfLu1EJN4KkyoGdXGmVfSAhxz35Qo9RDCJ\
             \n  data: bW9ua2V5IGVsZXBoYW50\
             \n  executable: false\
             \n\"[103,27,132,107,42,149,72,113,24,138,225,109,209,31,158,6,26,11,8,76,24,128,131,\
             215,156,80,251,114,103,220,111,235,56,22,87,5,209,56,53,12,224,170,10,66,82,42,11,138,\
             51,76,120,27,166,200,237,16,200,31,23,5,57,22,131,221]\":\
             \n  balance: 30
             \n  owner: AwAR5mAbNPbvQ4CvMeBxwWE8caigQoMC2chkWAbh2b9V
             \n  data: Y29tYSBtb2Nh
             \n  executable: true";

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let path = tmpfile.path();
        let mut file = File::create(path).unwrap();
        file.write_all(yaml_string_keypair.as_bytes()).unwrap();

        let mut genesis_config = GenesisConfig::default();
        load_genesis_accounts(path.to_str().unwrap(), &mut genesis_config).expect("genesis");
        remove_file(path).unwrap();

        assert_eq!(genesis_config.accounts.len(), 3);
    }

    #[test_case(true; "add bls pubkey")]
    #[test_case(false; "no bls pubkey")]
    // It's wrong to have (true, false) combination, Alpenglow requires BLS keys
    fn test_append_validator_accounts_to_genesis(add_bls_pubkey: bool) {
        // Test invalid file returns error
        assert!(load_validator_accounts(
            "unknownfile",
            100,
            &Rent::default(),
            &mut GenesisConfig::default(),
        )
        .is_err());

        let mut genesis_config = GenesisConfig::default();

        let generate_bls_pubkey = || {
            if add_bls_pubkey {
                Some(BLSKeypair::new().public.to_string())
            } else {
                None
            }
        };
        let validator_accounts = vec![
            StakedValidatorAccountInfo {
                identity_account: solana_pubkey::new_rand().to_string(),
                vote_account: solana_pubkey::new_rand().to_string(),
                stake_account: solana_pubkey::new_rand().to_string(),
                bls_pubkey: generate_bls_pubkey(),
                balance_lamports: 100000000000,
                stake_lamports: 10000000000,
            },
            StakedValidatorAccountInfo {
                identity_account: solana_pubkey::new_rand().to_string(),
                vote_account: solana_pubkey::new_rand().to_string(),
                stake_account: solana_pubkey::new_rand().to_string(),
                bls_pubkey: generate_bls_pubkey(),
                balance_lamports: 200000000000,
                stake_lamports: 20000000000,
            },
            StakedValidatorAccountInfo {
                identity_account: solana_pubkey::new_rand().to_string(),
                vote_account: solana_pubkey::new_rand().to_string(),
                stake_account: solana_pubkey::new_rand().to_string(),
                bls_pubkey: generate_bls_pubkey(),
                balance_lamports: 300000000000,
                stake_lamports: 30000000000,
            },
        ];

        let serialized = serde_yaml::to_string(&validator_accounts).unwrap();

        // write accounts to file
        let filename = if add_bls_pubkey {
            "test_append_validator_accounts_to_genesis_with_bls.yml"
        } else {
            "test_append_validator_accounts_to_genesis_without_bls.yml"
        };
        let path = Path::new(filename);
        let mut file = File::create(path).unwrap();
        file.write_all(b"validator_accounts:\n").unwrap();
        file.write_all(serialized.as_bytes()).unwrap();

        load_validator_accounts(filename, 100, &Rent::default(), &mut genesis_config)
            .expect("Failed to load validator accounts");

        remove_file(path).unwrap();

        let accounts_per_validator = 3;
        let expected_accounts_len = validator_accounts.len() * accounts_per_validator;
        {
            assert_eq!(genesis_config.accounts.len(), expected_accounts_len);

            // test account data matches
            for b64_account in validator_accounts.iter() {
                // check identity
                let identity_pk = b64_account.identity_account.parse().unwrap();
                assert_eq!(
                    system_program::id(),
                    genesis_config.accounts[&identity_pk].owner
                );
                assert_eq!(
                    b64_account.balance_lamports,
                    genesis_config.accounts[&identity_pk].lamports
                );

                // check vote account
                let vote_pk = b64_account.vote_account.parse().unwrap();
                let vote_data = genesis_config.accounts[&vote_pk].data.clone();
                let vote_state = VoteStateV4::deserialize(&vote_data, &vote_pk).unwrap();
                assert_eq!(vote_state.node_pubkey, identity_pk);
                assert_eq!(vote_state.authorized_withdrawer, identity_pk);
                let authorized_voters = &vote_state.authorized_voters;
                assert_eq!(authorized_voters.first().unwrap().1, &identity_pk);
                if add_bls_pubkey {
                    assert_eq!(
                        bls_pubkey_to_compressed_bytes(
                            &BLSPubkey::from_str(b64_account.bls_pubkey.as_ref().unwrap()).unwrap()
                        ),
                        vote_state.bls_pubkey_compressed.unwrap()
                    );
                } else {
                    assert!(b64_account.bls_pubkey.is_none());
                    assert!(vote_state.bls_pubkey_compressed.is_none());
                }

                // check stake account
                let stake_pk = b64_account.stake_account.parse().unwrap();
                assert_eq!(
                    b64_account.stake_lamports,
                    genesis_config.accounts[&stake_pk].lamports
                );

                let stake_data = genesis_config.accounts[&stake_pk].data.clone();
                let stake_state =
                    borsh1::try_from_slice_unchecked::<StakeStateV2>(&stake_data).unwrap();
                assert!(
                    matches!(stake_state, StakeStateV2::Stake(_, _, _)),
                    "Expected StakeStateV2::Stake variant"
                );

                if let StakeStateV2::Stake(meta, stake, stake_flags) = stake_state {
                    assert_eq!(meta.authorized.staker, identity_pk);
                    assert_eq!(meta.authorized.withdrawer, identity_pk);

                    assert_eq!(stake.delegation.voter_pubkey, vote_pk);
                    let stake_account = AccountSharedData::new(
                        b64_account.stake_lamports,
                        StakeStateV2::size_of(),
                        &stake::program::id(),
                    );
                    let rent_exempt_reserve =
                        &Rent::default().minimum_balance(stake_account.data().len());
                    assert_eq!(
                        stake.delegation.stake,
                        b64_account.stake_lamports - rent_exempt_reserve
                    );

                    assert_eq!(stake_flags, stake::stake_flags::StakeFlags::empty());
                }
            }
        }
    }
}
