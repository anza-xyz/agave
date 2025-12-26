use {
    crate::rpc::account_resolver,
    jsonrpc_core::{Error, Result},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_account_decoder::{
        encode_ui_account,
        parse_account_data::{AccountAdditionalDataV3, SplTokenAdditionalDataV2},
        parse_token::get_token_account_mint,
        UiAccount, UiAccountData, UiAccountEncoding,
    },
    solana_pubkey::Pubkey,
    solana_rpc_client_api::response::RpcKeyedAccount,
    solana_runtime::bank::Bank,
    spl_token_2022_interface::{
        extension::{
            interest_bearing_mint::InterestBearingConfig, scaled_ui_amount::ScaledUiAmountConfig,
            BaseStateWithExtensions, StateWithExtensions,
        },
        state::Mint,
    },
    std::{collections::HashMap, sync::Arc},
};

pub fn get_parsed_token_account(
    bank: &Bank,
    pubkey: &Pubkey,
    account: AccountSharedData,
    // only used for simulation results
    overwrite_accounts: Option<&HashMap<Pubkey, AccountSharedData>>,
) -> UiAccount {
    let additional_data = get_token_account_mint(account.data()).and_then(|mint_pubkey| {
        if mint_pubkey == spl_token_interface::native_mint::id() {
            Some(AccountAdditionalDataV3 {
                spl_token_additional_data: Some(SplTokenAdditionalDataV2::with_decimals(
                    spl_token_interface::native_mint::DECIMALS,
                )),
            })
        } else {
            account_resolver::get_account_from_overwrites_or_bank(
                &mint_pubkey,
                bank,
                overwrite_accounts,
            )
            .and_then(|mint_account| get_additional_mint_data(bank, mint_account.data()).ok())
            .map(|data| AccountAdditionalDataV3 {
                spl_token_additional_data: Some(data),
            })
        }
    });

    encode_ui_account(
        pubkey,
        &account,
        UiAccountEncoding::JsonParsed,
        additional_data,
        None,
    )
}

pub fn get_parsed_token_accounts<I>(
    bank: Arc<Bank>,
    keyed_accounts: I,
) -> impl Iterator<Item = RpcKeyedAccount>
where
    I: Iterator<Item = (Pubkey, AccountSharedData)>,
{
    let mut mint_data: HashMap<Pubkey, AccountAdditionalDataV3> = HashMap::new();
    keyed_accounts.filter_map(move |(pubkey, account)| {
        let additional_data = get_token_account_mint(account.data()).and_then(|mint_pubkey| {
            mint_data.get(&mint_pubkey).cloned().or_else(|| {
                let (_, data) = get_mint_owner_and_additional_data(&bank, &mint_pubkey).ok()?;
                let data = AccountAdditionalDataV3 {
                    spl_token_additional_data: Some(data),
                };
                mint_data.insert(mint_pubkey, data);
                Some(data)
            })
        });

        let maybe_encoded_account = encode_ui_account(
            &pubkey,
            &account,
            UiAccountEncoding::JsonParsed,
            additional_data,
            None,
        );
        if let UiAccountData::Json(_) = &maybe_encoded_account.data {
            Some(RpcKeyedAccount {
                pubkey: pubkey.to_string(),
                account: maybe_encoded_account,
            })
        } else {
            None
        }
    })
}

/// Analyze a mint Pubkey that may be the native_mint and get the mint-account owner (token
/// program_id) and decimals
pub(crate) fn get_mint_owner_and_additional_data(
    bank: &Bank,
    mint: &Pubkey,
) -> Result<(Pubkey, SplTokenAdditionalDataV2)> {
    if mint == &spl_token_interface::native_mint::id() {
        Ok((
            spl_token_interface::id(),
            SplTokenAdditionalDataV2::with_decimals(spl_token_interface::native_mint::DECIMALS),
        ))
    } else {
        let mint_account = bank.get_account(mint).ok_or_else(|| {
            Error::invalid_params("Invalid param: could not find mint".to_string())
        })?;
        let mint_data = get_additional_mint_data(bank, mint_account.data())?;
        Ok((*mint_account.owner(), mint_data))
    }
}

fn get_additional_mint_data(bank: &Bank, data: &[u8]) -> Result<SplTokenAdditionalDataV2> {
    StateWithExtensions::<Mint>::unpack(data)
        .map_err(|_| {
            Error::invalid_params("Invalid param: Token mint could not be unpacked".to_string())
        })
        .map(|mint| {
            let interest_bearing_config = mint
                .get_extension::<InterestBearingConfig>()
                .map(|x| (*x, bank.clock().unix_timestamp))
                .ok();
            let scaled_ui_amount_config = mint
                .get_extension::<ScaledUiAmountConfig>()
                .map(|x| (*x, bank.clock().unix_timestamp))
                .ok();
            SplTokenAdditionalDataV2 {
                decimals: mint.base.decimals,
                interest_bearing_config,
                scaled_ui_amount_config,
            }
        })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::Account as OnchainAccount,
        solana_account_decoder::UiAccountData,
        solana_program_option::COption,
        solana_program_pack::Pack,
        solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
        spl_token_2022_interface::state::{Account as TokenAccount, AccountState},
    };

    #[test]
    fn test_get_parsed_token_account_native_mint_returns_json() {
        let genesis = create_genesis_config(100);
        let bank = Bank::new_for_tests(&genesis.genesis_config);

        let owner_pubkey = solana_pubkey::new_rand();
        let mut data = vec![0; TokenAccount::get_packed_len()];
        let mut token_account = TokenAccount::unpack_unchecked(&data).unwrap();
        token_account.mint = spl_token_interface::native_mint::id();
        token_account.owner = owner_pubkey;
        token_account.amount = 1;
        token_account.state = AccountState::Initialized;
        token_account.is_native = COption::None;
        token_account.close_authority = COption::None;
        TokenAccount::pack(token_account, &mut data).unwrap();

        let onchain_account = OnchainAccount {
            lamports: 0,
            data,
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };
        let account = AccountSharedData::from(onchain_account);

        let ui = get_parsed_token_account(&bank, &solana_pubkey::new_rand(), account, None);
        match ui.data {
            UiAccountData::Json(parsed) => {
                assert_eq!(parsed.program, "spl-token".to_string());
            }
            _ => panic!("expected Json parsed data for native mint"),
        }
    }
}
