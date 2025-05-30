use anyhow::anyhow;
use solana_cli_config::Config;
use solana_client::rpc_client::RpcClient;
use std::{
    io::{self, Read, Write}, str::FromStr
};

use borsh::{to_vec, BorshDeserialize, BorshSerialize};
use solana_sdk::{
    instruction::AccountMeta, message::Message, pubkey::Pubkey, signature::Keypair, signer::Signer,
    system_program, transaction::Transaction,
};

use crate::api::get_last_deployed_slot;

const OTTER_VERIFY_PROGRAMID: &str = "verifycLy8mB96wd9wqq3WDXQwM4oU6r42Th37Db9fC";
const OTTER_SIGNER: &str = "9VWiUUhgNoRwTH5NVehYJEDwcotwYX3VgW4MChiHPAqU";

pub fn prompt_user_input(message: &str) -> bool {
    let mut buffer = [0; 1];
    print!("{}", message);
    let _ = io::stdout().flush();
    io::stdin()
        .read_exact(&mut buffer)
        .expect("Unable to read user input");
    matches!(buffer[0] as char, 'Y' | 'y')
}

#[derive(BorshSerialize, BorshDeserialize)]
pub struct InputParams {
    pub version: String,
    pub git_url: String,
    pub commit: String,
    pub args: Vec<String>,
    pub deployed_slot: u64,
}

#[derive(PartialEq)]
pub enum OtterVerifyInstructions {
    Initialize,
    Update,
    Close,
}

impl OtterVerifyInstructions {
    fn get_discriminant(&self) -> Vec<u8> {
        match self {
            OtterVerifyInstructions::Initialize => vec![175, 175, 109, 31, 13, 152, 155, 237],
            OtterVerifyInstructions::Update => vec![219, 200, 88, 176, 158, 63, 253, 127],
            OtterVerifyInstructions::Close => vec![98, 165, 201, 177, 108, 65, 206, 96],
        }
    }
}

fn create_ix_data(params: &InputParams, ix: &OtterVerifyInstructions) -> Vec<u8> {
    let mut data = ix.get_discriminant(); // Discriminant for the instruction
    let params_data = to_vec(&params).expect("Unable to serialize params");
    data.extend_from_slice(&params_data);
    data
}

fn get_user_config() -> anyhow::Result<(Keypair, RpcClient)> {
    let config_file = solana_cli_config::CONFIG_FILE
        .as_ref()
        .ok_or_else(|| anyhow!("unable to get config file path"))?;
    let cli_config: Config = Config::load(config_file)?;

    let signer = solana_clap_utils::keypair::keypair_from_path(
        &Default::default(),
        &cli_config.keypair_path,
        "keypair",
        false,
    )
    .map_err(|err| anyhow!("Unable to get signer from path: {}", err))?;

    let rpc_client = RpcClient::new(cli_config.json_rpc_url.clone());
    Ok((signer, rpc_client))
}

fn process_otter_verify_ixs(
    params: &InputParams,
    pda_account: Pubkey,
    program_address: Pubkey,
    instruction: OtterVerifyInstructions,
    rpc_client: RpcClient,
) -> anyhow::Result<()> {
    let user_config = get_user_config()?;
    let signer = user_config.0;
    let signer_pubkey = signer.pubkey();
    let connection = rpc_client;

    let ix_data = if instruction != OtterVerifyInstructions::Close {
        create_ix_data(params, &instruction)
    } else {
        instruction.get_discriminant()
    };
    let otter_verify_program_id = Pubkey::from_str(OTTER_VERIFY_PROGRAMID)?;

    let mut accounts_meta_vec = vec![
        AccountMeta::new(pda_account, false),
        AccountMeta::new_readonly(signer_pubkey, true),
        AccountMeta::new_readonly(program_address, false),
    ];

    if instruction != OtterVerifyInstructions::Close {
        accounts_meta_vec.push(AccountMeta::new_readonly(system_program::ID, false));
    }

    let ix = solana_sdk::instruction::Instruction::new_with_bytes(
        otter_verify_program_id,
        &ix_data,
        accounts_meta_vec,
    );
    let message = Message::new(&[ix], Some(&signer_pubkey));

    let mut tx = Transaction::new_unsigned(message);

    tx.sign(&[&signer], connection.get_latest_blockhash()?);

    let tx_id = connection
        .send_and_confirm_transaction_with_spinner(&tx)
        .map_err(|err| {
            println!("{:?}", err);
            anyhow!("Failed to send transaction to the network.")
        })?;
    println!("Program uploaded successfully. Transaction ID: {}", tx_id);
    Ok(())
}

