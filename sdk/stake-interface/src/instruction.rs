// Remove the following `allow` when the `Redelegate` variant is renamed to
// `Unused` starting from v3.
// Required to avoid warnings from uses of deprecated types during trait derivations.
#![allow(deprecated)]

use {
    crate::state::{Authorized, Lockup, StakeAuthorize},
    solana_clock::{Epoch, UnixTimestamp},
    solana_pubkey::Pubkey,
};
#[cfg(feature = "bincode")]
use {
    crate::{config, state::StakeStateV2},
    solana_instruction::{AccountMeta, Instruction},
    solana_sdk_ids::sysvar::{
        clock::ID as CLOCK_ID, rent::ID as RENT_ID, stake_history::ID as STAKE_HISTORY_ID,
    },
    solana_system_interface::program::ID,
};

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum StakeInstruction {
    /// Initialize a stake with lockup and authorization information
    ///
    /// # Account references
    ///   0. `[WRITE]` Uninitialized stake account
    ///   1. `[]` Rent sysvar
    ///
    /// [`Authorized`] carries pubkeys that must sign staker transactions
    /// and withdrawer transactions; [`Lockup`] carries information about
    /// withdrawal restrictions.
    Initialize(Authorized, Lockup),

    /// Authorize a key to manage stake or withdrawal
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account to be updated
    ///   1. `[]` Clock sysvar
    ///   2. `[SIGNER]` The stake or withdraw authority
    ///   3. Optional: `[SIGNER]` Lockup authority, if updating StakeAuthorize::Withdrawer before
    ///      lockup expiration
    Authorize(Pubkey, StakeAuthorize),

    /// Delegate a stake to a particular vote account
    ///
    /// # Account references
    ///   0. `[WRITE]` Initialized stake account to be delegated
    ///   1. `[]` Vote account to which this stake will be delegated
    ///   2. `[]` Clock sysvar
    ///   3. `[]` Stake history sysvar that carries stake warmup/cooldown history
    ///   4. `[]` Unused account, formerly the stake config
    ///   5. `[SIGNER]` Stake authority
    ///
    /// The entire balance of the staking account is staked. `DelegateStake`
    /// can be called multiple times, but re-delegation is delayed by one epoch.
    DelegateStake,

    /// Split `u64` tokens and stake off a stake account into another stake account.
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account to be split; must be in the Initialized or Stake state
    ///   1. `[WRITE]` Uninitialized stake account that will take the split-off amount
    ///   2. `[SIGNER]` Stake authority
    Split(u64),

    /// Withdraw unstaked lamports from the stake account
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account from which to withdraw
    ///   1. `[WRITE]` Recipient account
    ///   2. `[]` Clock sysvar
    ///   3. `[]` Stake history sysvar that carries stake warmup/cooldown history
    ///   4. `[SIGNER]` Withdraw authority
    ///   5. Optional: `[SIGNER]` Lockup authority, if before lockup expiration
    ///
    /// The `u64` is the portion of the stake account balance to be withdrawn,
    /// must be `<= StakeAccount.lamports - staked_lamports`.
    Withdraw(u64),

    /// Deactivates the stake in the account
    ///
    /// # Account references
    ///   0. `[WRITE]` Delegated stake account
    ///   1. `[]` Clock sysvar
    ///   2. `[SIGNER]` Stake authority
    Deactivate,

    /// Set stake lockup
    ///
    /// If a lockup is not active, the withdraw authority may set a new lockup
    /// If a lockup is active, the lockup custodian may update the lockup parameters
    ///
    /// # Account references
    ///   0. `[WRITE]` Initialized stake account
    ///   1. `[SIGNER]` Lockup authority or withdraw authority
    SetLockup(LockupArgs),

    /// Merge two stake accounts.
    ///
    /// Both accounts must have identical lockup and authority keys. A merge
    /// is possible between two stakes in the following states with no additional
    /// conditions:
    ///
    /// * two deactivated stakes
    /// * an inactive stake into an activating stake during its activation epoch
    ///
    /// For the following cases, the voter pubkey and vote credits observed must match:
    ///
    /// * two activated stakes
    /// * two activating accounts that share an activation epoch, during the activation epoch
    ///
    /// All other combinations of stake states will fail to merge, including all
    /// "transient" states, where a stake is activating or deactivating with a
    /// non-zero effective stake.
    ///
    /// # Account references
    ///   0. `[WRITE]` Destination stake account for the merge
    ///   1. `[WRITE]` Source stake account for to merge.  This account will be drained
    ///   2. `[]` Clock sysvar
    ///   3. `[]` Stake history sysvar that carries stake warmup/cooldown history
    ///   4. `[SIGNER]` Stake authority
    Merge,

    /// Authorize a key to manage stake or withdrawal with a derived key
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account to be updated
    ///   1. `[SIGNER]` Base key of stake or withdraw authority
    ///   2. `[]` Clock sysvar
    ///   3. Optional: `[SIGNER]` Lockup authority, if updating [`StakeAuthorize::Withdrawer`]
    ///      before lockup expiration
    AuthorizeWithSeed(AuthorizeWithSeedArgs),

    /// Initialize a stake with authorization information
    ///
    /// This instruction is similar to `Initialize` except that the withdraw authority
    /// must be a signer, and no lockup is applied to the account.
    ///
    /// # Account references
    ///   0. `[WRITE]` Uninitialized stake account
    ///   1. `[]` Rent sysvar
    ///   2. `[]` The stake authority
    ///   3. `[SIGNER]` The withdraw authority
    InitializeChecked,

    /// Authorize a key to manage stake or withdrawal
    ///
    /// This instruction behaves like `Authorize` with the additional requirement that the new
    /// stake or withdraw authority must also be a signer.
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account to be updated
    ///   1. `[]` Clock sysvar
    ///   2. `[SIGNER]` The stake or withdraw authority
    ///   3. `[SIGNER]` The new stake or withdraw authority
    ///   4. Optional: `[SIGNER]` Lockup authority, if updating [`StakeAuthorize::Withdrawer`]
    ///      before lockup expiration
    AuthorizeChecked(StakeAuthorize),

    /// Authorize a key to manage stake or withdrawal with a derived key
    ///
    /// This instruction behaves like `AuthorizeWithSeed` with the additional requirement that
    /// the new stake or withdraw authority must also be a signer.
    ///
    /// # Account references
    ///   0. `[WRITE]` Stake account to be updated
    ///   1. `[SIGNER]` Base key of stake or withdraw authority
    ///   2. `[]` Clock sysvar
    ///   3. `[SIGNER]` The new stake or withdraw authority
    ///   4. Optional: `[SIGNER]` Lockup authority, if updating [`StakeAuthorize::Withdrawer`]
    ///      before lockup expiration
    AuthorizeCheckedWithSeed(AuthorizeCheckedWithSeedArgs),

    /// Set stake lockup
    ///
    /// This instruction behaves like `SetLockup` with the additional requirement that
    /// the new lockup authority also be a signer.
    ///
    /// If a lockup is not active, the withdraw authority may set a new lockup
    /// If a lockup is active, the lockup custodian may update the lockup parameters
    ///
    /// # Account references
    ///   0. `[WRITE]` Initialized stake account
    ///   1. `[SIGNER]` Lockup authority or withdraw authority
    ///   2. Optional: `[SIGNER]` New lockup authority
    SetLockupChecked(LockupCheckedArgs),

    /// Get the minimum stake delegation, in lamports
    ///
    /// # Account references
    ///   None
    ///
    /// Returns the minimum delegation as a little-endian encoded u64 value.
    /// Programs can use the [`get_minimum_delegation()`] helper function to invoke and
    /// retrieve the return value for this instruction.
    ///
    /// [`get_minimum_delegation()`]: crate::tools::get_minimum_delegation
    GetMinimumDelegation,

    /// Deactivate stake delegated to a vote account that has been delinquent for at least
    /// [`crate::MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION`] epochs.
    ///
    /// No signer is required for this instruction as it is a common good to deactivate abandoned
    /// stake.
    ///
    /// # Account references
    ///   0. `[WRITE]` Delegated stake account
    ///   1. `[]` Delinquent vote account for the delegated stake account
    ///   2. `[]` Reference vote account that has voted at least once in the last
    ///      [`crate::MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION`] epochs
    DeactivateDelinquent,

    /// Redelegate activated stake to another vote account.
    ///
    /// Upon success:
    ///   * the balance of the delegated stake account will be reduced to the undelegated amount in
    ///     the account (rent exempt minimum and any additional lamports not part of the delegation),
    ///     and scheduled for deactivation.
    ///   * the provided uninitialized stake account will receive the original balance of the
    ///     delegated stake account, minus the rent exempt minimum, and scheduled for activation to
    ///     the provided vote account. Any existing lamports in the uninitialized stake account
    ///     will also be included in the re-delegation.
    ///
    /// # Account references
    ///   0. `[WRITE]` Delegated stake account to be redelegated. The account must be fully
    ///      activated and carry a balance greater than or equal to the minimum delegation amount
    ///      plus rent exempt minimum
    ///   1. `[WRITE]` Uninitialized stake account that will hold the redelegated stake
    ///   2. `[]` Vote account to which this stake will be re-delegated
    ///   3. `[]` Unused account, formerly the stake config
    ///   4. `[SIGNER]` Stake authority
    ///
    #[deprecated(since = "2.1.0", note = "Redelegate will not be enabled")]
    Redelegate,

    /// Move stake between accounts with the same authorities and lockups, using Staker authority.
    ///
    /// The source account must be fully active. If its entire delegation is moved, it immediately
    /// becomes inactive. Otherwise, at least the minimum delegation of active stake must remain.
    ///
    /// The destination account must be fully active or fully inactive. If it is active, it must
    /// be delegated to the same vote account as the source. If it is inactive, it
    /// immediately becomes active, and must contain at least the minimum delegation. The
    /// destination must be pre-funded with the rent-exempt reserve.
    ///
    /// This instruction only affects or moves active stake. Additional unstaked lamports are never
    /// moved, activated, or deactivated, and accounts are never deallocated.
    ///
    /// # Account references
    ///   0. `[WRITE]` Active source stake account
    ///   1. `[WRITE]` Active or inactive destination stake account
    ///   2. `[SIGNER]` Stake authority
    ///
    /// The `u64` is the portion of the stake to move, which may be the entire delegation
    MoveStake(u64),

    /// Move unstaked lamports between accounts with the same authorities and lockups, using Staker
    /// authority.
    ///
    /// The source account must be fully active or fully inactive. The destination may be in any
    /// mergeable state (active, inactive, or activating, but not in warmup cooldown). Only lamports that
    /// are neither backing a delegation nor required for rent-exemption may be moved.
    ///
    /// # Account references
    ///   0. `[WRITE]` Active or inactive source stake account
    ///   1. `[WRITE]` Mergeable destination stake account
    ///   2. `[SIGNER]` Stake authority
    ///
    /// The `u64` is the portion of available lamports to move
    MoveLamports(u64),
}

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct LockupArgs {
    pub unix_timestamp: Option<UnixTimestamp>,
    pub epoch: Option<Epoch>,
    pub custodian: Option<Pubkey>,
}

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct LockupCheckedArgs {
    pub unix_timestamp: Option<UnixTimestamp>,
    pub epoch: Option<Epoch>,
}

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AuthorizeWithSeedArgs {
    pub new_authorized_pubkey: Pubkey,
    pub stake_authorize: StakeAuthorize,
    pub authority_seed: String,
    pub authority_owner: Pubkey,
}

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AuthorizeCheckedWithSeedArgs {
    pub stake_authorize: StakeAuthorize,
    pub authority_seed: String,
    pub authority_owner: Pubkey,
}

