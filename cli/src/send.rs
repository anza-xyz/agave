use {
    crate::{
        cli::{
            CliCommand, CliCommandInfo, CliConfig, CliError, ProcessResult,
            message_into_signed_transaction,
        },
        compute_budget::ComputeUnitConfig,
        message_builder::MessageBuilder,
    },
    clap::{App, Arg, ArgMatches, SubCommand},
    serde::Deserialize,
    solana_clap_utils::{compute_budget::ComputeUnitLimit, keypair::DefaultSigner},
    solana_cli_output::CliSignature,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_message::VersionedMessage,
    solana_pubkey::{ParsePubkeyError, Pubkey},
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{error::Error, fs::File, rc::Rc, str::FromStr},
    thiserror::Error,
};

pub trait SendSubCommands {
    fn send_subcommands(self) -> Self;
}

impl SendSubCommands for App<'_, '_> {
    fn send_subcommands(self) -> Self {
        self.subcommand(
            SubCommand::with_name("send")
                .about("Build, sign, and send a transaction message from a JSON file")
                .arg(
                    Arg::with_name("message_file")
                        .index(1)
                        .value_name("MESSAGE_FILE")
                        .takes_value(true)
                        .required(true)
                        .help(
                            "JSON file containing instructions and optional compute-unit settings",
                        ),
                ),
        )
    }
}

