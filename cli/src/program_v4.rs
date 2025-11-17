use {
    crate::{
        checks::*,
        cli::{CliCommand, CliCommandInfo, CliConfig, CliError, ProcessResult},
        compute_budget::{
            simulate_and_update_compute_unit_limit, ComputeUnitConfig, WithComputeUnitConfig,
        },
        feature::{status_from_account, CliFeatureStatus},
        program::calculate_max_chunk_size,
    },
    agave_feature_set::{raise_cpi_nesting_limit_to_8, FeatureSet, FEATURE_NAMES},
    clap::{value_t, App, AppSettings, Arg, ArgMatches, SubCommand},
    log::*,
    solana_account::Account,
    solana_account_decoder::{UiAccount, UiAccountEncoding, UiDataSliceConfig},
    solana_clap_utils::{
        compute_budget::{compute_unit_price_arg, ComputeUnitLimit},
        input_parsers::{pubkey_of, pubkey_of_signer, signer_of},
        input_validators::{is_valid_pubkey, is_valid_signer},
        keypair::{DefaultSigner, SignerIndex},
        offline::{OfflineArgs, DUMP_TRANSACTION_MESSAGE, SIGN_ONLY_ARG},
    },
    solana_cli_output::{
        return_signers_with_config, CliProgramId, CliProgramV4, CliProgramsV4, ReturnSignersConfig,
    },
    solana_client::{
        connection_cache::ConnectionCache,
        send_and_confirm_transactions_in_parallel::{
            send_and_confirm_transactions_in_parallel_blocking_v2, SendAndConfirmConfigV2,
        },
        tpu_client::{TpuClient, TpuClientConfig},
    },
    solana_instruction::Instruction,
    solana_loader_v4_interface::{
        instruction,
        state::{
            LoaderV4State,
            LoaderV4Status::{self, Retracted},
        },
    },
    solana_message::Message,
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionBudget, invoke_context::InvokeContext,
    },
    solana_pubkey::Pubkey,
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        filter::{Memcmp, RpcFilterType},
        request::MAX_MULTIPLE_ACCOUNTS,
    },
    solana_rpc_client_nonce_utils::blockhash_query::BlockhashQuery,
    solana_sbpf::{elf::Executable, verifier::RequisiteVerifier},
    solana_sdk_ids::{loader_v4, system_program},
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, MAX_PERMITTED_DATA_LENGTH},
    solana_transaction::Transaction,
    std::{
        cmp::Ordering,
        fs::File,
        io::{Read, Write},
        mem::size_of,
        num::Saturating,
        ops::Range,
        rc::Rc,
        sync::Arc,
    },
};

#[derive(Debug, PartialEq, Eq, Default)]
pub struct AdditionalCliConfig {
    pub use_rpc: bool,
    pub sign_only: bool,
    pub dump_transaction_message: bool,
    pub blockhash_query: BlockhashQuery,
    pub compute_unit_price: Option<u64>,
}

