pub use solana_transaction_status_client_types::ParsedInstruction;
use {
    crate::{
        parse_address_lookup_table::parse_address_lookup_table,
        parse_associated_token::parse_associated_token,
        parse_bpf_loader::{parse_bpf_loader, parse_bpf_upgradeable_loader},
        parse_stake::parse_stake,
        parse_system::parse_system,
        parse_token::parse_token,
        parse_vote::parse_vote,
    },
    inflector::Inflector,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    solana_account_decoder::parse_token::spl_token_ids,
    solana_message::{AccountKeys, compiled_instruction::CompiledInstruction},
    solana_pubkey::Pubkey,
    solana_sdk_ids::{address_lookup_table, stake, system_program, vote},
    std::{
        collections::HashMap,
        str::{Utf8Error, from_utf8},
    },
    thiserror::Error,
};

static PARSABLE_PROGRAM_IDS: std::sync::LazyLock<HashMap<Pubkey, ParsableProgram>> =
    std::sync::LazyLock::new(|| {
        [
            (
                address_lookup_table::id(),
                ParsableProgram::AddressLookupTable,
            ),
            (
                spl_associated_token_account_interface::program::id(),
                ParsableProgram::SplAssociatedTokenAccount,
            ),
            (spl_memo_interface::v1::id(), ParsableProgram::SplMemo),
            (spl_memo_interface::v3::id(), ParsableProgram::SplMemo),
            (solana_sdk_ids::bpf_loader::id(), ParsableProgram::BpfLoader),
            (
                solana_sdk_ids::bpf_loader_upgradeable::id(),
                ParsableProgram::BpfUpgradeableLoader,
            ),
            (stake::id(), ParsableProgram::Stake),
            (system_program::id(), ParsableProgram::System),
            (vote::id(), ParsableProgram::Vote),
        ]
        .into_iter()
        .chain(
            spl_token_ids()
                .into_iter()
                .map(|spl_token_id| (spl_token_id, ParsableProgram::SplToken)),
        )
        .collect()
    });

#[derive(Error, Debug)]
pub enum ParseInstructionError {
    #[error("{0:?} instruction not parsable")]
    InstructionNotParsable(ParsableProgram),

    #[error("{0:?} instruction key mismatch")]
    InstructionKeyMismatch(ParsableProgram),

    #[error("Program not parsable")]
    ProgramNotParsable,

    #[error("Internal error, please report")]
    SerdeJsonError(#[from] serde_json::error::Error),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedInstructionEnum {
    #[serde(rename = "type")]
    pub instruction_type: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub info: Value,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ParsableProgram {
    AddressLookupTable,
    SplAssociatedTokenAccount,
    SplMemo,
    SplToken,
    BpfLoader,
    BpfUpgradeableLoader,
    Stake,
    System,
    Vote,
}

pub fn parse(
    program_id: &Pubkey,
    instruction: &CompiledInstruction,
    account_keys: &AccountKeys,
    stack_height: Option<u32>,
) -> Result<ParsedInstruction, ParseInstructionError> {
    let program_name = PARSABLE_PROGRAM_IDS
        .get(program_id)
        .ok_or(ParseInstructionError::ProgramNotParsable)?;
    let parsed_json = match program_name {
        ParsableProgram::AddressLookupTable => {
            serde_json::to_value(parse_address_lookup_table(instruction, account_keys)?)?
        }
        ParsableProgram::SplAssociatedTokenAccount => {
            serde_json::to_value(parse_associated_token(instruction, account_keys)?)?
        }
        ParsableProgram::SplMemo => parse_memo(instruction)?,
        ParsableProgram::SplToken => serde_json::to_value(parse_token(instruction, account_keys)?)?,
        ParsableProgram::BpfLoader => {
            serde_json::to_value(parse_bpf_loader(instruction, account_keys)?)?
        }
        ParsableProgram::BpfUpgradeableLoader => {
            serde_json::to_value(parse_bpf_upgradeable_loader(instruction, account_keys)?)?
        }
        ParsableProgram::Stake => serde_json::to_value(parse_stake(instruction, account_keys)?)?,
        ParsableProgram::System => serde_json::to_value(parse_system(instruction, account_keys)?)?,
        ParsableProgram::Vote => serde_json::to_value(parse_vote(instruction, account_keys)?)?,
    };
    Ok(ParsedInstruction {
        program: format!("{program_name:?}").to_kebab_case(),
        program_id: program_id.to_string(),
        parsed: parsed_json,
        stack_height,
    })
}

fn parse_memo(instruction: &CompiledInstruction) -> Result<Value, ParseInstructionError> {
    parse_memo_data(&instruction.data)
        .map(Value::String)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplMemo))
}

pub fn parse_memo_data(data: &[u8]) -> Result<String, Utf8Error> {
    from_utf8(data).map(|s| s.to_string())
}

pub(crate) fn check_num_accounts(
    accounts: &[u8],
    num: usize,
    parsable_program: ParsableProgram,
) -> Result<(), ParseInstructionError> {
    if accounts.len() < num {
        Err(ParseInstructionError::InstructionKeyMismatch(
            parsable_program,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {super::*, serde_json::json};

    #[test]
    fn test_parse() {
        let no_keys = AccountKeys::new(&[], None);
        let memo_instruction = CompiledInstruction {
            program_id_index: 0,
            accounts: vec![],
            data: vec![240, 159, 166, 150],
        };
        assert_eq!(
            parse(
                &spl_memo_interface::v1::id(),
                &memo_instruction,
                &no_keys,
                None
            )
            .unwrap(),
            ParsedInstruction {
                program: "spl-memo".to_string(),
                program_id: spl_memo_interface::v1::id().to_string(),
                parsed: json!("🦖"),
                stack_height: None,
            }
        );
        assert_eq!(
            parse(
                &spl_memo_interface::v3::id(),
                &memo_instruction,
                &no_keys,
                Some(1)
            )
            .unwrap(),
            ParsedInstruction {
                program: "spl-memo".to_string(),
                program_id: spl_memo_interface::v3::id().to_string(),
                parsed: json!("🦖"),
                stack_height: Some(1),
            }
        );

        let non_parsable_program_id = Pubkey::from([1; 32]);
        assert!(parse(&non_parsable_program_id, &memo_instruction, &no_keys, None).is_err());
    }

    #[test]
    fn test_parse_memo() {
        let good_memo = "good memo".to_string();
        assert_eq!(
            parse_memo(&CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: good_memo.as_bytes().to_vec(),
            })
            .unwrap(),
            Value::String(good_memo),
        );

        let bad_memo = vec![128u8];
        assert!(std::str::from_utf8(&bad_memo).is_err());
        assert!(
            parse_memo(&CompiledInstruction {
                program_id_index: 0,
                data: bad_memo,
                accounts: vec![],
            })
            .is_err(),
        );
    }
}
