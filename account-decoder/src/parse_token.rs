use {
    crate::{
        parse_account_data::{ParsableAccount, ParseAccountError, SplTokenAdditionalDataV2},
        parse_token_extension::parse_extension,
    },
    solana_program_option::COption,
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    spl_token_2022_interface::{
        extension::{BaseStateWithExtensions, StateWithExtensions},
        generic_token_account::GenericTokenAccount,
        state::{Account, AccountState, Mint, Multisig},
    },
    std::str::FromStr,
};
pub use {
    solana_account_decoder_client_types::token::{
        TokenAccountType, UiAccountState, UiMint, UiMultisig, UiTokenAccount, UiTokenAmount,
        real_number_string, real_number_string_trimmed,
    },
    spl_generic_token::{is_known_spl_token_id, spl_token_ids},
};

pub fn parse_token_v3(
    data: &[u8],
    additional_data: Option<&SplTokenAdditionalDataV2>,
) -> Result<TokenAccountType, ParseAccountError> {
    if let Ok(account) = StateWithExtensions::<Account>::unpack(data) {
        let additional_data = additional_data.as_ref().ok_or_else(|| {
            ParseAccountError::AdditionalDataMissing(
                "no mint_decimals provided to parse spl-token account".to_string(),
            )
        })?;
        let extension_types = account.get_extension_types().unwrap_or_default();
        let ui_extensions = extension_types
            .iter()
            .map(|extension_type| parse_extension::<Account>(extension_type, &account))
            .collect();
        return Ok(TokenAccountType::Account(UiTokenAccount {
            mint: account.base.mint.to_string(),
            owner: account.base.owner.to_string(),
            token_amount: token_amount_to_ui_amount_v3(account.base.amount, additional_data),
            delegate: match account.base.delegate {
                COption::Some(pubkey) => Some(pubkey.to_string()),
                COption::None => None,
            },
            state: convert_account_state(account.base.state),
            is_native: account.base.is_native(),
            rent_exempt_reserve: match account.base.is_native {
                COption::Some(reserve) => {
                    Some(token_amount_to_ui_amount_v3(reserve, additional_data))
                }
                COption::None => None,
            },
            delegated_amount: if account.base.delegate.is_none() {
                None
            } else {
                Some(token_amount_to_ui_amount_v3(
                    account.base.delegated_amount,
                    additional_data,
                ))
            },
            close_authority: match account.base.close_authority {
                COption::Some(pubkey) => Some(pubkey.to_string()),
                COption::None => None,
            },
            extensions: ui_extensions,
        }));
    }
    if let Ok(mint) = StateWithExtensions::<Mint>::unpack(data) {
        let extension_types = mint.get_extension_types().unwrap_or_default();
        let ui_extensions = extension_types
            .iter()
            .map(|extension_type| parse_extension::<Mint>(extension_type, &mint))
            .collect();
        return Ok(TokenAccountType::Mint(UiMint {
            mint_authority: match mint.base.mint_authority {
                COption::Some(pubkey) => Some(pubkey.to_string()),
                COption::None => None,
            },
            supply: mint.base.supply.to_string(),
            decimals: mint.base.decimals,
            is_initialized: mint.base.is_initialized,
            freeze_authority: match mint.base.freeze_authority {
                COption::Some(pubkey) => Some(pubkey.to_string()),
                COption::None => None,
            },
            extensions: ui_extensions,
        }));
    }
    if data.len() == Multisig::get_packed_len() {
        let multisig = Multisig::unpack(data)
            .map_err(|_| ParseAccountError::AccountNotParsable(ParsableAccount::SplToken))?;
        Ok(TokenAccountType::Multisig(UiMultisig {
            num_required_signers: multisig.m,
            num_valid_signers: multisig.n,
            is_initialized: multisig.is_initialized,
            signers: multisig
                .signers
                .iter()
                .filter_map(|pubkey| {
                    if pubkey != &Pubkey::default() {
                        Some(pubkey.to_string())
                    } else {
                        None
                    }
                })
                .collect(),
        }))
    } else {
        Err(ParseAccountError::AccountNotParsable(
            ParsableAccount::SplToken,
        ))
    }
}

pub fn convert_account_state(state: AccountState) -> UiAccountState {
    match state {
        AccountState::Uninitialized => UiAccountState::Uninitialized,
        AccountState::Initialized => UiAccountState::Initialized,
        AccountState::Frozen => UiAccountState::Frozen,
    }
}