impl AdditionalCliConfig {
    fn from_matches(matches: &ArgMatches<'_>) -> Self {
        Self {
            use_rpc: matches.is_present("use-rpc"),
            sign_only: matches.is_present(SIGN_ONLY_ARG.name),
            dump_transaction_message: matches.is_present(DUMP_TRANSACTION_MESSAGE.name),
            blockhash_query: BlockhashQuery::new_from_matches(matches),
            compute_unit_price: value_t!(matches, "compute_unit_price", u64).ok(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProgramV4CliCommand {
    Deploy {
        additional_cli_config: AdditionalCliConfig,
        program_address: Pubkey,
        buffer_address: Option<Pubkey>,
        upload_signer_index: Option<SignerIndex>,
        authority_signer_index: SignerIndex,
        path_to_elf: Option<String>,
        upload_range: Range<Option<usize>>,
    },
    Retract {
        additional_cli_config: AdditionalCliConfig,
        program_address: Pubkey,
        authority_signer_index: SignerIndex,
        close_program_entirely: bool,
    },
    TransferAuthority {
        additional_cli_config: AdditionalCliConfig,
        program_address: Pubkey,
        authority_signer_index: SignerIndex,
        new_authority_signer_index: SignerIndex,
    },
    Finalize {
        additional_cli_config: AdditionalCliConfig,
        program_address: Pubkey,
        authority_signer_index: SignerIndex,
        next_version_signer_index: SignerIndex,
    },
    Show {
        account_pubkey: Option<Pubkey>,
        authority: Pubkey,
        all: bool,
    },
    Dump {
        account_pubkey: Option<Pubkey>,
        output_location: String,
    },
}

pub trait ProgramV4SubCommands {
    fn program_v4_subcommands(self) -> Self;
}

impl ProgramV4SubCommands for App<'_, '_> {
    fn program_v4_subcommands(self) -> Self {
        self.subcommand(
            SubCommand::with_name("program-v4")
                .about("Program V4 management")
                .setting(AppSettings::SubcommandRequiredElseHelp)
                .subcommand(
                    SubCommand::with_name("deploy")
                        .about("Deploy a new or redeploy an existing program")
                        .arg(
                            Arg::with_name("path-to-elf")
                                .index(1)
                                .value_name("PATH-TO-ELF")
                                .takes_value(true)
                                .help("./target/deploy/program.so"),
                        )
                        .arg(
                            Arg::with_name("start-offset")
                                .long("start-offset")
                                .value_name("START_OFFSET")
                                .takes_value(true)
                                .help("Optionally starts writing at this byte offset"),
                        )
                        .arg(
                            Arg::with_name("end-offset")
                                .long("end-offset")
                                .value_name("END_OFFSET")
                                .takes_value(true)
                                .help("Optionally stops writing after this byte offset"),
                        )
                        .arg(
                            Arg::with_name("program-keypair")
                                .long("program-keypair")
                                .value_name("PROGRAM_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help("Program account signer for deploying a new program"),
                        )
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .help("Program address for redeploying an existing program"),
                        )
                        .arg(
                            Arg::with_name("buffer")
                                .long("buffer")
                                .value_name("BUFFER_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help("Optional intermediate buffer account to write data to"),
                        )
                        .arg(
                            Arg::with_name("authority")
                                .long("authority")
                                .value_name("AUTHORITY_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help(
                                    "Program authority [default: the default configured keypair]",
                                ),
                        )
                        .arg(Arg::with_name("use-rpc").long("use-rpc").help(
                            "Send transactions to the configured RPC instead of validator TPUs",
                        ))
                        .offline_args()
                        .arg(compute_unit_price_arg()),
                )
                .subcommand(
                    SubCommand::with_name("retract")
                        .about("Reverse deployment or close a program entirely")
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .required(true)
                                .help("Executable program's address"),
                        )
                        .arg(
                            Arg::with_name("authority")
                                .long("authority")
                                .value_name("AUTHORITY_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help(
                                    "Program authority [default: the default configured keypair]",
                                ),
                        )
                        .arg(
                            Arg::with_name("close-program-entirely")
                                .long("close-program-entirely")
                                .help("Reset the program account and retrieve its funds"),
                        )
                        .offline_args()
                        .arg(compute_unit_price_arg()),
                )
                .subcommand(
                    SubCommand::with_name("transfer-authority")
                        .about("Transfer the authority of a program to a different address")
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .required(true)
                                .help("Executable program's address"),
                        )
                        .arg(
                            Arg::with_name("authority")
                                .long("authority")
                                .value_name("AUTHORITY_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help(
                                    "Current program authority [default: the default configured \
                                     keypair]",
                                ),
                        )
                        .arg(
                            Arg::with_name("new-authority")
                                .long("new-authority")
                                .value_name("NEW_AUTHORITY_SIGNER")
                                .takes_value(true)
                                .required(true)
                                .validator(is_valid_signer)
                                .help("New program authority"),
                        )
                        .offline_args()
                        .arg(compute_unit_price_arg()),
                )
                .subcommand(
                    SubCommand::with_name("finalize")
                        .about("Finalize a program to make it immutable")
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .required(true)
                                .help("Executable program's address"),
                        )
                        .arg(
                            Arg::with_name("authority")
                                .long("authority")
                                .value_name("AUTHORITY_SIGNER")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help(
                                    "Program authority [default: the default configured keypair]",
                                ),
                        )
                        .arg(
                            Arg::with_name("next-version")
                                .long("next-version")
                                .value_name("NEXT_VERSION")
                                .takes_value(true)
                                .validator(is_valid_signer)
                                .help(
                                    "Reserves the address and links it as the programs \
                                     next-version, which is a hint that frontends can show to \
                                     users",
                                ),
                        )
                        .offline_args()
                        .arg(compute_unit_price_arg()),
                )
                .subcommand(
                    SubCommand::with_name("show")
                        .about("Display information about a buffer or program")
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .help("Executable program's address"),
                        )
                        .arg(
                            Arg::with_name("all")
                                .long("all")
                                .conflicts_with("program-id")
                                .conflicts_with("authority")
                                .help("Show accounts for all authorities"),
                        )
                        .arg(pubkey!(
                            Arg::with_name("authority")
                                .long("authority")
                                .value_name("AUTHORITY")
                                .conflicts_with("all"),
                            "Authority [default: the default configured keypair]."
                        )),
                )
                .subcommand(
                    SubCommand::with_name("download")
                        .about("Download the executable of a program to a file")
                        .arg(
                            Arg::with_name("path-to-elf")
                                .index(1)
                                .value_name("PATH-TO-ELF")
                                .takes_value(true)
                                .help("./target/deploy/program.so"),
                        )
                        .arg(
                            Arg::with_name("program-id")
                                .long("program-id")
                                .value_name("PROGRAM_ID")
                                .takes_value(true)
                                .help("Executable program's address"),
                        ),
                ),
        )
    }
}

pub fn parse_program_v4_subcommand(
    matches: &ArgMatches<'_>,
    default_signer: &DefaultSigner,
    wallet_manager: &mut Option<Rc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let (subcommand, sub_matches) = matches.subcommand();
    let response = match (subcommand, sub_matches) {
        ("deploy", Some(matches)) => {
            let mut bulk_signers = vec![Some(
                default_signer.signer_from_path(matches, wallet_manager)?,
            )];

            let path_to_elf = matches
                .value_of("path-to-elf")
                .map(|location| location.to_string());

            let program_address = pubkey_of(matches, "program-id");
            let mut program_pubkey = if let Ok((program_signer, Some(program_pubkey))) =
                signer_of(matches, "program-keypair", wallet_manager)
            {
                bulk_signers.push(program_signer);
                Some(program_pubkey)
            } else {
                pubkey_of_signer(matches, "program-keypair", wallet_manager)?
            };

            let buffer_pubkey = if let Ok((buffer_signer, Some(buffer_pubkey))) =
                signer_of(matches, "buffer", wallet_manager)
            {
                if program_address.is_none() && program_pubkey.is_none() {
                    program_pubkey = Some(buffer_pubkey);
                }
                bulk_signers.push(buffer_signer);
                Some(buffer_pubkey)
            } else {
                pubkey_of_signer(matches, "buffer", wallet_manager)?
            };

            let (authority, authority_pubkey) = signer_of(matches, "authority", wallet_manager)?;
            bulk_signers.push(authority);

            let signer_info =
                default_signer.generate_unique_signers(bulk_signers, matches, wallet_manager)?;
            let program_signer_index = signer_info.index_of_or_none(program_pubkey);
            let buffer_signer_index = signer_info.index_of_or_none(buffer_pubkey);
            let upload_signer_index = buffer_signer_index.or(program_signer_index);
            let authority_signer_index = signer_info
                .index_of(authority_pubkey)
                .expect("Authority signer is missing");
            assert!(
                program_address.is_some() != program_signer_index.is_some(),
                "Requires either --program-keypair or --program-id",
            );

            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::from_matches(matches),
                    program_address: program_address.or(program_pubkey).unwrap(),
                    buffer_address: buffer_pubkey,
                    upload_signer_index: path_to_elf.as_ref().and(upload_signer_index),
                    authority_signer_index,
                    path_to_elf,
                    upload_range: value_t!(matches, "start-offset", usize).ok()
                        ..value_t!(matches, "end-offset", usize).ok(),
                }),
                signers: signer_info.signers,
            }
        }
        ("retract", Some(matches)) => {
            let mut bulk_signers = vec![Some(
                default_signer.signer_from_path(matches, wallet_manager)?,
            )];

            let (authority, authority_pubkey) = signer_of(matches, "authority", wallet_manager)?;
            bulk_signers.push(authority);

            let signer_info =
                default_signer.generate_unique_signers(bulk_signers, matches, wallet_manager)?;

            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Retract {
                    additional_cli_config: AdditionalCliConfig::from_matches(matches),
                    program_address: pubkey_of(matches, "program-id")
                        .expect("Program address is missing"),
                    authority_signer_index: signer_info
                        .index_of(authority_pubkey)
                        .expect("Authority signer is missing"),
                    close_program_entirely: matches.is_present("close-program-entirely"),
                }),
                signers: signer_info.signers,
            }
        }
        ("transfer-authority", Some(matches)) => {
            let mut bulk_signers = vec![Some(
                default_signer.signer_from_path(matches, wallet_manager)?,
            )];

            let (authority, authority_pubkey) = signer_of(matches, "authority", wallet_manager)?;
            bulk_signers.push(authority);

            let (new_authority, new_authority_pubkey) =
                signer_of(matches, "new-authority", wallet_manager)?;
            bulk_signers.push(new_authority);

            let signer_info =
                default_signer.generate_unique_signers(bulk_signers, matches, wallet_manager)?;

            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::TransferAuthority {
                    additional_cli_config: AdditionalCliConfig::from_matches(matches),
                    program_address: pubkey_of(matches, "program-id")
                        .expect("Program address is missing"),
                    authority_signer_index: signer_info
                        .index_of(authority_pubkey)
                        .expect("Authority signer is missing"),
                    new_authority_signer_index: signer_info
                        .index_of(new_authority_pubkey)
                        .expect("New authority signer is missing"),
                }),
                signers: signer_info.signers,
            }
        }
        ("finalize", Some(matches)) => {
            let mut bulk_signers = vec![Some(
                default_signer.signer_from_path(matches, wallet_manager)?,
            )];

            let (authority, authority_pubkey) = signer_of(matches, "authority", wallet_manager)?;
            bulk_signers.push(authority);

            if let Ok((next_version, _next_version_pubkey)) =
                signer_of(matches, "next-version", wallet_manager)
            {
                bulk_signers.push(next_version);
            }

            let signer_info =
                default_signer.generate_unique_signers(bulk_signers, matches, wallet_manager)?;
            let authority_signer_index = signer_info
                .index_of(authority_pubkey)
                .expect("Authority signer is missing");

            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Finalize {
                    additional_cli_config: AdditionalCliConfig::from_matches(matches),
                    program_address: pubkey_of(matches, "program-id")
                        .expect("Program address is missing"),
                    authority_signer_index,
                    next_version_signer_index: pubkey_of(matches, "next-version")
                        .and_then(|pubkey| signer_info.index_of(Some(pubkey)))
                        .unwrap_or(authority_signer_index),
                }),
                signers: signer_info.signers,
            }
        }
        ("show", Some(matches)) => {
            let authority =
                if let Some(authority) = pubkey_of_signer(matches, "authority", wallet_manager)? {
                    authority
                } else {
                    default_signer
                        .signer_from_path(matches, wallet_manager)?
                        .pubkey()
                };

            CliCommandInfo::without_signers(CliCommand::ProgramV4(ProgramV4CliCommand::Show {
                account_pubkey: pubkey_of(matches, "program-id"),
                authority,
                all: matches.is_present("all"),
            }))
        }
        ("download", Some(matches)) => {
            CliCommandInfo::without_signers(CliCommand::ProgramV4(ProgramV4CliCommand::Dump {
                account_pubkey: pubkey_of(matches, "program-id"),
                output_location: matches.value_of("path-to-elf").unwrap().to_string(),
            }))
        }
        _ => unreachable!(),
    };
    Ok(response)
}

