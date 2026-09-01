use {
    crate::{
        checks::{check_account_for_balance_with_commitment, get_fee_for_messages},
        cli::CliError,
        compute_budget::{UpdateComputeUnitLimitResult, simulate_and_update_compute_unit_limit},
        stake, vote,
    },
    clap::ArgMatches,
    solana_clap_utils::{
        compute_budget::ComputeUnitLimit, input_parsers::lamports_of_sol, offline::SIGN_ONLY_ARG,
    },
    solana_cli_output::display::build_balance_message,
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SpendAmount {
    All,
    Available,
    Some(u64),
    RentExempt,
    AllForAccountCreation { create_account_min_balance: u64 },
}

impl Default for SpendAmount {
    fn default() -> Self {
        Self::Some(u64::default())
    }
}

impl SpendAmount {
    pub fn new(amount: Option<u64>, sign_only: bool) -> Result<Self, CliError> {
        match amount {
            Some(lamports) => Ok(Self::Some(lamports)),
            None if !sign_only => Ok(Self::All),
            _ => Err(CliError::BadParameter(
                "ALL amount not supported for sign-only operations".to_string(),
            )),
        }
    }

    pub fn new_from_matches(matches: &ArgMatches<'_>, name: &str) -> Result<Self, CliError> {
        let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
        let amount = lamports_of_sol(matches, name);
        if amount.is_some() {
            return SpendAmount::new(amount, sign_only);
        }
        match matches.value_of(name).unwrap_or("ALL") {
            "ALL" if !sign_only => Ok(SpendAmount::All),
            "AVAILABLE" if !sign_only => Ok(SpendAmount::Available),
            _ => Err(CliError::BadParameter(
                "Only specific amounts are supported for sign-only operations".to_string(),
            )),
        }
    }
}

struct SpendAndFee {
    spend: u64,
    fee: u64,
}

pub async fn resolve_spend_tx_and_check_account_balance<F>(
    rpc_client: &RpcClient,
    sign_only: bool,
    amount: SpendAmount,
    blockhash: &Hash,
    from_pubkey: &Pubkey,
    compute_unit_limit: ComputeUnitLimit,
    build_message: F,
    commitment: CommitmentConfig,
) -> Result<(Message, u64), CliError>
where
    F: Fn(u64) -> Message,
{
    resolve_spend_tx_and_check_account_balances(
        rpc_client,
        sign_only,
        amount,
        blockhash,
        from_pubkey,
        from_pubkey,
        compute_unit_limit,
        build_message,
        commitment,
    )
    .await
}

pub async fn resolve_spend_tx_and_check_account_balances<F>(
    rpc_client: &RpcClient,
    sign_only: bool,
    amount: SpendAmount,
    blockhash: &Hash,
    from_pubkey: &Pubkey,
    fee_pubkey: &Pubkey,
    compute_unit_limit: ComputeUnitLimit,
    build_message: F,
    commitment: CommitmentConfig,
) -> Result<(Message, u64), CliError>
where
    F: Fn(u64) -> Message,
{
    if sign_only {
        let (message, SpendAndFee { spend, fee: _ }) = resolve_spend_message(
            rpc_client,
            amount,
            None,
            0,
            from_pubkey,
            fee_pubkey,
            0,
            compute_unit_limit,
            build_message,
        )
        .await?;
        Ok((message, spend))
    } else {
        let account = rpc_client
            .get_account_with_commitment(from_pubkey, commitment)
            .await?
            .value
            .unwrap_or_default();
        let mut from_balance = account.lamports;
        let from_rent_exempt_minimum =
            if amount == SpendAmount::RentExempt || amount == SpendAmount::Available {
                rpc_client
                    .get_minimum_balance_for_rent_exemption(account.data.len())
                    .await?
            } else {
                0
            };
        if amount == SpendAmount::RentExempt && account.owner == solana_sdk_ids::vote::id() {
            // On top of the rent-exempt minimum, the vote program also reserves the rewards that
            // are pending distribution to the account's stake delegators.
            let pending_delegator_rewards =
                vote::get_pending_delegator_rewards(&account, from_pubkey)?;
            from_balance = from_balance.saturating_sub(pending_delegator_rewards);
        }
        if amount == SpendAmount::Available && account.owner == solana_sdk_ids::stake::id() {
            let state = stake::get_account_stake_state(
                rpc_client,
                from_pubkey,
                account,
                true,
                None,
                false,
                None,
            )
            .await?;
            let mut subtract_rent_exempt_minimum = false;
            if let Some(active_stake) = state.active_stake {
                from_balance = from_balance.saturating_sub(active_stake);
                subtract_rent_exempt_minimum = true;
            }
            if let Some(activating_stake) = state.activating_stake {
                from_balance = from_balance.saturating_sub(activating_stake);
                subtract_rent_exempt_minimum = true;
            }
            if subtract_rent_exempt_minimum {
                from_balance = from_balance.saturating_sub(from_rent_exempt_minimum);
            }
        }
        let (message, SpendAndFee { spend, fee }) = resolve_spend_message(
            rpc_client,
            amount,
            Some(blockhash),
            from_balance,
            from_pubkey,
            fee_pubkey,
            from_rent_exempt_minimum,
            compute_unit_limit,
            build_message,
        )
        .await?;
        if from_pubkey == fee_pubkey {
            if from_balance == 0 || from_balance < spend.saturating_add(fee) {
                return Err(CliError::InsufficientFundsForSpendAndFee(
                    build_balance_message(spend, false, false),
                    build_balance_message(fee, false, false),
                    *from_pubkey,
                ));
            }
        } else {
            if from_balance < spend {
                return Err(CliError::InsufficientFundsForSpend(
                    build_balance_message(spend, false, false),
                    *from_pubkey,
                ));
            }
            if !check_account_for_balance_with_commitment(rpc_client, fee_pubkey, fee, commitment)
                .await?
            {
                return Err(CliError::InsufficientFundsForFee(
                    build_balance_message(fee, false, false),
                    *fee_pubkey,
                ));
            }
        }
        Ok((message, spend))
    }
}

async fn resolve_spend_message<F>(
    rpc_client: &RpcClient,
    amount: SpendAmount,
    blockhash: Option<&Hash>,
    from_account_transferable_balance: u64,
    from_pubkey: &Pubkey,
    fee_pubkey: &Pubkey,
    from_rent_exempt_minimum: u64,
    compute_unit_limit: ComputeUnitLimit,
    build_message: F,
) -> Result<(Message, SpendAndFee), CliError>
where
    F: Fn(u64) -> Message,
{
    let (fee, compute_unit_info) = match blockhash {
        Some(blockhash) => {
            // If the from account is the same as the fee payer, it's impossible
            // to give a correct amount for the simulation with `SpendAmount::All`
            // or `SpendAmount::RentExempt`.
            // To know how much to transfer, we need to know the transaction fee,
            // but the transaction fee is dependent on the amount of compute
            // units used, which requires simulation.
            // To get around this limitation, we simulate against an amount of
            // `0`, since there are few situations in which `SpendAmount` can
            // be `All` or `RentExempt` *and also* the from account is the fee
            // payer.
            let lamports = if from_pubkey == fee_pubkey {
                match amount {
                    SpendAmount::Some(lamports) => lamports,
                    SpendAmount::AllForAccountCreation {
                        create_account_min_balance,
                    } => create_account_min_balance,
                    SpendAmount::All | SpendAmount::Available | SpendAmount::RentExempt => 0,
                }
            } else {
                match amount {
                    SpendAmount::Some(lamports) => lamports,
                    SpendAmount::AllForAccountCreation { .. }
                    | SpendAmount::All
                    | SpendAmount::Available => from_account_transferable_balance,
                    SpendAmount::RentExempt => {
                        from_account_transferable_balance.saturating_sub(from_rent_exempt_minimum)
                    }
                }
            };
            let mut dummy_message = build_message(lamports);

            dummy_message.recent_blockhash = *blockhash;
            let compute_unit_info =
                if let UpdateComputeUnitLimitResult::UpdatedInstructionIndex(ix_index) =
                    simulate_and_update_compute_unit_limit(
                        &compute_unit_limit,
                        rpc_client,
                        &mut dummy_message,
                    )
                    .await?
                {
                    Some((ix_index, dummy_message.instructions[ix_index].data.clone()))
                } else {
                    None
                };
            (
                get_fee_for_messages(rpc_client, &[&dummy_message]).await?,
                compute_unit_info,
            )
        }
        None => (0, None), // Offline, cannot calculate fee
    };

    let (mut message, spend_and_fee) = match amount {
        SpendAmount::Some(lamports) => (
            build_message(lamports),
            SpendAndFee {
                spend: lamports,
                fee,
            },
        ),
        SpendAmount::All | SpendAmount::AllForAccountCreation { .. } | SpendAmount::Available => {
            let lamports = if from_pubkey == fee_pubkey {
                from_account_transferable_balance.saturating_sub(fee)
            } else {
                from_account_transferable_balance
            };
            (
                build_message(lamports),
                SpendAndFee {
                    spend: lamports,
                    fee,
                },
            )
        }
        SpendAmount::RentExempt => {
            let mut lamports = if from_pubkey == fee_pubkey {
                from_account_transferable_balance.saturating_sub(fee)
            } else {
                from_account_transferable_balance
            };
            lamports = lamports.saturating_sub(from_rent_exempt_minimum);
            (
                build_message(lamports),
                SpendAndFee {
                    spend: lamports,
                    fee,
                },
            )
        }
    };
    // After build message, update with correct compute units
    if let Some((ix_index, ix_data)) = compute_unit_info {
        message.instructions[ix_index].data = ix_data;
    }
    Ok((message, spend_and_fee))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::json,
        solana_account::Account,
        solana_account_decoder::{UiAccountEncoding, encode_ui_account},
        solana_rpc_client_api::{
            request::RpcRequest,
            response::{Response, RpcResponseContext},
        },
        solana_vote_program::{
            vote_instruction::withdraw,
            vote_state::{VoteStateV4, VoteStateVersions},
        },
        std::collections::HashMap,
    };

    fn vote_account(lamports: u64, pending_delegator_rewards: u64) -> Account {
        let vote_state = VoteStateV4 {
            pending_delegator_rewards,
            ..VoteStateV4::default()
        };
        let mut data = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&VoteStateVersions::new_v4(vote_state), &mut data).unwrap();
        Account {
            lamports,
            data,
            owner: solana_sdk_ids::vote::id(),
            ..Account::default()
        }
    }

    /// Resolves the amount that `withdraw-from-vote-account ALL` withdraws from
    /// `vote_account`, with the fee paid by another account.
    async fn resolve_rent_exempt_spend(vote_account: Account, rent_exempt_minimum: u64) -> u64 {
        let vote_account_pubkey = Pubkey::new_unique();
        let account_response = json!(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None
            },
            value: json!(encode_ui_account(
                &vote_account_pubkey,
                &vote_account,
                UiAccountEncoding::Base64,
                None,
                None,
            )),
        });
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetAccountInfo, account_response);
        mocks.insert(
            RpcRequest::GetMinimumBalanceForRentExemption,
            json!(rent_exempt_minimum),
        );
        let rpc_client = RpcClient::new_mock_with_mocks("".to_string(), mocks);

        let withdraw_authority = Pubkey::new_unique();
        let destination_pubkey = Pubkey::new_unique();
        let fee_payer = Pubkey::new_unique();
        let (_message, spend) = resolve_spend_tx_and_check_account_balances(
            &rpc_client,
            false,
            SpendAmount::RentExempt,
            &Hash::default(),
            &vote_account_pubkey,
            &fee_payer,
            ComputeUnitLimit::Default,
            |lamports| {
                Message::new(
                    &[withdraw(
                        &vote_account_pubkey,
                        &withdraw_authority,
                        lamports,
                        &destination_pubkey,
                    )],
                    Some(&fee_payer),
                )
            },
            CommitmentConfig::processed(),
        )
        .await
        .unwrap();
        spend
    }

    #[tokio::test]
    async fn test_rent_exempt_spend_reserves_pending_delegator_rewards() {
        const RENT_EXEMPT_MINIMUM: u64 = 27_000_000;
        const PENDING_DELEGATOR_REWARDS: u64 = 5_000_000;
        const WITHDRAWABLE: u64 = 1_000_000;
        const RESERVED_BALANCE: u64 = RENT_EXEMPT_MINIMUM + PENDING_DELEGATOR_REWARDS;
        const BALANCE: u64 = RESERVED_BALANCE + WITHDRAWABLE;
        const BALANCE_WITHOUT_PENDING_REWARDS: u64 = RENT_EXEMPT_MINIMUM + WITHDRAWABLE;

        // Without pending rewards, everything in excess of the rent-exempt
        // minimum is withdrawable.
        assert_eq!(
            resolve_rent_exempt_spend(
                vote_account(BALANCE_WITHOUT_PENDING_REWARDS, 0),
                RENT_EXEMPT_MINIMUM,
            )
            .await,
            WITHDRAWABLE
        );

        // Pending delegator rewards are reserved as well, matching the balance
        // the vote program requires the account to retain.
        assert_eq!(
            resolve_rent_exempt_spend(
                vote_account(BALANCE, PENDING_DELEGATOR_REWARDS),
                RENT_EXEMPT_MINIMUM,
            )
            .await,
            WITHDRAWABLE
        );

        // Nothing is withdrawable when the balance only covers the reserves.
        assert_eq!(
            resolve_rent_exempt_spend(
                vote_account(RESERVED_BALANCE, PENDING_DELEGATOR_REWARDS),
                RENT_EXEMPT_MINIMUM,
            )
            .await,
            0
        );
    }
}
