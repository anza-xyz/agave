pub use crate::core::{
    ParseAccountError, ParsableAccount, StringAmount, StringDecimals, UiFeeCalculator,
};
#[cfg(feature = "token")]
pub use crate::core::SplTokenAdditionalDataV2;
pub use solana_account_decoder_client_types::ParsedAccount;
use {
    crate::{
        parse_address_lookup_table::parse_address_lookup_table,
        parse_bpf_loader::parse_bpf_upgradeable_loader, parse_config::parse_config,
        parse_nonce::parse_nonce, parse_stake::parse_stake, parse_sysvar::parse_sysvar,
        parse_token::parse_token_v3, parse_vote::parse_vote,
    },
    inflector::Inflector,
    solana_clock::UnixTimestamp,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        address_lookup_table, bpf_loader_upgradeable, config, stake, system_program, sysvar, vote,
    },
    spl_token_2022_interface::extension::interest_bearing_mint::InterestBearingConfig,
    std::collections::HashMap,
};

pub static PARSABLE_PROGRAM_IDS: std::sync::LazyLock<HashMap<Pubkey, ParsableAccount>> =
    std::sync::LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert(
            address_lookup_table::id(),
            ParsableAccount::AddressLookupTable,
        );
        m.insert(
            bpf_loader_upgradeable::id(),
            ParsableAccount::BpfUpgradeableLoader,
        );
        m.insert(config::id(), ParsableAccount::Config);
        m.insert(system_program::id(), ParsableAccount::Nonce);
        m.insert(
            spl_token_2022_interface::id(),
            ParsableAccount::SplToken2022,
        );
        m.insert(spl_token_interface::id(), ParsableAccount::SplToken);
        m.insert(stake::id(), ParsableAccount::Stake);
        m.insert(sysvar::id(), ParsableAccount::Sysvar);
        m.insert(vote::id(), ParsableAccount::Vote);
        m
    });

#[derive(Clone, Copy, Default)]
pub struct AccountAdditionalDataV3 {
    pub spl_token_additional_data: Option<SplTokenAdditionalDataV2>,
}

#[derive(Clone, Copy, Default)]
pub struct SplTokenAdditionalData {
    pub decimals: u8,
    pub interest_bearing_config: Option<(InterestBearingConfig, UnixTimestamp)>,
}

impl SplTokenAdditionalData {
    pub fn with_decimals(decimals: u8) -> Self {
        Self {
            decimals,
            ..Default::default()
        }
    }
}

impl From<SplTokenAdditionalData> for SplTokenAdditionalDataV2 {
    fn from(v: SplTokenAdditionalData) -> Self {
        Self {
            decimals: v.decimals,
            interest_bearing_config: v.interest_bearing_config,
            scaled_ui_amount_config: None,
        }
    }
}

pub fn parse_account_data_v3(
    pubkey: &Pubkey,
    program_id: &Pubkey,
    data: &[u8],
    additional_data: Option<AccountAdditionalDataV3>,
) -> Result<ParsedAccount, ParseAccountError> {
    let program_name = PARSABLE_PROGRAM_IDS
        .get(program_id)
        .ok_or(ParseAccountError::ProgramNotParsable)?;
    let additional_data = additional_data.unwrap_or_default();
    let parsed_json = match program_name {
        ParsableAccount::AddressLookupTable => {
            serde_json::to_value(parse_address_lookup_table(data)?)?
        }
        ParsableAccount::BpfUpgradeableLoader => {
            serde_json::to_value(parse_bpf_upgradeable_loader(data)?)?
        }
        ParsableAccount::Config => serde_json::to_value(parse_config(data, pubkey)?)?,
        ParsableAccount::Nonce => serde_json::to_value(parse_nonce(data)?)?,
        ParsableAccount::SplToken | ParsableAccount::SplToken2022 => serde_json::to_value(
            parse_token_v3(data, additional_data.spl_token_additional_data.as_ref())?,
        )?,
        ParsableAccount::Stake => serde_json::to_value(parse_stake(data)?)?,
        ParsableAccount::Sysvar => serde_json::to_value(parse_sysvar(data, pubkey)?)?,
        ParsableAccount::Vote => serde_json::to_value(parse_vote(data, pubkey)?)?,
    };
    Ok(ParsedAccount {
        program: format!("{program_name:?}").to_kebab_case(),
        parsed: parsed_json,
        space: data.len() as u64,
    })
}

#[cfg(all(test, feature = "full"))]
mod test {
    use {
        super::*,
        solana_nonce::{
            state::{Data, State},
            versions::Versions,
        },
        solana_vote_interface::{
            program::id as vote_program_id,
            state::{VoteStateV4, VoteStateVersions},
        },
    };

    #[test]
    fn test_parse_account_data() {
        let account_pubkey = solana_pubkey::new_rand();
        let other_program = solana_pubkey::new_rand();
        let data = vec![0; 4];
        assert!(parse_account_data_v3(&account_pubkey, &other_program, &data, None).is_err());

        let vote_state = VoteStateV4::default();
        let mut vote_account_data: Vec<u8> = vec![0; VoteStateV4::size_of()];
        let versioned = VoteStateVersions::new_v4(vote_state);
        VoteStateV4::serialize(&versioned, &mut vote_account_data).unwrap();
        let parsed = parse_account_data_v3(
            &account_pubkey,
            &vote_program_id(),
            &vote_account_data,
            None,
        )
        .unwrap();
        assert_eq!(parsed.program, "vote".to_string());
        assert_eq!(parsed.space, VoteStateV4::size_of() as u64);

        let nonce_data = Versions::new(State::Initialized(Data::default()));
        let nonce_account_data = bincode::serialize(&nonce_data).unwrap();
        let parsed = parse_account_data_v3(
            &account_pubkey,
            &system_program::id(),
            &nonce_account_data,
            None,
        )
        .unwrap();
        assert_eq!(parsed.program, "nonce".to_string());
        assert_eq!(parsed.space, State::size() as u64);
    }
}