pub fn parse_send(
    matches: &ArgMatches<'_>,
    default_signer: &DefaultSigner,
    wallet_manager: &mut Option<Rc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::Send {
            message_file: matches.value_of("message_file").unwrap().to_string(),
        },
        signers: vec![default_signer.signer_from_path(matches, wallet_manager)?],
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JsonMessage {
    instructions: Vec<JsonInstruction>,
    #[serde(default)]
    compute_unit_price: Option<u64>,
    #[serde(default)]
    compute_unit_limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JsonInstruction {
    program_id: String,
    #[serde(default)]
    accounts: Vec<JsonAccountMeta>,
    #[serde(default)]
    data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JsonAccountMeta {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

#[derive(Debug, Error)]
enum JsonInstructionParseError {
    #[error("invalid programId '{program_id}': {source}")]
    InvalidProgramId {
        program_id: String,
        #[source]
        source: ParsePubkeyError,
    },
    #[error("invalid pubkey '{pubkey}' for account at index {index}: {source}")]
    InvalidAccountPubkey {
        index: usize,
        pubkey: String,
        #[source]
        source: ParsePubkeyError,
    },
}

impl TryFrom<JsonInstruction> for Instruction {
    type Error = JsonInstructionParseError;

    fn try_from(instruction: JsonInstruction) -> Result<Self, Self::Error> {
        let program_id = Pubkey::from_str(&instruction.program_id).map_err(|source| {
            JsonInstructionParseError::InvalidProgramId {
                program_id: instruction.program_id,
                source,
            }
        })?;
        Ok(Self {
            program_id,
            accounts: instruction
                .accounts
                .into_iter()
                .enumerate()
                .map(|(index, account)| {
                    let pubkey = Pubkey::from_str(&account.pubkey).map_err(|source| {
                        JsonInstructionParseError::InvalidAccountPubkey {
                            index,
                            pubkey: account.pubkey,
                            source,
                        }
                    })?;
                    Ok(AccountMeta {
                        pubkey,
                        is_signer: account.is_signer,
                        is_writable: account.is_writable,
                    })
                })
                .collect::<Result<_, Self::Error>>()?,
            data: instruction.data,
        })
    }
}

fn read_message_file(message_file: &str) -> Result<JsonMessage, CliError> {
    let file = File::open(message_file).map_err(|source| CliError::UnableToOpenFile {
        path: message_file.to_string(),
        source,
    })?;
    serde_json::from_reader(file).map_err(|source| CliError::BadTransactionMessageJson {
        path: message_file.to_string(),
        source,
    })
}

fn build_message(
    json_message: JsonMessage,
    payer: &Pubkey,
    blockhash: Hash,
) -> Result<VersionedMessage, Box<dyn Error>> {
    let instructions = json_message
        .instructions
        .into_iter()
        .map(Instruction::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MessageBuilder::new(instructions)
        .with_compute_unit_config(&ComputeUnitConfig {
            compute_unit_price: json_message.compute_unit_price,
            compute_unit_limit: json_message
                .compute_unit_limit
                .map_or(ComputeUnitLimit::Default, ComputeUnitLimit::Static),
        })
        .with_blockhash(blockhash)
        .build(payer))
}

pub async fn process_send(
    rpc_client: &RpcClient,
    config: &CliConfig<'_>,
    message_file: &str,
) -> ProcessResult {
    let json_message = read_message_file(message_file)?;
    let signer = config
        .signers
        .first()
        .ok_or("Default keypair must be set to send a message")?;
    let blockhash = rpc_client.get_latest_blockhash().await?;
    let message = build_message(json_message, &signer.pubkey(), blockhash)?;
    let transaction = message_into_signed_transaction(message, &[*signer], blockhash, false)?;
    let signature = rpc_client
        .send_and_confirm_transaction_with_spinner_and_config(
            &transaction,
            config.commitment,
            config.send_transaction_config,
        )
        .await?;

    Ok(config.output_format.formatted_string(&CliSignature {
        signature: signature.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::clap_app::get_clap_app,
        solana_keypair::Keypair,
        solana_signer::Signer,
        std::io::Write,
        tempfile::{NamedTempFile, tempdir},
    };

    #[test]
    fn read_message_file_reports_open_and_schema_errors() {
        let directory = tempdir().unwrap();
        let missing_file = directory.path().join("missing.json");
        assert!(matches!(
            read_message_file(missing_file.to_str().unwrap()),
            Err(CliError::UnableToOpenFile { path, .. }) if path == missing_file.to_string_lossy()
        ));

        let mut malformed_file = NamedTempFile::new().unwrap();
        malformed_file.write_all(br#"{"instructions":{}}"#).unwrap();
        assert!(matches!(
            read_message_file(malformed_file.path().to_str().unwrap()),
            Err(CliError::BadTransactionMessageJson { path, .. })
                if path == malformed_file.path().to_string_lossy()
        ));
    }

    #[test]
    fn parse_send_uses_default_signer() {
        let signer = Keypair::new();
        let directory = tempdir().unwrap();
        let keypair_file = directory.path().join("keypair.json");
        solana_keypair::write_keypair_file(&signer, &keypair_file).unwrap();
        let default_signer = DefaultSigner::new("keypair", keypair_file.to_str().unwrap());
        let matches = get_clap_app("test", "desc", "version").get_matches_from(vec![
            "test",
            "send",
            "message.json",
        ]);

        let command_info = crate::cli::parse_command(&matches, &default_signer, &mut None).unwrap();
        assert_eq!(
            command_info.command,
            CliCommand::Send {
                message_file: "message.json".to_string(),
            }
        );
        assert_eq!(command_info.signers[0].pubkey(), signer.pubkey());
    }

    #[test]
    fn parse_json_message_uses_camel_case() {
        let program_id = Pubkey::new_unique();
        let account = Pubkey::new_unique();
        let json = format!(
            r#"{{"instructions":[{{"programId":"{program_id}","accounts":[{{"pubkey":"{account}","isSigner":true,"isWritable":false}}],"data":[1,2]}}],"computeUnitLimit":123}}"#
        );
        let message: JsonMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(message.compute_unit_limit, Some(123));
        let instruction =
            Instruction::try_from(message.instructions.into_iter().next().unwrap()).unwrap();
        assert_eq!(instruction.program_id, program_id);
        assert_eq!(instruction.accounts[0].pubkey, account);
        assert_eq!(instruction.data, [1, 2]);

        let rust_style = json.replace("programId", "program_id");
        assert!(serde_json::from_str::<JsonMessage>(&rust_style).is_err());
    }

    #[test]
    fn build_json_message_produces_v1_message() {
        let payer = Pubkey::new_unique();
        let blockhash = Hash::new_unique();
        let json = format!(
            r#"{{"instructions":[{{"programId":"{}"}}],"computeUnitPrice":10,"computeUnitLimit":100000}}"#,
            Pubkey::new_unique()
        );

        let message =
            build_message(serde_json::from_str(&json).unwrap(), &payer, blockhash).unwrap();
        let VersionedMessage::V1(message) = message else {
            panic!("expected a V1 message");
        };
        assert_eq!(message.lifetime_specifier, blockhash);
        assert_eq!(message.config.compute_unit_limit, Some(100_000));
        assert_eq!(message.config.priority_fee, Some(1));
    }
}