pub fn token_amount_to_ui_amount_v3(
    amount: u64,
    additional_data: &SplTokenAdditionalDataV2,
) -> UiTokenAmount {
    let decimals = additional_data.decimals;
    let (ui_amount, ui_amount_string) = if let Some((interest_bearing_config, unix_timestamp)) =
        additional_data.interest_bearing_config
    {
        let ui_amount_string =
            interest_bearing_config.amount_to_ui_amount(amount, decimals, unix_timestamp);
        (
            ui_amount_string
                .as_ref()
                .and_then(|x| f64::from_str(x).ok()),
            ui_amount_string.unwrap_or("".to_string()),
        )
    } else if let Some((scaled_ui_amount_config, unix_timestamp)) =
        additional_data.scaled_ui_amount_config
    {
        let ui_amount_string =
            scaled_ui_amount_config.amount_to_ui_amount(amount, decimals, unix_timestamp);
        (
            ui_amount_string
                .as_ref()
                .and_then(|x| f64::from_str(x).ok()),
            ui_amount_string.unwrap_or("".to_string()),
        )
    } else {
        let ui_amount = 10_usize
            .checked_pow(decimals as u32)
            .map(|dividend| amount as f64 / dividend as f64);
        (ui_amount, real_number_string_trimmed(amount, decimals))
    };
    UiTokenAmount {
        ui_amount,
        decimals,
        amount: amount.to_string(),
        ui_amount_string,
    }
}