pub fn process_program_v4_subcommand(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    program_subcommand: &ProgramV4CliCommand,
) -> ProcessResult {
    match program_subcommand {
        ProgramV4CliCommand::Deploy {
            additional_cli_config,
            program_address,
            buffer_address,
            upload_signer_index,
            authority_signer_index,
            path_to_elf,
            upload_range,
        } => {
            let mut program_data = Vec::new();
            if let Some(path_to_elf) = path_to_elf {
                let mut file = File::open(path_to_elf)
                    .map_err(|err| format!("Unable to open program file: {err}"))?;
                file.read_to_end(&mut program_data)
                    .map_err(|err| format!("Unable to read program file: {err}"))?;
            }
            process_deploy_program(
                rpc_client,
                config,
                additional_cli_config,
                program_address,
                buffer_address.as_ref(),
                upload_signer_index.as_ref(),
                authority_signer_index,
                &program_data,
                upload_range.clone(),
            )
        }
        ProgramV4CliCommand::Retract {
            additional_cli_config,
            program_address,
            authority_signer_index,
            close_program_entirely,
        } => process_retract_program(
            rpc_client,
            config,
            additional_cli_config,
            authority_signer_index,
            program_address,
            *close_program_entirely,
        ),
        ProgramV4CliCommand::TransferAuthority {
            additional_cli_config,
            program_address,
            authority_signer_index,
            new_authority_signer_index,
        } => process_transfer_authority_of_program(
            rpc_client,
            config,
            additional_cli_config,
            authority_signer_index,
            new_authority_signer_index,
            program_address,
        ),
        ProgramV4CliCommand::Finalize {
            additional_cli_config,
            program_address,
            authority_signer_index,
            next_version_signer_index,
        } => process_finalize_program(
            rpc_client,
            config,
            additional_cli_config,
            authority_signer_index,
            next_version_signer_index,
            program_address,
        ),
        ProgramV4CliCommand::Show {
            account_pubkey,
            authority,
            all,
        } => process_show(rpc_client, config, *account_pubkey, *authority, *all),
        ProgramV4CliCommand::Dump {
            account_pubkey,
            output_location,
        } => process_dump(rpc_client, config, *account_pubkey, output_location),
    }
}