pub async fn upload_program(
    git_url: String,
    commit: &Option<String>,
    args: Vec<String>,
    program_address: Pubkey,
    connection_url: Option<String>,
) -> anyhow::Result<()> {
    if prompt_user_input("Do you want to update it to On-Chain Program ?. (Y/n) ") {
        println!("Uploading the program verification params to the Solana blockchain...");
        
        let cli_config = get_user_config()?;
        
        let signer_pubkey = cli_config.0.pubkey();
        let connection = match connection_url.as_deref() {
            Some("m") => {
                RpcClient::new("https://api.mainnet-beta.solana.com")
            },
            Some("d") => {
                RpcClient::new("https://api.devnet.solana.com")
            },
            Some("l") => {
                RpcClient::new("http://localhost:8899")
            },
            Some(url) => {
                RpcClient::new(url)
            },
            None => cli_config.1,
        };
        let rpc_url = connection.url();
        println!("Using connection url: {}", rpc_url);
        
        let last_deployed_slot = get_last_deployed_slot(&rpc_url, &program_address.to_string()).await
        .map_err(|err| anyhow!("Unable to get last deployed slot: {}", err))?;

        let input_params = InputParams {
            version: env!("CARGO_PKG_VERSION").to_string(),
            git_url,
            commit: commit.clone().unwrap_or_default(),
            args,
            deployed_slot: last_deployed_slot,
        };

        let otter_verify_program_id = Pubkey::from_str(OTTER_VERIFY_PROGRAMID)?;

        // Possible PDA-1: Signer is current signer then we can update the program
        let seeds: &[&[u8]; 3] = &[
            b"otter_verify",
            &signer_pubkey.to_bytes(),
            &program_address.to_bytes(),
        ];

        let (pda_account_1, _) = Pubkey::find_program_address(seeds, &otter_verify_program_id);

        // Possible PDA-2: signer is otter signer
        let otter_signer = Pubkey::from_str(OTTER_SIGNER)?;
        let seeds: &[&[u8]; 3] = &[
            b"otter_verify",
            &otter_signer.to_bytes(),
            &program_address.to_bytes(),
        ];
        let (pda_account_2, _) = Pubkey::find_program_address(seeds, &otter_verify_program_id);

        if connection.get_account(&pda_account_1).is_ok() {
            println!("Program already uploaded by the current signer. Updating the program.");
            process_otter_verify_ixs(
                &input_params,
                pda_account_1,
                program_address,
                OtterVerifyInstructions::Update,
                connection,
            )?;
        } else if connection.get_account(&pda_account_2).is_ok() {
            let wanna_create_new_pda = prompt_user_input(
                "Program already uploaded by another signer. Do you want to upload a new program? (Y/n)"
            );
            if wanna_create_new_pda {
                process_otter_verify_ixs(
                    &input_params,
                    pda_account_1,
                    program_address,
                    OtterVerifyInstructions::Initialize,
                    connection,
                )?;
            }
            return Ok(());
        } else {
            // Else Create new PDA and upload the program
            process_otter_verify_ixs(
                &input_params,
                pda_account_1,
                program_address,
                OtterVerifyInstructions::Initialize,
                connection,
            )?;
        }
    } else {
        println!("Exiting without uploading the program.");
    }

    Ok(())
}

pub async fn process_close(program_address: Pubkey) -> anyhow::Result<()> {
    let user_config = get_user_config()?;
    let signer = user_config.0;
    let signer_pubkey = signer.pubkey();
    let connection = user_config.1;
    let rpc_url = connection.url();

    let last_deployed_slot = get_last_deployed_slot(&rpc_url, &program_address.to_string()).await
        .map_err(|err| anyhow!("Unable to get last deployed slot: {}", err))?;

    let otter_verify_program_id = Pubkey::from_str(OTTER_VERIFY_PROGRAMID)?;

    let seeds: &[&[u8]; 3] = &[
        b"otter_verify",
        &signer_pubkey.to_bytes(),
        &program_address.to_bytes(),
    ];

    let (pda_account, _) = Pubkey::find_program_address(seeds, &otter_verify_program_id);

    if connection.get_account(&pda_account).is_ok() {
        process_otter_verify_ixs(
            &InputParams {
                version: "".to_string(),
                git_url: "".to_string(),
                commit: "".to_string(),
                args: vec![],
                deployed_slot: last_deployed_slot,
            },
            pda_account,
            program_address,
            OtterVerifyInstructions::Close,
            connection,
        )?;
    } else {
        return Err(anyhow!(
            "Program account does not exist. Please provide the program address not PDA address."
        ));
    }

    Ok(())
}
