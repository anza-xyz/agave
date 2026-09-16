//! Borrowed mirrors of the transaction status metadata types from
//! `solana-transaction-status-client-types`.
//!
//! These types mirror `TransactionStatusMeta` and the types it embeds,
//! defined here so the plugin interface does not depend on agave-internal
//! crates. Variable-length data is exposed as slices rather than owned
//! `Vec`s: `Vec` is `#[repr(Rust)]` with no stable layout, so only
//! references to slices cross the plugin boundary. Types that already come
//! from stable published crates (`CompiledInstruction`, `LoadedAddresses`,
//! `TransactionError`, `RewardType`) are borrowed as-is rather than
//! mirrored. The conversion happens at the agave boundary
//! (`solana-geyser-plugin-manager`) before a plugin is notified; if the
//! internal types drift, that conversion fails to compile rather than
//! silently breaking plugins.

use {
    solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses},
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
    solana_transaction_error::TransactionError,
};

/// A duplicate representation of a token amount.
///
/// Mirrors `solana_account_decoder_client_types::token::UiTokenAmount`.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct UiTokenAmount<'a> {
    /// The token amount scaled by decimals, when representable as an f64.
    pub ui_amount: Option<f64>,
    /// The number of decimals of the token.
    pub decimals: u8,
    /// The raw token amount as a string.
    pub amount: &'a str,
    /// The scaled token amount as a string.
    pub ui_amount_string: &'a str,
}

/// A duplicate representation of a transaction token balance.
///
/// Mirrors `solana_transaction_status_client_types::TransactionTokenBalance`.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct TransactionTokenBalance<'a> {
    /// Index of the account in the transaction.
    pub account_index: u8,
    /// The token mint address.
    pub mint: &'a str,
    /// The token amount.
    pub ui_token_amount: UiTokenAmount<'a>,
    /// The account owner address.
    pub owner: &'a str,
    /// The token program id.
    pub program_id: &'a str,
}

/// An inner instruction paired with its invocation stack height.
///
/// Mirrors `solana_transaction_status_client_types::InnerInstruction`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InnerInstruction<'a> {
    /// The compiled instruction.
    pub instruction: &'a CompiledInstruction,
    /// Invocation stack height of the instruction.
    pub stack_height: Option<u32>,
}

/// The inner instructions invoked by one top-level instruction.
///
/// Mirrors `solana_transaction_status_client_types::InnerInstructions`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InnerInstructions<'a> {
    /// The index of the top-level transaction instruction.
    pub index: u8,
    /// The inner instructions it invoked.
    pub instructions: &'a [InnerInstruction<'a>],
}

/// A reward credited to an account in a block.
///
/// Mirrors `solana_transaction_status_client_types::Reward`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Reward<'a> {
    /// The address the reward was credited to, base-58 encoded.
    pub pubkey: &'a str,
    /// The signed change in lamports.
    pub lamports: i64,
    /// Account balance in lamports after `lamports` was applied.
    pub post_balance: u64,
    /// The type of the reward.
    pub reward_type: Option<RewardType>,
    /// Vote account commission when the reward was credited, only present
    /// for voting and staking rewards.
    pub commission: Option<u8>,
    /// Vote account commission in basis points (SIMD-0291).
    pub commission_bps: Option<u16>,
}

/// A block's rewards together with the number of reward distribution
/// partitions.
///
/// Mirrors `solana_transaction_status::RewardsAndNumPartitions`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct RewardsAndNumPartitions<'a> {
    /// The rewards credited in the block.
    pub rewards: &'a [Reward<'a>],
    /// The number of partitions the epoch rewards are distributed over.
    pub num_partitions: Option<u64>,
}

/// Return data emitted by a transaction.
///
/// Mirrors `solana_transaction_context::transaction::TransactionReturnData`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct TransactionReturnData<'a> {
    /// The program that emitted the return data.
    pub program_id: Pubkey,
    /// The return data.
    pub data: &'a [u8],
}

/// Metadata of a transaction's execution status.
///
/// Mirrors `solana_transaction_status_client_types::TransactionStatusMeta`.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct TransactionStatusMeta<'a> {
    /// The execution result of the transaction.
    pub status: Result<(), &'a TransactionError>,
    /// The fee charged for the transaction.
    pub fee: u64,
    /// Account balances in lamports before the transaction.
    pub pre_balances: &'a [u64],
    /// Account balances in lamports after the transaction.
    pub post_balances: &'a [u64],
    /// The inner instructions invoked by the transaction, when recorded.
    pub inner_instructions: Option<&'a [InnerInstructions<'a>]>,
    /// The log messages emitted by the transaction, when recorded.
    pub log_messages: Option<&'a [&'a str]>,
    /// Token balances before the transaction, when recorded.
    pub pre_token_balances: Option<&'a [TransactionTokenBalance<'a>]>,
    /// Token balances after the transaction, when recorded.
    pub post_token_balances: Option<&'a [TransactionTokenBalance<'a>]>,
    /// Rewards credited by the transaction, when recorded.
    pub rewards: Option<&'a [Reward<'a>]>,
    /// Addresses loaded from address lookup tables.
    pub loaded_addresses: &'a LoadedAddresses,
    /// Return data emitted by the transaction, when present.
    pub return_data: Option<TransactionReturnData<'a>>,
    /// Compute units consumed by the transaction, when recorded.
    pub compute_units_consumed: Option<u64>,
    /// Cost units of the transaction, when recorded.
    pub cost_units: Option<u64>,
}