// This function can be used for the following use-cases
// * Upload a new buffer account without deploying it (preparation for two-step redeployment)
//   - buffer_address must be `Some(program_signer.pubkey())`
//   - upload_signer_index must be `Some(program_signer_index)`
// * Upload a new program account and deploy it
//   - buffer_address must be `None`
//   - upload_signer_index must be `Some(program_signer_index)`
// * Single-step redeploy an existing program using the original program account
//   - buffer_address must be `None`
//   - upload_signer_index must be `None`
// * Single-step redeploy an existing program using a buffer account
//   - buffer_address must be `Some(buffer_signer.pubkey())`
//   - upload_signer_index must be `Some(buffer_signer_index)`
// * Two-step redeploy an existing program using a buffer account
//   - buffer_address must be `Some(buffer_signer.pubkey())`
//   - upload_signer_index must be None
pub fn process_deploy_program(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    additional_cli_config: &AdditionalCliConfig,
    program_address: &Pubkey,
    buffer_address: Option<&Pubkey>,
    upload_signer_index: Option<&SignerIndex>,
    auth_signer_index: &SignerIndex,
    program_data: &[u8],
    upload_range: Range<Option<usize>>,
) -> ProcessResult {
    let payer_pubkey = config.signers[0].pubkey();
    let authority_pubkey = config.signers[*auth_signer_index].pubkey();

    // Check requested command makes sense given the on-chain state
    let program_account = rpc_client
        .get_account_with_commitment(program_address, config.commitment)?
        .value;
    let buffer_account = if let Some(buffer_address) = buffer_address {
        rpc_client
            .get_account_with_commitment(buffer_address, config.commitment)?
            .value
    } else {
        None
    };
    let lamports_required = rpc_client.get_minimum_balance_for_rent_exemption(
        LoaderV4State::program_data_offset().saturating_add(program_data.len()),
    )?;
    let program_account_exists = program_account
        .as_ref()
        .map(|account| loader_v4::check_id(&account.owner))
        .unwrap_or(false);
    if upload_signer_index
        .map(|index| &config.signers[*index].pubkey() == program_address)
        .unwrap_or(false)
    {
        // Deploy new program
        if program_account_exists {
            return Err(
                "Program account does exist already. Did you perhaps intent to redeploy an \
                 existing program instead? Then use --program-id instead of --program-keypair."
                    .into(),
            );
        }
    } else {
        // Redeploy an existing program
        if !program_account_exists {
            return Err(
                "Program account does not exist. Did you perhaps intent to deploy a new program \
                 instead? Then use --program-keypair instead of --program-id."
                    .into(),
            );
        }
    }
    if let Some(program_account) = program_account.as_ref() {
        if !system_program::check_id(&program_account.owner)
            && !loader_v4::check_id(&program_account.owner)
        {
            return Err(format!("{program_address} is not owned by loader-v4").into());
        }
    }
    if let Some(buffer_account) = buffer_account.as_ref() {
        if !system_program::check_id(&buffer_account.owner)
            && !loader_v4::check_id(&buffer_account.owner)
        {
            return Err(format!("{} is not owned by loader-v4", buffer_address.unwrap()).into());
        }
    }

    // Download feature set
    let mut feature_set = FeatureSet::default();
    for feature_ids in FEATURE_NAMES
        .keys()
        .cloned()
        .collect::<Vec<Pubkey>>()
        .chunks(MAX_MULTIPLE_ACCOUNTS)
    {
        rpc_client
            .get_multiple_accounts(feature_ids)?
            .into_iter()
            .zip(feature_ids)
            .for_each(|(account, feature_id)| {
                let activation_slot = account.and_then(status_from_account);

                if let Some(CliFeatureStatus::Active(slot)) = activation_slot {
                    feature_set.activate(feature_id, slot);
                }
            });
    }
    let program_runtime_environment = agave_syscalls::create_program_runtime_environment_v1(
        &feature_set.runtime_features(),
        &SVMTransactionExecutionBudget::new_with_defaults(
            feature_set.is_active(&raise_cpi_nesting_limit_to_8::id()),
        ),
        true,
        false,
    )
    .unwrap();

    // Verify the program
    let upload_range =
        upload_range.start.unwrap_or(0)..upload_range.end.unwrap_or(program_data.len());
    const MAX_LEN: usize =
        (MAX_PERMITTED_DATA_LENGTH as usize).saturating_sub(LoaderV4State::program_data_offset());
    if program_data.len() > MAX_LEN {
        return Err(format!(
            "Program length {} exceeds maximum length {}",
            program_data.len(),
            MAX_LEN,
        )
        .into());
    }
    if upload_range.end > program_data.len() {
        return Err(format!(
            "Range end {} exceeds program length {}",
            upload_range.end,
            program_data.len(),
        )
        .into());
    }
    let executable =
        Executable::<InvokeContext>::from_elf(program_data, Arc::new(program_runtime_environment))
            .map_err(|err| format!("ELF error: {err}"))?;
    executable
        .verify::<RequisiteVerifier>()
        .map_err(|err| format!("ELF error: {err}"))?;

    // Create and add retract and set_program_length instructions
    let mut initial_instructions = Vec::default();
    if let Some(program_account) = program_account.as_ref() {
        if let Some(retract_instruction) =
            build_retract_instruction(program_account, program_address, &authority_pubkey)?
        {
            initial_instructions.insert(0, retract_instruction);
        }
        let (mut set_program_length_instructions, _lamports_required) =
            build_set_program_length_instructions(
                rpc_client.clone(),
                config,
                auth_signer_index,
                program_account,
                program_address,
                program_data.len() as u32,
            )?;
        if !set_program_length_instructions.is_empty() {
            initial_instructions.append(&mut set_program_length_instructions);
        }
    }

    let upload_address = buffer_address.unwrap_or(program_address);
    let (upload_account, upload_account_length) = if buffer_address.is_some() {
        (buffer_account, upload_range.len())
    } else {
        (program_account, program_data.len())
    };
    let existing_lamports = upload_account
        .as_ref()
        .map(|account| account.lamports)
        .unwrap_or(0);
    // Create and add create_buffer message
    if let Some(upload_account) = upload_account.as_ref() {
        if system_program::check_id(&upload_account.owner) {
            initial_instructions.append(&mut vec![
                system_instruction::transfer(&payer_pubkey, upload_address, lamports_required),
                system_instruction::assign(upload_address, &loader_v4::id()),
                instruction::set_program_length(
                    upload_address,
                    &authority_pubkey,
                    upload_account_length as u32,
                    &payer_pubkey,
                ),
            ]);
        }
    } else {
        initial_instructions.append(&mut instruction::create_buffer(
            &payer_pubkey,
            upload_address,
            lamports_required,
            &authority_pubkey,
            upload_account_length as u32,
            &payer_pubkey,
        ));
    }

    // Create and add write messages
    let mut write_messages = vec![];
    if upload_signer_index.is_none() {
        if upload_account.is_none() {
            return Err(format!(
                "No ELF was provided or uploaded to the account {upload_address:?}",
            )
            .into());
        }
    } else {
        if upload_range.is_empty() {
            return Err(format!("Attempting to upload empty range {upload_range:?}").into());
        }
        let first_write_message = Message::new(
            &[instruction::write(
                upload_address,
                &authority_pubkey,
                0,
                Vec::new(),
            )],
            Some(&payer_pubkey),
        );
        let chunk_size = calculate_max_chunk_size(first_write_message);
        for (chunk, i) in program_data[upload_range.clone()]
            .chunks(chunk_size)
            .zip(0usize..)
        {
            write_messages.push(vec![instruction::write(
                upload_address,
                &authority_pubkey,
                (upload_range.start as u32).saturating_add(i.saturating_mul(chunk_size) as u32),
                chunk.to_vec(),
            )]);
        }
    }

    let final_instructions = if buffer_address == Some(program_address) {
        // Upload to buffer only and skip actual deployment
        Vec::default()
    } else if let Some(buffer_address) = buffer_address {
        // Redeployment with a buffer
        vec![
            instruction::copy(
                program_address,
                &authority_pubkey,
                buffer_address,
                upload_range.start as u32,
                0,
                upload_range.len() as u32,
            ),
            instruction::deploy(program_address, &authority_pubkey),
            instruction::set_program_length(buffer_address, &authority_pubkey, 0, &payer_pubkey),
        ]
    } else {
        // Initial deployment or redeployment without a buffer
        vec![instruction::deploy(program_address, &authority_pubkey)]
    };

    send_messages(
        rpc_client,
        config,
        additional_cli_config,
        auth_signer_index,
        if initial_instructions.is_empty() {
            Vec::default()
        } else {
            vec![initial_instructions]
        },
        write_messages,
        if final_instructions.is_empty() {
            Vec::default()
        } else {
            vec![final_instructions]
        },
        lamports_required.saturating_sub(existing_lamports),
        config.output_format.formatted_string(&CliProgramId {
            program_id: program_address.to_string(),
            signature: None,
        }),
    )
}

fn process_retract_program(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    additional_cli_config: &AdditionalCliConfig,
    auth_signer_index: &SignerIndex,
    program_address: &Pubkey,
    close_program_entirely: bool,
) -> ProcessResult {
    let payer_pubkey = config.signers[0].pubkey();
    let authority_pubkey = config.signers[*auth_signer_index].pubkey();

    let Some(program_account) = rpc_client
        .get_account_with_commitment(program_address, config.commitment)?
        .value
    else {
        return Err("Program account does not exist".into());
    };
    if !loader_v4::check_id(&program_account.owner) {
        return Err(format!("{program_address} is not owned by loader-v4").into());
    }

    let mut instructions = Vec::default();
    let retract_instruction =
        build_retract_instruction(&program_account, program_address, &authority_pubkey)?;
    if let Some(retract_instruction) = retract_instruction {
        instructions.push(retract_instruction);
    }
    if close_program_entirely {
        let set_program_length_instruction =
            instruction::set_program_length(program_address, &authority_pubkey, 0, &payer_pubkey);
        instructions.push(set_program_length_instruction);
    } else if instructions.is_empty() {
        return Err("Program is retracted already".into());
    }

    send_messages(
        rpc_client,
        config,
        additional_cli_config,
        auth_signer_index,
        vec![instructions],
        Vec::default(),
        Vec::default(),
        0,
        config.output_format.formatted_string(&CliProgramId {
            program_id: program_address.to_string(),
            signature: None,
        }),
    )
}

fn process_transfer_authority_of_program(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    additional_cli_config: &AdditionalCliConfig,
    auth_signer_index: &SignerIndex,
    new_auth_signer_index: &SignerIndex,
    program_address: &Pubkey,
) -> ProcessResult {
    if let Some(program_account) = rpc_client
        .get_account_with_commitment(program_address, config.commitment)?
        .value
    {
        if !loader_v4::check_id(&program_account.owner) {
            return Err(format!("{program_address} is not owned by loader-v4").into());
        }
    } else {
        return Err(format!("Unable to find the account {program_address}").into());
    }

    let authority_pubkey = config.signers[*auth_signer_index].pubkey();
    let new_authority_pubkey = config.signers[*new_auth_signer_index].pubkey();

    let messages = vec![vec![instruction::transfer_authority(
        program_address,
        &authority_pubkey,
        &new_authority_pubkey,
    )]];

    send_messages(
        rpc_client,
        config,
        additional_cli_config,
        auth_signer_index,
        messages,
        Vec::default(),
        Vec::default(),
        0,
        config.output_format.formatted_string(&CliProgramId {
            program_id: program_address.to_string(),
            signature: None,
        }),
    )
}