#[cfg(feature = "bincode")]
pub fn initialize(stake_pubkey: &Pubkey, authorized: &Authorized, lockup: &Lockup) -> Instruction {
    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::Initialize(*authorized, *lockup),
        vec![
            AccountMeta::new(*stake_pubkey, false),
            AccountMeta::new_readonly(RENT_ID, false),
        ],
    )
}

#[cfg(feature = "bincode")]
pub fn initialize_checked(stake_pubkey: &Pubkey, authorized: &Authorized) -> Instruction {
    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::InitializeChecked,
        vec![
            AccountMeta::new(*stake_pubkey, false),
            AccountMeta::new_readonly(RENT_ID, false),
            AccountMeta::new_readonly(authorized.staker, false),
            AccountMeta::new_readonly(authorized.withdrawer, true),
        ],
    )
}

#[cfg(feature = "bincode")]
pub fn create_account_with_seed(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    base: &Pubkey,
    seed: &str,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::create_account_with_seed(
            from_pubkey,
            stake_pubkey,
            base,
            seed,
            lamports,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        initialize(stake_pubkey, authorized, lockup),
    ]
}

#[cfg(feature = "bincode")]
pub fn create_account(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::create_account(
            from_pubkey,
            stake_pubkey,
            lamports,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        initialize(stake_pubkey, authorized, lockup),
    ]
}