pub fn get_token_account_mint(data: &[u8]) -> Option<Pubkey> {
    Account::valid_account_data(data)
        .then(|| Pubkey::try_from(data.get(..32)?).ok())
        .flatten()
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::parse_token_extension::{
            UiConfidentialTransferFeeConfig, UiConfidentialTransferMint, UiGroupMemberPointer,
            UiGroupPointer, UiInterestBearingConfig, UiMemoTransfer, UiMetadataPointer,
            UiMintCloseAuthority, UiPausableConfig, UiPermanentDelegate, UiPermissionedBurnConfig,
            UiScaledUiAmountConfig, UiTokenGroup, UiTokenMetadata, UiTransferFee,
            UiTransferFeeConfig, UiTransferHook,
        },
        solana_account_decoder_client_types::token::UiExtension,
        solana_zk_sdk_pod::encryption::elgamal::{PodElGamalCiphertext, PodElGamalPubkey},
        spl_token_2022_interface::extension::{
            AccountType, BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
            confidential_transfer::ConfidentialTransferMint,
            confidential_transfer_fee::ConfidentialTransferFeeConfig,
            group_member_pointer::GroupMemberPointer, group_pointer::GroupPointer,
            immutable_owner::ImmutableOwner, interest_bearing_mint::InterestBearingConfig,
            memo_transfer::MemoTransfer, metadata_pointer::MetadataPointer,
            mint_close_authority::MintCloseAuthority, pausable::PausableConfig,
            permanent_delegate::PermanentDelegate, permissioned_burn::PermissionedBurnConfig,
            scaled_ui_amount::ScaledUiAmountConfig,
            transfer_fee::{TransferFee, TransferFeeConfig},
            transfer_hook::TransferHook,
        },
        spl_token_group_interface::state::TokenGroup,
        spl_token_metadata_interface::{
            solana_borsh::v1::get_instance_packed_len, state::TokenMetadata,
        },
    };

    const INT_SECONDS_PER_YEAR: i64 = 6 * 6 * 24 * 36524;

    #[test]
    fn test_parse_token() {
        let mint_pubkey = Pubkey::new_from_array([2; 32]);
        let owner_pubkey = Pubkey::new_from_array([3; 32]);
        let mut account_data = vec![0; Account::get_packed_len()];
        let mut account = Account::unpack_unchecked(&account_data).unwrap();
        account.mint = mint_pubkey;
        account.owner = owner_pubkey;
        account.amount = 42;
        account.state = AccountState::Initialized;
        account.is_native = COption::None;
        account.close_authority = COption::Some(owner_pubkey);
        Account::pack(account, &mut account_data).unwrap();

        assert!(parse_token_v3(&account_data, None).is_err());
        assert_eq!(
            parse_token_v3(
                &account_data,
                Some(&SplTokenAdditionalDataV2::with_decimals(2))
            )
            .unwrap(),
            TokenAccountType::Account(UiTokenAccount {
                mint: mint_pubkey.to_string(),
                owner: owner_pubkey.to_string(),
                token_amount: UiTokenAmount {
                    ui_amount: Some(0.42),
                    decimals: 2,
                    amount: "42".to_string(),
                    ui_amount_string: "0.42".to_string()
                },
                delegate: None,
                state: UiAccountState::Initialized,
                is_native: false,
                rent_exempt_reserve: None,
                delegated_amount: None,
                close_authority: Some(owner_pubkey.to_string()),
                extensions: vec![],
            }),
        );

        let mut mint_data = vec![0; Mint::get_packed_len()];
        let mut mint = Mint::unpack_unchecked(&mint_data).unwrap();
        mint.mint_authority = COption::Some(owner_pubkey);
        mint.supply = 42;
        mint.decimals = 3;
        mint.is_initialized = true;
        mint.freeze_authority = COption::Some(owner_pubkey);
        Mint::pack(mint, &mut mint_data).unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![],
            }),
        );

        let signer1 = Pubkey::new_from_array([1; 32]);
        let signer2 = Pubkey::new_from_array([2; 32]);
        let signer3 = Pubkey::new_from_array([3; 32]);
        let mut multisig_data = vec![0; Multisig::get_packed_len()];
        let mut signers = [Pubkey::default(); 11];
        signers[0] = signer1;
        signers[1] = signer2;
        signers[2] = signer3;
        let mut multisig = Multisig::unpack_unchecked(&multisig_data).unwrap();
        multisig.m = 2;
        multisig.n = 3;
        multisig.is_initialized = true;
        multisig.signers = signers;
        Multisig::pack(multisig, &mut multisig_data).unwrap();

        assert_eq!(
            parse_token_v3(&multisig_data, None).unwrap(),
            TokenAccountType::Multisig(UiMultisig {
                num_required_signers: 2,
                num_valid_signers: 3,
                is_initialized: true,
                signers: vec![
                    signer1.to_string(),
                    signer2.to_string(),
                    signer3.to_string()
                ],
            }),
        );

        let bad_data = vec![0; 4];
        assert!(parse_token_v3(&bad_data, None).is_err());
    }

    #[test]
    fn test_get_token_account_mint() {
        let mint_pubkey = Pubkey::new_from_array([2; 32]);
        let mut account_data = vec![0; Account::get_packed_len()];
        let mut account = Account::unpack_unchecked(&account_data).unwrap();
        account.mint = mint_pubkey;
        account.state = AccountState::Initialized;
        Account::pack(account, &mut account_data).unwrap();

        let expected_mint_pubkey = Pubkey::from([2; 32]);
        assert_eq!(
            get_token_account_mint(&account_data),
            Some(expected_mint_pubkey)
        );
    }

    #[test]
    fn test_ui_token_amount_real_string() {
        assert_eq!(&real_number_string(1, 0), "1");
        assert_eq!(&real_number_string_trimmed(1, 0), "1");
        let token_amount =
            token_amount_to_ui_amount_v3(1, &SplTokenAdditionalDataV2::with_decimals(0));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(1, 0)
        );
        assert_eq!(token_amount.ui_amount, Some(1.0));
        assert_eq!(&real_number_string(10, 0), "10");
        assert_eq!(&real_number_string_trimmed(10, 0), "10");
        let token_amount =
            token_amount_to_ui_amount_v3(10, &SplTokenAdditionalDataV2::with_decimals(0));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(10, 0)
        );
        assert_eq!(token_amount.ui_amount, Some(10.0));
        assert_eq!(&real_number_string(1, 9), "0.000000001");
        assert_eq!(&real_number_string_trimmed(1, 9), "0.000000001");
        let token_amount =
            token_amount_to_ui_amount_v3(1, &SplTokenAdditionalDataV2::with_decimals(9));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(1, 9)
        );
        assert_eq!(token_amount.ui_amount, Some(0.000000001));
        assert_eq!(&real_number_string(1_000_000_000, 9), "1.000000000");
        assert_eq!(&real_number_string_trimmed(1_000_000_000, 9), "1");
        let token_amount = token_amount_to_ui_amount_v3(
            1_000_000_000,
            &SplTokenAdditionalDataV2::with_decimals(9),
        );
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(1_000_000_000, 9)
        );
        assert_eq!(token_amount.ui_amount, Some(1.0));
        assert_eq!(&real_number_string(1_234_567_890, 3), "1234567.890");
        assert_eq!(&real_number_string_trimmed(1_234_567_890, 3), "1234567.89");
        let token_amount = token_amount_to_ui_amount_v3(
            1_234_567_890,
            &SplTokenAdditionalDataV2::with_decimals(3),
        );
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(1_234_567_890, 3)
        );
        assert_eq!(token_amount.ui_amount, Some(1234567.89));
        assert_eq!(
            &real_number_string(1_234_567_890, 25),
            "0.0000000000000001234567890"
        );
        assert_eq!(
            &real_number_string_trimmed(1_234_567_890, 25),
            "0.000000000000000123456789"
        );
        let token_amount = token_amount_to_ui_amount_v3(
            1_234_567_890,
            &SplTokenAdditionalDataV2::with_decimals(20),
        );
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(1_234_567_890, 20)
        );
        assert_eq!(token_amount.ui_amount, None);
    }

    #[test]
    fn test_ui_token_amount_with_interest() {
        // constant 5%
        let config = InterestBearingConfig {
            initialization_timestamp: 0.into(),
            pre_update_average_rate: 500.into(),
            last_update_timestamp: INT_SECONDS_PER_YEAR.into(),
            current_rate: 500.into(),
            ..Default::default()
        };
        let additional_data = SplTokenAdditionalDataV2 {
            decimals: 18,
            interest_bearing_config: Some((config, INT_SECONDS_PER_YEAR)),
            ..Default::default()
        };
        const ONE: u64 = 1_000_000_000_000_000_000;
        const TEN: u64 = 10_000_000_000_000_000_000;
        let token_amount = token_amount_to_ui_amount_v3(ONE, &additional_data);
        assert!(
            token_amount
                .ui_amount_string
                .starts_with("1.051271096376024117")
        );
        assert!((token_amount.ui_amount.unwrap() - 1.0512710963760241f64).abs() < f64::EPSILON);
        let token_amount = token_amount_to_ui_amount_v3(TEN, &additional_data);
        assert!(
            token_amount
                .ui_amount_string
                .starts_with("10.512710963760241611")
        );
        assert!((token_amount.ui_amount.unwrap() - 10.512710963760242f64).abs() < f64::EPSILON);

        // huge case
        let config = InterestBearingConfig {
            initialization_timestamp: 0.into(),
            pre_update_average_rate: 32767.into(),
            last_update_timestamp: 0.into(),
            current_rate: 32767.into(),
            ..Default::default()
        };
        let additional_data = SplTokenAdditionalDataV2 {
            decimals: 0,
            interest_bearing_config: Some((config, INT_SECONDS_PER_YEAR * 1_000)),
            ..Default::default()
        };
        let token_amount = token_amount_to_ui_amount_v3(u64::MAX, &additional_data);
        assert_eq!(token_amount.ui_amount, Some(f64::INFINITY));
        assert_eq!(token_amount.ui_amount_string, "inf");
    }

    #[test]
    fn test_ui_token_amount_with_multiplier() {
        // 2x multiplier
        let config = ScaledUiAmountConfig {
            new_multiplier: 2f64.into(),
            ..Default::default()
        };
        let additional_data = SplTokenAdditionalDataV2 {
            decimals: 18,
            scaled_ui_amount_config: Some((config, 0)),
            ..Default::default()
        };
        const ONE: u64 = 1_000_000_000_000_000_000;
        const TEN: u64 = 10_000_000_000_000_000_000;
        let token_amount = token_amount_to_ui_amount_v3(ONE, &additional_data);
        assert_eq!(token_amount.ui_amount_string, "2");
        assert!(token_amount.ui_amount_string.starts_with("2"));
        assert!((token_amount.ui_amount.unwrap() - 2.0).abs() < f64::EPSILON);
        let token_amount = token_amount_to_ui_amount_v3(TEN, &additional_data);
        assert!(token_amount.ui_amount_string.starts_with("20"));
        assert!((token_amount.ui_amount.unwrap() - 20.0).abs() < f64::EPSILON);

        // huge case
        let config = ScaledUiAmountConfig {
            new_multiplier: f64::INFINITY.into(),
            ..Default::default()
        };
        let additional_data = SplTokenAdditionalDataV2 {
            decimals: 0,
            scaled_ui_amount_config: Some((config, 0)),
            ..Default::default()
        };
        let token_amount = token_amount_to_ui_amount_v3(u64::MAX, &additional_data);
        assert_eq!(token_amount.ui_amount, Some(f64::INFINITY));
        assert_eq!(token_amount.ui_amount_string, "inf");
    }

    #[test]
    fn test_ui_token_amount_real_string_zero() {
        assert_eq!(&real_number_string(0, 0), "0");
        assert_eq!(&real_number_string_trimmed(0, 0), "0");
        let token_amount =
            token_amount_to_ui_amount_v3(0, &SplTokenAdditionalDataV2::with_decimals(0));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(0, 0)
        );
        assert_eq!(token_amount.ui_amount, Some(0.0));
        assert_eq!(&real_number_string(0, 9), "0.000000000");
        assert_eq!(&real_number_string_trimmed(0, 9), "0");
        let token_amount =
            token_amount_to_ui_amount_v3(0, &SplTokenAdditionalDataV2::with_decimals(9));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(0, 9)
        );
        assert_eq!(token_amount.ui_amount, Some(0.0));
        assert_eq!(&real_number_string(0, 25), "0.0000000000000000000000000");
        assert_eq!(&real_number_string_trimmed(0, 25), "0");
        let token_amount =
            token_amount_to_ui_amount_v3(0, &SplTokenAdditionalDataV2::with_decimals(20));
        assert_eq!(
            token_amount.ui_amount_string,
            real_number_string_trimmed(0, 20)
        );
        assert_eq!(token_amount.ui_amount, None);
    }

    #[test]
    fn test_parse_token_account_with_extensions() {
        let mint_pubkey = Pubkey::new_from_array([2; 32]);
        let owner_pubkey = Pubkey::new_from_array([3; 32]);

        let account_base = Account {
            mint: mint_pubkey,
            owner: owner_pubkey,
            amount: 42,
            state: AccountState::Initialized,
            is_native: COption::None,
            close_authority: COption::Some(owner_pubkey),
            delegate: COption::None,
            delegated_amount: 0,
        };
        let account_size = ExtensionType::try_calculate_account_len::<Account>(&[
            ExtensionType::ImmutableOwner,
            ExtensionType::MemoTransfer,
        ])
        .unwrap();
        let mut account_data = vec![0; account_size];
        let mut account_state =
            StateWithExtensionsMut::<Account>::unpack_uninitialized(&mut account_data).unwrap();

        account_state.base = account_base;
        account_state.pack_base();
        account_state.init_account_type().unwrap();

        assert!(parse_token_v3(&account_data, None).is_err());
        assert_eq!(
            parse_token_v3(
                &account_data,
                Some(&SplTokenAdditionalDataV2::with_decimals(2))
            )
            .unwrap(),
            TokenAccountType::Account(UiTokenAccount {
                mint: mint_pubkey.to_string(),
                owner: owner_pubkey.to_string(),
                token_amount: UiTokenAmount {
                    ui_amount: Some(0.42),
                    decimals: 2,
                    amount: "42".to_string(),
                    ui_amount_string: "0.42".to_string()
                },
                delegate: None,
                state: UiAccountState::Initialized,
                is_native: false,
                rent_exempt_reserve: None,
                delegated_amount: None,
                close_authority: Some(owner_pubkey.to_string()),
                extensions: vec![],
            }),
        );

        let mut account_data = vec![0; account_size];
        let mut account_state =
            StateWithExtensionsMut::<Account>::unpack_uninitialized(&mut account_data).unwrap();

        account_state.base = account_base;
        account_state.pack_base();
        account_state.init_account_type().unwrap();

        account_state
            .init_extension::<ImmutableOwner>(true)
            .unwrap();
        let memo_transfer = account_state.init_extension::<MemoTransfer>(true).unwrap();
        memo_transfer.require_incoming_transfer_memos = true.into();

        assert!(parse_token_v3(&account_data, None).is_err());
        assert_eq!(
            parse_token_v3(
                &account_data,
                Some(&SplTokenAdditionalDataV2::with_decimals(2))
            )
            .unwrap(),
            TokenAccountType::Account(UiTokenAccount {
                mint: mint_pubkey.to_string(),
                owner: owner_pubkey.to_string(),
                token_amount: UiTokenAmount {
                    ui_amount: Some(0.42),
                    decimals: 2,
                    amount: "42".to_string(),
                    ui_amount_string: "0.42".to_string()
                },
                delegate: None,
                state: UiAccountState::Initialized,
                is_native: false,
                rent_exempt_reserve: None,
                delegated_amount: None,
                close_authority: Some(owner_pubkey.to_string()),
                extensions: vec![
                    UiExtension::ImmutableOwner,
                    UiExtension::MemoTransfer(UiMemoTransfer {
                        require_incoming_transfer_memos: true,
                    }),
                ],
            }),
        );
    }

    #[test]
    fn test_parse_token_mint_with_extensions() {
        let owner_pubkey = Pubkey::new_from_array([3; 32]);
        let mint_size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::MintCloseAuthority])
                .unwrap();
        let mint_base = Mint {
            mint_authority: COption::Some(owner_pubkey),
            supply: 42,
            decimals: 3,
            is_initialized: true,
            freeze_authority: COption::Some(owner_pubkey),
        };
        let mut mint_data = vec![0; mint_size];
        let mut mint_state =
            StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();

        mint_state.base = mint_base;
        mint_state.pack_base();
        mint_state.init_account_type().unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![],
            }),
        );

        let mut mint_data = vec![0; mint_size];
        let mut mint_state =
            StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();

        let mint_close_authority = mint_state
            .init_extension::<MintCloseAuthority>(true)
            .unwrap();
        mint_close_authority.close_authority = owner_pubkey.into();

        mint_state.base = mint_base;
        mint_state.pack_base();
        mint_state.init_account_type().unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![UiExtension::MintCloseAuthority(UiMintCloseAuthority {
                    close_authority: Some(owner_pubkey.to_string()),
                })],
            }),
        );

        // Negative case: a close authority left at its default should parse to
        // `close_authority: None` rather than the default pubkey.
        let mut mint_data = vec![0; mint_size];
        let mut mint_state =
            StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();

        mint_state
            .init_extension::<MintCloseAuthority>(true)
            .unwrap();

        mint_state.base = mint_base;
        mint_state.pack_base();
        mint_state.init_account_type().unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![UiExtension::MintCloseAuthority(UiMintCloseAuthority {
                    close_authority: None,
                })],
            }),
        );
    }

    #[test]
    fn test_parse_token_mint_with_permissioned_burn() {
        let owner_pubkey = Pubkey::new_from_array([3; 32]);
        let authority_pubkey = Pubkey::new_from_array([4; 32]);
        let mint_size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::PermissionedBurn])
                .unwrap();
        let mint_base = Mint {
            mint_authority: COption::Some(owner_pubkey),
            supply: 42,
            decimals: 3,
            is_initialized: true,
            freeze_authority: COption::Some(owner_pubkey),
        };
        let mut mint_data = vec![0; mint_size];
        let mut mint_state =
            StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();

        let permissioned_burn = mint_state
            .init_extension::<PermissionedBurnConfig>(true)
            .unwrap();
        permissioned_burn.authority = authority_pubkey.into();

        mint_state.base = mint_base;
        mint_state.pack_base();
        mint_state.init_account_type().unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![UiExtension::PermissionedBurnConfig(
                    UiPermissionedBurnConfig {
                        authority: Some(authority_pubkey.to_string()),
                    }
                )],
            }),
        );

        // Negative case: a permissioned burn config with no authority set should
        // parse to `authority: None` rather than the default pubkey.
        let mut mint_data = vec![0; mint_size];
        let mut mint_state =
            StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();

        mint_state
            .init_extension::<PermissionedBurnConfig>(true)
            .unwrap();

        mint_state.base = mint_base;
        mint_state.pack_base();
        mint_state.init_account_type().unwrap();

        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            TokenAccountType::Mint(UiMint {
                mint_authority: Some(owner_pubkey.to_string()),
                supply: 42.to_string(),
                decimals: 3,
                is_initialized: true,
                freeze_authority: Some(owner_pubkey.to_string()),
                extensions: vec![UiExtension::PermissionedBurnConfig(
                    UiPermissionedBurnConfig { authority: None }
                )],
            }),
        );
    }

    // Shared mint base used by the per-extension decoder tests below. Its
    // authorities are distinct from the extension authorities so a mix-up would
    // surface in the assertions.
    fn extension_mint_base() -> Mint {
        let owner = Pubkey::new_from_array([3; 32]);
        Mint {
            mint_authority: COption::Some(owner),
            supply: 42,
            decimals: 3,
            is_initialized: true,
            freeze_authority: COption::Some(owner),
        }
    }

    fn expected_mint(extensions: Vec<UiExtension>) -> TokenAccountType {
        let owner = Pubkey::new_from_array([3; 32]);
        TokenAccountType::Mint(UiMint {
            mint_authority: Some(owner.to_string()),
            supply: 42.to_string(),
            decimals: 3,
            is_initialized: true,
            freeze_authority: Some(owner.to_string()),
            extensions,
        })
    }

    // Packs a mint of the given total account length, running `init` to write
    // the extension(s) before the base and account type are finalized.
    fn build_mint_data<F>(account_size: usize, init: F) -> Vec<u8>
    where
        F: FnOnce(&mut StateWithExtensionsMut<Mint>),
    {
        let mut mint_data = vec![0; account_size];
        {
            let mut mint_state =
                StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data).unwrap();
            init(&mut mint_state);
            mint_state.base = extension_mint_base();
            mint_state.pack_base();
            mint_state.init_account_type().unwrap();
        }
        mint_data
    }

    #[test]
    fn test_parse_mint_transfer_fee_config() {
        let config_authority = Pubkey::new_from_array([4; 32]);
        let withdraw_authority = Pubkey::new_from_array([5; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::TransferFeeConfig])
                .unwrap();

        // Authorities set.
        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<TransferFeeConfig>(true).unwrap();
            ext.transfer_fee_config_authority = config_authority.into();
            ext.withdraw_withheld_authority = withdraw_authority.into();
            ext.withheld_amount = 0u64.into();
            ext.older_transfer_fee = TransferFee {
                epoch: 1u64.into(),
                maximum_fee: 100u64.into(),
                transfer_fee_basis_points: 10u16.into(),
            };
            ext.newer_transfer_fee = TransferFee {
                epoch: 2u64.into(),
                maximum_fee: 200u64.into(),
                transfer_fee_basis_points: 20u16.into(),
            };
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TransferFeeConfig(UiTransferFeeConfig {
                transfer_fee_config_authority: Some(config_authority.to_string()),
                withdraw_withheld_authority: Some(withdraw_authority.to_string()),
                withheld_amount: 0,
                older_transfer_fee: UiTransferFee {
                    epoch: 1,
                    maximum_fee: 100,
                    transfer_fee_basis_points: 10,
                },
                newer_transfer_fee: UiTransferFee {
                    epoch: 2,
                    maximum_fee: 200,
                    transfer_fee_basis_points: 20,
                },
            })]),
        );

        // Both authorities left at their null defaults.
        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<TransferFeeConfig>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TransferFeeConfig(UiTransferFeeConfig {
                transfer_fee_config_authority: None,
                withdraw_withheld_authority: None,
                withheld_amount: 0,
                older_transfer_fee: UiTransferFee {
                    epoch: 0,
                    maximum_fee: 0,
                    transfer_fee_basis_points: 0,
                },
                newer_transfer_fee: UiTransferFee {
                    epoch: 0,
                    maximum_fee: 0,
                    transfer_fee_basis_points: 0,
                },
            })]),
        );
    }

    #[test]
    fn test_parse_mint_interest_bearing_config() {
        let rate_authority = Pubkey::new_from_array([4; 32]);
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[
            ExtensionType::InterestBearingConfig,
        ])
        .unwrap();

        // Rate authority set.
        let mint_data = build_mint_data(size, |state| {
            let ext = state
                .init_extension::<InterestBearingConfig>(true)
                .unwrap();
            ext.rate_authority = rate_authority.into();
            ext.initialization_timestamp = 100i64.into();
            ext.pre_update_average_rate = 200i16.into();
            ext.last_update_timestamp = 300i64.into();
            ext.current_rate = 400i16.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::InterestBearingConfig(
                UiInterestBearingConfig {
                    rate_authority: Some(rate_authority.to_string()),
                    initialization_timestamp: 100,
                    pre_update_average_rate: 200,
                    last_update_timestamp: 300,
                    current_rate: 400,
                }
            )]),
        );

        // Rate authority left at its null default.
        let mint_data = build_mint_data(size, |state| {
            state
                .init_extension::<InterestBearingConfig>(true)
                .unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::InterestBearingConfig(
                UiInterestBearingConfig {
                    rate_authority: None,
                    initialization_timestamp: 0,
                    pre_update_average_rate: 0,
                    last_update_timestamp: 0,
                    current_rate: 0,
                }
            )]),
        );
    }

    #[test]
    fn test_parse_mint_permanent_delegate() {
        let delegate = Pubkey::new_from_array([4; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::PermanentDelegate])
                .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<PermanentDelegate>(true).unwrap();
            ext.delegate = delegate.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::PermanentDelegate(UiPermanentDelegate {
                delegate: Some(delegate.to_string()),
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<PermanentDelegate>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::PermanentDelegate(UiPermanentDelegate {
                delegate: None,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_confidential_transfer_mint() {
        let authority = Pubkey::new_from_array([4; 32]);
        let auditor = PodElGamalPubkey([9u8; 32]);
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[
            ExtensionType::ConfidentialTransferMint,
        ])
        .unwrap();

        // Authority and auditor ElGamal pubkey set.
        let mint_data = build_mint_data(size, |state| {
            let ext = state
                .init_extension::<ConfidentialTransferMint>(true)
                .unwrap();
            ext.authority = authority.into();
            ext.auto_approve_new_accounts = true.into();
            ext.auditor_elgamal_pubkey = auditor.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ConfidentialTransferMint(
                UiConfidentialTransferMint {
                    authority: Some(authority.to_string()),
                    auto_approve_new_accounts: true,
                    auditor_elgamal_pubkey: Some(auditor.to_string()),
                }
            )]),
        );

        // Authority and auditor left at their null defaults.
        let mint_data = build_mint_data(size, |state| {
            state
                .init_extension::<ConfidentialTransferMint>(true)
                .unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ConfidentialTransferMint(
                UiConfidentialTransferMint {
                    authority: None,
                    auto_approve_new_accounts: false,
                    auditor_elgamal_pubkey: None,
                }
            )]),
        );
    }

    #[test]
    fn test_parse_mint_confidential_transfer_fee_config() {
        let authority = Pubkey::new_from_array([4; 32]);
        let withdraw_elgamal = PodElGamalPubkey([9u8; 32]);
        let empty_withheld = PodElGamalCiphertext([0u8; 64]).to_string();
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[
            ExtensionType::ConfidentialTransferFeeConfig,
        ])
        .unwrap();

        // Authority and withdraw-withheld ElGamal pubkey set.
        let mint_data = build_mint_data(size, |state| {
            let ext = state
                .init_extension::<ConfidentialTransferFeeConfig>(true)
                .unwrap();
            ext.authority = authority.into();
            ext.withdraw_withheld_authority_elgamal_pubkey = withdraw_elgamal;
            ext.harvest_to_mint_enabled = true.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ConfidentialTransferFeeConfig(
                UiConfidentialTransferFeeConfig {
                    authority: Some(authority.to_string()),
                    withdraw_withheld_authority_elgamal_pubkey: Some(withdraw_elgamal.to_string()),
                    harvest_to_mint_enabled: true,
                    withheld_amount: empty_withheld.clone(),
                }
            )]),
        );

        // Authority left at its null default. The withdraw-withheld ElGamal
        // pubkey is a plain (non-nullable) field, so the decoder always wraps it
        // in `Some`; a zeroed value therefore decodes to the all-zero base64
        // string rather than `None`.
        let mint_data = build_mint_data(size, |state| {
            state
                .init_extension::<ConfidentialTransferFeeConfig>(true)
                .unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ConfidentialTransferFeeConfig(
                UiConfidentialTransferFeeConfig {
                    authority: None,
                    withdraw_withheld_authority_elgamal_pubkey: Some(
                        PodElGamalPubkey([0u8; 32]).to_string()
                    ),
                    harvest_to_mint_enabled: false,
                    withheld_amount: empty_withheld,
                }
            )]),
        );
    }

    #[test]
    fn test_parse_mint_metadata_pointer() {
        let authority = Pubkey::new_from_array([4; 32]);
        let metadata_address = Pubkey::new_from_array([5; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::MetadataPointer])
                .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<MetadataPointer>(true).unwrap();
            ext.authority = authority.into();
            ext.metadata_address = metadata_address.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::MetadataPointer(UiMetadataPointer {
                authority: Some(authority.to_string()),
                metadata_address: Some(metadata_address.to_string()),
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<MetadataPointer>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::MetadataPointer(UiMetadataPointer {
                authority: None,
                metadata_address: None,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_token_metadata() {
        let update_authority = Pubkey::new_from_array([4; 32]);
        let metadata_mint = Pubkey::new_from_array([5; 32]);

        let build = |update_authority: MintMetadataAuthority| {
            let token_metadata = TokenMetadata {
                update_authority: match update_authority {
                    MintMetadataAuthority::Set(pubkey) => pubkey.into(),
                    MintMetadataAuthority::Null => Pubkey::default().into(),
                },
                mint: metadata_mint,
                name: "name".to_string(),
                symbol: "sym".to_string(),
                uri: "uri".to_string(),
                additional_metadata: vec![],
            };
            let variable_len = get_instance_packed_len(&token_metadata).unwrap();
            let account_size = Account::LEN
                + std::mem::size_of::<AccountType>()
                + std::mem::size_of::<ExtensionType>()
                + std::mem::size_of::<u16>()
                + variable_len;
            build_mint_data(account_size, |state| {
                state
                    .init_variable_len_extension(&token_metadata, false)
                    .unwrap();
            })
        };

        let mint_data = build(MintMetadataAuthority::Set(update_authority));
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TokenMetadata(UiTokenMetadata {
                update_authority: Some(update_authority.to_string()),
                mint: metadata_mint.to_string(),
                name: "name".to_string(),
                symbol: "sym".to_string(),
                uri: "uri".to_string(),
                additional_metadata: vec![],
            })]),
        );

        let mint_data = build(MintMetadataAuthority::Null);
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TokenMetadata(UiTokenMetadata {
                update_authority: None,
                mint: metadata_mint.to_string(),
                name: "name".to_string(),
                symbol: "sym".to_string(),
                uri: "uri".to_string(),
                additional_metadata: vec![],
            })]),
        );
    }

    #[test]
    fn test_parse_mint_transfer_hook() {
        let authority = Pubkey::new_from_array([4; 32]);
        let program_id = Pubkey::new_from_array([5; 32]);
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::TransferHook])
            .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<TransferHook>(true).unwrap();
            ext.authority = authority.into();
            ext.program_id = program_id.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TransferHook(UiTransferHook {
                authority: Some(authority.to_string()),
                program_id: Some(program_id.to_string()),
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<TransferHook>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TransferHook(UiTransferHook {
                authority: None,
                program_id: None,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_group_pointer() {
        let authority = Pubkey::new_from_array([4; 32]);
        let group_address = Pubkey::new_from_array([5; 32]);
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::GroupPointer])
            .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<GroupPointer>(true).unwrap();
            ext.authority = authority.into();
            ext.group_address = group_address.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::GroupPointer(UiGroupPointer {
                authority: Some(authority.to_string()),
                group_address: Some(group_address.to_string()),
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<GroupPointer>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::GroupPointer(UiGroupPointer {
                authority: None,
                group_address: None,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_group_member_pointer() {
        let authority = Pubkey::new_from_array([4; 32]);
        let member_address = Pubkey::new_from_array([5; 32]);
        let size = ExtensionType::try_calculate_account_len::<Mint>(&[
            ExtensionType::GroupMemberPointer,
        ])
        .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<GroupMemberPointer>(true).unwrap();
            ext.authority = authority.into();
            ext.member_address = member_address.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::GroupMemberPointer(UiGroupMemberPointer {
                authority: Some(authority.to_string()),
                member_address: Some(member_address.to_string()),
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<GroupMemberPointer>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::GroupMemberPointer(UiGroupMemberPointer {
                authority: None,
                member_address: None,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_token_group() {
        let update_authority = Pubkey::new_from_array([4; 32]);
        let group_mint = Pubkey::new_from_array([5; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::TokenGroup]).unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<TokenGroup>(true).unwrap();
            ext.update_authority = update_authority.into();
            ext.mint = group_mint;
            ext.size = 1u64.into();
            ext.max_size = 10u64.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TokenGroup(UiTokenGroup {
                update_authority: Some(update_authority.to_string()),
                mint: group_mint.to_string(),
                size: 1,
                max_size: 10,
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<TokenGroup>(true).unwrap();
            ext.mint = group_mint;
            ext.max_size = 10u64.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::TokenGroup(UiTokenGroup {
                update_authority: None,
                mint: group_mint.to_string(),
                size: 0,
                max_size: 10,
            })]),
        );
    }

    #[test]
    fn test_parse_mint_scaled_ui_amount_config() {
        let authority = Pubkey::new_from_array([4; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::ScaledUiAmount])
                .unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<ScaledUiAmountConfig>(true).unwrap();
            ext.authority = authority.into();
            ext.multiplier = 2f64.into();
            ext.new_multiplier_effective_timestamp = 50i64.into();
            ext.new_multiplier = 3f64.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ScaledUiAmountConfig(
                UiScaledUiAmountConfig {
                    authority: Some(authority.to_string()),
                    multiplier: "2".to_string(),
                    new_multiplier_effective_timestamp: 50,
                    new_multiplier: "3".to_string(),
                }
            )]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<ScaledUiAmountConfig>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::ScaledUiAmountConfig(
                UiScaledUiAmountConfig {
                    authority: None,
                    multiplier: "0".to_string(),
                    new_multiplier_effective_timestamp: 0,
                    new_multiplier: "0".to_string(),
                }
            )]),
        );
    }

    #[test]
    fn test_parse_mint_pausable_config() {
        let authority = Pubkey::new_from_array([4; 32]);
        let size =
            ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::Pausable]).unwrap();

        let mint_data = build_mint_data(size, |state| {
            let ext = state.init_extension::<PausableConfig>(true).unwrap();
            ext.authority = authority.into();
            ext.paused = true.into();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::PausableConfig(UiPausableConfig {
                authority: Some(authority.to_string()),
                paused: true,
            })]),
        );

        let mint_data = build_mint_data(size, |state| {
            state.init_extension::<PausableConfig>(true).unwrap();
        });
        assert_eq!(
            parse_token_v3(&mint_data, None).unwrap(),
            expected_mint(vec![UiExtension::PausableConfig(UiPausableConfig {
                authority: None,
                paused: false,
            })]),
        );
    }

    enum MintMetadataAuthority {
        Set(Pubkey),
        Null,
    }
}