fn process_finalize_program(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    additional_cli_config: &AdditionalCliConfig,
    auth_signer_index: &SignerIndex,
    next_version_signer_index: &SignerIndex,
    program_address: &Pubkey,
) -> ProcessResult {
    if let Some(program_account) = rpc_client
        .get_account_with_commitment(program_address, config.commitment)?
        .value
    {
        if !loader_v4::check_id(&program_account.owner) {
            return Err(format!("{program_address} is not owned by loader-v4").into());
        }
    } else {
        return Err(format!("Unable to find the account {program_address}").into());
    }

    let authority_pubkey = config.signers[*auth_signer_index].pubkey();
    let next_version_pubkey = config.signers[*next_version_signer_index].pubkey();

    let messages = vec![vec![instruction::finalize(
        program_address,
        &authority_pubkey,
        &next_version_pubkey,
    )]];

    send_messages(
        rpc_client,
        config,
        additional_cli_config,
        auth_signer_index,
        messages,
        Vec::default(),
        Vec::default(),
        0,
        config.output_format.formatted_string(&CliProgramId {
            program_id: program_address.to_string(),
            signature: None,
        }),
    )
}

fn process_show(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    program_address: Option<Pubkey>,
    authority: Pubkey,
    all: bool,
) -> ProcessResult {
    if let Some(program_address) = program_address {
        if let Some(account) = rpc_client
            .get_account_with_commitment(&program_address, config.commitment)?
            .value
        {
            if loader_v4::check_id(&account.owner) {
                if let Ok(state) = solana_loader_v4_program::get_state(&account.data) {
                    let status = match state.status {
                        LoaderV4Status::Retracted => "retracted",
                        LoaderV4Status::Deployed => "deployed",
                        LoaderV4Status::Finalized => "finalized",
                    };
                    Ok(config.output_format.formatted_string(&CliProgramV4 {
                        program_id: program_address.to_string(),
                        owner: account.owner.to_string(),
                        authority: state.authority_address_or_next_version.to_string(),
                        last_deploy_slot: state.slot,
                        data_len: account
                            .data
                            .len()
                            .saturating_sub(LoaderV4State::program_data_offset()),
                        status: status.to_string(),
                    }))
                } else {
                    Err(format!("{program_address} program state is invalid").into())
                }
            } else {
                Err(format!("{program_address} is not owned by loader-v4").into())
            }
        } else {
            Err(format!("Unable to find the account {program_address}").into())
        }
    } else {
        let authority_pubkey = if all { None } else { Some(authority) };
        let programs = get_programs(rpc_client, config, authority_pubkey)?;
        Ok(config.output_format.formatted_string(&programs))
    }
}

pub fn process_dump(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    account_pubkey: Option<Pubkey>,
    output_location: &str,
) -> ProcessResult {
    if let Some(account_pubkey) = account_pubkey {
        if let Some(account) = rpc_client
            .get_account_with_commitment(&account_pubkey, config.commitment)?
            .value
        {
            if loader_v4::check_id(&account.owner) {
                let mut f = File::create(output_location)?;
                f.write_all(&account.data[LoaderV4State::program_data_offset()..])?;
                Ok(format!("Wrote program to {output_location}"))
            } else {
                Err(format!("{account_pubkey} is not owned by loader-v4").into())
            }
        } else {
            Err(format!("Unable to find the account {account_pubkey}").into())
        }
    } else {
        Err("No account specified".into())
    }
}