#[cfg(feature = "bincode")]
pub fn create_account_with_seed_checked(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    base: &Pubkey,
    seed: &str,
    authorized: &Authorized,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::create_account_with_seed(
            from_pubkey,
            stake_pubkey,
            base,
            seed,
            lamports,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        initialize_checked(stake_pubkey, authorized),
    ]
}

#[cfg(feature = "bincode")]
pub fn create_account_checked(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    authorized: &Authorized,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::create_account(
            from_pubkey,
            stake_pubkey,
            lamports,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        initialize_checked(stake_pubkey, authorized),
    ]
}

#[cfg(feature = "bincode")]
fn _split(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new(*split_stake_pubkey, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    Instruction::new_with_bincode(ID, &StakeInstruction::Split(lamports), account_metas)
}

#[cfg(feature = "bincode")]
pub fn split(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::allocate(
            split_stake_pubkey,
            StakeStateV2::size_of() as u64,
        ),
        solana_system_interface::instruction::assign(split_stake_pubkey, &ID),
        _split(
            stake_pubkey,
            authorized_pubkey,
            lamports,
            split_stake_pubkey,
        ),
    ]
}

#[cfg(feature = "bincode")]
pub fn split_with_seed(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey, // derived using create_with_seed()
    base: &Pubkey,               // base
    seed: &str,                  // seed
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::allocate_with_seed(
            split_stake_pubkey,
            base,
            seed,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        _split(
            stake_pubkey,
            authorized_pubkey,
            lamports,
            split_stake_pubkey,
        ),
    ]
}