#[allow(clippy::too_many_arguments)]
fn send_messages(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    additional_cli_config: &AdditionalCliConfig,
    auth_signer_index: &SignerIndex,
    initial_messages: Vec<Vec<Instruction>>,
    write_messages: Vec<Vec<Instruction>>,
    final_messages: Vec<Vec<Instruction>>,
    balance_needed: u64,
    ok_result: String,
) -> ProcessResult {
    let payer_pubkey = config.signers[0].pubkey();
    let blockhash = additional_cli_config
        .blockhash_query
        .get_blockhash(&rpc_client, config.commitment)?;
    let compute_unit_config = ComputeUnitConfig {
        compute_unit_price: additional_cli_config.compute_unit_price,
        compute_unit_limit: ComputeUnitLimit::Simulated,
    };
    let simulate_messages = |message_prototypes: Vec<Vec<Instruction>>| {
        let mut messages = Vec::with_capacity(message_prototypes.len());
        for instructions in message_prototypes.into_iter() {
            let mut message = Message::new_with_blockhash(
                &instructions.with_compute_unit_config(&compute_unit_config),
                Some(&payer_pubkey),
                &blockhash,
            );
            simulate_and_update_compute_unit_limit(
                &ComputeUnitLimit::Simulated,
                &rpc_client,
                &mut message,
            )?;
            messages.push(message);
        }
        Ok::<Vec<solana_message::Message>, Box<dyn std::error::Error>>(messages)
    };
    let initial_messages = simulate_messages(initial_messages)?;
    let write_messages = simulate_messages(write_messages)?;
    let final_messages = simulate_messages(final_messages)?;

    let mut fee = Saturating(0);
    for message in initial_messages.iter() {
        fee += rpc_client.get_fee_for_message(message)?;
    }
    for message in final_messages.iter() {
        fee += rpc_client.get_fee_for_message(message)?;
    }
    // Assume all write messages cost the same
    if let Some(message) = write_messages.first() {
        fee += rpc_client
            .get_fee_for_message(message)?
            .saturating_mul(write_messages.len() as u64);
    }
    check_account_for_spend_and_fee_with_commitment(
        &rpc_client,
        &payer_pubkey,
        balance_needed,
        fee.0,
        config.commitment,
    )?;

    let send_or_return_message = |message: Message| {
        let signers = (0..message.header.num_required_signatures)
            .map(|signer_index| {
                let key = message.account_keys[signer_index as usize];
                config
                    .signers
                    .iter()
                    .find(|signer| signer.pubkey() == key)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(&signers, blockhash)?;
        if additional_cli_config.sign_only {
            return_signers_with_config(
                &tx,
                &config.output_format,
                &ReturnSignersConfig {
                    dump_transaction_message: additional_cli_config.dump_transaction_message,
                },
            )
        } else {
            rpc_client
                .send_and_confirm_transaction_with_spinner_and_config(
                    &tx,
                    config.commitment,
                    config.send_transaction_config,
                )
                .map_err(|err| format!("Failed to send message: {err}").into())
                .map(|_| String::new())
        }
    };

    for message in initial_messages.into_iter() {
        let result = send_or_return_message(message)?;
        if additional_cli_config.sign_only {
            return Ok(result);
        }
    }

    if !write_messages.is_empty() {
        let connection_cache = {
            #[cfg(feature = "dev-context-only-utils")]
            let cache = ConnectionCache::new_quic_for_tests("connection_cache_cli_program_quic", 1);
            #[cfg(not(feature = "dev-context-only-utils"))]
            let cache = ConnectionCache::new_quic("connection_cache_cli_program_quic", 1);
            cache
        };
        let transaction_errors = match connection_cache {
            ConnectionCache::Udp(cache) => TpuClient::new_with_connection_cache(
                rpc_client.clone(),
                &config.websocket_url,
                TpuClientConfig::default(),
                cache,
            )?
            .send_and_confirm_messages_with_spinner(
                &write_messages,
                &[config.signers[0], config.signers[*auth_signer_index]],
            ),
            ConnectionCache::Quic(cache) => {
                let tpu_client_fut =
                    solana_client::nonblocking::tpu_client::TpuClient::new_with_connection_cache(
                        rpc_client.get_inner_client().clone(),
                        &config.websocket_url,
                        solana_client::tpu_client::TpuClientConfig::default(),
                        cache,
                    );
                let tpu_client = (!additional_cli_config.use_rpc).then(|| {
                    rpc_client
                        .runtime()
                        .block_on(tpu_client_fut)
                        .expect("Should return a valid tpu client")
                });

                send_and_confirm_transactions_in_parallel_blocking_v2(
                    rpc_client.clone(),
                    tpu_client,
                    &write_messages,
                    &[config.signers[0], config.signers[*auth_signer_index]],
                    SendAndConfirmConfigV2 {
                        resign_txs_count: Some(5),
                        with_spinner: true,
                        rpc_send_transaction_config: config.send_transaction_config,
                    },
                )
            }
        }
        .map_err(|err| format!("Data writes to account failed: {err}"))?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        if !transaction_errors.is_empty() {
            for transaction_error in &transaction_errors {
                error!("{transaction_error:?}");
            }
            return Err(format!("{} write transactions failed", transaction_errors.len()).into());
        }
    }

    for message in final_messages.into_iter() {
        let result = send_or_return_message(message)?;
        if additional_cli_config.sign_only {
            return Ok(result);
        }
    }

    Ok(ok_result)
}

fn build_retract_instruction(
    account: &Account,
    buffer_address: &Pubkey,
    authority: &Pubkey,
) -> Result<Option<Instruction>, Box<dyn std::error::Error>> {
    if !loader_v4::check_id(&account.owner) {
        return Ok(None);
    }

    if let Ok(LoaderV4State {
        slot: _,
        authority_address_or_next_version,
        status,
    }) = solana_loader_v4_program::get_state(&account.data)
    {
        if authority != authority_address_or_next_version {
            return Err(
                "Program authority does not match with the provided authority address".into(),
            );
        }

        match status {
            Retracted => Ok(None),
            LoaderV4Status::Deployed => Ok(Some(instruction::retract(buffer_address, authority))),
            LoaderV4Status::Finalized => Err("Program is immutable".into()),
        }
    } else {
        Err("Program account's state could not be deserialized".into())
    }
}

fn build_set_program_length_instructions(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    auth_signer_index: &SignerIndex,
    account: &Account,
    buffer_address: &Pubkey,
    program_data_length: u32,
) -> Result<(Vec<Instruction>, u64), Box<dyn std::error::Error>> {
    let expected_account_data_len =
        LoaderV4State::program_data_offset().saturating_add(program_data_length as usize);

    let lamports_required =
        rpc_client.get_minimum_balance_for_rent_exemption(expected_account_data_len)?;

    if !loader_v4::check_id(&account.owner) {
        return Ok((Vec::default(), lamports_required));
    }

    let payer_pubkey = config.signers[0].pubkey();
    let authority_pubkey = config.signers[*auth_signer_index].pubkey();
    let expected_account_data_len =
        LoaderV4State::program_data_offset().saturating_add(program_data_length as usize);

    let lamports_required =
        rpc_client.get_minimum_balance_for_rent_exemption(expected_account_data_len)?;

    if !loader_v4::check_id(&account.owner) {
        return Ok((Vec::default(), lamports_required));
    }

    if !account.data.is_empty() {
        if let Ok(LoaderV4State {
            slot: _,
            authority_address_or_next_version,
            status,
        }) = solana_loader_v4_program::get_state(&account.data)
        {
            if &authority_pubkey != authority_address_or_next_version {
                return Err(
                    "Program authority does not match with the provided authority address".into(),
                );
            }

            if matches!(status, LoaderV4Status::Finalized) {
                return Err("Program is immutable".into());
            }
        } else {
            return Err("Program account's state could not be deserialized".into());
        }
    }

    let set_program_length_instruction = instruction::set_program_length(
        buffer_address,
        &authority_pubkey,
        program_data_length,
        &payer_pubkey,
    );

    match account.data.len().cmp(&expected_account_data_len) {
        Ordering::Less => {
            if account.lamports < lamports_required {
                let extra_lamports_required = lamports_required.saturating_sub(account.lamports);
                Ok((
                    vec![
                        system_instruction::transfer(
                            &payer_pubkey,
                            buffer_address,
                            extra_lamports_required,
                        ),
                        set_program_length_instruction,
                    ],
                    extra_lamports_required,
                ))
            } else {
                Ok((vec![set_program_length_instruction], 0))
            }
        }
        Ordering::Equal => {
            if account.lamports < lamports_required {
                return Err("Program account has less lamports than required for its size".into());
            }
            Ok((vec![], 0))
        }
        Ordering::Greater => {
            if account.lamports < lamports_required {
                return Err("Program account has less lamports than required for its size".into());
            }
            Ok((vec![set_program_length_instruction], 0))
        }
    }
}

fn get_accounts_with_filter(
    rpc_client: Arc<RpcClient>,
    _config: &CliConfig,
    filters: Vec<RpcFilterType>,
    length: usize,
) -> Result<Vec<(Pubkey, UiAccount)>, Box<dyn std::error::Error>> {
    let results = rpc_client.get_program_ui_accounts_with_config(
        &loader_v4::id(),
        RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: Some(UiDataSliceConfig { offset: 0, length }),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        },
    )?;
    Ok(results)
}

fn get_programs(
    rpc_client: Arc<RpcClient>,
    config: &CliConfig,
    authority_pubkey: Option<Pubkey>,
) -> Result<CliProgramsV4, Box<dyn std::error::Error>> {
    let filters = if let Some(authority_pubkey) = authority_pubkey {
        vec![
            (RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                size_of::<u64>(),
                authority_pubkey.as_ref(),
            ))),
        ]
    } else {
        vec![]
    };

    let results = get_accounts_with_filter(
        rpc_client,
        config,
        filters,
        LoaderV4State::program_data_offset(),
    )?;

    let mut programs = vec![];
    for (program, account) in results.iter() {
        let data_bytes = account.data.decode().expect(
            "It should be impossible at this point for the account data not to be decodable. \
             Ensure that the account was fetched using a binary encoding.",
        );
        if let Ok(state) = solana_loader_v4_program::get_state(data_bytes.as_slice()) {
            let status = match state.status {
                LoaderV4Status::Retracted => "retracted",
                LoaderV4Status::Deployed => "deployed",
                LoaderV4Status::Finalized => "finalized",
            };
            programs.push(CliProgramV4 {
                program_id: program.to_string(),
                owner: account.owner.to_string(),
                authority: state.authority_address_or_next_version.to_string(),
                last_deploy_slot: state.slot,
                status: status.to_string(),
                data_len: data_bytes
                    .len()
                    .saturating_sub(LoaderV4State::program_data_offset()),
            });
        } else {
            return Err(format!("Error parsing Program account {program}").into());
        }
    }
    Ok(CliProgramsV4 { programs })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{clap_app::get_clap_app, cli::parse_command},
        serde_json::json,
        solana_keypair::{keypair_from_seed, read_keypair_file, write_keypair_file, Keypair},
        solana_rpc_client_api::{
            request::RpcRequest,
            response::{Response, RpcResponseContext},
        },
        std::collections::HashMap,
    };

    fn program_authority() -> solana_keypair::Keypair {
        keypair_from_seed(&[3u8; 32]).unwrap()
    }

    fn rpc_client_no_existing_program() -> RpcClient {
        RpcClient::new_mock("succeeds".to_string())
    }

    fn rpc_client_with_program_data(data: &str, loader_is_owner: bool) -> RpcClient {
        let owner = if loader_is_owner {
            "LoaderV411111111111111111111111111111111111"
        } else {
            "Vote111111111111111111111111111111111111111"
        };
        let account_info_response = json!(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None
            },
            value: json!({
                "data": [data, "base64"],
                "lamports": 42,
                "owner": owner,
                "executable": true,
                "rentEpoch": 1,
            }),
        });
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetAccountInfo, account_info_response);
        RpcClient::new_mock_with_mocks("".to_string(), mocks)
    }

    fn rpc_client_wrong_account_owner() -> RpcClient {
        rpc_client_with_program_data(
            "AAAAAAAAAADtSSjGKNHCxurpAziQWZVhKVknOlxj+TY2wUYUrIc30QAAAAAAAAAA",
            false,
        )
    }

    fn rpc_client_wrong_authority() -> RpcClient {
        rpc_client_with_program_data(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            true,
        )
    }

    fn rpc_client_with_program_retracted() -> RpcClient {
        rpc_client_with_program_data(
            "AAAAAAAAAADtSSjGKNHCxurpAziQWZVhKVknOlxj+TY2wUYUrIc30QAAAAAAAAAA",
            true,
        )
    }

    fn rpc_client_with_program_deployed() -> RpcClient {
        rpc_client_with_program_data(
            "AAAAAAAAAADtSSjGKNHCxurpAziQWZVhKVknOlxj+TY2wUYUrIc30QEAAAAAAAAA",
            true,
        )
    }

    fn rpc_client_with_program_finalized() -> RpcClient {
        rpc_client_with_program_data(
            "AAAAAAAAAADtSSjGKNHCxurpAziQWZVhKVknOlxj+TY2wUYUrIc30QIAAAAAAAAA",
            true,
        )
    }

    #[test]
    fn test_deploy() {
        let mut config = CliConfig::default();
        let mut program_data = Vec::new();
        let mut file = File::open("tests/fixtures/noop.so").unwrap();
        file.read_to_end(&mut program_data).unwrap();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let program_signer = keypair_from_seed(&[2u8; 32]).unwrap();
        let authority_signer = program_authority();

        config.signers.push(&payer);
        config.signers.push(&program_signer);
        config.signers.push(&authority_signer);

        assert!(process_deploy_program(
            Arc::new(rpc_client_no_existing_program()),
            &config,
            &AdditionalCliConfig::default(),
            &program_signer.pubkey(),
            None,
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_ok());

        assert!(process_deploy_program(
            Arc::new(rpc_client_no_existing_program()),
            &config,
            &AdditionalCliConfig::default(),
            &program_signer.pubkey(),
            Some(&program_signer.pubkey()),
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_ok());

        assert!(process_deploy_program(
            Arc::new(rpc_client_wrong_account_owner()),
            &config,
            &AdditionalCliConfig::default(),
            &program_signer.pubkey(),
            None,
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_with_program_deployed()),
            &config,
            &AdditionalCliConfig::default(),
            &program_signer.pubkey(),
            None,
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_err());
    }

    #[test]
    fn test_redeploy() {
        let mut config = CliConfig::default();
        let mut program_data = Vec::new();
        let mut file = File::open("tests/fixtures/noop.so").unwrap();
        file.read_to_end(&mut program_data).unwrap();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let program_address = Pubkey::new_unique();
        let authority_signer = program_authority();

        config.signers.push(&payer);
        config.signers.push(&authority_signer);

        // Redeploying a non-existent program should fail
        assert!(process_deploy_program(
            Arc::new(rpc_client_no_existing_program()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_with_program_retracted()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_ok());

        assert!(process_deploy_program(
            Arc::new(rpc_client_with_program_deployed()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_ok());

        assert!(process_deploy_program(
            Arc::new(rpc_client_with_program_finalized()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_wrong_account_owner()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_wrong_authority()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            None,
            None,
            &1,
            &program_data,
            None..None,
        )
        .is_err());
    }

    #[test]
    fn test_redeploy_from_source() {
        let mut config = CliConfig::default();
        let mut program_data = Vec::new();
        let mut file = File::open("tests/fixtures/noop.so").unwrap();
        file.read_to_end(&mut program_data).unwrap();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let buffer_signer = keypair_from_seed(&[2u8; 32]).unwrap();
        let program_address = Pubkey::new_unique();
        let authority_signer = program_authority();

        config.signers.push(&payer);
        config.signers.push(&buffer_signer);
        config.signers.push(&authority_signer);

        // Redeploying a non-existent program should fail
        assert!(process_deploy_program(
            Arc::new(rpc_client_no_existing_program()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            Some(&buffer_signer.pubkey()),
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_wrong_account_owner()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            Some(&buffer_signer.pubkey()),
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_err());

        assert!(process_deploy_program(
            Arc::new(rpc_client_wrong_authority()),
            &config,
            &AdditionalCliConfig::default(),
            &program_address,
            Some(&buffer_signer.pubkey()),
            Some(&1),
            &2,
            &program_data,
            None..None,
        )
        .is_err());
    }

    #[test]
    fn test_retract() {
        let mut config = CliConfig::default();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let program_signer = keypair_from_seed(&[2u8; 32]).unwrap();
        let authority_signer = program_authority();

        config.signers.push(&payer);
        config.signers.push(&authority_signer);

        for close_program_entirely in [false, true] {
            assert!(process_retract_program(
                Arc::new(rpc_client_no_existing_program()),
                &config,
                &AdditionalCliConfig::default(),
                &1,
                &program_signer.pubkey(),
                close_program_entirely,
            )
            .is_err());

            assert!(
                process_retract_program(
                    Arc::new(rpc_client_with_program_retracted()),
                    &config,
                    &AdditionalCliConfig::default(),
                    &1,
                    &program_signer.pubkey(),
                    close_program_entirely,
                )
                .is_ok()
                    == close_program_entirely
            );

            assert!(process_retract_program(
                Arc::new(rpc_client_with_program_deployed()),
                &config,
                &AdditionalCliConfig::default(),
                &1,
                &program_signer.pubkey(),
                close_program_entirely,
            )
            .is_ok());

            assert!(process_retract_program(
                Arc::new(rpc_client_with_program_finalized()),
                &config,
                &AdditionalCliConfig::default(),
                &1,
                &program_signer.pubkey(),
                close_program_entirely,
            )
            .is_err());

            assert!(process_retract_program(
                Arc::new(rpc_client_wrong_account_owner()),
                &config,
                &AdditionalCliConfig::default(),
                &1,
                &program_signer.pubkey(),
                close_program_entirely,
            )
            .is_err());

            assert!(process_retract_program(
                Arc::new(rpc_client_wrong_authority()),
                &config,
                &AdditionalCliConfig::default(),
                &1,
                &program_signer.pubkey(),
                close_program_entirely,
            )
            .is_err());
        }
    }

    #[test]
    fn test_transfer_authority() {
        let mut config = CliConfig::default();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let program_signer = keypair_from_seed(&[2u8; 32]).unwrap();
        let authority_signer = program_authority();
        let new_authority_signer = program_authority();

        config.signers.push(&payer);
        config.signers.push(&authority_signer);
        config.signers.push(&new_authority_signer);

        assert!(process_transfer_authority_of_program(
            Arc::new(rpc_client_with_program_deployed()),
            &config,
            &AdditionalCliConfig::default(),
            &1,
            &2,
            &program_signer.pubkey(),
        )
        .is_ok());
    }

    #[test]
    fn test_finalize() {
        let mut config = CliConfig::default();

        let payer = keypair_from_seed(&[1u8; 32]).unwrap();
        let program_signer = keypair_from_seed(&[2u8; 32]).unwrap();
        let authority_signer = program_authority();
        let next_version_signer = keypair_from_seed(&[4u8; 32]).unwrap();

        config.signers.push(&payer);
        config.signers.push(&authority_signer);
        config.signers.push(&next_version_signer);

        assert!(process_finalize_program(
            Arc::new(rpc_client_with_program_deployed()),
            &config,
            &AdditionalCliConfig::default(),
            &1,
            &2,
            &program_signer.pubkey(),
        )
        .is_ok());
    }

    fn make_tmp_path(name: &str) -> String {
        let out_dir = std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
        let keypair = Keypair::new();

        let path = format!("{}/tmp/{}-{}", out_dir, name, keypair.pubkey());

        // whack any possible collision
        let _ignored = std::fs::remove_dir_all(&path);
        // whack any possible collision
        let _ignored = std::fs::remove_file(&path);

        path
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_cli_parse_deploy() {
        let test_commands = get_clap_app("test", "desc", "version");

        let default_keypair = Keypair::new();
        let keypair_file = make_tmp_path("keypair_file");
        write_keypair_file(&default_keypair, &keypair_file).unwrap();
        let default_signer = DefaultSigner::new("", &keypair_file);

        let program_keypair = Keypair::new();
        let program_keypair_file = make_tmp_path("program_keypair_file");
        write_keypair_file(&program_keypair, &program_keypair_file).unwrap();

        let buffer_keypair = Keypair::new();
        let buffer_keypair_file = make_tmp_path("buffer_keypair_file");
        write_keypair_file(&buffer_keypair, &buffer_keypair_file).unwrap();

        let authority_keypair = Keypair::new();
        let authority_keypair_file = make_tmp_path("authority_keypair_file");
        write_keypair_file(&authority_keypair, &authority_keypair_file).unwrap();

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--program-keypair",
            &program_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: None,
                    upload_signer_index: Some(1),
                    authority_signer_index: 0,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&program_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--buffer",
            &program_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: Some(program_keypair.pubkey()),
                    upload_signer_index: Some(1),
                    authority_signer_index: 0,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&program_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--program-id",
            &program_keypair_file,
            "--buffer",
            &buffer_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: Some(buffer_keypair.pubkey()),
                    upload_signer_index: Some(1),
                    authority_signer_index: 0,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&buffer_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--program-keypair",
            &program_keypair_file,
            "--authority",
            &authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: None,
                    upload_signer_index: Some(1),
                    authority_signer_index: 2,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&program_keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--program-id",
            &program_keypair_file,
            "--authority",
            &authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: None,
                    upload_signer_index: None,
                    authority_signer_index: 1,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--program-id",
            &program_keypair_file,
            "--buffer",
            &buffer_keypair_file,
            "--authority",
            &authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: Some(buffer_keypair.pubkey()),
                    upload_signer_index: Some(1),
                    authority_signer_index: 2,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&buffer_keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "--program-id",
            &program_keypair_file,
            "--buffer",
            &buffer_keypair_file,
            "--authority",
            &authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    buffer_address: Some(buffer_keypair.pubkey()),
                    upload_signer_index: None,
                    authority_signer_index: 2,
                    path_to_elf: None,
                    upload_range: None..None,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&buffer_keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                ],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "deploy",
            "/Users/test/program.so",
            "--start-offset",
            "16",
            "--end-offset",
            "32",
            "--program-id",
            &program_keypair_file,
            "--use-rpc",
            "--with-compute-unit-price",
            "1",
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
                    additional_cli_config: AdditionalCliConfig {
                        use_rpc: true,
                        sign_only: false,
                        dump_transaction_message: false,
                        blockhash_query: BlockhashQuery::default(),
                        compute_unit_price: Some(1),
                    },
                    program_address: program_keypair.pubkey(),
                    buffer_address: None,
                    upload_signer_index: None,
                    authority_signer_index: 0,
                    path_to_elf: Some("/Users/test/program.so".to_string()),
                    upload_range: Some(16)..Some(32),
                }),
                signers: vec![Box::new(read_keypair_file(&keypair_file).unwrap()),],
            }
        );
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_cli_parse_retract() {
        let test_commands = get_clap_app("test", "desc", "version");

        let default_keypair = Keypair::new();
        let keypair_file = make_tmp_path("keypair_file");
        write_keypair_file(&default_keypair, &keypair_file).unwrap();
        let default_signer = DefaultSigner::new("", &keypair_file);

        let program_keypair = Keypair::new();
        let program_keypair_file = make_tmp_path("program_keypair_file");
        write_keypair_file(&program_keypair, &program_keypair_file).unwrap();

        let authority_keypair = Keypair::new();
        let authority_keypair_file = make_tmp_path("authority_keypair_file");
        write_keypair_file(&authority_keypair, &authority_keypair_file).unwrap();

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "retract",
            "--program-id",
            &program_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Retract {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    authority_signer_index: 0,
                    close_program_entirely: false,
                }),
                signers: vec![Box::new(read_keypair_file(&keypair_file).unwrap()),],
            }
        );

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "retract",
            "--program-id",
            &program_keypair_file,
            "--authority",
            &authority_keypair_file,
            "--close-program-entirely",
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Retract {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    authority_signer_index: 1,
                    close_program_entirely: true,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap())
                ],
            }
        );
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_cli_parse_transfer_authority() {
        let test_commands = get_clap_app("test", "desc", "version");

        let default_keypair = Keypair::new();
        let keypair_file = make_tmp_path("keypair_file");
        write_keypair_file(&default_keypair, &keypair_file).unwrap();
        let default_signer = DefaultSigner::new("", &keypair_file);

        let program_keypair = Keypair::new();
        let program_keypair_file = make_tmp_path("program_keypair_file");
        write_keypair_file(&program_keypair, &program_keypair_file).unwrap();

        let authority_keypair = Keypair::new();
        let authority_keypair_file = make_tmp_path("authority_keypair_file");
        write_keypair_file(&authority_keypair, &authority_keypair_file).unwrap();

        let new_authority_keypair = Keypair::new();
        let new_authority_keypair_file = make_tmp_path("new_authority_keypair_file");
        write_keypair_file(&new_authority_keypair, &new_authority_keypair_file).unwrap();

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "transfer-authority",
            "--program-id",
            &program_keypair_file,
            "--authority",
            &authority_keypair_file,
            "--new-authority",
            &new_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::TransferAuthority {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    authority_signer_index: 1,
                    new_authority_signer_index: 2,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                    Box::new(read_keypair_file(&new_authority_keypair_file).unwrap()),
                ],
            }
        );
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_cli_parse_finalize() {
        let test_commands = get_clap_app("test", "desc", "version");

        let default_keypair = Keypair::new();
        let keypair_file = make_tmp_path("keypair_file");
        write_keypair_file(&default_keypair, &keypair_file).unwrap();
        let default_signer = DefaultSigner::new("", &keypair_file);

        let program_keypair = Keypair::new();
        let program_keypair_file = make_tmp_path("program_keypair_file");
        write_keypair_file(&program_keypair, &program_keypair_file).unwrap();

        let authority_keypair = Keypair::new();
        let authority_keypair_file = make_tmp_path("authority_keypair_file");
        write_keypair_file(&authority_keypair, &authority_keypair_file).unwrap();

        let next_version_keypair = Keypair::new();
        let next_version_keypair_file = make_tmp_path("next_version_keypair_file");
        write_keypair_file(&next_version_keypair, &next_version_keypair_file).unwrap();

        let test_command = test_commands.clone().get_matches_from(vec![
            "test",
            "program-v4",
            "finalize",
            "--program-id",
            &program_keypair_file,
            "--authority",
            &authority_keypair_file,
            "--next-version",
            &next_version_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_command, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ProgramV4(ProgramV4CliCommand::Finalize {
                    additional_cli_config: AdditionalCliConfig::default(),
                    program_address: program_keypair.pubkey(),
                    authority_signer_index: 1,
                    next_version_signer_index: 2,
                }),
                signers: vec![
                    Box::new(read_keypair_file(&keypair_file).unwrap()),
                    Box::new(read_keypair_file(&authority_keypair_file).unwrap()),
                    Box::new(read_keypair_file(&next_version_keypair_file).unwrap()),
                ],
            }
        );
    }
}