#[cfg(feature = "bincode")]
pub fn merge(
    destination_stake_pubkey: &Pubkey,
    source_stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
) -> Vec<Instruction> {
    let account_metas = vec![
        AccountMeta::new(*destination_stake_pubkey, false),
        AccountMeta::new(*source_stake_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(STAKE_HISTORY_ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    vec![Instruction::new_with_bincode(
        ID,
        &StakeInstruction::Merge,
        account_metas,
    )]
}

#[cfg(feature = "bincode")]
pub fn create_account_and_delegate_stake(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    let mut instructions = create_account(from_pubkey, stake_pubkey, authorized, lockup, lamports);
    instructions.push(delegate_stake(
        stake_pubkey,
        &authorized.staker,
        vote_pubkey,
    ));
    instructions
}

#[cfg(feature = "bincode")]
#[allow(clippy::too_many_arguments)]
pub fn create_account_with_seed_and_delegate_stake(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    base: &Pubkey,
    seed: &str,
    vote_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    let mut instructions = create_account_with_seed(
        from_pubkey,
        stake_pubkey,
        base,
        seed,
        authorized,
        lockup,
        lamports,
    );
    instructions.push(delegate_stake(
        stake_pubkey,
        &authorized.staker,
        vote_pubkey,
    ));
    instructions
}

#[cfg(feature = "bincode")]
pub fn authorize(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    new_authorized_pubkey: &Pubkey,
    stake_authorize: StakeAuthorize,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::Authorize(*new_authorized_pubkey, stake_authorize),
        account_metas,
    )
}

#[cfg(feature = "bincode")]
pub fn authorize_checked(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    new_authorized_pubkey: &Pubkey,
    stake_authorize: StakeAuthorize,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
        AccountMeta::new_readonly(*new_authorized_pubkey, true),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::AuthorizeChecked(stake_authorize),
        account_metas,
    )
}

#[cfg(feature = "bincode")]
pub fn authorize_with_seed(
    stake_pubkey: &Pubkey,
    authority_base: &Pubkey,
    authority_seed: String,
    authority_owner: &Pubkey,
    new_authorized_pubkey: &Pubkey,
    stake_authorize: StakeAuthorize,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*authority_base, true),
        AccountMeta::new_readonly(CLOCK_ID, false),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    let args = AuthorizeWithSeedArgs {
        new_authorized_pubkey: *new_authorized_pubkey,
        stake_authorize,
        authority_seed,
        authority_owner: *authority_owner,
    };

    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::AuthorizeWithSeed(args),
        account_metas,
    )
}

#[cfg(feature = "bincode")]
pub fn authorize_checked_with_seed(
    stake_pubkey: &Pubkey,
    authority_base: &Pubkey,
    authority_seed: String,
    authority_owner: &Pubkey,
    new_authorized_pubkey: &Pubkey,
    stake_authorize: StakeAuthorize,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*authority_base, true),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(*new_authorized_pubkey, true),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    let args = AuthorizeCheckedWithSeedArgs {
        stake_authorize,
        authority_seed,
        authority_owner: *authority_owner,
    };

    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::AuthorizeCheckedWithSeed(args),
        account_metas,
    )
}

#[cfg(feature = "bincode")]
pub fn delegate_stake(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*vote_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(STAKE_HISTORY_ID, false),
        // For backwards compatibility we pass the stake config, although this account is unused
        AccountMeta::new_readonly(config::ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];
    Instruction::new_with_bincode(ID, &StakeInstruction::DelegateStake, account_metas)
}

#[cfg(feature = "bincode")]
pub fn withdraw(
    stake_pubkey: &Pubkey,
    withdrawer_pubkey: &Pubkey,
    to_pubkey: &Pubkey,
    lamports: u64,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new(*to_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(STAKE_HISTORY_ID, false),
        AccountMeta::new_readonly(*withdrawer_pubkey, true),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    Instruction::new_with_bincode(ID, &StakeInstruction::Withdraw(lamports), account_metas)
}

#[cfg(feature = "bincode")]
pub fn deactivate_stake(stake_pubkey: &Pubkey, authorized_pubkey: &Pubkey) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(CLOCK_ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];
    Instruction::new_with_bincode(ID, &StakeInstruction::Deactivate, account_metas)
}

#[cfg(feature = "bincode")]
pub fn set_lockup(
    stake_pubkey: &Pubkey,
    lockup: &LockupArgs,
    custodian_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*custodian_pubkey, true),
    ];
    Instruction::new_with_bincode(ID, &StakeInstruction::SetLockup(*lockup), account_metas)
}

#[cfg(feature = "bincode")]
pub fn set_lockup_checked(
    stake_pubkey: &Pubkey,
    lockup: &LockupArgs,
    custodian_pubkey: &Pubkey,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*custodian_pubkey, true),
    ];

    let lockup_checked = LockupCheckedArgs {
        unix_timestamp: lockup.unix_timestamp,
        epoch: lockup.epoch,
    };
    if let Some(new_custodian) = lockup.custodian {
        account_metas.push(AccountMeta::new_readonly(new_custodian, true));
    }
    Instruction::new_with_bincode(
        ID,
        &StakeInstruction::SetLockupChecked(lockup_checked),
        account_metas,
    )
}

#[cfg(feature = "bincode")]
pub fn get_minimum_delegation() -> Instruction {
    Instruction::new_with_bincode(ID, &StakeInstruction::GetMinimumDelegation, Vec::default())
}

#[cfg(feature = "bincode")]
pub fn deactivate_delinquent_stake(
    stake_account: &Pubkey,
    delinquent_vote_account: &Pubkey,
    reference_vote_account: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_account, false),
        AccountMeta::new_readonly(*delinquent_vote_account, false),
        AccountMeta::new_readonly(*reference_vote_account, false),
    ];
    Instruction::new_with_bincode(ID, &StakeInstruction::DeactivateDelinquent, account_metas)
}

#[cfg(feature = "bincode")]
fn _redelegate(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
    uninitialized_stake_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new(*uninitialized_stake_pubkey, false),
        AccountMeta::new_readonly(*vote_pubkey, false),
        // For backwards compatibility we pass the stake config, although this account is unused
        AccountMeta::new_readonly(config::ID, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];
    Instruction::new_with_bincode(ID, &StakeInstruction::Redelegate, account_metas)
}

#[cfg(feature = "bincode")]
#[deprecated(since = "2.1.0", note = "Redelegate will not be enabled")]
pub fn redelegate(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
    uninitialized_stake_pubkey: &Pubkey,
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::allocate(
            uninitialized_stake_pubkey,
            StakeStateV2::size_of() as u64,
        ),
        solana_system_interface::instruction::assign(uninitialized_stake_pubkey, &ID),
        _redelegate(
            stake_pubkey,
            authorized_pubkey,
            vote_pubkey,
            uninitialized_stake_pubkey,
        ),
    ]
}

#[cfg(feature = "bincode")]
#[deprecated(since = "2.1.0", note = "Redelegate will not be enabled")]
pub fn redelegate_with_seed(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
    uninitialized_stake_pubkey: &Pubkey, // derived using create_with_seed()
    base: &Pubkey,                       // base
    seed: &str,                          // seed
) -> Vec<Instruction> {
    vec![
        solana_system_interface::instruction::allocate_with_seed(
            uninitialized_stake_pubkey,
            base,
            seed,
            StakeStateV2::size_of() as u64,
            &ID,
        ),
        _redelegate(
            stake_pubkey,
            authorized_pubkey,
            vote_pubkey,
            uninitialized_stake_pubkey,
        ),
    ]
}

#[cfg(feature = "bincode")]
pub fn move_stake(
    source_stake_pubkey: &Pubkey,
    destination_stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*source_stake_pubkey, false),
        AccountMeta::new(*destination_stake_pubkey, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    Instruction::new_with_bincode(ID, &StakeInstruction::MoveStake(lamports), account_metas)
}

#[cfg(feature = "bincode")]
pub fn move_lamports(
    source_stake_pubkey: &Pubkey,
    destination_stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*source_stake_pubkey, false),
        AccountMeta::new(*destination_stake_pubkey, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    Instruction::new_with_bincode(ID, &StakeInstruction::MoveLamports(lamports), account_metas)
}
